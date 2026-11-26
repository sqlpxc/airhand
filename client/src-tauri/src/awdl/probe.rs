//! 这台 Mac 现在是不是用 Wi-Fi 联网、AWDL 开着没有。只读，不需要 root。
//!
//! 网卡是有线还是 Wi-Fi 看系统给它标的"功能类型"（`SIOCGIFFUNCTIONALTYPE`，Network.framework 的
//! `nw_interface_type` 用的也是它）：名字靠不住（USB 网卡、雷雳扩展坞也叫 enX）。

use std::ffi::CStr;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// `<sys/sockio.h>`：`_IOWR('i', 17, struct ifreq)`、`_IOWR('i', 173, struct ifreq)`
const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;
const SIOCGIFFUNCTIONALTYPE: libc::c_ulong = 0xc020_69ad;

/// `<net/if.h>` 的 `IFRTYPE_FUNCTIONAL_*`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Wired,
    Wifi,
    Cellular,
    /// 隧道（VPN 的 utun、ipsec……）、桥接和别的：说不上走的是哪块物理网卡
    Other,
}

impl Kind {
    fn from_functional_type(t: u32) -> Kind {
        match t {
            2 => Kind::Wired,
            3 => Kind::Wifi,
            5 => Kind::Cellular,
            _ => Kind::Other,
        }
    }
}

/// 一个开着、连着、有可用地址的网卡
#[derive(Debug, Clone)]
pub(super) struct Link {
    pub kind: Kind,
    pub ip: IpAddr,
}

/// 去远处的流量从哪块网卡出去：Wi-Fi 就是 true。
///
/// 先按路由表看出口（往一个文档用的地址 `connect` 一个 UDP 套接字，不发包，看系统选的源地址在哪块网卡上）；
/// 出口是 VPN 这类隧道时看不出底下是哪块网卡，就看现在有没有接着网线：接着就算有线，不然有 Wi-Fi 就是 Wi-Fi。
/// 被控端和控制端到中转服务器、到对方的直连，系统都按同样的路由选网卡，所以只看默认的出口
pub fn wifi_in_use() -> bool {
    let links = links();
    decide(egress().as_ref(), &links)
}

/// awdl0 开着
pub fn awdl_up() -> bool {
    flags("awdl0").is_some_and(|f| f & libc::IFF_UP != 0)
}

pub(super) fn decide(egress: Option<&IpAddr>, links: &[Link]) -> bool {
    if let Some(kind) = egress
        .and_then(|ip| links.iter().find(|l| l.ip == *ip))
        .map(|l| l.kind)
    {
        match kind {
            Kind::Wifi => return true,
            Kind::Wired | Kind::Cellular => return false,
            Kind::Other => {}
        }
    }
    !links.iter().any(|l| l.kind == Kind::Wired) && links.iter().any(|l| l.kind == Kind::Wifi)
}

/// 去远处时系统选的源地址（IPv4 优先；没有 IPv4 的默认路由时看 IPv6）
fn egress() -> Option<IpAddr> {
    let probes = [
        (
            SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
            SocketAddr::new(Ipv4Addr::new(192, 0, 2, 1).into(), 9),
        ),
        (
            SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
            SocketAddr::new(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1).into(), 9),
        ),
    ];
    probes.into_iter().find_map(|(bind, to)| {
        let s = UdpSocket::bind(bind).ok()?;
        s.connect(to).ok()?;
        let ip = s.local_addr().ok()?.ip();
        (!ip.is_unspecified()).then_some(ip)
    })
}

/// 开着（`IFF_UP | IFF_RUNNING`）、有可用地址（不是回环、不是本地链路）的网卡
fn links() -> Vec<Link> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return out;
    }
    let mut p = head;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        let up = libc::IFF_UP as u32 | libc::IFF_RUNNING as u32;
        if ifa.ifa_flags & up != up || ifa.ifa_addr.is_null() {
            continue;
        }
        let Some(ip) = (unsafe { ip_of(ifa.ifa_addr) }) else {
            continue;
        };
        if !usable(ip) {
            continue;
        }
        let name = unsafe { CStr::from_ptr(ifa.ifa_name) }.to_string_lossy();
        let kind = functional_type(&name).map_or(Kind::Other, Kind::from_functional_type);
        out.push(Link { kind, ip });
    }
    unsafe { libc::freeifaddrs(head) };
    out
}

