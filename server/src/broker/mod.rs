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
//! 控制码，这些请求不算猜错、不受猜码限速的影响，被控端不在线时服务器记下它们（[`Waiter`]），被控端重新上线时通知它们重连。
//!
//! 控制码对了的请求也限速：每批准一个，被控端就要开一个会话（先请本机用户同意的被控端还要问一次），并且占着它的
//! 一个会话槽位——控制端一直不开流的话，占到配对超时（[`relay`]）为止。不限速的话，知道控制码的人反复发起连接，
//! 就能一直占满它的会话槽位、让它不停地弹出请求。所以同一个来源（[`limiter_key`]）一分钟内向同一台被控端发起的会话
//! 最多 [`Config::connects_per_minute`] 个，再多回答 `RateLimited`。带凭证的请求也算：凭证只说明它不是在猜码。
//! 只数批准了的请求，被限速的来源本来就连上过这个控制码，这个回答不透露控制码存不存在。
//!
//! 注册不能用来探测控制码：取回控制码要出示密钥，密钥由服务器签名（[`Broker::issue_secret`]），
//! 控制码空闲时也只认服务器为**这个控制码**签发过的密钥，对不上就分配一个新的——回答和控制码有没有人用无关。
//! 新分配的控制码按来源限量（[`Config::new_codes_per_ip`]），防止批量占用控制码。
//!
//! 资源：每个来源同时最多 [`Config::max_conns_per_ip`] 条连接；QUIC 的流控窗口按"转发"来定（见 `main.rs`），
//! 不读数据的一方最多让服务器替它缓存几 MiB，而不是几百 MiB。
//!
//! # 分成几块
//!
//! * [`registry`]（控制码登记）：被控端注册、取回控制码、换密钥、放弃控制码；控制端按控制码发起会话、等被控端上线；
//! * [`relay`]（会话中转）：两端的会话流按 `(会话 ID, 通道)` 配对后透传，数据报按会话 ID 转发；
//! * [`limits`]（限速）：每个来源的连接数、猜错控制码的次数、新分配的控制码、向同一台被控端发起的会话，
//!   每台被控端的会话数；
//! * [`tokens`]（凭证）：控制码的密钥、重连凭证（服务器签名、常数时间比较）。
//!
//! 这里（`mod.rs`）是一条连接的入口：版本握手，然后按角色交给 [`registry`]。
//!
//! # 锁
//!
//! 状态都在 [`Broker`] 的几把同步锁里，拿着锁从不 `await`。同时拿几把的时候按 [`lock::Rank`] 的顺序：
//!
//! ```text
//! hosts → reserved → pending → bindings → waiters → routes → failures → connects → new_codes → conns
//! ```
//!
//! 实际会同时拿的：`register` 拿着 hosts、reserved、bindings 去数 new_codes；`connect` 拿着 hosts 去拿 failures、
//! connects、pending、waiters（拿着 hosts 才不会错过同时发生的上线）；`announce` 拿着 hosts 去拿 waiters；
//! 清理绑定表时拿着 hosts 去拿 bindings。别的都只拿一把。调试构建（包括测试）里每次加锁都会检查这个顺序
//! （见 [`lock`]），按相反的顺序拿会直接 panic，而不是偶尔死锁。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use airhand_link::wire::{read_frame, write_frame, write_msg};
use airhand_proto::signal::{ClientHello, ServerHello, ServerMsg};
use airhand_proto::{MAX_SIGNAL_MSG, Role, SIGNAL_MIN, SIGNAL_VERSION, features};
use anyhow::{Context, Result, bail};
use quinn::{SendStream, VarInt};
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::codes::{Barrier, Bindings};

mod limits;
mod lock;
mod registry;
mod relay;
#[cfg(test)]
mod tests;
mod tokens;

use limits::{Limiter, limiter_key};
use lock::{Rank, Ranked};
use registry::{HostConn, HostEntry, Reserved, Waiter};
use relay::{Pending, Route};

/// 握手、等第一条消息、等会话头最多等这么久
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// 同一个来源（[`limits::registration_key`]）10 分钟内最多分配这么多个**新**控制码（取回自己原来的控制码不算）
    pub new_codes_per_ip: usize,
    /// 同一个来源（[`limiter_key`]）一分钟内最多向同一台被控端发起这么多个会话（见模块说明）。
    /// 不应该比 `max_sessions_per_host` 小：不然同一个出口 IP 后面的人坐不满一台被控端
    pub connects_per_minute: usize,
}

