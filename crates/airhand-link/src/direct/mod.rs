//! 直连：在中转之外再打通一条两端之间的路（协议见 `airhand_proto` 的"直连"）。
//!
//! * [`Punch`]（`socket`）：连中转的那个 UDP socket 也用来打洞（同一个端口、同一个 NAT 映射）；
//! * `tls`：直连的 QUIC 配置。被控端在连中转的 endpoint 上接受直连，控制端从它连中转的 endpoint 发起；
//! * 对面是谁由会话密钥证明（[`prove_controller`] / [`host`] 的检查）：证明里绑定了这条 QUIC 连接的 TLS 导出密钥，
//!   中间人就算两头各建一条连接，也转发不了这个证明；
//! * 控制、大块通道每一帧在中转上写一份，直连通着的话再从直连写一份（[`Mirror`]），接收端合并（`crate::merge`）；
//! * 画面、声音走哪条路由被控端决定（[`Route`]，`select` 里的规则），控制端只管把探测数据报弹回去；
//! * [`host`]：被控端这边（[`DirectHub`]：一条服务器连接上接受直连、分发探测数据报；每个会话一个任务：
//!   发候选地址、打洞、换路）；[`controller`]：控制端这边（回答候选地址、发起连接、弹回探测）。
//!
//! 直连断了（换了网络、NAT 的映射变了）会话照常在中转上继续：控制、大块通道中转上本来就有完整的一份，
//! 画面由被控端改回中转、编一个关键帧。之后被控端再发一轮候选地址，通了就接着用。

pub mod controller;
pub mod host;
pub mod select;
mod socket;
pub(crate) mod tls;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::session::MAX_DIRECT_ADDRS;
use airhand_proto::{Role, SessionLane};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use ring::hmac;
use tokio::sync::mpsc;
use tracing::debug;

pub use host::{DirectHub, HostLink, HubTasks};
pub use socket::Punch;
pub(crate) use socket::bind;

use crate::merge;
use crate::secure::DirectKey;

/// 数据报、帧从哪条路来（画面、声音走哪条路）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Via {
    Relay = 0,
    Direct = 1,
}

/// 控制、大块通道在直连上的那一份：每一帧也从这里发（见模块说明）。直连没通时什么也不做
#[derive(Clone, Default)]
pub struct Mirror(Arc<Mutex<Option<mpsc::Sender<Bytes>>>>);

/// 直连上每条通道最多排着这么多帧没写出去，再多就不写了（中转上还有一份）
const MIRROR_QUEUE: usize = 64;

impl Mirror {
    /// 写任务每加密一帧就调用一次（`sealed`：带长度前缀的密文）
    pub(crate) fn offer(&self, sealed: &Bytes) {
        let mut tx = self.0.lock().unwrap();
        if let Some(t) = &*tx
            && let Err(mpsc::error::TrySendError::Closed(_)) = t.try_send(sealed.clone())
        {
            *tx = None;
        }
    }

    /// 从现在起每一帧也写进直连上的 `send`。返回写的任务（直连断了、[`Mirror::stop`] 之后结束）
    pub(crate) fn start(&self, mut send: SendStream) -> impl Future<Output = ()> + Send + 'static {
        let (tx, mut rx) = mpsc::channel::<Bytes>(MIRROR_QUEUE);
        *self.0.lock().unwrap() = Some(tx);
        async move {
            while let Some(b) = rx.recv().await {
                if send.write_chunk(b).await.is_err() {
                    return;
                }
            }
            let _ = send.finish();
        }
    }

    pub(crate) fn stop(&self) {
        *self.0.lock().unwrap() = None;
    }
}

/// 控制、大块通道各自的 [`Mirror`]
#[derive(Clone, Default)]
pub struct Mirrors {
    pub control: Mirror,
    pub bulk: Mirror,
}

impl Mirrors {
    /// 在直连上开本端的控制、大块通道，开始镜像。返回写的任务
    async fn start(
        &self,
        conn: &Connection,
    ) -> Result<Vec<impl Future<Output = ()> + Send + 'static>> {
        let control = open_lane(conn, SessionLane::Control).await?;
        let bulk = open_lane(conn, SessionLane::Bulk).await?;
        Ok(vec![self.control.start(control), self.bulk.start(bulk)])
    }

    fn stop(&self) {
        self.control.stop();
        self.bulk.stop();
    }
}

