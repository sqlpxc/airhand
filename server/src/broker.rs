//! 信令与会话中转。
//!
//! 服务器只做三件事：
//! 1. 为被控端分配 6 位控制码（断线重连可取回原控制码；保存了控制码的被控端一直为它保留，见 [`crate::codes`]）；
//! 2. 控制端按控制码发起会话时通知被控端；
//! 3. 把双方的会话流按 `(会话 ID, 通道)` 配对后，以 `Bytes` 块为单位零拷贝透传；会话的数据报（声音，见
//!    `airhand_proto::datagram`）按开头的会话 ID 在两端之间原样转发（[`Route`]）。
//!
//! 服务器从不解析会话内容：会话是端到端加密的（见 `airhand_proto::handshake`），服务器只看会话头里的会话 ID、
//! 通道编号和优先级。会话有几条通道、每条是什么，服务器都不管（只限制数量，`airhand_proto::MAX_LANES`）：
//! 控制通道（0 号）两端都到了，会话就开始；它结束了，整个会话结束。所以会话层加通道、改格式都不用升级服务器。
//!
//! 版本握手见 `airhand_proto::signal` 的"版本握手"：服务器接受一个区间的信令版本，不要求相等。
//!
//! 防暴力猜码：控制码不存在算一次猜错（[`FAIL_LIMIT`]），并且晚 400 ms 回答。控制码存在、但被控端还没准备好
//! （正在注册、正在断开）时回答得一模一样（也算猜错、也晚回答），不能靠回答的快慢看出控制码存在。
//! 之前连上过的控制端带着重连凭证重连（见 `airhand_proto::signal` 模块说明的"重连凭证"）：凭证证明它本来就知道这个
//! 控制码，这些请求不算猜错、不受限速影响，被控端不在线时服务器记下它们（[`Waiter`]），被控端重新上线时通知它们重连。
//!
//! 注册不能用来探测控制码：取回控制码要出示密钥，密钥由服务器签名（[`Broker::issue_secret`]），
//! 控制码空闲时也只认服务器为**这个控制码**签发过的密钥，对不上就分配一个新的——回答和控制码有没有人用无关。
//! 新分配的控制码按来源限量（[`Config::new_codes_per_ip`]），防止批量占用控制码。
//!
//! 资源：每个来源同时最多 [`Config::max_conns_per_ip`] 条连接；QUIC 的流控窗口按"转发"来定（见 `main.rs`），
//! 不读数据的一方最多让服务器替它缓存几 MiB，而不是几百 MiB。

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use airhand_proto::framing::{read_frame, read_msg, write_frame, write_msg};
use airhand_proto::signal::{
    ClientHello, ClientMsg, ConnectError, Peer, Reclaim, ResumeToken, ServerHello, ServerMsg,
    SessionGrant,
};
use airhand_proto::{
    CODE_MAX, CODE_MIN, MAX_SIGNAL_MSG, Role, SIGNAL_MIN, SIGNAL_VERSION, SessionHeader, datagram,
    features,
};
use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream, VarInt};
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::codes::{Barrier, Bindings, Owner};

/// 被控端掉线后，控制码保留的时长（期间只有原设备能取回）。固定的控制码（`keep`）一直保留，见 [`crate::codes`]。
const RESERVE_TTL: Duration = Duration::from_secs(10 * 60);
/// 会话配对等待时长。
const PENDING_TTL: Duration = Duration::from_secs(20);
/// 失败尝试统计窗口与上限（防暴力猜码）。
const FAIL_WINDOW: Duration = Duration::from_secs(10 * 60);
const FAIL_LIMIT: usize = 20;
/// 新分配控制码的统计窗口（上限见 [`Config::new_codes_per_ip`]）
const NEW_CODE_WINDOW: Duration = Duration::from_secs(10 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 每条控制端连接最多同时等这么多个被控端上线，再多就忘掉最早的
const MAX_WAITING: usize = 32;
/// 绑定变化等待落盘的最长时间（超过了记日志，按内存里的状态继续）
const PERSIST_WAIT: Duration = Duration::from_secs(6);

/// 关闭连接的原因（QUIC 的应用错误码）
mod close {
    pub const REPLACED: u32 = 2;
    pub const SIGNAL: u32 = 3;
    pub const REGISTER_REFUSED: u32 = 4;
    pub const TOO_MANY_CONNECTIONS: u32 = 5;
}

pub struct Config {
    pub max_sessions_per_host: usize,
    /// 固定控制码的绑定表文件，`None` 表示只在内存里
    pub db: Option<std::path::PathBuf>,
    /// 同一个来源（[`limiter_key`]）最多同时有这么多条连接。网页控制台每个标签页一条，
    /// 一个公司的出口 IP 后面可能有很多台被控端，所以默认值比较宽
    pub max_conns_per_ip: usize,
    /// 同一个来源（[`registration_key`]）10 分钟内最多分配这么多个**新**控制码（取回自己原来的控制码不算）
    pub new_codes_per_ip: usize,
}

impl Config {
    pub const DEFAULT_MAX_CONNS_PER_IP: usize = 128;
    pub const DEFAULT_NEW_CODES_PER_IP: usize = 30;
}

/// Bounded signaling output. A non-reading peer is disconnected on overflow,
/// rather than retaining arbitrarily many replies or silently losing grants.
#[derive(Clone)]
struct SignalTx {
    tx: mpsc::Sender<ServerMsg>,
    conn: quinn::Connection,
}
impl SignalTx {
    fn send(&self, msg: ServerMsg) -> Result<(), ()> {
        self.tx.try_send(msg).map_err(|_| {
            self.conn
                .close(VarInt::from_u32(close::SIGNAL), b"signal queue overflow");
        })
    }
}

/// 被控端取回控制码用的密钥：现在的，以及换密钥之后被控端可能还没保存、所以暂时也认的旧密钥
#[derive(Clone, Copy)]
struct Secrets {
    cur: [u8; 16],
    prev: Option<[u8; 16]>,
}

impl Secrets {
    fn new(cur: [u8; 16]) -> Self {
        Self { cur, prev: None }
    }

    fn owner(&self, secret: &[u8; 16]) -> Owner {
        if ct_eq(&self.cur, secret) {
            Owner::Current
        } else if self.prev.is_some_and(|p| ct_eq(&p, secret)) {
            Owner::Previous
        } else {
            Owner::Other
        }
    }
}

struct HostEntry {
    conn_id: u64,
    ready: bool,
    secrets: Secrets,
    /// 被控端发过 [`ClientMsg::Release`]：断开后不再保留这个控制码
    released: bool,
    name: String,
    os: String,
    tx: SignalTx,
    conn: quinn::Connection,
    sessions: Arc<AtomicUsize>,
}

struct Reserved {
    secrets: Secrets,
    until: Instant,
}

/// 重连凭证的前一半：服务器为控制码 `code` 签的名（见 [`Broker::resume_token`]）
fn token_proof(key: &[u8; 32], code: u32) -> [u8; 8] {
    let mac = hmac_sha256(key, &[b"airhand resume proof v1\0", &code.to_le_bytes()]);
    mac[..8].try_into().unwrap()
}

/// 重连凭证的后一半：只对密钥 `secret` 有效（见 [`Broker::resume_token`]）
fn token_auth(code: u32, secret: &[u8; 16]) -> [u8; 8] {
    let mut h = Sha256::new();
    h.update(b"airhand resume token v2\0");
    h.update(code.to_le_bytes());
    h.update(secret);
    h.finalize()[..8].try_into().unwrap()
}

/// 比较凭证、密钥，用时与内容无关（不能靠测量回答的快慢逐字节猜出来）
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// HMAC-SHA256（RFC 2104）。密钥 32 字节，短于 SHA-256 的块长，直接补零
fn hmac_sha256(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for (i, k) in key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// 控制码密钥的签名：`密钥 = 随机数（8 字节）| HMAC(服务器密钥, 控制码 | 随机数) 的前 8 字节`
fn secret_mac(key: &[u8; 32], code: u32, nonce: &[u8]) -> [u8; 8] {
    let mac = hmac_sha256(
        key,
        &[b"airhand code secret v1\0", &code.to_le_bytes(), nonce],
    );
    mac[..8].try_into().unwrap()
}

/// 等被控端上线的控制端（带着本服务器为这个控制码发的凭证，回答过它 [`ConnectError::Offline`]）。
/// 被控端用这个控制码上线时发 [`ServerMsg::HostOnline`]，然后忘掉。凭证的后一半对不上（被控端换过密钥）的也通知：
/// 它重连时会收到 [`ConnectError::Revoked`]，而不是一直等下去
struct Waiter {
    conn_id: u64,
    tx: SignalTx,
}

/// 一个会话的两端（转发数据报用，见 [`Broker::relay_datagrams`]）
struct Route {
    host: (u64, quinn::Connection),
    ctrl: (u64, quinn::Connection),
}

impl Route {
    /// 连接 `conn_id` 发来的数据报转给谁；不是这个会话的两端时为 `None`
    fn peer_of(&self, conn_id: u64) -> Option<&quinn::Connection> {
        if conn_id == self.host.0 {
            Some(&self.ctrl.1)
        } else if conn_id == self.ctrl.0 {
            Some(&self.host.1)
        } else {
            None
        }
    }
}

/// 发起连接请求的控制端（一条连接）
struct Ctrl<'a> {
    conn_id: u64,
    /// 限速按这个来源统计（[`limiter_key`]）
    ip: IpAddr,
    peer: SocketAddr,
    name: &'a str,
    tx: &'a SignalTx,
}

/// 连接请求被拒绝的原因（发给控制端的是 [`Denied::error`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denied {
    /// 控制码不存在，或者被控端还没准备好（正在注册、正在断开）：算一次猜错，晚一点回答。
    /// 这两种情况必须答得一模一样，否则能靠回答的快慢看出控制码存在
    Unknown,
    /// 回答带凭证的请求：被控端不在线（凭证的前一半是本服务器为这个控制码签的，回答实情不泄露什么）
    Offline,
    /// 回答带凭证的请求：凭证作废了，或者不是本服务器为这个控制码发的
    Revoked,
    /// 猜错次数过多
    Limited,
    /// 对方会话数已满
    Busy,
}

impl Denied {
    fn error(self) -> ConnectError {
        match self {
            Denied::Unknown => ConnectError::NotFound,
            Denied::Offline => ConnectError::Offline,
            Denied::Revoked => ConnectError::Revoked,
            Denied::Limited => ConnectError::RateLimited,
            Denied::Busy => ConnectError::Busy,
        }
    }
}

/// 注册被拒绝的原因（关闭连接时告诉被控端）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// 这个来源短时间内申请了太多新控制码
    Limited,
    /// 控制码用完了
    Exhausted,
}

impl Refused {
    fn reason(self) -> &'static [u8] {
        match self {
            Refused::Limited => {
                b"too many new control codes from this network, try again in a few minutes"
            }
            Refused::Exhausted => b"the server has no free control codes left",
        }
    }
}

/// 会话槽位，Drop 时自动归还。
struct SlotGuard(Arc<AtomicUsize>);
impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一个来源的连接，Drop 时归还（见 [`Config::max_conns_per_ip`]）
struct ConnSlot<'a> {
    conns: &'a Mutex<HashMap<IpAddr, usize>>,
    key: IpAddr,
}
impl Drop for ConnSlot<'_> {
    fn drop(&mut self) {
        let mut conns = self.conns.lock().unwrap();
        if let Some(n) = conns.get_mut(&self.key) {
            *n -= 1;
            if *n == 0 {
                conns.remove(&self.key);
            }
        }
    }
}

