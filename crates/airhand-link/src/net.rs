//! QUIC 连接与信令握手（被控端与控制端共用）。
//!
//! # 服务器证书
//!
//! 配置了指纹（[`Trust::Pinned`]）就只认那一个。没有配置时**第一次连上时记下服务器的指纹**（[`Trust::FirstUse`]，
//! 由各端的 [`TrustStore`] 保存），之后只认它，证书变了就报 [`Problem::CertMismatch`]：
//! 第一次连接之后，中间人就冒充不了服务器了。记下的指纹跟着服务器地址走，换了服务器重新记。
//!
//! # 地址
//!
//! 服务器的域名可能同时有 IPv6 和 IPv4 地址，而本机不一定能用 IPv6：按 RFC 8305（Happy Eyeballs）
//! 把两种地址交替排好，每隔 250 ms 多试一个，先连上的那个胜出。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::framing::{read_frame, write_frame};
use airhand_proto::signal::{ClientHello, ServerHello};
use airhand_proto::{ALPN, Features, MAX_SIGNAL_MSG, Role, SIGNAL_MIN, SIGNAL_VERSION};
use anyhow::{Context, Result, anyhow, bail};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Connection, Endpoint, RecvStream, SendStream, TransportConfig, VarInt};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::problem::Problem;

/// 连接中转服务器所需的配置：桌面客户端编译进程序，网页控制台从环境变量读取。
#[derive(Clone)]
pub struct LinkConfig {
    /// 中转服务器地址 `host:port`（UDP）
    pub server: String,
    /// 怎样校验服务器的证书（见模块说明）
    pub trust: Trust,
    /// 告诉服务器的本机名称（被控端的托盘里显示"谁在控制"），空字符串表示不知道
    pub name: String,
    /// 本机操作系统，仅用于展示
    pub os: String,
}

impl std::fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkConfig")
            .field("server", &self.server)
            .field("pinned", &self.pinned())
            .field("name", &self.name)
            .field("os", &self.os)
            .finish()
    }
}

/// 怎样校验中转服务器的证书
#[derive(Clone)]
pub enum Trust {
    /// 配置了指纹：只认这一个
    Pinned([u8; 32]),
    /// 没有配置：第一次连上时记下服务器的指纹，之后只认它（Trust On First Use）
    FirstUse(Arc<dyn TrustStore>),
}

/// 第一次连上服务器时记下的证书指纹存在哪里（桌面版是 config.toml，Android 是应用的设置，网页控制台是数据目录里的文件）
pub trait TrustStore: Send + Sync + 'static {
    /// 之前为 `server` 记下的指纹
    fn load(&self, server: &str) -> Option<[u8; 32]>;
    /// 第一次连上 `server`：记下它的指纹
    fn save(&self, server: &str, fp: [u8; 32]);
}

/// 只记在内存里的 [`TrustStore`]（本次运行有效；测试、没有地方保存时用）
#[derive(Default)]
pub struct MemoryTrust(Mutex<Option<(String, [u8; 32])>>);

impl MemoryTrust {
    /// 预先知道的指纹（之前保存下来的），`server` 换了就不算数
    pub fn with(server: &str, fp: Option<[u8; 32]>) -> Self {
        Self(Mutex::new(fp.map(|fp| (server.to_string(), fp))))
    }
}

impl TrustStore for MemoryTrust {
    fn load(&self, server: &str) -> Option<[u8; 32]> {
        match &*self.0.lock().unwrap() {
            Some((s, fp)) if s == server => Some(*fp),
            _ => None,
        }
    }

    fn save(&self, server: &str, fp: [u8; 32]) {
        *self.0.lock().unwrap() = Some((server.to_string(), fp));
    }
}

impl LinkConfig {
    /// `fingerprint`：服务器日志里打印的 64 位十六进制指纹（允许用 `:` 或空白分隔）。
    /// 空字符串表示没有配置：第一次连上时记下服务器的指纹（先只记在内存里，用 [`LinkConfig::with_store`] 换成能保存下来的）。
    pub fn new(
        server: impl Into<String>,
        fingerprint: &str,
        name: impl Into<String>,
        os: impl Into<String>,
    ) -> Result<Self> {
        let trust = match parse_fingerprint(fingerprint)? {
            Some(fp) => Trust::Pinned(fp),
            None => Trust::FirstUse(Arc::new(MemoryTrust::default())),
        };
        Ok(Self {
            server: server.into(),
            trust,
            name: name.into(),
            os: os.into(),
        })
    }