unsafe fn ip_of(sa: *const libc::sockaddr) -> Option<IpAddr> {
    match unsafe { (*sa).sa_family } as i32 {
        libc::AF_INET => {
            let a = unsafe { &*(sa as *const libc::sockaddr_in) };
            Some(Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr)).into())
        }
        libc::AF_INET6 => {
            let a = unsafe { &*(sa as *const libc::sockaddr_in6) };
            Some(Ipv6Addr::from(a.sin6_addr.s6_addr).into())
        }
        _ => None,
    }
}

fn usable(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(v6) => !v6.is_loopback() && !v6.is_unspecified() && !v6.is_unicast_link_local(),
    }
}

fn request(name: &str) -> Option<libc::ifreq> {
    let mut r: libc::ifreq = unsafe { std::mem::zeroed() };
    let bytes = name.as_bytes();
    if bytes.len() >= r.ifr_name.len() {
        return None;
    }
    for (d, s) in r.ifr_name.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    Some(r)
}

/// 对一块网卡做一次 `SIOCGIF*`（UDP 套接字只是用来发 ioctl）
fn ioctl(name: &str, op: libc::c_ulong) -> Option<libc::ifreq> {
    let mut r = request(name)?;
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return None;
    }
    let ok = unsafe { libc::ioctl(fd, op, &mut r) } == 0;
    unsafe { libc::close(fd) };
    ok.then_some(r)
}

fn flags(name: &str) -> Option<i32> {
    ioctl(name, SIOCGIFFLAGS).map(|r| unsafe { r.ifr_ifru.ifru_flags } as u16 as i32)
}

fn functional_type(name: &str) -> Option<u32> {
    ioctl(name, SIOCGIFFUNCTIONALTYPE).map(|r| unsafe { r.ifr_ifru.ifru_functional_type })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: Kind, ip: &str) -> Link {
        Link {
            kind,
            ip: ip.parse().unwrap(),
        }
    }

    #[test]
    fn decides_by_the_egress_interface() {
        let wifi = link(Kind::Wifi, "192.168.1.5");
        let wired = link(Kind::Wired, "192.168.1.6");
        let vpn = link(Kind::Other, "10.8.0.2");
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        // 出口是哪块就是哪块（两块都接着的时候服务顺序决定出口）
        assert!(decide(
            Some(&ip("192.168.1.5")),
            &[wifi.clone(), wired.clone()]
        ));
        assert!(!decide(
            Some(&ip("192.168.1.6")),
            &[wifi.clone(), wired.clone()]
        ));
        // VPN：接着网线就当走网线，只有 Wi-Fi 就是 Wi-Fi
        assert!(!decide(
            Some(&ip("10.8.0.2")),
            &[wifi.clone(), wired.clone(), vpn.clone()]
        ));
        assert!(decide(Some(&ip("10.8.0.2")), &[wifi.clone(), vpn.clone()]));
        // 没有默认路由（没联网）、手机热点（蜂窝）
        assert!(decide(None, std::slice::from_ref(&wifi)));
        assert!(!decide(None, &[]));
        let cell = link(Kind::Cellular, "100.64.0.9");
        assert!(!decide(Some(&ip("100.64.0.9")), &[cell, wifi]));
    }

    #[test]
    fn link_local_and_loopback_addresses_are_ignored() {
        for s in ["127.0.0.1", "169.254.3.4", "::1", "fe80::1", "0.0.0.0"] {
            assert!(!usable(s.parse().unwrap()), "{s}");
        }
        for s in ["192.168.1.5", "10.0.0.1", "2001:db8::5", "fd00::3"] {
            assert!(usable(s.parse().unwrap()), "{s}");
        }
    }

    /// 本机：回环网卡的功能类型是 loopback，能读到；awdl0 不在也只是 false
    #[test]
    fn reads_this_mac() {
        assert_eq!(functional_type("lo0"), Some(1));
        assert!(flags("lo0").is_some_and(|f| f & libc::IFF_LOOPBACK != 0));
        assert!(flags("nope9").is_none());
        let _ = (awdl_up(), wifi_in_use());
        assert!(links().iter().all(|l| !l.ip.is_loopback()));
    }
}