/// 按来源统计一段时间内发生了几次（猜错的次数、新分配的控制码）
struct Limiter {
    window: Duration,
    hits: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl Limiter {
    fn new(window: Duration) -> Self {
        Self {
            window,
            hits: Mutex::default(),
        }
    }

    fn count(&self, key: IpAddr) -> usize {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        let Some(v) = hits.get_mut(&key) else {
            return 0;
        };
        while v.front().is_some_and(|t| now - *t >= self.window) {
            v.pop_front();
        }
        v.len()
    }

    fn hit(&self, key: IpAddr) {
        self.hits
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push_back(Instant::now());
    }

    fn cleanup(&self) {
        let now = Instant::now();
        self.hits.lock().unwrap().retain(|_, v| {
            while v.front().is_some_and(|t| now - *t >= self.window) {
                v.pop_front();
            }
            !v.is_empty()
        });
    }
}

type Stream = (SendStream, RecvStream);

/// 两端都到了的一条通道：`(通道编号, 被控端的流, 控制端的流)`
type LanePair = (u8, Stream, Stream);

/// 一个会话：从服务器批准开始，到控制通道结束为止
struct Pending {
    host_conn: u64,
    ctrl_conn: u64,
    /// 两端的连接：会话建立后按它们转发数据报
    host_link: quinn::Connection,
    ctrl_link: quinn::Connection,
    created: Instant,
    /// 只有一端到了的通道，在等另一端
    half: HashMap<u8, (Role, Stream)>,
    /// 已经配好的通道（第 i 位是 i 号通道）：同一条通道不能再开
    paired: u8,
    /// 两端都到了、在等控制通道的通道（会话还没开始）
    ready: Vec<LanePair>,
    /// 会话开始了（控制通道两端都到了）：之后配好的通道交给它（[`Broker::run_session`]）
    live: Option<mpsc::UnboundedSender<LanePair>>,
    /// 会话开始时交给 [`Broker::run_session`]
    slot: Option<SlotGuard>,
    label: String,
}

pub struct Broker {
    cfg: Config,
    /// 给控制码密钥签名的服务器密钥（随数据库保存，见 [`Bindings::server_key`]）
    key: [u8; 32],
    hosts: Mutex<HashMap<u32, HostEntry>>,
    reserved: Mutex<HashMap<u32, Reserved>>,
    pending: Mutex<HashMap<u64, Pending>>,
    /// 猜错控制码（按 [`limiter_key`]）
    failures: Limiter,
    /// 新分配的控制码（按 [`registration_key`]）
    new_codes: Limiter,
    /// 每个来源（[`limiter_key`]）现在有几条连接
    conns: Mutex<HashMap<IpAddr, usize>>,
    /// 固定的控制码。加锁顺序：hosts → reserved → bindings → waiters
    bindings: Mutex<Bindings>,
    /// 控制码 → 等它上线的控制端
    waiters: Mutex<HashMap<u32, Vec<Waiter>>>,
    /// 建立了的会话（会话 ID → 两端），转发数据报用
    routes: Mutex<HashMap<u64, Route>>,
    next_conn: AtomicU64,
}

impl Broker {
    pub fn new(cfg: Config) -> Arc<Self> {
        let bindings = Bindings::open(cfg.db.as_deref());
        Arc::new(Self {
            cfg,
            key: bindings.server_key(),
            hosts: Mutex::default(),
            reserved: Mutex::default(),
            pending: Mutex::default(),
            failures: Limiter::new(FAIL_WINDOW),
            new_codes: Limiter::new(NEW_CODE_WINDOW),
            conns: Mutex::default(),
            bindings: Mutex::new(bindings),
            waiters: Mutex::default(),
            routes: Mutex::default(),
            next_conn: AtomicU64::new(1),
        })
    }

    /// 控制码 `code`、密钥 `secret` 对应的重连凭证：
    ///
    /// * 前一半是服务器用自己的密钥给控制码签的名（[`token_proof`]）：证明拿着它的控制端连上过这个控制码，
    ///   服务器重启之后也认得，被控端不在线也能验证（[`Broker::vouches`]）；
    /// * 后一半由控制码和被控端的密钥算出（[`token_auth`]）：密钥只有服务器和被控端知道，所以只能从服务器那里得到，
    ///   也不能从它反推出密钥（取回控制码要用）。被控端取回控制码时密钥不变，凭证仍然有效（包括服务器重启之后）；
    ///   换了密钥（[`ClientMsg::Rekey`]）就作废了。
    fn resume_token(&self, code: u32, secret: &[u8; 16]) -> ResumeToken {
        let mut t = [0u8; 16];
        t[..8].copy_from_slice(&token_proof(&self.key, code));
        t[8..].copy_from_slice(&token_auth(code, secret));
        t
    }

    /// `token` 是不是本服务器给控制码 `code` 的控制端发的（不管现在还有没有效）
    fn vouches(&self, code: u32, token: &ResumeToken) -> bool {
        ct_eq(&token[..8], &token_proof(&self.key, code))
    }

    /// 为控制码 `code` 签发一个新的密钥（见 [`secret_mac`]）
    fn issue_secret(&self, code: u32) -> [u8; 16] {
        let nonce: [u8; 8] = rand::random();
        let mut s = [0u8; 16];
        s[..8].copy_from_slice(&nonce);
        s[8..].copy_from_slice(&secret_mac(&self.key, code, &nonce));
        s
    }

    /// `secret` 是不是本服务器为 `code` 签发的（服务器重启之后也认得）
    fn issued_here(&self, code: u32, secret: &[u8; 16]) -> bool {
        ct_eq(&secret[8..], &secret_mac(&self.key, code, &secret[..8]))
    }

    /// 周期性清理过期数据。
    pub async fn janitor(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut next_expiry = Instant::now();
        loop {
            tick.tick().await;
            let now = Instant::now();
            self.reserved.lock().unwrap().retain(|_, r| r.until > now);
            // 控制通道一直没配上的会话（开始了的会话由 run_session 自己清理）
            let expired: Vec<Pending> = {
                let mut p = self.pending.lock().unwrap();
                let ids: Vec<u64> = p
                    .iter()
                    .filter(|(_, v)| v.live.is_none() && now - v.created > PENDING_TTL)
                    .map(|(k, _)| *k)
                    .collect();
                ids.into_iter().filter_map(|id| p.remove(&id)).collect()
            };
            for p in expired {
                debug!("session pairing timed out: {}", p.label);
                let halves = p.half.into_values().map(|(_, s)| s);
                let pairs = p.ready.into_iter().flat_map(|(_, a, b)| [a, b]);
                for (mut s, mut r) in halves.chain(pairs) {
                    let _ = s.reset(VarInt::from_u32(1));
                    let _ = r.stop(VarInt::from_u32(1));
                }
            }
            self.failures.cleanup();
            self.new_codes.cleanup();
            if now >= next_expiry {
                let hosts = self.hosts.lock().unwrap();
                let mut bindings = self.bindings.lock().unwrap();
                bindings.expire(|code| hosts.contains_key(&code));
                next_expiry = now + Duration::from_secs(3600);
            }
        }
    }

    /// 等绑定表落盘（在任何 broker 锁之外等）。超时记日志，按内存里的状态继续
    async fn persisted(&self, barrier: Barrier) {
        if !barrier.wait(PERSIST_WAIT).await {
            warn!(
                "fixed control codes: persistence barrier did not complete; keeping in-memory ownership"
            );
        }
    }

    /// 等到现在为止的绑定变化都落盘（退出时调用）
    pub async fn flush_bindings(&self) {
        let barrier = self.bindings.lock().unwrap().barrier();
        self.persisted(barrier).await;
    }

    /// (在线的被控端, 等待配对的会话, 固定的控制码)
    pub fn stats(&self) -> (usize, usize, usize) {
        (
            self.hosts.lock().unwrap().len(),
            self.pending.lock().unwrap().len(),
            self.bindings.lock().unwrap().len(),
        )
    }

    /// 占用来源 `key` 的一个连接名额；满了返回 `None`
    fn admit(&self, key: IpAddr) -> Option<ConnSlot<'_>> {
        let mut conns = self.conns.lock().unwrap();
        let n = conns.entry(key).or_default();
        if *n >= self.cfg.max_conns_per_ip {
            return None;
        }
        *n += 1;
        Some(ConnSlot {
            conns: &self.conns,
            key,
        })
    }

