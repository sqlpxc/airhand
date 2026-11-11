//! 直连：在中转之外再打通一条两端之间的路（协议见 `airhand_proto` 的"直连"）。
//!
//! * [`Punch`]（`socket`）：连中转的那个 UDP socket 也用来打洞（同一个端口、同一个 NAT 映射）；
//! * `tls`：直连的 QUIC 配置和证书。两端各有一张临时生成的自签名证书，指纹在加密的控制通道上交换
//!   （`DirectOffer::cert`），TLS 双向认证只认它：握手完成时就知道对面是谁，中间人冒充不了任何一边
//!   （和 WebRTC 把 DTLS 证书的指纹放进信令里一样）；
//! * 直连上对方的流和数据报由 [`crate::lanes::accept_direct`] 交给会话，和中转上来的放在一起合并（`crate::merge`）；
//! * 本端往哪条路发由会话的 [`Paths`] 决定：直连通了接上去（控制通道每一帧、Ping / Pong / 确认两条路都发），
//!   断了拿下来；
//! * 候选地址两种地址族都给（[`candidates`]）：连中转的 socket 是双栈的（`socket::bind`），中转走 IPv4 时
//!   两端也能用公网 IPv6 直连：IPv6 一般没有 NAT，打洞只是让两边的防火墙放行，IPv4 打不通（对称 NAT、运营商级 NAT）时往往通；
//! * 画面、声音走哪条路由被控端决定（[`Paths::media`]，`select` 里的规则）：它在两条路上各发探测
//!   （`HostDatagram::Probe`），控制端从同一条路弹回来（见 [`crate::controller::HostReceiver`]）；
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

use airhand_proto::session::MAX_DIRECT_ADDRS;
use quinn::{Connection, Endpoint};

pub use crate::lanes::Via;
pub use crate::paths::Paths;
pub use host::{DirectHub, HostLink};
pub use socket::Punch;
pub(crate) use socket::bind;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapped_addresses_are_canonical() {
        let mapped: SocketAddr = "[::ffff:203.0.113.9]:5000".parse().unwrap();
        assert_eq!(canonical(mapped), "203.0.113.9:5000".parse().unwrap());
        let v6: SocketAddr = "[2001:db8::1]:5000".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }
}