    /// 没有配置指纹时，第一次连上时记下的指纹存进 `store`（配置了指纹时不用它）
    pub fn with_store(mut self, store: Arc<dyn TrustStore>) -> Self {
        if let Trust::FirstUse(_) = self.trust {
            self.trust = Trust::FirstUse(store);
        }
        self
    }

    /// 是否配置了固定的指纹（否则是第一次连上时记下的，见模块说明）
    pub fn pinned(&self) -> bool {
        matches!(self.trust, Trust::Pinned(_))
    }

    /// 这次连接认哪个指纹：配置的，或者之前记下的；`None` 表示第一次连接，记下对方的
    fn expected(&self) -> Option<[u8; 32]> {
        match &self.trust {
            Trust::Pinned(fp) => Some(*fp),
            Trust::FirstUse(store) => store.load(&self.server),
        }
    }
}

/// 指纹写成 64 位小写十六进制（保存、日志里用）
pub fn format_fingerprint(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// 解析证书指纹：64 位十六进制，允许用 `:` 或空白分隔；空字符串表示没有配置（`None`）。
pub fn parse_fingerprint(s: &str) -> Result<Option<[u8; 32]>> {
    if let Some(c) = s
        .chars()
        .find(|c| !c.is_ascii_hexdigit() && *c != ':' && !c.is_ascii_whitespace())
    {
        bail!("the server certificate fingerprint contains {c:?}: expected 64 hex digits");
    }
    let hex: Vec<u8> = s.bytes().filter(u8::is_ascii_hexdigit).collect();
    if hex.is_empty() {
        return Ok(None);
    }
    if hex.len() != 64 {
        bail!(
            "the server certificate fingerprint must be 64 hex digits, got {}",
            hex.len()
        );
    }
    let mut out = [0u8; 32];
    for (b, pair) in out.iter_mut().zip(hex.chunks(2)) {
        *b = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    Ok(Some(out))
}

/// 与服务器的连接状态（托盘 / 控制台按各自的界面语言显示）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum ServerState {
    Connecting,
    Connected,
    /// 断开的原因（类型，由托盘 / 控制台按各自的界面语言显示）
    Disconnected {
        reason: Problem,
    },
}

/// 连接状态变化的回调
pub type Report = Arc<dyn Fn(ServerState) + Send + Sync>;

pub struct Signal {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    /// 和服务器商定的信令版本，双方都支持的信令能力位（见 `airhand_proto::signal` 的"版本握手"）
    pub version: u16,
    pub features: Features,
    /// 保持 Endpoint 存活
    _endpoint: Endpoint,
}

pub fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(VarInt::from_u32(256)); // 3 lanes per session + signaling
    t.max_concurrent_uni_streams(VarInt::from_u32(0));
    // 短保活间隔：既能防止 NAT 映射过期，也能让掉线在 10 秒内被发现并重连
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    t.keep_alive_interval(Some(Duration::from_secs(2)));
    t.stream_receive_window(VarInt::from_u32(16 * 1024 * 1024));
    t.receive_window(VarInt::from_u32(64 * 1024 * 1024));
    t.send_window(64 * 1024 * 1024);
    // 声音数据报（约 12 KB/s）：网络跟不上时宁可丢掉旧的，也不要在发送缓冲里越积越晚（缓冲满了 quinn 丢最旧的）
    t.datagram_send_buffer_size(64 * 1024);
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

/// 一次连接尝试的证书校验结果（TLS 错误经过 QUIC 之后只剩一段文字，用这些来可靠地识别）
#[derive(Default)]
struct Seen {
    /// 证书指纹不符
    mismatch: AtomicBool,
    /// 服务器出示的证书的指纹
    fp: Mutex<Option<[u8; 32]>>,
}

/// `expected`：只认这个指纹；`None` 表示第一次连接，什么证书都接受（指纹记在 `seen` 里）
fn client_config(expected: Option<[u8; 32]>, seen: Arc<Seen>) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
            provider,
            pin: expected,
            seen,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(cfg)
}