    pub async fn handle_incoming(self: Arc<Self>, incoming: quinn::Incoming) {
        let peer = incoming.remote_address();
        let conn = match timeout(HANDSHAKE_TIMEOUT, incoming).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                debug!(%peer, "handshake failed: {e}");
                return;
            }
            Err(_) => {
                debug!(%peer, "handshake timed out");
                return;
            }
        };
        // 握手完成之后才算（地址已经验证过，伪造源地址占不了别人的名额）
        let Some(_slot) = self.admit(limiter_key(peer.ip())) else {
            info!(%peer, "too many connections from this network, refusing");
            conn.close(
                VarInt::from_u32(close::TOO_MANY_CONNECTIONS),
                b"too many connections from this network",
            );
            return;
        };
        if let Err(e) = self.handle_conn(conn.clone(), peer).await {
            debug!(%peer, "connection ended: {e:#}");
        }
        conn.close(VarInt::from_u32(0), b"bye");
    }

    async fn handle_conn(
        self: &Arc<Self>,
        conn: quinn::Connection,
        peer: SocketAddr,
    ) -> Result<()> {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (mut send, mut recv) = timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
            .await
            .context("timed out waiting for the control stream")??;
        let mut buf = Vec::new();
        let got = timeout(
            HANDSHAKE_TIMEOUT,
            read_frame(&mut recv, MAX_SIGNAL_MSG, &mut buf),
        )
        .await
        .context("timed out waiting for Hello")??;
        if !got {
            bail!("peer closed the stream");
        }
        // 版本握手（见 airhand_proto::signal 的"版本握手"）：任何版本的客户端都能读出它的开头
        let hello = ClientHello::decode(&buf).context("the first message must be Hello")?;
        let role = hello.role;
        let name: String = hello.name.chars().take(64).collect();
        let os: String = hello.os.chars().take(64).collect();
        let answer = hello.negotiate(SIGNAL_MIN, SIGNAL_VERSION, features::signal::SUPPORTED);
        write_frame(&mut send, &answer.encode()).await?;
        let ServerHello::Welcome { version, .. } = answer else {
            info!(%peer, min = hello.min_version, max = hello.version, "rejected client: no common signal version");
            let _ = send.finish();
            tokio::time::sleep(Duration::from_millis(500)).await;
            return Ok(());
        };
        info!(%peer, conn_id, ?role, %name, %os, version, "client connected");

        let (tx, rx) = mpsc::channel::<ServerMsg>(64);
        let tx = SignalTx {
            tx,
            conn: conn.clone(),
        };
        let writer_conn = conn.clone();
        let writer = tokio::spawn(async move {
            writer_loop(send, rx).await;
            writer_conn.close(VarInt::from_u32(close::SIGNAL), b"signal writer ended");
        });
        let acceptor = tokio::spawn(self.clone().accept_sessions(conn.clone(), conn_id, role));
        let datagrams = tokio::spawn(self.clone().relay_datagrams(conn.clone(), conn_id));

        // 被控端注册到的控制码：断开时按它直接找到自己的登记，不用遍历所有被控端
        let mut host_code = None;
        let res = match role {
            Role::Host => {
                let host = HostConn {
                    conn: &conn,
                    conn_id,
                    peer,
                    name: &name,
                    os: &os,
                    tx: &tx,
                };
                self.host_loop(&host, recv, &mut buf, &mut host_code).await
            }
            Role::Controller => {
                self.controller_loop(conn_id, peer, name, tx, recv, &mut buf)
                    .await
            }
        };

        acceptor.abort();
        datagrams.abort();
        writer.abort();
        if let Some(code) = host_code {
            self.unregister(conn_id, code);
        }
        info!(%peer, conn_id, ?role, "client disconnected");
        res
    }

    async fn host_loop(
        self: &Arc<Self>,
        host: &HostConn<'_>,
        mut recv: RecvStream,
        buf: &mut Vec<u8>,
        code: &mut Option<u32>,
    ) -> Result<()> {
        let peer = host.peer;
        let conn_id = host.conn_id;
        while let Some(msg) = read_msg::<_, ClientMsg>(&mut recv, MAX_SIGNAL_MSG, buf).await? {
            match (msg, *code) {
                (
                    ClientMsg::Register {
                        reclaim,
                        keep,
                        rekey,
                    },
                    None,
                ) => {
                    let (got, barrier) = match self.register(host, reclaim, keep, rekey) {
                        Ok(r) => r,
                        Err(refused) => {
                            info!(%peer, conn_id, ?refused, "registration refused");
                            host.conn
                                .close(VarInt::from_u32(close::REGISTER_REFUSED), refused.reason());
                            return Ok(());
                        }
                    };
                    *code = Some(got);
                    // 绑定有实质变化（新绑定、换了密钥）时，落盘之后才告诉被控端（在所有 broker 锁之外等）
                    if let Some(barrier) = barrier {
                        self.persisted(barrier).await;
                    }
                    self.announce(conn_id, got);
                    info!(%peer, conn_id, code = got, keep, rekey, reclaimed = reclaim.is_some_and(|r| r.code == got), "host registered");
                }
                (ClientMsg::Release, Some(c)) => {
                    if self.release(conn_id, c) {
                        info!(%peer, conn_id, code = c, "host released its control code");
                    }
                }
                (ClientMsg::Rekey, Some(c)) => {
                    let Some((secret, barrier)) = self.rekey(conn_id, c) else {
                        continue;
                    };
                    if let Some(barrier) = barrier {
                        self.persisted(barrier).await;
                    }
                    let _ = host.tx.send(ServerMsg::Rekeyed { secret });
                    info!(%peer, conn_id, code = c, "host changed its secret, earlier reconnect tokens are void");
                }
                (other, _) => warn!(%peer, "invalid message from host: {other:?}"),
            }
        }
        Ok(())
    }

    /// 分配（或者还回原来的）控制码并登记被控端（还不发布，见 [`Broker::announce`]）。
    /// `rekey`：还回原来的控制码时换一个密钥（被控端之前的 [`ClientMsg::Rekey`] 没等到回答，见 `ClientMsg::Register`）。
    /// 返回控制码，以及绑定有实质变化时要等它落盘的屏障
    fn register(
        &self,
        host: &HostConn<'_>,
        reclaim: Option<Reclaim>,
        keep: bool,
        rekey: bool,
    ) -> Result<(u32, Option<Barrier>), Refused> {
        let mut hosts = self.hosts.lock().unwrap();
        let mut reserved = self.reserved.lock().unwrap();
        let mut bindings = self.bindings.lock().unwrap();
        let now = Instant::now();

        let mut chosen: Option<(u32, Secrets)> = None;
        if let Some(r) = reclaim.filter(|r| (CODE_MIN..=CODE_MAX).contains(&r.code)) {
            let owner = if let Some(old) = hosts.get(&r.code) {
                old.secrets.owner(&r.secret)
            } else if let Some(res) = reserved.get(&r.code).filter(|res| res.until > now) {
                res.secrets.owner(&r.secret)
            } else {
                match bindings.check(r.code, &r.secret) {
                    // 空闲的控制码（服务器重启过，或者保留期过了、还没给别人）：只还给出示了本服务器为它签发的密钥的
                    Owner::Free if self.issued_here(r.code, &r.secret) => Owner::Current,
                    Owner::Free => Owner::Other,
                    owner => owner,
                }
            };
            let secrets = match owner {
                Owner::Current if !rekey => Some(Secrets::new(r.secret)),
                // 被控端没来得及保存上次换的密钥，或者请求换的密钥没等到回答（服务器可能没收到、或者没记下来就重启了）：
                // 签发一个新的给它，用它手里的密钥算出的重连凭证随之作废；它手里的密钥继续认到它保存了新的为止
                Owner::Current | Owner::Previous => Some(Secrets {
                    cur: self.issue_secret(r.code),
                    prev: Some(r.secret),
                }),
                Owner::Free | Owner::Other => None,
            };
            if let Some(secrets) = secrets {
                // 旧连接尚未超时（例如网络切换后立即重连）：顶替旧连接
                if let Some(old) = hosts.remove(&r.code) {
                    old.conn
                        .close(VarInt::from_u32(close::REPLACED), b"replaced");
                }
                reserved.remove(&r.code);
                chosen = Some((r.code, secrets));
            }
        }

        let (code, secrets) = match chosen {
            Some(chosen) => chosen,
            None => {
                let source = registration_key(host.peer.ip());
                if self.new_codes.count(source) >= self.cfg.new_codes_per_ip {
                    return Err(Refused::Limited);
                }
                let free = |c: &u32| {
                    !hosts.contains_key(c) && !reserved.contains_key(c) && !bindings.contains(*c)
                };
                let mut rng = rand::rng();
                // 随机挑几次（平常第一次就是空闲的），挑不到再从随机位置开始逐个找
                let code = (0..64)
                    .map(|_| rng.random_range(CODE_MIN..=CODE_MAX))
                    .find(free)
                    .or_else(|| {
                        let count = CODE_MAX - CODE_MIN + 1;
                        let start = rng.random_range(0..count);
                        (0..count)
                            .map(|i| CODE_MIN + (start + i) % count)
                            .find(free)
                    })
                    .ok_or(Refused::Exhausted)?;
                self.new_codes.hit(source);
                (code, Secrets::new(self.issue_secret(code)))
            }
        };
        let barrier = if keep {
            bindings
                .bind(code, &secrets.cur, secrets.prev.as_ref())
                .then(|| bindings.barrier())
        } else {
            // 这个控制码是它的（取回的或者新分配的），它不再保存控制码了：解除绑定
            bindings.remove(code);
            None
        };
        drop(bindings);

        hosts.insert(
            code,
            HostEntry {
                conn_id: host.conn_id,
                ready: false,
                secrets,
                released: false,
                name: host.name.to_string(),
                os: host.os.to_string(),
                tx: host.tx.clone(),
                conn: host.conn.clone(),
                sessions: Arc::new(AtomicUsize::new(0)),
            },
        );
        Ok((code, barrier))
    }

    fn announce(&self, conn_id: u64, code: u32) {
        let mut hosts = self.hosts.lock().unwrap();
        let Some(h) = hosts.get_mut(&code).filter(|h| h.conn_id == conn_id) else {
            return;
        };
        h.ready = true;
        let secret = h.secrets.cur;
        let tx = &h.tx;
        // 在通知控制端之前（并且还持有锁）发出：被控端收到的第一条消息必须是 Registered，
        // 控制端收到通知后马上发起的会话（Incoming）只能排在它后面
        let _ = tx.send(ServerMsg::Registered { code, secret });

        // 叫醒在等这个控制码的控制端（它们的凭证是本服务器为这个控制码发的）。凭证作废了的（被控端换过密钥、
        // 控制码换了主人）重连时会收到 Revoked，而不是一直等下去
        if let Some(list) = self.waiters.lock().unwrap().remove(&code) {
            for w in &list {
                debug!(
                    code,
                    ctrl = w.conn_id,
                    "telling a waiting controller that the host is back"
                );
                let _ = w.tx.send(ServerMsg::HostOnline { code });
            }
        }
    }

    /// 被控端断开：控制码保留一段时间（固定的控制码一直保留）
    fn unregister(&self, conn_id: u64, code: u32) {
        let mut hosts = self.hosts.lock().unwrap();
        // 被同一台被控端的新连接顶替了：登记已经是新连接的
        if !hosts.get(&code).is_some_and(|h| h.conn_id == conn_id) {
            return;
        }
        let h = hosts.remove(&code).unwrap();
        if h.released {
            return;
        }
        self.reserved.lock().unwrap().insert(
            code,
            Reserved {
                secrets: h.secrets,
                until: Instant::now() + RESERVE_TTL,
            },
        );
        self.bindings.lock().unwrap().touch(code);
    }

    /// 被控端放弃控制码：解除绑定，断开后也不保留
    fn release(&self, conn_id: u64, code: u32) -> bool {
        let mut hosts = self.hosts.lock().unwrap();
        let Some(h) = hosts.get_mut(&code).filter(|h| h.conn_id == conn_id) else {
            return false;
        };
        h.released = true;
        self.bindings.lock().unwrap().remove(code);
        true
    }

    /// 被控端的用户断开了所有控制端：换一个密钥，之前发出的重连凭证随之作废。
    /// 被控端手里的旧密钥继续认，直到它带着新密钥回来（它可能没来得及保存新的）。
    /// 返回新的密钥，以及固定的控制码要等它落盘的屏障
    fn rekey(&self, conn_id: u64, code: u32) -> Option<([u8; 16], Option<Barrier>)> {
        let mut hosts = self.hosts.lock().unwrap();
        let h = hosts
            .get_mut(&code)
            .filter(|h| h.conn_id == conn_id && !h.released)?;
        let secrets = Secrets {
            cur: self.issue_secret(code),
            prev: Some(h.secrets.cur),
        };
        h.secrets = secrets;
        let mut bindings = self.bindings.lock().unwrap();
        let barrier = bindings.contains(code).then(|| {
            bindings.bind(code, &secrets.cur, secrets.prev.as_ref());
            bindings.barrier()
        });
        Some((secrets.cur, barrier))
    }

    async fn controller_loop(
        self: &Arc<Self>,
        conn_id: u64,
        peer: SocketAddr,
        name: String,
        tx: SignalTx,
        mut recv: RecvStream,
        buf: &mut Vec<u8>,
    ) -> Result<()> {
        let ctrl = Ctrl {
            conn_id,
            ip: limiter_key(peer.ip()),
            peer,
            name: &name,
            tx: &tx,
        };
        // 这条连接在等哪些被控端上线（最早的在前面）
        let mut waiting: Vec<u32> = Vec::new();
        let result = async {
            while let Some(msg) = read_msg::<_, ClientMsg>(&mut recv, MAX_SIGNAL_MSG, buf).await? {
                let ClientMsg::Connect {
                    req_id,
                    code,
                    resume,
                } = msg
                else {
                    warn!(%peer, "invalid message from controller");
                    continue;
                };
                let result = self.connect(&ctrl, code, resume, &mut waiting);
                if let Err(denied) = result {
                    if denied == Denied::Unknown {
                        self.record_failure(ctrl.ip);
                        // 放慢猜码速度
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    info!(%peer, code, "connect request failed: {denied:?}");
                }
                let result = result.map_err(Denied::error);
                let _ = tx.send(ServerMsg::connect_result(req_id, result));
            }
            anyhow::Ok(())
        }
        .await;
        let mut all = self.waiters.lock().unwrap();
        for code in waiting {
            forget_waiter(&mut all, code, conn_id);
        }
        result
    }

    /// `resume` 是控制端带来的重连凭证（见 [`Broker::resume_token`]）。
    fn connect(
        &self,
        ctrl: &Ctrl,
        code: u32,
        resume: Option<ResumeToken>,
        waiting: &mut Vec<u32>,
    ) -> Result<SessionGrant, Denied> {
        let hosts = self.hosts.lock().unwrap();
        let host = match resume {
            Some(token) => {
                // 不是本服务器为这个控制码发的（伪造的、旧版本的、别的服务器的）：回答只取决于凭证本身，
                // 和控制码在不在、有没有人用都无关，所以凭证不能用来猜码
                if !self.vouches(code, &token) {
                    return Err(Denied::Revoked);
                }
                // 是本服务器为这个控制码发的：控制端本来就知道这个控制码，回答实情，不算猜错，也不必限速
                match hosts.get(&code) {
                    Some(h) if h.ready => {
                        if !ct_eq(&self.resume_token(code, &h.secrets.cur), &token) {
                            // 被控端换过密钥（断开过所有控制端）、控制码换了主人
                            return Err(Denied::Revoked);
                        }
                        h
                    }
                    // 不在线，或者正在上线：等它
                    _ => {
                        self.wait(ctrl, code, waiting);
                        return Err(Denied::Offline);
                    }
                }
            }
            None => {
                // 先检查限速再查控制码：被限速时不透露控制码是否存在
                if self.failure_count(ctrl.ip) >= FAIL_LIMIT {
                    return Err(Denied::Limited);
                }
                // 还没准备好的被控端（正在注册、正在断开）和不存在的控制码答得一样
                match hosts.get(&code) {
                    Some(h) if h.ready => h,
                    _ => return Err(Denied::Unknown),
                }
            }
        };
        // 预占一个会话槽位
        let prev = host.sessions.fetch_add(1, Ordering::AcqRel);
        let slot = SlotGuard(host.sessions.clone());
        if prev >= self.cfg.max_sessions_per_host {
            return Err(Denied::Busy);
        }
        let session_id = loop {
            let id: u64 = rand::rng().random();
            if id != 0 && !self.pending.lock().unwrap().contains_key(&id) {
                break id;
            }
        };
        let label = format!("{}@{} -> {code}", ctrl.name, ctrl.peer);
        self.pending.lock().unwrap().insert(
            session_id,
            Pending {
                host_conn: host.conn_id,
                ctrl_conn: ctrl.conn_id,
                host_link: host.conn.clone(),
                ctrl_link: ctrl.tx.conn.clone(),
                created: Instant::now(),
                half: HashMap::new(),
                paired: 0,
                ready: Vec::new(),
                live: None,
                slot: Some(slot),
                label: label.clone(),
            },
        );
        let incoming = ServerMsg::Incoming {
            session_id,
            peer: Peer {
                name: ctrl.name.to_string(),
                ip: ctrl.peer.ip(),
            },
        };
        if host.tx.send(incoming).is_err() {
            // 被控端正在断开
            self.pending.lock().unwrap().remove(&session_id);
            return Err(match resume {
                Some(_) => {
                    self.wait(ctrl, code, waiting);
                    Denied::Offline
                }
                None => Denied::Unknown,
            });
        }
        info!(session_id, "{label}: session created");
        Ok(SessionGrant {
            session_id,
            host_name: host.name.clone(),
            host_os: host.os.clone(),
            resume: self.resume_token(code, &host.secrets.cur),
        })
    }

    /// 记下 `ctrl` 在等 `code` 上线（调用方持有 hosts 锁，所以不会错过同时发生的上线）。
    /// `waiting` 是这条连接在等的控制码，最多 [`MAX_WAITING`] 个
    fn wait(&self, ctrl: &Ctrl, code: u32, waiting: &mut Vec<u32>) {
        let mut all = self.waiters.lock().unwrap();
        let list = all.entry(code).or_default();
        list.retain(|w| w.conn_id != ctrl.conn_id);
        list.push(Waiter {
            conn_id: ctrl.conn_id,
            tx: ctrl.tx.clone(),
        });
        waiting.retain(|c| *c != code);
        waiting.push(code);
        if waiting.len() > MAX_WAITING {
            let oldest = waiting.remove(0);
            forget_waiter(&mut all, oldest, ctrl.conn_id);
        }
    }

    fn failure_count(&self, ip: IpAddr) -> usize {
        self.failures.count(ip)
    }

    fn record_failure(&self, ip: IpAddr) {
        self.failures.hit(ip);
    }

    /// 转发连接 `conn_id` 发来的数据报：开头的会话 ID 是这条连接所在的会话，就原样转给会话的另一端，否则丢掉。
    /// 数据报本来就可能丢：另一端的发送缓冲满了、太大了，都直接丢掉
    async fn relay_datagrams(self: Arc<Self>, conn: quinn::Connection, conn_id: u64) {
        while let Ok(d) = conn.read_datagram().await {
            let Some(session_id) = datagram::session(&d) else {
                continue;
            };
            let to = self
                .routes
                .lock()
                .unwrap()
                .get(&session_id)
                .and_then(|r| r.peer_of(conn_id))
                .cloned();
            if let Some(to) = to {
                let _ = to.send_datagram(d);
            }
        }
    }

    async fn accept_sessions(self: Arc<Self>, conn: quinn::Connection, conn_id: u64, role: Role) {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                stream = conn.accept_bi() => {
                    let Ok((send, recv)) = stream else { break; };
                    let me = self.clone();
                    tasks.spawn(async move {
                        if let Err(e) = me.attach_stream(conn_id, role, send, recv).await {
                            debug!(conn_id, "session stream rejected: {e:#}");
                        }
                    });
                }
                Some(_) = tasks.join_next() => {}
            }
        }
    }

    async fn attach_stream(
        self: Arc<Self>,
        conn_id: u64,
        role: Role,
        send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        let mut hdr = [0u8; SessionHeader::LEN];
        timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut hdr))
            .await
            .context("timed out reading the session header")??;
        let reject = |mut send: SendStream, mut recv: RecvStream| {
            let _ = send.reset(VarInt::from_u32(1));
            let _ = recv.stop(VarInt::from_u32(1));
        };
        let Some(h) = SessionHeader::decode(&hdr).filter(|h| h.role == role) else {
            reject(send, recv);
            bail!("invalid session header");
        };

        let session_id = h.session_id;
        let mut pending = self.pending.lock().unwrap();
        let Some(p) = pending.get_mut(&session_id) else {
            drop(pending);
            reject(send, recv);
            bail!("no such session: {session_id}");
        };
        let expected = match role {
            Role::Host => p.host_conn,
            Role::Controller => p.ctrl_conn,
        };
        if expected != conn_id {
            drop(pending);
            reject(send, recv);
            bail!("the session doesn't belong to this connection");
        }
        // 这一端往这条通道写数据时用它自己在会话头里给的优先级
        send.set_priority(i32::from(h.priority))?;
        let mine = (send, recv);
        if p.paired & (1 << h.lane) != 0 {
            drop(pending);
            let (send, recv) = mine;
            reject(send, recv);
            bail!("session lane {} is already paired", h.lane);
        }
        let pair = match p.half.remove(&h.lane) {
            None => {
                p.half.insert(h.lane, (role, mine));
                return Ok(());
            }
            Some((other, theirs)) if other != role => match role {
                Role::Host => (h.lane, mine, theirs),
                Role::Controller => (h.lane, theirs, mine),
            },
            // 同一端把同一条通道开了两次
            Some(first) => {
                p.half.insert(h.lane, first);
                drop(pending);
                let (send, recv) = mine;
                reject(send, recv);
                bail!("duplicate session lane {}", h.lane);
            }
        };
        p.paired |= 1 << h.lane;
        if let Some(live) = &p.live {
            // 会话已经开始了：交给它转发（它已经结束的话，流随之关闭）
            let _ = live.send(pair);
            return Ok(());
        }
        if pair.0 != airhand_proto::SessionLane::Control as u8 {
            p.ready.push(pair);
            return Ok(());
        }
        // 控制通道两端都到了：会话开始
        let (tx, rx) = mpsc::unbounded_channel();
        p.live = Some(tx);
        let mut lanes = std::mem::take(&mut p.ready);
        lanes.push(pair);
        let slot = p.slot.take();
        let label = p.label.clone();
        let route = Route {
            host: (p.host_conn, p.host_link.clone()),
            ctrl: (p.ctrl_conn, p.ctrl_link.clone()),
        };
        drop(pending);
        info!(session_id, "{label}: session established");
        tokio::spawn(self.run_session(session_id, label, route, lanes, rx, slot));
        Ok(())
    }

    /// 一个开始了的会话：转发它的每条通道（`lanes` 是已经配好的，之后配好的从 `more` 来）和数据报，
    /// 直到控制通道结束。结束时停掉其它通道、不再转发数据报、归还会话槽位
    async fn run_session(
        self: Arc<Self>,
        session_id: u64,
        label: String,
        route: Route,
        lanes: Vec<LanePair>,
        mut more: mpsc::UnboundedReceiver<LanePair>,
        slot: Option<SlotGuard>,
    ) {
        self.routes.lock().unwrap().insert(session_id, route);
        let route = RouteGuard(self.clone(), session_id);
        let mut relays = tokio::task::JoinSet::new();
        let mut control = None;
        /// 开始转发一条通道，记下控制通道是哪个任务
        fn spawn(
            relays: &mut tokio::task::JoinSet<(u64, u64)>,
            control: &mut Option<tokio::task::Id>,
            (lane, host, ctrl): LanePair,
        ) {
            let id = relays.spawn(relay(host, ctrl)).id();
            if lane == airhand_proto::SessionLane::Control as u8 {
                *control = Some(id);
            }
        }
        for pair in lanes {
            spawn(&mut relays, &mut control, pair);
        }
        loop {
            tokio::select! {
                pair = more.recv() => match pair {
                    Some(pair) => spawn(&mut relays, &mut control, pair),
                    None => break,
                },
                done = relays.join_next_with_id() => match done {
                    // 控制通道结束了：整个会话结束
                    Some(Ok((id, _))) if Some(id) == control => break,
                    Some(Err(e)) if Some(e.id()) == control => break,
                    Some(_) => {}
                    None => break,
                },
            }
        }
        // 不再接受这个会话的通道；还在等另一端的通道随之关闭
        let p = self.pending.lock().unwrap().remove(&session_id);
        if let Some(p) = p {
            for (_, (mut s, mut r)) in p.half.into_values() {
                let _ = s.reset(VarInt::from_u32(0));
                let _ = r.stop(VarInt::from_u32(0));
            }
        }
        drop(relays);
        drop(route);
        info!(session_id, "{label}: session ended");
        drop(slot);
    }
}