impl Config {
    pub const DEFAULT_MAX_CONNS_PER_IP: usize = 128;
    pub const DEFAULT_NEW_CODES_PER_IP: usize = 30;
    /// 发起之后不开流的会话占着槽位到配对超时（20 多秒）：一分钟 6 个，平均占着 2 个多，
    /// 一个来源靠它占不满默认的 4 个槽位。正常使用（输错配对码再试、断线重连）到不了这个数
    pub const DEFAULT_CONNECTS_PER_MINUTE: usize = 6;
}

/// Bounded signaling output. A non-reading peer is disconnected on overflow,
/// rather than retaining arbitrarily many replies or silently losing grants.
#[derive(Clone)]
struct SignalTx {
    tx: mpsc::Sender<ServerMsg>,
    conn: quinn::Connection,
}

impl SignalTx {
    pub(super) fn send(&self, msg: ServerMsg) -> Result<(), ()> {
        self.tx.try_send(msg).map_err(|_| {
            self.conn
                .close(VarInt::from_u32(close::SIGNAL), b"signal queue overflow");
        })
    }
}

pub struct Broker {
    cfg: Config,
    /// 给控制码密钥签名的服务器密钥（随数据库保存，见 [`Bindings::server_key`]）
    key: [u8; 32],
    // 锁的先后见模块说明的"锁"（按字段的顺序排）
    /// 在线的被控端（控制码 → 登记）
    hosts: Ranked<HashMap<u32, HostEntry>>,
    /// 掉线后保留着的控制码
    reserved: Ranked<HashMap<u32, Reserved>>,
    /// 会话（会话 ID → 等配对的通道），从服务器批准到控制通道结束
    pending: Ranked<HashMap<u64, Pending>>,
    /// 固定的控制码
    bindings: Ranked<Bindings>,
    /// 控制码 → 等它上线的控制端
    waiters: Ranked<HashMap<u32, Vec<Waiter>>>,
    /// 建立了的会话（会话 ID → 两端），转发数据报用
    routes: Ranked<HashMap<u64, Route>>,
    /// 猜错控制码（按 [`limiter_key`]）
    failures: Limiter,
    /// 批准了的连接请求（按 [`limiter_key`] 和控制码）
    connects: Limiter<(std::net::IpAddr, u32)>,
    /// 新分配的控制码（按 [`limits::registration_key`]）
    new_codes: Limiter,
    /// 每个来源（[`limiter_key`]）现在有几条连接
    conns: Ranked<HashMap<std::net::IpAddr, usize>>,
    next_conn: AtomicU64,
}

async fn writer_loop(mut send: SendStream, mut rx: mpsc::Receiver<ServerMsg>) {
    while let Some(msg) = rx.recv().await {
        if write_msg(&mut send, &msg).await.is_err() {
            break;
        }
    }
}

impl Broker {
    pub fn new(cfg: Config) -> Arc<Self> {
        let bindings = Bindings::open(cfg.db.as_deref());
        Arc::new(Self {
            cfg,
            key: bindings.server_key(),
            hosts: Ranked::default_with(Rank::Hosts),
            reserved: Ranked::default_with(Rank::Reserved),
            pending: Ranked::default_with(Rank::Pending),
            bindings: Ranked::new(Rank::Bindings, bindings),
            waiters: Ranked::default_with(Rank::Waiters),
            routes: Ranked::default_with(Rank::Routes),
            failures: Limiter::new(Rank::Failures, limits::FAIL_WINDOW),
            connects: Limiter::new(Rank::Connects, limits::CONNECT_WINDOW),
            new_codes: Limiter::new(Rank::NewCodes, limits::NEW_CODE_WINDOW),
            conns: Ranked::default_with(Rank::Conns),
            next_conn: AtomicU64::new(1),
        })
    }

    /// 周期性清理过期数据。
    pub async fn janitor(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut next_expiry = Instant::now();
        loop {
            tick.tick().await;
            let now = Instant::now();
            self.expire_reserved(now);
            self.expire_pending(now);
            self.failures.cleanup();
            self.connects.cleanup();
            self.new_codes.cleanup();
            if now >= next_expiry {
                self.expire_bindings();
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
        let barrier = self.bindings.lock().barrier();
        self.persisted(barrier).await;
    }

    /// (在线的被控端, 等待配对的会话, 固定的控制码)
    pub fn stats(&self) -> (usize, usize, usize) {
        // 一把一把地拿（写在一个元组表达式里的话，三把锁的守卫会一直活到表达式结束，同时拿着三把）
        let hosts = self.hosts.lock().len();
        let pending = self.pending.lock().len();
        let fixed = self.bindings.lock().len();
        (hosts, pending, fixed)
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
}