/// 解析服务器地址，返回 (全部地址，按连接的先后排好, SNI)。
async fn resolve(addr: &str) -> Result<(Vec<SocketAddr>, String)> {
    let host = addr
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(addr)
        .trim_matches(['[', ']']);
    let sni = if host.parse::<std::net::IpAddr>().is_ok() {
        "airhand".to_string()
    } else {
        host.to_string()
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("can't resolve {addr}"))?
        .collect();
    if addrs.is_empty() {
        bail!("{addr} has no usable IP address");
    }
    Ok((interleave(addrs), sni))
}

/// RFC 8305：去掉重复的地址，IPv6 和 IPv4 交替排列，系统给的第一个地址的类型排在前面
fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let mut unique: Vec<SocketAddr> = Vec::with_capacity(addrs.len());
    for a in addrs {
        if !unique.contains(&a) {
            unique.push(a);
        }
    }
    let first_v6 = unique.first().is_some_and(SocketAddr::is_ipv6);
    let (mut a, mut b): (Vec<_>, Vec<_>) =
        unique.into_iter().partition(|x| x.is_ipv6() == first_v6);
    a.reverse();
    b.reverse();
    let mut out = Vec::with_capacity(a.len() + b.len());
    while let Some(x) = a.pop() {
        out.push(x);
        if let Some(y) = b.pop() {
            out.push(y);
        }
    }
    out.extend(b.into_iter().rev());
    out
}

/// 连上服务器的一个地址（QUIC + TLS），返回连接和对方证书的指纹
async fn attempt(
    addr: SocketAddr,
    sni: Arc<str>,
    expected: Option<[u8; 32]>,
    mismatch: Arc<AtomicBool>,
) -> Result<(Endpoint, Connection, [u8; 32])> {
    let bind: SocketAddr = if addr.is_ipv6() {
        "[::]:0".parse()?
    } else {
        "0.0.0.0:0".parse()?
    };
    let seen = Arc::new(Seen::default());
    let mut endpoint = Endpoint::client(bind).context("can't create a UDP socket")?;
    endpoint.set_default_client_config(client_config(expected, seen.clone())?);
    let result = endpoint.connect(addr, &sni)?.await;
    if seen.mismatch.load(Ordering::Relaxed) {
        mismatch.store(true, Ordering::Relaxed);
    }
    let conn = result.with_context(|| format!("can't connect to {addr}"))?;
    let fp = seen
        .fp
        .lock()
        .unwrap()
        .ok_or_else(|| anyhow!("{addr} didn't present a certificate"))?;
    Ok((endpoint, conn, fp))
}

/// Happy Eyeballs（RFC 8305）：按顺序每隔 [`STAGGER`] 多试一个地址（前一个失败了马上试下一个），
/// 先连上的胜出，其余的取消
async fn connect_any(
    server: &str,
    addrs: Vec<SocketAddr>,
    sni: String,
    expected: Option<[u8; 32]>,
    mismatch: Arc<AtomicBool>,
) -> Result<(Endpoint, Connection, [u8; 32])> {
    const STAGGER: Duration = Duration::from_millis(250);
    type Attempts = JoinSet<Result<(Endpoint, Connection, [u8; 32])>>;
    let sni: Arc<str> = sni.into();
    let mut next = addrs.into_iter();
    let mut tasks: Attempts = JoinSet::new();
    let mut last_err: Option<anyhow::Error> = None;
    let spawn = |tasks: &mut Attempts, addr: SocketAddr| {
        tasks.spawn(attempt(addr, sni.clone(), expected, mismatch.clone()));
    };
    if let Some(addr) = next.next() {
        spawn(&mut tasks, addr);
    }
    let deadline = tokio::time::sleep(CONNECT_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        let stagger = tokio::time::sleep(STAGGER);
        tokio::select! {
            _ = &mut deadline => {
                return Err(last_err.unwrap_or_else(|| anyhow!("timed out connecting to {server}")));
            }
            Some(done) = tasks.join_next() => match done {
                // 返回时 tasks 被丢掉，其它还在进行的尝试随之取消
                Ok(Ok(won)) => return Ok(won),
                Ok(Err(e)) => {
                    last_err = Some(e);
                    match next.next() {
                        Some(addr) => spawn(&mut tasks, addr),
                        None if tasks.is_empty() => return Err(last_err.unwrap()),
                        None => {}
                    }
                }
                Err(e) => return Err(anyhow!("connection attempt failed: {e}")),
            },
            _ = stagger, if next.len() > 0 => {
                if let Some(addr) = next.next() {
                    spawn(&mut tasks, addr);
                }
            }
        }
    }
}

