//! 直连：在中转之外再打通一条两端之间的路（协议见 `airhand_proto` 的"直连"）。
//!
//! * [`Punch`]（`socket`）：连中转的那个 UDP socket 也用来打洞（同一个端口、同一个 NAT 映射）；
//! * `tls`：直连的 QUIC 配置和证书。两端各有一张临时生成的自签名证书，指纹在加密的控制通道上交换
//!   （`DirectOffer::cert`），TLS 双向认证只认它：握手完成时就知道对面是谁，中间人冒充不了任何一边
//!   （和 WebRTC 把 DTLS 证书的指纹放进信令里一样）；
//! * 直连上对方的流和数据报由 [`crate::lanes::accept_direct`] 交给会话，和中转上来的放在一起合并（`crate::merge`）；
//! * 控制通道每一帧、每个控制数据报在中转上发一份，直连通着的话再从直连发一份（[`Mirror`]）；
//! * 候选地址两种地址族都给（[`candidates`]）：连中转的 socket 是双栈的（`socket::bind`），中转走 IPv4 时
//!   两端也能用公网 IPv6 直连：IPv6 一般没有 NAT，打洞只是让两边的防火墙放行，IPv4 打不通（对称 NAT、运营商级 NAT）时往往通；
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

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use airhand_proto::session::MAX_DIRECT_ADDRS;
use bytes::Bytes;
use quinn::{Connection, Endpoint, SendStream};
use tokio::sync::mpsc;

pub use crate::lanes::Via;
pub use host::{DirectHub, HostLink};
pub use socket::Punch;
pub(crate) use socket::bind;

/// 控制通道、控制数据报在直连上的那一份：每一帧、每个数据报也从这里发（见模块说明）。直连没通时什么也不做
#[derive(Clone, Default)]
pub struct Mirror(Arc<Mutex<Option<MirrorTo>>>);

struct MirrorTo {
    /// 本端在直连上开的控制通道（的写任务）
    frames: mpsc::Sender<Bytes>,
    conn: Connection,
}

/// 直连上最多排着这么多帧没写出去，再多就不写了（中转上还有一份）
const MIRROR_QUEUE: usize = 64;

impl Mirror {
    /// 写任务每加密一帧就调用一次（`sealed`：带长度前缀的密文）
    pub(crate) fn offer(&self, sealed: &Bytes) {
        let mut to = self.0.lock().unwrap();
        if let Some(t) = &*to
            && let Err(mpsc::error::TrySendError::Closed(_)) = t.frames.try_send(sealed.clone())
        {
            *to = None;
        }
    }

    /// 一个控制数据报（加密好的）也从直连发
    pub(crate) fn datagram(&self, d: &Bytes) {
        if let Some(t) = &*self.0.lock().unwrap() {
            let _ = t.conn.send_datagram(d.clone());
        }
    }

    /// 从现在起每一帧也写进直连 `conn` 上的 `send`（本端在直连上开的控制通道），每个控制数据报也从 `conn` 发。
    /// 返回写的任务（直连断了、[`Mirror::stop`] 之后结束）
    pub(crate) fn start(
        &self,
        mut send: SendStream,
        conn: Connection,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (tx, mut rx) = mpsc::channel::<Bytes>(MIRROR_QUEUE);
        *self.0.lock().unwrap() = Some(MirrorTo { frames: tx, conn });
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

/// 问路由用的公网地址（[`route_ip`]：UDP 的 connect 不发包，哪个公网地址都行）
const PUBLIC_V4: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 53);
const PUBLIC_V6: SocketAddr = SocketAddr::new(
    IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
    53,
);

/// 本端的候选地址（端口都是连中转的那个）：服务器看到的公网地址（`observed`），往中转去时用的地址（中转在局域网里
/// 的话是局域网地址），和往公网去时用的 IPv4（多半是局域网地址）、IPv6 地址（多半就是公网地址，没有 NAT）。
/// socket 只能说 IPv4 时不给 IPv6 的
pub(crate) fn candidates(
    endpoint: &Endpoint,
    relay: &Connection,
    observed: SocketAddr,
) -> Vec<SocketAddr> {
    let mut out = vec![canonical(observed)];
    let Ok(local) = endpoint.local_addr() else {
        return out;
    };
    for to in [canonical(relay.remote_address()), PUBLIC_V4, PUBLIC_V6] {
        if to.is_ipv6() && local.is_ipv4() {
            continue;
        }
        if let Some(ip) = route_ip(to) {
            let a = SocketAddr::new(ip, local.port());
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    out.truncate(MAX_DIRECT_ADDRS);
    out
}

/// 往 `to` 去时本机用哪个地址：UDP 的 connect 不发包，只是选好路由。没有路由、只有链路本地地址时为 `None`
fn route_ip(to: SocketAddr) -> Option<IpAddr> {
    let bind: SocketAddr = if to.is_ipv4() {
        (Ipv4Addr::UNSPECIFIED, 0).into()
    } else {
        (Ipv6Addr::UNSPECIFIED, 0).into()
    };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(to).ok()?;
    let ip = s.local_addr().ok()?.ip();
    let usable = match ip {
        IpAddr::V4(ip) => !ip.is_unspecified(),
        IpAddr::V6(ip) => !(ip.is_unspecified() || ip.is_unicast_link_local()),
    };
    usable.then_some(ip)
}

/// IPv4 映射的 IPv6 地址（双栈的 socket 看到的 IPv4 对端）写成 IPv4
pub(crate) fn canonical(a: SocketAddr) -> SocketAddr {
    SocketAddr::new(a.ip().to_canonical(), a.port())
}

/// 对方的候选地址里本端用得上的：本端的 socket 说得了的地址族（双栈的两种都行，只有 IPv4 的只要 IPv4），
/// 不是 0.0.0.0、组播、广播，最多 [`MAX_DIRECT_ADDRS`] 个
pub(crate) fn usable(theirs: &[SocketAddr], endpoint: &Endpoint) -> Vec<SocketAddr> {
    let Ok(local) = endpoint.local_addr() else {
        return Vec::new();
    };
    let mut out: Vec<SocketAddr> = theirs
        .iter()
        .take(MAX_DIRECT_ADDRS)
        .map(|a| canonical(*a))
        .filter(|a| (a.is_ipv4() || local.is_ipv6()) && a.port() != 0)
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