/// 被控端的画面、声音走哪里（被控端的直连任务决定，写任务每一帧看一眼）
#[derive(Clone, Default)]
pub struct Route {
    /// 走这条直连；`None`：走中转
    pub media: Option<Connection>,
}

/// 把直连上对方开的通道流交给哪条通道的接收端（`crate::merge::Merge`）
#[derive(Clone, Default)]
pub struct Attach {
    pub control: Option<merge::Attach>,
    pub bulk: Option<merge::Attach>,
    pub video: Option<merge::Attach>,
}

impl Attach {
    fn lane(&self, lane: u8) -> Option<&merge::Attach> {
        match lane {
            l if l == SessionLane::Control as u8 => self.control.as_ref(),
            l if l == SessionLane::Bulk as u8 => self.bulk.as_ref(),
            l if l == SessionLane::Video as u8 => self.video.as_ref(),
            _ => None,
        }
    }
}

/// 在直连上开一条本端发的通道：一条单向流，开头一个字节是通道编号
pub(crate) async fn open_lane(conn: &Connection, lane: SessionLane) -> Result<SendStream> {
    let mut send = tokio::time::timeout(Duration::from_secs(5), conn.open_uni())
        .await
        .context("timed out opening a direct stream")??;
    send.set_priority(lane.priority() as i32)?;
    send.write_all(&[lane as u8]).await?;
    Ok(send)
}

/// 接受直连上对方开的通道流，交给各自的通道。直连断了就结束
pub(crate) async fn accept_lanes(conn: Connection, attach: Attach) {
    while let Ok(mut recv) = conn.accept_uni().await {
        let mut lane = [0u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut lane)).await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        match attach.lane(lane[0]) {
            Some(tx) => {
                let _ = tx.send(recv);
            }
            None => {
                debug!(
                    lane = lane[0],
                    "ignoring a direct stream for an unknown lane"
                );
                stop(recv);
            }
        }
    }
}

fn stop(mut recv: RecvStream) {
    let _ = recv.stop(0u32.into());
}

/// 本端的候选地址：服务器看到的公网地址（`observed`），和这台机器往中转去时用的局域网地址（端口都是连中转的那个）
pub(crate) fn candidates(
    endpoint: &Endpoint,
    relay: &Connection,
    observed: Option<SocketAddr>,
) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    if let Some(o) = observed {
        out.push(canonical(o));
    }
    if let (Ok(local), Some(ip)) = (endpoint.local_addr(), route_ip(relay.remote_address())) {
        out.push(SocketAddr::new(ip, local.port()));
    }
    out.dedup();
    out.truncate(MAX_DIRECT_ADDRS);
    out
}

/// 往 `to` 去时本机用哪个地址：UDP 的 connect 不发包，只是选好路由（局域网地址，或者没有 NAT 时的公网地址）
fn route_ip(to: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if to.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(to).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// IPv4 映射的 IPv6 地址（双栈的服务器看到的 IPv4 客户端）写成 IPv4
fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

/// 对方的候选地址里本端用得上的：和本端的 socket 同一种地址，不是 0.0.0.0、组播、广播，最多 [`MAX_DIRECT_ADDRS`] 个
pub(crate) fn usable(theirs: &[SocketAddr], endpoint: &Endpoint) -> Vec<SocketAddr> {
    let Ok(local) = endpoint.local_addr() else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddr> = theirs
        .iter()
        .take(MAX_DIRECT_ADDRS)
        .map(|a| canonical(*a))
        .filter(|a| a.is_ipv4() == local.is_ipv4() && a.port() != 0)
        .filter(|a| match a.ip() {
            IpAddr::V4(ip) => !(ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast()),
            IpAddr::V6(ip) => !(ip.is_unspecified() || ip.is_multicast()),
        })
        .collect();
    out.dedup();
    out
}

// ───────────────────────────── 证明 ─────────────────────────────

/// 控制端连上之后在第一条双向流上发：`会话 ID u64 LE | 证明 32 字节`；被控端回它的证明（32 字节）
pub(crate) const HELLO_LEN: usize = 8 + 32;

/// 这条 QUIC 连接的 TLS 导出密钥（两头一样；中间人两头各建一条连接的话就不一样了）
fn exporter(conn: &Connection, session_id: u64) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    conn.export_keying_material(
        &mut out,
        b"EXPORTER-airhand-direct",
        &session_id.to_le_bytes(),
    )
    .map_err(|_| anyhow::anyhow!("can't export keying material from the direct connection"))?;
    Ok(out)
}