/// 会话结束时不再转发它的数据报
struct RouteGuard(Arc<Broker>, u64);
impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.0.routes.lock().unwrap().remove(&self.1);
    }
}

async fn writer_loop(mut send: SendStream, mut rx: mpsc::Receiver<ServerMsg>) {
    while let Some(msg) = rx.recv().await {
        if write_msg(&mut send, &msg).await.is_err() {
            break;
        }
    }
}

/// 双向透传，任一方向结束即拆除整个会话。返回两个方向各自转发的字节数。
async fn relay(a: (SendStream, RecvStream), b: (SendStream, RecvStream)) -> (u64, u64) {
    let (mut a_tx, mut a_rx) = a;
    let (mut b_tx, mut b_rx) = b;
    let ab = Arc::new(AtomicU64::new(0));
    let ba = Arc::new(AtomicU64::new(0));
    tokio::select! {
        _ = pipe(&mut a_rx, &mut b_tx, &ab) => {},
        _ = pipe(&mut b_rx, &mut a_tx, &ba) => {},
    }
    for tx in [&mut a_tx, &mut b_tx] {
        let _ = tx.finish();
    }
    // 给对端一点时间读完已发送数据，然后彻底关闭
    tokio::time::sleep(Duration::from_millis(200)).await;
    for rx in [&mut a_rx, &mut b_rx] {
        let _ = rx.stop(VarInt::from_u32(0));
    }
    (ab.load(Ordering::Relaxed), ba.load(Ordering::Relaxed))
}