/// 连上服务器（QUIC + TLS，所有地址一起算）最多等这么久
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 连接服务器并完成 Hello 握手。
///
/// 失败时返回的 [`Problem`]：证书指纹不符 → `CertMismatch`，版本不一致 → `Incompatible`，
/// 其它（解析地址、网络、超时……）→ `ServerUnreachable`。
pub async fn connect_signal(cfg: &LinkConfig, role: Role) -> std::result::Result<Signal, Problem> {
    let mismatch = Arc::new(AtomicBool::new(false));
    handshake(cfg, role, mismatch.clone()).await.map_err(|e| {
        if mismatch.load(Ordering::Relaxed) {
            Problem::CertMismatch
        } else {
            Problem::from_error(e, |detail| Problem::ServerUnreachable { detail })
        }
    })
}

async fn handshake(cfg: &LinkConfig, role: Role, mismatch: Arc<AtomicBool>) -> Result<Signal> {
    let (addrs, sni) = resolve(&cfg.server).await?;
    let expected = cfg.expected();
    let (endpoint, conn, fp) = connect_any(&cfg.server, addrs, sni, expected, mismatch).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let hello = ClientHello::new(role, cfg.name.clone(), cfg.os.clone());
    write_frame(&mut send, &hello.encode()).await?;
    let mut buf = Vec::new();
    let got = tokio::time::timeout(
        Duration::from_secs(8),
        read_frame(&mut recv, MAX_SIGNAL_MSG, &mut buf),
    )
    .await
    .map_err(|_| anyhow!("server handshake timed out"))??;
    if !got {
        bail!("the server closed the connection during the handshake");
    }
    let resp = ServerHello::decode(&buf).context("unexpected answer from the server")?;
    // 对方说的是 Airhand 的协议（版本对不上也算）：第一次连接的话，记下它的指纹
    if let (Trust::FirstUse(store), None) = (&cfg.trust, expected) {
        info!(server = %cfg.server, fingerprint = %format_fingerprint(&fp), "first connection to this server, remembering its certificate");
        store.save(&cfg.server, fp);
    }
    match resp {
        ServerHello::Welcome { version, features } => {
            anyhow::ensure!(
                (SIGNAL_MIN..=SIGNAL_VERSION).contains(&version),
                "the server chose signal version {version}, which we can't speak"
            );
            Ok(Signal {
                conn,
                send,
                recv,
                version,
                features: features.common(hello.features),
                _endpoint: endpoint,
            })
        }
        ServerHello::Incompatible { min, max } => Err(incompatible(min, max).into()),
    }
}

/// 服务器能说 `min..=max`，和本端没有交集：谁太旧了（`server` 比 `client` 大就是客户端该更新）
fn incompatible(min: u16, max: u16) -> Problem {
    if max < SIGNAL_MIN {
        Problem::Incompatible {
            server: max,
            client: SIGNAL_MIN,
        }
    } else {
        Problem::Incompatible {
            server: min,
            client: SIGNAL_VERSION,
        }
    }
}

/// 证书固定校验器：只比较叶子证书的 SHA-256 指纹，但仍然校验 TLS 1.3 握手签名。
/// `pin` 为 `None`（第一次连接）时什么证书都接受，指纹记进 `seen`。
struct PinnedVerifier {
    provider: Arc<CryptoProvider>,
    pin: Option<[u8; 32]>,
    seen: Arc<Seen>,
}