fn proof_input(role: Role, exporter: &[u8; 32]) -> Vec<u8> {
    let mut v = match role {
        Role::Controller => b"airhand direct controller".to_vec(),
        Role::Host => b"airhand direct host".to_vec(),
    };
    v.extend_from_slice(exporter);
    v
}

/// `role` 这一端在这条连接上的证明
fn proof(key: &DirectKey, role: Role, exporter: &[u8; 32]) -> [u8; 32] {
    let k = hmac::Key::new(hmac::HMAC_SHA256, &key.0);
    hmac::sign(&k, &proof_input(role, exporter))
        .as_ref()
        .try_into()
        .expect("32 bytes")
}

fn verify(key: &DirectKey, role: Role, exporter: &[u8; 32], theirs: &[u8]) -> bool {
    let k = hmac::Key::new(hmac::HMAC_SHA256, &key.0);
    hmac::verify(&k, &proof_input(role, exporter), theirs).is_ok()
}

/// 控制端：在刚连上的直连上证明自己是这个会话的控制端，并确认对面是这个会话的被控端
pub(crate) async fn prove_controller(
    conn: &Connection,
    session_id: u64,
    key: &DirectKey,
) -> Result<()> {
    let ex = exporter(conn, session_id)?;
    let (mut send, mut recv) = conn.open_bi().await?;
    let mut hello = Vec::with_capacity(HELLO_LEN);
    hello.extend_from_slice(&session_id.to_le_bytes());
    hello.extend_from_slice(&proof(key, Role::Controller, &ex));
    send.write_all(&hello).await?;
    let _ = send.finish();
    let mut theirs = [0u8; 32];
    recv.read_exact(&mut theirs).await?;
    anyhow::ensure!(
        verify(key, Role::Host, &ex, &theirs),
        "the direct peer isn't this session's host"
    );
    Ok(())
}

/// 被控端：检查控制端的证明，回答自己的
pub(crate) async fn prove_host(
    conn: &Connection,
    session_id: u64,
    key: &DirectKey,
    send: &mut SendStream,
    theirs: &[u8; 32],
) -> Result<()> {
    let ex = exporter(conn, session_id)?;
    anyhow::ensure!(
        verify(key, Role::Controller, &ex, theirs),
        "the direct peer isn't this session's controller"
    );
    send.write_all(&proof(key, Role::Host, &ex)).await?;
    let _ = send.finish();
    Ok(())
}

// ───────────────────────────── 探测 ─────────────────────────────

/// 探测包的明文：`走的路 u8 | 被控端发出的时刻（微秒） u64 LE`。控制端原样（重新加密之后）从同一条路弹回去
pub(crate) fn probe_payload(via: Via, t_us: u64) -> [u8; 9] {
    let mut p = [0u8; 9];
    p[0] = via as u8;
    p[1..].copy_from_slice(&t_us.to_le_bytes());
    p
}

/// 弹回来的探测包：从 `via` 发出、`via` 回来的才算，返回发出的时刻
pub(crate) fn probe_sent_at(payload: &[u8], via: Via) -> Option<u64> {
    let p: &[u8; 9] = payload.try_into().ok()?;
    (p[0] == via as u8).then(|| u64::from_le_bytes(p[1..].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_payload_roundtrip() {
        let p = probe_payload(Via::Direct, 123_456_789);
        assert_eq!(probe_sent_at(&p, Via::Direct), Some(123_456_789));
        // 从别的路弹回来的不算
        assert_eq!(probe_sent_at(&p, Via::Relay), None);
        assert_eq!(probe_sent_at(&p[..8], Via::Direct), None);
    }

    #[test]
    fn proofs_depend_on_role_key_and_connection() {
        let key = DirectKey([1; 32]);
        let ex = [2u8; 32];
        let c = proof(&key, Role::Controller, &ex);
        assert!(verify(&key, Role::Controller, &ex, &c));
        // 被控端的证明换不成控制端的；别的连接（导出密钥不同）、别的会话（密钥不同）都不行
        assert!(!verify(&key, Role::Host, &ex, &c));
        assert!(!verify(&key, Role::Controller, &[3; 32], &c));
        assert!(!verify(&DirectKey([9; 32]), Role::Controller, &ex, &c));
        assert!(!verify(&key, Role::Controller, &ex, &c[..31]));
    }

    #[test]
    fn mapped_addresses_are_canonical() {
        let mapped: SocketAddr = "[::ffff:203.0.113.9]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), "203.0.113.9:5000".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }
}