async fn pipe(rx: &mut RecvStream, tx: &mut SendStream, counter: &AtomicU64) -> Result<()> {
    while let Some(chunk) = rx.read_chunk(256 * 1024, true).await? {
        counter.fetch_add(chunk.bytes.len() as u64, Ordering::Relaxed);
        tx.write_chunk(chunk.bytes).await?;
    }
    Ok(())
}

/// 连接 `conn_id` 不再等 `code` 上线了
fn forget_waiter(all: &mut HashMap<u32, Vec<Waiter>>, code: u32, conn_id: u64) {
    if let Some(list) = all.get_mut(&code) {
        list.retain(|w| w.conn_id != conn_id);
        if list.is_empty() {
            all.remove(&code);
        }
    }
}

/// 被控端的一条连接（注册、换密钥时用）
struct HostConn<'a> {
    conn: &'a quinn::Connection,
    conn_id: u64,
    peer: SocketAddr,
    name: &'a str,
    os: &'a str,
    tx: &'a SignalTx,
}

/// 新分配控制码的限量按这个来源统计：IPv6 按 /48 归并（一个用户手里通常有一整个 /56 或 /48，
/// 按 /64 算的话换个子网就绕过去了；正常使用时新分配控制码很少，归并得粗一点也不会误伤）
fn registration_key(ip: IpAddr) -> IpAddr {
    match limiter_key(ip) {
        IpAddr::V6(v6) => {
            let mut seg = v6.segments();
            seg[3..].fill(0);
            IpAddr::V6(seg.into())
        }
        v4 => v4,
    }
}