impl std::fmt::Debug for PinnedVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedVerifier")
            .field("pin", &self.pin.map(|p| format_fingerprint(&p)))
            .finish()
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        *self.seen.fp.lock().unwrap() = Some(got);
        match self.pin {
            None => Ok(ServerCertVerified::assertion()),
            Some(pin) if got == pin => Ok(ServerCertVerified::assertion()),
            Some(_) => {
                self.seen.mismatch.store(true, Ordering::Relaxed);
                Err(rustls::Error::General(
                    "server certificate fingerprint mismatch".into(),
                ))
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 与服务器的断线重连（被控端、控制端共用）：
///
/// ```ignore
/// let mut reconnect = Reconnect::new(report.clone());
/// loop {
///     reconnect.begin();
///     let result = connect_once(..).await; // 一直运行到断开
///     reconnect.end("host", result).await;
/// }
/// ```
///
/// 带抖动的指数退避（0.5 → 10 秒）；连上后稳定超过 20 秒再断开就从头开始；
/// 版本不一致这类重试也没用的问题每分钟才重试一次。
pub struct Reconnect {
    report: Report,
    delay: Duration,
    started: Instant,
}

impl Reconnect {
    const FIRST: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(10);
    const STABLE: Duration = Duration::from_secs(20);
    const FATAL: Duration = Duration::from_secs(60);

    pub fn new(report: Report) -> Self {
        Self {
            report,
            delay: Self::FIRST,
            started: Instant::now(),
        }
    }

    /// 开始一次连接
    pub fn begin(&mut self) {
        (self.report)(ServerState::Connecting);
        self.started = Instant::now();
    }

    /// 一次连接结束。`Ok` 表示需要立即重连（例如更换控制码）；`Err` 表示掉线：记录原因，等待后返回。
    ///
    /// [`connect_signal`] 的错误本身就是 [`Problem`]；其它（连上之后的）错误算作"连接断开"。
    pub async fn end(&mut self, role: &str, result: Result<()>) {
        let Err(err) = result else {
            self.delay = Self::FIRST;
            return;
        };
        let reason = Problem::from_error(err, |detail| Problem::ServerLost { detail });
        warn!("{role} disconnected from the server: {reason}");
        let wait = if reason.is_fatal() {
            Self::FATAL
        } else {
            if self.started.elapsed() > Self::STABLE {
                self.delay = Self::FIRST;
            }
            let d = self.delay;
            self.delay = (d * 2).min(Self::MAX);
            d + Duration::from_millis(rand::random_range(0..=d.as_millis() as u64 / 4))
        };
        (self.report)(ServerState::Disconnected { reason });
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint() {
        let hex = "3a".repeat(32);
        assert_eq!(parse_fingerprint(&hex).unwrap(), Some([0x3a; 32]));
        // 冒号、空白分隔，大小写都可以
        let colons = ["3A"; 32].join(":");
        assert_eq!(parse_fingerprint(&colons).unwrap(), Some([0x3a; 32]));
        assert_eq!(
            parse_fingerprint(&format!(" {} \n", ["3a"; 32].join(" "))).unwrap(),
            Some([0x3a; 32])
        );
        assert_eq!(parse_fingerprint("").unwrap(), None);
        assert_eq!(parse_fingerprint("  ").unwrap(), None);
        assert!(parse_fingerprint(&"3a".repeat(31)).is_err());
        // 整行日志粘贴进来：报错而不是从里面凑出 64 个"十六进制"字符
        assert!(parse_fingerprint(&format!("fingerprint: {hex}")).is_err());
        assert_eq!(format_fingerprint(&[0x3a; 32]), hex);
    }

    #[test]
    fn addresses_alternate_between_families() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let (v6a, v6b, v4a, v4b, v4c) = (
            a("[2001:db8::1]:4433"),
            a("[2001:db8::2]:4433"),
            a("192.0.2.1:4433"),
            a("192.0.2.2:4433"),
            a("192.0.2.3:4433"),
        );
        assert_eq!(
            interleave(vec![v6a, v6b, v4a, v4b, v4c, v4a]),
            [v6a, v4a, v6b, v4b, v4c]
        );
        assert_eq!(interleave(vec![v4a, v6a, v4b]), [v4a, v6a, v4b]);
        assert_eq!(interleave(vec![v4a]), [v4a]);
    }

    #[test]
    fn trust_follows_the_server_address() {
        let store = MemoryTrust::with("a:1", Some([1; 32]));
        assert_eq!(store.load("a:1"), Some([1; 32]));
        assert_eq!(store.load("b:1"), None);
        store.save("b:1", [2; 32]);
        assert_eq!(store.load("a:1"), None);
        assert_eq!(store.load("b:1"), Some([2; 32]));

        let cfg = LinkConfig::new("a:1", "", "n", "o").unwrap();
        assert!(!cfg.pinned());
        assert_eq!(cfg.expected(), None);
        let cfg = cfg.with_store(Arc::new(MemoryTrust::with("a:1", Some([3; 32]))));
        assert_eq!(cfg.expected(), Some([3; 32]));
        let pinned = LinkConfig::new("a:1", &"ab".repeat(32), "n", "o")
            .unwrap()
            .with_store(Arc::new(MemoryTrust::with("a:1", Some([3; 32]))));
        assert!(pinned.pinned());
        assert_eq!(pinned.expected(), Some([0xab; 32]));
    }
}
