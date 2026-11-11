//! 直连：在中转之外再打通一条两端之间的路（协议见 `airhand_proto` 的"直连"）。
//!
//! * [`Punch`]（`socket`）：连中转的那个 UDP socket 也用来打洞（同一个端口、同一个 NAT 映射）；
//! * `tls`：直连的 QUIC 配置和证书。两端各有一张临时生成的自签名证书，指纹在加密的控制通道上交换
//!   （`DirectOffer::cert`），TLS 双向认证只认它：握手完成时就知道对面是谁，中间人冒充不了任何一边
//!   （和 WebRTC 把 DTLS 证书的指纹放进信令里一样）；
//! * 直连上对方的流和数据报由 [`crate::lanes::accept_direct`] 交给会话，和中转上来的放在一起合并（`crate::merge`）；
//! * 控制通道每一帧在中转上写一份，直连通着的话再从直连写一份（[`Mirror`]）；
//! * 画面、声音走哪条路由被控端决定（[`Route`]，`select` 里的规则），控制端只管把探测数据报弹回去；
//! * [`host`]：被控端这边（[`DirectHub`]：一条服务器连接上接受直连；每个会话一个任务：发候选地址、打洞、换路）；
//!   [`controller`]：控制端这边（回答候选地址、发起连接、弹回探测）。
//!
//! 直连断了（换了网络、NAT 的映射变了）会话照常在中转上继续：控制通道中转上本来就有完整的一份，
//! 画面由被控端改回中转、编一个关键帧。之后被控端再发一轮候选地址，通了就接着用。

pub mod controller;
pub mod host;
pub mod select;
mod socket;
pub(crate) mod tls;

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use airhand_proto::session::MAX_DIRECT_ADDRS;
use bytes::Bytes;
use quinn::{Connection, Endpoint, SendStream};
use tokio::sync::mpsc;

pub use crate::lanes::Via;
pub use host::{DirectHub, HostLink};
pub use socket::Punch;
pub(crate) use socket::bind;

/// 控制通道在直连上的那一份：每一帧也从这里发（见模块说明）。直连没通时什么也不做
#[derive(Clone, Default)]
pub struct Mirror(Arc<Mutex<Option<mpsc::Sender<Bytes>>>>);

/// 直连上最多排着这么多帧没写出去，再多就不写了（中转上还有一份）
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

    /// 从现在起每一帧也写进直连上的 `send`（本端在直连上开的控制通道）。返回写的任务（直连断了、[`Mirror::stop`] 之后结束）
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

/// 被控端的画面、声音走哪里（被控端的直连任务决定，写任务每一帧看一眼）
#[derive(Clone, Default)]
pub struct Route {
    /// 走这条直连；`None`：走中转
    pub media: Option<Connection>,
}

/// 本端的候选地址：服务器看到的公网地址（`observed`），和这台机器往中转去时用的局域网地址（端口都是连中转的那个）
pub(crate) fn candidates(
    endpoint: &Endpoint,
    relay: &Connection,
    observed: SocketAddr,
) -> Vec<SocketAddr> {
    let mut out = vec![canonical(observed)];
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
    fn mapped_addresses_are_canonical() {
        let mapped: SocketAddr = "[::ffff:203.0.113.9]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), "203.0.113.9:5000".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }
}