/// IPv6 按 /64 归并，防止换地址绕过限速。
fn limiter_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => IpAddr::V4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut seg = v6.segments();
            seg[4..].fill(0);
            IpAddr::V6(seg.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use airhand_link::Problem;
    use airhand_link::controller::{
        ConnectOptions, ControllerService, Frontend, SessionEvent, SessionUi, UiClosed,
    };
    use airhand_link::host::{Access, Gate, HostSession, Incoming};
    use airhand_link::net::{LinkConfig, Signal, connect_signal};
    use airhand_link::secure::Identity;
    use airhand_proto::handshake::PublicKey;
    use airhand_proto::session::{ByeReason, QualitySettings};
    use airhand_proto::signal::connect_outcome;
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    use tokio::sync::watch;

    use super::*;

    /// 谁都让进来的被控端（测试用）
    struct OpenGate(Identity);

    impl Gate for OpenGate {
        fn identity(&self) -> Identity {
            self.0.clone()
        }
        fn access(&self) -> Access {
            Access::Open
        }
        fn pairing_code(&self) -> Option<String> {
            None
        }
        fn pairing_failed(&self) {}
        fn is_known(&self, _: &PublicKey) -> bool {
            false
        }
        fn paired(&self, _: PublicKey, _: &str) {}
    }

    /// 回应这个被控端收到的会话请求：端到端握手、打开会话。会话一直开着，直到任务被中止
    fn answer(sig: Signal, code: u32) -> tokio::task::JoinHandle<()> {
        let gate = OpenGate(Identity::generate());
        tokio::spawn(async move {
            let mut sessions = Vec::new();
            let (_kick_tx, kick) = watch::channel(ByeReason::Kicked);
            let mut buf = Vec::new();
            // 整个拿进来（只用到其中几个字段的话，别的字段在函数返回时就被丢掉了：信令流随之关闭）
            let mut sig = sig;
            while let Ok(Some(m)) =
                read_msg::<_, ServerMsg>(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf).await
            {
                if let ServerMsg::Incoming { session_id, peer } = m {
                    let inc = Incoming {
                        conn: sig.conn.clone(),
                        session_id,
                        code,
                        peer,
                        kick: kick.clone(),
                    };
                    if let Ok(s) = HostSession::open(&inc, 2, 16, &gate).await {
                        sessions.push(s);
                    }
                }
            }
        })
    }

    #[test]
    fn limiter_key_groups_addresses() {
        let key = |s: &str| limiter_key(s.parse().unwrap()).to_string();
        assert_eq!(key("203.0.113.9"), "203.0.113.9");
        // 同一个 /64 里换地址也算同一个来源
        assert_eq!(key("2001:db8:1:2:aaaa::1"), "2001:db8:1:2::");
        assert_eq!(key("2001:db8:1:2:bbbb::9"), "2001:db8:1:2::");
        // IPv4 映射地址按 IPv4 计
        assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
        // 新分配控制码的限量：IPv6 按 /48
        let reg = |s: &str| registration_key(s.parse().unwrap()).to_string();
        assert_eq!(reg("2001:db8:1:2:aaaa::1"), "2001:db8:1::");
        assert_eq!(reg("2001:db8:1:ff00::1"), "2001:db8:1::");
        assert_eq!(reg("::ffff:203.0.113.9"), "203.0.113.9");
    }

    #[test]
    fn resume_token_binds_code_and_secret() {
        let broker = Broker::new(test_config(None));
        let t = broker.resume_token(123_456, &[1; 16]);
        assert!(ct_eq(&t, &broker.resume_token(123_456, &[1; 16])));
        assert!(!ct_eq(&t, &broker.resume_token(123_457, &[1; 16])));
        assert!(!ct_eq(&t, &broker.resume_token(123_456, &[2; 16])));
        // 前一半只和控制码、服务器有关：换了密钥也认得是自己发的，别的控制码、别的服务器不认
        assert!(broker.vouches(123_456, &broker.resume_token(123_456, &[2; 16])));
        assert!(!broker.vouches(123_457, &t));
        assert!(!Broker::new(test_config(None)).vouches(123_456, &t));
        assert!(!broker.vouches(123_456, &[0; 16]));
        assert!(!ct_eq(&t, &t[..15]));
    }

    /// RFC 4231 测试用例 2（密钥 "Jefe" 补零到 32 字节，按 HMAC 的规则和原来一样）
    #[test]
    fn hmac_matches_rfc4231() {
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(b"Jefe");
        let mac = hmac_sha256(&key, &[b"what do ya ", b"want for nothing?"]);
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn secrets_are_signed_for_one_code() {
        let broker = Broker::new(test_config(None));
        let s = broker.issue_secret(123_456);
        assert!(broker.issued_here(123_456, &s));
        assert!(!broker.issued_here(123_457, &s));
        let mut forged = s;
        forged[15] ^= 1;
        assert!(!broker.issued_here(123_456, &forged));
        // 别的服务器（不同的密钥）签发的不认
        assert!(!Broker::new(test_config(None)).issued_here(123_456, &s));
    }

    fn test_config(db: Option<std::path::PathBuf>) -> Config {
        Config {
            max_sessions_per_host: 8,
            db,
            max_conns_per_ip: Config::DEFAULT_MAX_CONNS_PER_IP,
            new_codes_per_ip: 1000,
        }
    }

    /// 进程内的服务器（只监听 127.0.0.1），固定控制码只记在内存里
    async fn start() -> (Arc<Broker>, LinkConfig) {
        start_with(None).await
    }

    async fn start_with(db: Option<std::path::PathBuf>) -> (Arc<Broker>, LinkConfig) {
        start_cfg(test_config(db)).await
    }

    async fn start_cfg(cfg: Config) -> (Arc<Broker>, LinkConfig) {
        let ck = rcgen::generate_simple_self_signed(vec!["airhand".into()]).unwrap();
        let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
        let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()).into();
        let endpoint = quinn::Endpoint::server(
            crate::server_config(chain, key).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let server = endpoint.local_addr().unwrap().to_string();
        let broker = Broker::new(cfg);
        let b = broker.clone();
        tokio::spawn(async move {
            while let Some(incoming) = endpoint.accept().await {
                tokio::spawn(b.clone().handle_incoming(incoming));
            }
        });
        let cfg = LinkConfig::new(server, "", "test", "test").unwrap();
        (broker, cfg)
    }

    async fn next(sig: &mut Signal) -> ServerMsg {
        let mut buf = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(5),
            read_msg(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf),
        )
        .await
        .expect("timed out waiting for a server message")
        .unwrap()
        .expect("the server closed the control stream")
    }

    async fn register(cfg: &LinkConfig, reclaim: Option<Reclaim>) -> (Signal, Reclaim) {
        register_with(cfg, reclaim, false).await
    }

    async fn register_with(
        cfg: &LinkConfig,
        reclaim: Option<Reclaim>,
        keep: bool,
    ) -> (Signal, Reclaim) {
        register_full(cfg, reclaim, keep, false).await
    }

    async fn register_full(
        cfg: &LinkConfig,
        reclaim: Option<Reclaim>,
        keep: bool,
        rekey: bool,
    ) -> (Signal, Reclaim) {
        let mut sig = connect_signal(cfg, Role::Host).await.unwrap();
        write_msg(
            &mut sig.send,
            &ClientMsg::Register {
                reclaim,
                keep,
                rekey,
            },
        )
        .await
        .unwrap();
        match next(&mut sig).await {
            ServerMsg::Registered { code, secret } => (sig, Reclaim { code, secret }),
            other => panic!("{other:?}"),
        }
    }

    /// 发一个连接请求，下一条消息必须是它的回答
    async fn ask(
        ctrl: &mut Signal,
        req_id: u32,
        code: u32,
        resume: Option<ResumeToken>,
    ) -> Result<SessionGrant, ConnectError> {
        write_msg(
            &mut ctrl.send,
            &ClientMsg::Connect {
                req_id,
                code,
                resume,
            },
        )
        .await
        .unwrap();
        match next(ctrl).await {
            ServerMsg::ConnectResult {
                req_id: r,
                grant,
                error,
            } if r == req_id => connect_outcome(grant, error),
            other => panic!("{other:?}"),
        }
    }

    async fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn waiter_count(broker: &Broker) -> usize {
        broker.waiters.lock().unwrap().values().map(Vec::len).sum()
    }

    #[tokio::test]
    async fn blocked_video_lane_does_not_block_control_or_audio() {
        use airhand_link::transport::{FromHost, HostReceiver, open_session};
        use airhand_proto::framing::read_packet;
        use airhand_proto::media::{self, AudioHeader};
        use airhand_proto::session::HostMsg;
        let (_broker, cfg) = start().await;
        let (mut host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let grant = ask(&mut ctrl, 1, id.code, None).await.unwrap();
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
        let h = open_session(&host.conn, grant.session_id, Role::Host)
            .await
            .unwrap();
        let c = open_session(&ctrl.conn, grant.session_id, Role::Controller)
            .await
            .unwrap();
        let (mut host_control, _host_control_rx) = h.control;
        let (mut host_audio, _host_audio_rx) = h.audio;
        let (_ctrl_control_tx, mut ctrl_control) = c.control;
        let (_ctrl_audio_tx, mut ctrl_audio) = c.audio;
        let _video = c.video; // deliberately never read it
        let (mut video_tx, _video_rx) = h.video;
        let stalled = tokio::spawn(async move {
            let chunk = vec![0u8; 1024 * 1024];
            for _ in 0..128 {
                video_tx.write_all(&chunk).await.unwrap();
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !stalled.is_finished(),
            "video must be blocked by the unread lane"
        );
        write_msg(&mut host_control, &HostMsg::Pong { t: 77 })
            .await
            .unwrap();
        let mut buf = Vec::new();
        let msg: HostMsg = timeout(
            Duration::from_secs(2),
            read_msg(&mut ctrl_control, airhand_proto::MAX_CONTROL_MSG, &mut buf),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(matches!(msg, HostMsg::Pong { t: 77 }));
        let audio = AudioHeader {
            seq: 1,
            start: true,
            capture_us: 0,
        };
        host_audio
            .write_all(&media::frame(&audio.encode(), &[&[1]]))
            .await
            .unwrap();
        let packet = timeout(
            Duration::from_secs(2),
            read_packet(&mut ctrl_audio, media::MAX_AUDIO_PACKET),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(AudioHeader::decode(&packet), Some(audio));
        stalled.abort();
        let _ = stalled.await;
        // Also verify receiver cancellation does not damage framing, and clean
        // media EOF cannot hide a deliberate terminal control message.
        // 这一次是真正的会话：经服务器转发的流上做端到端握手，之后都是密文
        use airhand_link::secure::{self, InitiatorParams, ResponderParams};
        use airhand_link::transport::Openers;
        use airhand_proto::SessionLane;
        let grant = ask(&mut ctrl, 2, id.code, None).await.unwrap();
        let _ = next(&mut host).await;
        let h = open_session(&host.conn, grant.session_id, Role::Host)
            .await
            .unwrap();
        let c = open_session(&ctrl.conn, grant.session_id, Role::Controller)
            .await
            .unwrap();
        let gate = OpenGate(Identity::generate());
        let me = Identity::generate();
        let intro = secure::controller_intro("ctl", "test", QualitySettings::default(), false);
        let (mut ctl, mut ctl_rx) = h.control;
        let (mut csend, mut crecv) = c.control;
        let (init, resp) = tokio::join!(
            secure::initiate(
                &mut crecv,
                &mut csend,
                InitiatorParams {
                    session_id: grant.session_id,
                    code: id.code,
                    me: &me,
                    host_key: None,
                    pairing: None,
                    intro: &intro,
                },
            ),
            secure::respond(
                &mut ctl_rx,
                &mut ctl,
                ResponderParams {
                    session_id: grant.session_id,
                    code: id.code,
                    gate: &gate,
                },
            )
        );
        let (init, mut resp) = (init.unwrap(), resp.unwrap());
        let mut receiver = HostReceiver::new(
            crecv,
            c.bulk.1,
            c.video,
            c.audio,
            None,
            Openers {
                control: init.opener,
                bulk: init.secured.opener(SessionLane::Bulk),
                video: init.secured.opener(SessionLane::Video),
                audio: init.secured.opener(SessionLane::Audio),
                datagrams: init.secured.datagram_opener(),
            },
        );
        assert!(
            timeout(Duration::from_millis(10), receiver.recv())
                .await
                .is_err()
        );
        let (mut audio, _r2) = h.audio;
        let (mut video, _r3) = h.video;
        let mut bye = airhand_proto::framing::encode_with_spare(
            &HostMsg::Bye(ByeReason::Kicked),
            airhand_proto::handshake::TAG_LEN,
        );
        resp.sealer.seal_frame(&mut bye).unwrap();
        ctl.write_all(&bye).await.unwrap();
        ctl.finish().unwrap();
        audio.finish().unwrap();
        video.finish().unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .unwrap(),
            FromHost::Control(HostMsg::Bye(ByeReason::Kicked))
        ));
    }

    /// 带凭证的请求：不是本服务器为这个控制码发的凭证什么也不透露；被控端掉线期间一直重连不算猜码、
    /// 不受限速影响，被控端回来时马上收到通知
    #[tokio::test]
    async fn resume_and_wait_for_the_host() {
        let (broker, cfg) = start().await;
        let ip = limiter_key("127.0.0.1".parse().unwrap());
        let (host, id) = register(&cfg, None).await;
        let code = id.code;
        let other = if code == CODE_MAX { CODE_MIN } else { code + 1 };
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let token = ask(&mut ctrl, 1, code, None).await.unwrap().resume;

        // 不是为这个控制码发的凭证：不管控制码在不在线、存不存在都是同一个回答，不算猜错，也不等
        let mut stranger = connect_signal(&cfg, Role::Controller).await.unwrap();
        for (req, c, t) in [(1, code, [0; 16]), (2, other, [0; 16]), (3, other, token)] {
            assert_eq!(
                ask(&mut stranger, req, c, Some(t)).await.unwrap_err(),
                ConnectError::Revoked
            );
        }
        assert_eq!(broker.failure_count(ip), 0);
        assert_eq!(waiter_count(&broker), 0);

        // 被控端掉线
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        drop(host);

        // 这个 IP 已经被限速了，带凭证的重连照样不受影响，也不算猜错
        for _ in 0..FAIL_LIMIT {
            broker.record_failure(ip);
        }
        assert_eq!(
            ask(&mut ctrl, 10, other, None).await.unwrap_err(),
            ConnectError::RateLimited
        );
        for i in 0..5 {
            assert_eq!(
                ask(&mut ctrl, 11 + i, code, Some(token)).await.unwrap_err(),
                ConnectError::Offline
            );
        }
        assert_eq!(broker.failure_count(ip), FAIL_LIMIT);

        // 被控端取回控制码：在等的控制端收到通知（只一次），没在等的收不到
        let (_host, again) = register(&cfg, Some(id)).await;
        assert_eq!(again.code, code);
        assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code: c } if c == code));
        assert!(ask(&mut ctrl, 20, code, Some(token)).await.is_ok());
        // 下一条消息就是回答，说明 stranger 没有收到通知
        assert_eq!(
            ask(&mut stranger, 4, other, Some(token)).await.unwrap_err(),
            ConnectError::Revoked
        );

        // 每条连接最多等 MAX_WAITING 个被控端；连接断开后全部忘掉
        for i in 0..MAX_WAITING as u32 + 8 {
            let c = CODE_MIN + i;
            if c != code {
                let t = broker.resume_token(c, &[0; 16]);
                assert_eq!(
                    ask(&mut stranger, 100 + i, c, Some(t)).await.unwrap_err(),
                    ConnectError::Offline
                );
            }
        }
        assert_eq!(waiter_count(&broker), MAX_WAITING);
        stranger.conn.close(VarInt::from_u32(0), b"");
        wait_until("the waiters are forgotten", || waiter_count(&broker) == 0).await;
    }

    /// 控制端服务（airhand-link）：重连时自动带上凭证；被控端不在线时回答 HostOffline，
    /// 被控端回来时调用 host_online
    #[tokio::test]
    async fn controller_service_waits_for_the_host() {
        struct NoUi;
        impl SessionUi for NoUi {
            fn packet(&self, _: Vec<u8>) -> Result<(), UiClosed> {
                Ok(())
            }
            fn event(&self, _: SessionEvent) -> Result<(), UiClosed> {
                Ok(())
            }
        }

        let (broker, cfg) = start().await;
        let ip = limiter_key("127.0.0.1".parse().unwrap());
        let (host, id) = register(&cfg, None).await;
        let host_conn = host.conn.clone();
        let host = answer(host, id.code);
        let (online_tx, mut online_rx) = mpsc::unbounded_channel();
        let svc = ControllerService::start(
            Arc::new(cfg.clone()),
            tokio::runtime::Handle::current(),
            Arc::new(|_| {}),
            Arc::new(move |code| {
                let _ = online_tx.send(code);
            }),
            Identity::generate(),
        );
        let code = id.code.to_string();
        let connect = |resume| {
            svc.connect(
                &code,
                ConnectOptions {
                    resume,
                    ..Default::default()
                },
                Arc::new(NoUi),
                Frontend::Browser,
            )
        };
        let first = loop {
            match connect(None).await {
                // 控制端服务还没连上服务器
                Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(20)).await,
                r => break r,
            }
        };
        let token = Some(first.unwrap().resume);

        host_conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        host.abort();
        for _ in 0..FAIL_LIMIT {
            broker.record_failure(ip);
        }
        assert_eq!(connect(token).await.unwrap_err(), Problem::HostOffline);
        // 不带凭证：普通的连接请求，照常限速
        assert_eq!(connect(None).await.unwrap_err(), Problem::RateLimited);

        let (host, _) = register(&cfg, Some(id)).await;
        let _host = answer(host, id.code);
        let code = tokio::time::timeout(Duration::from_secs(5), online_rx.recv())
            .await
            .expect("host_online wasn't called")
            .unwrap();
        assert_eq!(code, id.code);
        assert!(connect(token).await.is_ok());
    }

    /// 测试用的数据库文件。放在 target/ 里：服务器的后台任务在测试结束前一直开着数据库，Windows 上删不掉
    fn temp_db() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../target/test-tmp")
            .join(format!("broker-{:016x}", rand::random::<u64>()));
        let file = dir.join("airhand.db");
        (dir, file)
    }

    /// 服务器重启之后（只剩数据库，内存里的东西都不记得了），控制端可能比被控端先回来：凭证照样可以用；
    /// 不保存控制码的被控端也能取回原来的控制码（密钥是这台服务器签发的）
    #[tokio::test]
    async fn resume_survives_a_server_restart() {
        let (dir, file) = temp_db();
        let (old, cfg) = start_with(Some(file.clone())).await;
        let (host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
        old.flush_bindings().await;
        drop((host, ctrl, old));

        let (_new, cfg) = start_with(Some(file)).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        assert_eq!(
            ask(&mut ctrl, 1, id.code, Some(token)).await.unwrap_err(),
            ConnectError::Offline
        );
        let (_host, again) = register(&cfg, Some(id)).await;
        assert_eq!(again, id);
        assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code } if code == id.code));
        assert!(ask(&mut ctrl, 2, id.code, Some(token)).await.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 注册不能用来探测控制码有没有人用：拿不出服务器为这个控制码签发的密钥，不管它有没有人用都分配一个新的
    #[tokio::test]
    async fn registration_does_not_reveal_whether_a_code_is_taken() {
        let (broker, cfg) = start().await;
        let (_victim, v) = register(&cfg, None).await;
        let free = if v.code == CODE_MAX {
            CODE_MIN
        } else {
            v.code + 1
        };
        assert!(!broker.hosts.lock().unwrap().contains_key(&free));
        for code in [v.code, free] {
            let (_s, got) = register(
                &cfg,
                Some(Reclaim {
                    code,
                    secret: [0xAB; 16],
                }),
            )
            .await;
            assert_ne!(got.code, code);
        }
        // 服务器为它签发的密钥：控制码空闲（例如保留期过了）时还给它
        let (host, own) = register(&cfg, None).await;
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || {
            !broker.hosts.lock().unwrap().contains_key(&own.code)
        })
        .await;
        broker.reserved.lock().unwrap().clear();
        let (_host, again) = register(&cfg, Some(own)).await;
        assert_eq!(again, own);
    }

    /// 本机用户断开所有控制端时换密钥：之前的重连凭证作废，控制码不变。
    /// 被控端用新密钥取回控制码；没来得及保存新密钥时，旧密钥也还认（同时再换一个），保存了之后旧的就不认了
    #[tokio::test]
    async fn rekey_voids_tokens_and_keeps_the_code() {
        let (dir, file) = temp_db();
        let (broker, cfg) = start_with(Some(file.clone())).await;
        let (mut host, id) = register_with(&cfg, None, true).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let old_token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));

        write_msg(&mut host.send, &ClientMsg::Rekey).await.unwrap();
        let ServerMsg::Rekeyed { secret } = next(&mut host).await else {
            panic!("expected Rekeyed");
        };
        assert_ne!(secret, id.secret);
        // 旧凭证作废了
        assert_eq!(
            ask(&mut ctrl, 2, id.code, Some(old_token))
                .await
                .unwrap_err(),
            ConnectError::Revoked
        );
        // 控制码本身照样能连，拿到的是新凭证
        let fresh = ask(&mut ctrl, 3, id.code, None).await.unwrap().resume;
        assert_ne!(fresh, old_token);
        assert!(ask(&mut ctrl, 4, id.code, Some(fresh)).await.is_ok());

        // 服务器重启（新的绑定在回答 Rekeyed 之前就落盘了）
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        broker.flush_bindings().await;
        drop((host, ctrl, broker));
        let (broker, cfg) = start_with(Some(file)).await;
        // 被控端没来得及保存新密钥，拿着旧的来：控制码还给它，并且换一个新的
        let (h1, again) = register_with(&cfg, Some(id), true).await;
        assert_eq!(again.code, id.code);
        assert!(again.secret != id.secret && again.secret != secret);
        h1.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        // 带着新密钥回来：之后旧密钥不再认
        let (h2, back) = register_with(&cfg, Some(again), true).await;
        assert_eq!(back, again);
        h2.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        broker.reserved.lock().unwrap().clear();
        let (_h3, stolen) = register_with(&cfg, Some(id), true).await;
        assert_ne!(stolen.code, id.code);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 被控端服务（airhand-link）：本机用户断开所有控制端时请服务器换密钥，并保存新密钥；被断开的控制端拿着旧凭证连不回来
    #[tokio::test]
    async fn host_service_rekeys_on_kick_all() {
        use airhand_link::host::{HostCmd, HostEvents};
        struct Events(mpsc::UnboundedSender<(Reclaim, bool)>);
        impl HostEvents for Events {
            fn server(&self, _: airhand_link::ServerState) {}
            async fn registered(&self, code: Reclaim, changed: bool) {
                let _ = self.0.send((code, changed));
            }
            async fn released(&self) {}
            fn incoming(&self, _: u64, _: &Peer) {}
            fn ended(&self, _: u64) {}
            fn offline(&self) {}
        }
        let (_broker, cfg) = start().await;
        let (tx, mut saved) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(airhand_link::host::run(
            Arc::new(cfg.clone()),
            true,
            None,
            Arc::new(Events(tx)),
            |_inc| async {},
            cmd_rx,
        ));
        let next_saved = async |saved: &mut mpsc::UnboundedReceiver<(Reclaim, bool)>| {
            timeout(Duration::from_secs(5), saved.recv())
                .await
                .expect("the host didn't save its code")
                .unwrap()
        };
        let (first, changed) = next_saved(&mut saved).await;
        assert!(changed);
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let token = ask(&mut ctrl, 1, first.code, None).await.unwrap().resume;

        cmd_tx.send(HostCmd::KickAll).unwrap();
        let (second, changed) = next_saved(&mut saved).await;
        assert!(changed);
        assert_eq!(second.code, first.code);
        assert_ne!(second.secret, first.secret);
        assert_eq!(
            ask(&mut ctrl, 2, first.code, Some(token))
                .await
                .unwrap_err(),
            ConnectError::Revoked
        );
        task.abort();
    }

    /// 被控端发了 Rekey 却没等到回答就断线了（服务器可能没收到、或者收到了还没记下就重启了）：
    /// 重新注册时带上 rekey，直到换成了为止（airhand-link 的被控端服务）
    #[tokio::test]
    async fn host_service_retries_an_unanswered_rekey() {
        use airhand_link::host::{HostCmd, HostEvents};
        struct Events;
        impl HostEvents for Events {
            fn server(&self, _: airhand_link::ServerState) {}
            async fn registered(&self, _: Reclaim, _: bool) {}
            async fn released(&self) {}
            fn incoming(&self, _: u64, _: &Peer) {}
            fn ended(&self, _: u64) {}
            fn offline(&self) {}
        }
        // 只会说信令的假服务器
        let ck = rcgen::generate_simple_self_signed(vec!["airhand".into()]).unwrap();
        let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
        let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()).into();
        let endpoint = quinn::Endpoint::server(
            crate::server_config(chain, key).unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        )
        .unwrap();
        let cfg =
            LinkConfig::new(endpoint.local_addr().unwrap().to_string(), "", "t", "t").unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        // 不保存控制码的被控端
        let task = tokio::spawn(airhand_link::host::run(
            Arc::new(cfg),
            false,
            None,
            Arc::new(Events),
            |_inc| async {},
            cmd_rx,
        ));
        let mut buf = Vec::new();
        // 接受被控端的一条连接，握手，返回它的注册请求
        let mut accept = async || {
            let incoming = timeout(Duration::from_secs(10), endpoint.accept())
                .await
                .expect("the host didn't (re)connect")
                .unwrap();
            let conn = incoming.await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            assert!(
                read_frame(&mut recv, MAX_SIGNAL_MSG, &mut buf)
                    .await
                    .unwrap()
            );
            let hello = ClientHello::decode(&buf).unwrap();
            assert_eq!(hello.role, Role::Host);
            write_frame(
                &mut send,
                &ServerHello::Welcome {
                    version: SIGNAL_VERSION,
                    features: airhand_proto::Features::NONE,
                }
                .encode(),
            )
            .await
            .unwrap();
            let reg: ClientMsg = read_msg(&mut recv, MAX_SIGNAL_MSG, &mut buf)
                .await
                .unwrap()
                .unwrap();
            (conn, send, recv, reg)
        };
        let (conn, mut send, mut recv, reg) = accept().await;
        assert!(
            matches!(
                reg,
                ClientMsg::Register {
                    reclaim: None,
                    rekey: false,
                    ..
                }
            ),
            "{reg:?}"
        );
        let id = Reclaim {
            code: 123_456,
            secret: [7; 16],
        };
        write_msg(
            &mut send,
            &ServerMsg::Registered {
                code: id.code,
                secret: id.secret,
            },
        )
        .await
        .unwrap();
        cmd_tx.send(HostCmd::KickAll).unwrap();
        let mut b = Vec::new();
        let rekey: ClientMsg = read_msg(&mut recv, MAX_SIGNAL_MSG, &mut b)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(rekey, ClientMsg::Rekey));
        // 还没回答就断了
        conn.close(VarInt::from_u32(0), b"gone");
        let (_conn, _send, _recv, reg) = accept().await;
        assert!(
            matches!(reg, ClientMsg::Register { reclaim: Some(r), keep: false, rekey: true } if r == id),
            "{reg:?}"
        );
        task.abort();
    }

    /// 不保存控制码的被控端发了 Rekey 却没等到回答（断线了），服务器又恰好重启（什么都没记下）：
    /// 被控端注册时带上 rekey，服务器还回控制码的同时换密钥，旧密钥不会"重新生效"、旧凭证也就恢复不了
    #[tokio::test]
    async fn rekey_on_register_survives_a_lost_answer_and_a_restart() {
        let (dir, file) = temp_db();
        let (old, cfg) = start_with(Some(file.clone())).await;
        let (host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
        old.flush_bindings().await;
        drop((host, ctrl, old));

        let (broker, cfg) = start_with(Some(file)).await;
        let (h1, again) = register_full(&cfg, Some(id), false, true).await;
        assert_eq!(again.code, id.code);
        assert_ne!(again.secret, id.secret);
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        assert_eq!(
            ask(&mut ctrl, 1, id.code, Some(token)).await.unwrap_err(),
            ConnectError::Revoked
        );
        // 新密钥又没收到、拿着旧的回来：控制码还给它，但再换一个，旧凭证一直作废
        h1.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        let (_h2, third) = register(&cfg, Some(id)).await;
        assert_eq!(third.code, id.code);
        assert!(third.secret != id.secret && third.secret != again.secret);
        assert_eq!(
            ask(&mut ctrl, 2, id.code, Some(token)).await.unwrap_err(),
            ConnectError::Revoked
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 我的设备里保存的凭证可能作废了（对方断开过所有控制端）：对方在线时回答 Revoked（控制端改用控制码直接连），
    /// 不在线时照样等它、对方上线时也通知，都不算猜错——从我的设备打开一台不在线的设备，不用再拿控制码试一次
    #[tokio::test]
    async fn stale_tokens_are_revoked_and_never_count_as_guesses() {
        let (broker, cfg) = start().await;
        let ip = limiter_key("127.0.0.1".parse().unwrap());
        let (mut host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let stale = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
        write_msg(&mut host.send, &ClientMsg::Rekey).await.unwrap();
        let ServerMsg::Rekeyed { secret } = next(&mut host).await else {
            panic!("expected Rekeyed");
        };
        assert_eq!(
            ask(&mut ctrl, 2, id.code, Some(stale)).await.unwrap_err(),
            ConnectError::Revoked
        );
        // 对方不在线：等它
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 0).await;
        drop(host);
        assert_eq!(
            ask(&mut ctrl, 3, id.code, Some(stale)).await.unwrap_err(),
            ConnectError::Offline
        );
        assert_eq!(broker.failure_count(ip), 0);
        // 对方回来了：照样通知；重连时才知道凭证作废了，改用控制码直接连
        let (_host, _) = register(
            &cfg,
            Some(Reclaim {
                code: id.code,
                secret,
            }),
        )
        .await;
        assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code } if code == id.code));
        assert_eq!(
            ask(&mut ctrl, 4, id.code, Some(stale)).await.unwrap_err(),
            ConnectError::Revoked
        );
        assert!(ask(&mut ctrl, 5, id.code, None).await.is_ok());
        assert_eq!(broker.failure_count(ip), 0);
    }

    /// 控制码存在、但被控端还没准备好（正在注册、正在断开）：回答和控制码不存在时一模一样——
    /// 一样算猜错、一样晚回答，不能靠回答的快慢看出控制码存在
    #[tokio::test]
    async fn a_host_that_isnt_ready_is_answered_like_an_unknown_code() {
        let (broker, cfg) = start().await;
        let ip = limiter_key("127.0.0.1".parse().unwrap());
        let (_host, id) = register(&cfg, None).await;
        broker
            .hosts
            .lock()
            .unwrap()
            .get_mut(&id.code)
            .unwrap()
            .ready = false;
        let unknown = (CODE_MIN..=CODE_MAX)
            .find(|c| !broker.hosts.lock().unwrap().contains_key(c))
            .unwrap();
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        for (req, code) in [(1, id.code), (2, unknown)] {
            let t = Instant::now();
            assert_eq!(
                ask(&mut ctrl, req, code, None).await.unwrap_err(),
                ConnectError::NotFound
            );
            assert!(t.elapsed() >= Duration::from_millis(400), "{code}");
        }
        assert_eq!(broker.failure_count(ip), 2);
    }

    /// 服务器不管会话有几条通道、各是什么：以后新加的通道（这里是 5 号）照样配对转发；
    /// 同一端把同一条通道开两次的拒绝；控制通道结束时整个会话结束，其它通道随之关闭
    #[tokio::test]
    async fn lanes_the_server_has_never_heard_of_are_relayed() {
        use airhand_proto::{SessionHeader, SessionLane};
        let (broker, cfg) = start().await;
        let (mut host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let sid = ask(&mut ctrl, 1, id.code, None).await.unwrap().session_id;
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
        let lane = async |conn: &quinn::Connection, role: Role, lane: u8| {
            let (mut send, recv) = conn.open_bi().await.unwrap();
            let h = SessionHeader {
                session_id: sid,
                role,
                lane,
                priority: 3,
            };
            send.write_all(&h.encode()).await.unwrap();
            (send, recv)
        };
        // 新的通道先到：控制通道还没配好，会话还没开始
        let (mut h5, _h5r) = lane(&host.conn, Role::Host, 5).await;
        let (_c5, mut c5r) = lane(&ctrl.conn, Role::Controller, 5).await;
        h5.write_all(b"future lane").await.unwrap();
        let (hc, mut hcr) = lane(&host.conn, Role::Host, SessionLane::Control as u8).await;
        let (mut cc, _ccr) = lane(&ctrl.conn, Role::Controller, SessionLane::Control as u8).await;
        let mut buf = [0u8; 11];
        timeout(Duration::from_secs(2), c5r.read_exact(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf, b"future lane");
        cc.write_all(b"ping").await.unwrap();
        let mut b4 = [0u8; 4];
        timeout(Duration::from_secs(2), hcr.read_exact(&mut b4))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&b4, b"ping");
        assert!(broker.routes.lock().unwrap().contains_key(&sid));
        // 同一端再开一次 5 号通道：拒绝
        let (_dup, mut dup_r) = lane(&host.conn, Role::Host, 5).await;
        assert!(
            timeout(Duration::from_secs(2), dup_r.read(&mut buf))
                .await
                .unwrap()
                .is_err()
        );
        // 控制通道结束：整个会话结束
        drop((hc, hcr, cc));
        wait_until("the session is gone", || {
            !broker.routes.lock().unwrap().contains_key(&sid)
        })
        .await;
        assert!(!broker.pending.lock().unwrap().contains_key(&sid));
    }

    /// 数据报按开头的会话 ID 在会话的两端之间转发；不是这个会话的连接发的、会话 ID 不对的、会话结束之后的都不转
    #[tokio::test]
    async fn datagrams_are_relayed_between_the_two_ends_of_a_session() {
        use airhand_link::transport::open_session;
        let (broker, cfg) = start().await;
        let (mut host, id) = register(&cfg, None).await;
        let mut ctrl = connect_signal(&cfg, Role::Controller).await.unwrap();
        let sid = ask(&mut ctrl, 1, id.code, None).await.unwrap().session_id;
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
        let h = open_session(&host.conn, sid, Role::Host).await.unwrap();
        let c = open_session(&ctrl.conn, sid, Role::Controller)
            .await
            .unwrap();
        wait_until("the session is established", || {
            broker.routes.lock().unwrap().contains_key(&sid)
        })
        .await;
        let dgram = |sid: u64, body: &[u8]| datagram::encode(sid, body).into();
        let recv = async |conn: &quinn::Connection| {
            let d = timeout(Duration::from_secs(2), conn.read_datagram())
                .await
                .expect("no datagram")
                .unwrap();
            assert_eq!(datagram::session(&d), Some(sid));
            d[datagram::HEADER..].to_vec()
        };
        host.conn.send_datagram(dgram(sid, b"hello")).unwrap();
        assert_eq!(recv(&ctrl.conn).await, b"hello");
        ctrl.conn.send_datagram(dgram(sid, b"back")).unwrap();
        assert_eq!(recv(&host.conn).await, b"back");
        // 别的连接冒用这个会话、会话 ID 不对：都不转（接着收到的是后面那个）
        let other = connect_signal(&cfg, Role::Controller).await.unwrap();
        other.conn.send_datagram(dgram(sid, b"intruder")).unwrap();
        host.conn.send_datagram(dgram(sid ^ 1, b"lost")).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        ctrl.conn.send_datagram(dgram(sid, b"back again")).unwrap();
        host.conn.send_datagram(dgram(sid, b"again")).unwrap();
        assert_eq!(recv(&host.conn).await, b"back again");
        assert_eq!(recv(&ctrl.conn).await, b"again");
        // 会话结束：不再转
        drop((h, c));
        wait_until("the session is gone", || {
            !broker.routes.lock().unwrap().contains_key(&sid)
        })
        .await;
    }

    /// 被控端（airhand-link 的 HostSession）的声音包走数据报，经服务器转发，控制端服务交给界面
    #[tokio::test]
    async fn controller_service_gets_audio_over_datagrams() {
        use airhand_proto::media::{self, AudioHeader, Kind};
        struct Ui(mpsc::UnboundedSender<Vec<u8>>);
        impl SessionUi for Ui {
            fn packet(&self, p: Vec<u8>) -> Result<(), UiClosed> {
                if Kind::of(&p) == Some(Kind::Audio) {
                    let _ = self.0.send(p);
                }
                Ok(())
            }
            fn event(&self, _: SessionEvent) -> Result<(), UiClosed> {
                Ok(())
            }
        }
        let (broker, cfg) = start().await;
        let (mut host, id) = register(&cfg, None).await;
        let svc = ControllerService::start(
            Arc::new(cfg.clone()),
            tokio::runtime::Handle::current(),
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            Identity::generate(),
        );
        let (tx, mut audio) = mpsc::unbounded_channel();
        let ui = Arc::new(Ui(tx));
        let code = id.code.to_string();
        let connecting = async {
            loop {
                let r = svc
                    .connect(
                        &code,
                        ConnectOptions {
                            settings: QualitySettings {
                                fps: 50,
                                ..QualitySettings::default()
                            },
                            audio: true,
                            ..ConnectOptions::default()
                        },
                        ui.clone(),
                        Frontend::Browser,
                    )
                    .await;
                match r {
                    Err(Problem::NotConnected) => {
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                    r => break r,
                }
            }
        };
        let gate = OpenGate(Identity::generate());
        let accepting = async {
            let ServerMsg::Incoming { session_id, peer } = next(&mut host).await else {
                panic!("expected Incoming");
            };
            let (_kick_tx, kick) = watch::channel(ByeReason::Kicked);
            let inc = Incoming {
                conn: host.conn.clone(),
                session_id,
                code: id.code,
                peer,
                kick,
            };
            let (s, admitted) = HostSession::open(&inc, 2, 16, &gate).await.unwrap();
            (session_id, s, admitted)
        };
        let (info, (sid, session, admitted)) = tokio::join!(connecting, accepting);
        let info = info.unwrap();
        // 两端的安全码一样；控制端记下的是被控端真正的公钥；开始时的画质和声音在握手里就到了
        assert_eq!(info.safety, admitted.safety);
        assert_eq!((admitted.settings.fps, admitted.audio), (50, true));
        assert_eq!(info.host_key, airhand_link::secure::hex(gate.0.public()));
        wait_until("the session is established", || {
            broker.routes.lock().unwrap().contains_key(&sid)
        })
        .await;
        for seq in 1..=5u32 {
            let h = AudioHeader {
                seq,
                start: seq == 1,
                capture_us: seq as u64,
            };
            let packet = media::frame(&h.encode(), &[&[seq as u8; 200]]);
            session.audio.send(packet.into()).await.unwrap();
        }
        let mut got = Vec::new();
        while got.len() < 5 {
            let p = timeout(Duration::from_secs(5), audio.recv())
                .await
                .expect("no audio")
                .unwrap();
            assert_eq!(p.len(), media::HEADER_LEN + 200);
            got.push(AudioHeader::decode(&p).unwrap().seq);
        }
        assert_eq!(got, [1, 2, 3, 4, 5]);
        assert!(host.conn.stats().frame_tx.datagram >= 5);
        drop(session);
    }

    /// 新分配的控制码按来源限量；取回自己原来的控制码不算
    #[tokio::test]
    async fn new_codes_are_limited_per_network() {
        let (_broker, cfg) = start_cfg(Config {
            new_codes_per_ip: 2,
            ..test_config(None)
        })
        .await;
        let (_a, a) = register(&cfg, None).await;
        let (_b, _) = register(&cfg, None).await;
        let mut sig = connect_signal(&cfg, Role::Host).await.unwrap();
        write_msg(
            &mut sig.send,
            &ClientMsg::Register {
                reclaim: None,
                keep: false,
                rekey: false,
            },
        )
        .await
        .unwrap();
        let reason = timeout(Duration::from_secs(5), sig.conn.closed())
            .await
            .unwrap();
        assert!(
            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
                if c.error_code == VarInt::from_u32(close::REGISTER_REFUSED)),
            "{reason:?}"
        );
        let (_again, back) = register(&cfg, Some(a)).await;
        assert_eq!(back, a);
    }

    #[tokio::test]
    async fn connections_are_limited_per_network() {
        let (broker, cfg) = start_cfg(Config {
            max_conns_per_ip: 2,
            ..test_config(None)
        })
        .await;
        let a = connect_signal(&cfg, Role::Controller).await.unwrap();
        let _b = connect_signal(&cfg, Role::Controller).await.unwrap();
        assert!(connect_signal(&cfg, Role::Controller).await.is_err());
        a.conn.close(VarInt::from_u32(0), b"");
        drop(a);
        wait_until("a connection slot is free", || {
            broker.conns.lock().unwrap().values().sum::<usize>() < 2
        })
        .await;
        assert!(connect_signal(&cfg, Role::Controller).await.is_ok());
    }

    /// 固定的控制码：服务器重启（只剩数据库里的绑定）之后，只有原来的被控端能取回，别人拿不走；
    /// 被控端放弃（Release）后解除绑定，也不再保留
    #[tokio::test]
    async fn fixed_codes_are_never_handed_out() {
        let (dir, file) = temp_db();
        let (old, cfg) = start_with(Some(file.clone())).await;
        let (host, id) = register_with(&cfg, None, true).await;
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || old.stats().0 == 0).await;
        old.flush_bindings().await;
        drop((host, old));

        let (broker, cfg) = start_with(Some(file.clone())).await;
        assert_eq!(broker.stats().2, 1);
        // 冒领：知道控制码、不知道密钥
        let thief = Reclaim {
            code: id.code,
            secret: [0xEE; 16],
        };
        let (_thief, got) = register(&cfg, Some(thief)).await;
        assert_ne!(got.code, id.code);
        // 原来的被控端
        let (mut host, again) = register_with(&cfg, Some(id), true).await;
        assert_eq!(again, id);

        // 放弃控制码：解除绑定，断开后不保留
        write_msg(&mut host.send, &ClientMsg::Release)
            .await
            .unwrap();
        wait_until("the binding is removed", || broker.stats().2 == 0).await;
        host.conn.close(VarInt::from_u32(0), b"");
        wait_until("the host is gone", || broker.stats().0 == 1).await;
        assert!(!broker.reserved.lock().unwrap().contains_key(&id.code));
        // Persistence is asynchronous; await the disk barrier outside broker locks.
        broker.flush_bindings().await;
        assert!(!Bindings::open(Some(&file)).contains(id.code));
        let _ = std::fs::remove_dir_all(dir);
    }
}
