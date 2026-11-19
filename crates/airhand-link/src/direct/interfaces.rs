//! 用活动网卡补全直连候选。系统访问中转或公网时选择的出口可能全在 VPN 上。

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;

use airhand_proto::session::MAX_DIRECT_ADDRS;
use if_addrs::Interface;
use tracing::debug;

/// 系统枚举可能阻塞。枚举失败时照样发中转观察到的地址和路由选出的地址。
pub(super) async fn add(out: &mut Vec<SocketAddr>, local: SocketAddr, round: u32) {
    let interfaces = tokio::task::spawn_blocking(if_addrs::get_if_addrs)
        .await
        .map_err(std::io::Error::other)
        .and_then(|r| r);
    match interfaces {
        Ok(interfaces) => append(out, local, round, &interfaces),
        Err(e) => debug!(%e, "couldn't enumerate network interfaces for the direct path"),
    }
}

fn append(out: &mut Vec<SocketAddr>, local: SocketAddr, round: u32, interfaces: &[Interface]) {
    let mut by_interface: BTreeMap<_, BTreeSet<SocketAddr>> = BTreeMap::new();
    for iface in interfaces.iter().filter(|i| i.is_oper_up()) {
        let ip = iface.ip().to_canonical();
        if ip.is_loopback() || !super::usable_ip(ip) || (ip.is_ipv6() && local.is_ipv4()) {
            continue;
        }
        by_interface
            .entry((iface.is_p2p(), &iface.name))
            .or_default()
            .insert(SocketAddr::new(ip, local.port()));
    }

    // 每块网卡先取一个地址，再取第二个。同一层里普通接口优先于点对点隧道，IPv4 优先于 IPv6。
    // 固定顺序也使下一轮可以从上次没发到的地方继续，与系统枚举地址的先后无关。
    let mut extra = Vec::new();
    for ((point_to_point, _), addrs) in by_interface {
        for (ordinal, addr) in addrs.into_iter().enumerate() {
            extra.push((ordinal, point_to_point, addr));
        }
    }
    extra.sort_unstable();
    let mut seen: BTreeSet<_> = out.iter().copied().collect();
    extra.retain(|(_, _, addr)| seen.insert(*addr));

    let room = MAX_DIRECT_ADDRS - out.len();
    if extra.len() > room {
        let skip = u64::from(round.saturating_sub(1)) * room as u64 % extra.len() as u64;
        extra.rotate_left(skip as usize);
    }
    out.extend(extra.into_iter().take(room).map(|(_, _, addr)| addr));
}

#[cfg(test)]
mod tests {
    use super::*;
    use if_addrs::{IfAddr, IfOperStatus, Ifv4Addr, Ifv6Addr};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn iface(name: &str, ip: &str) -> Interface {
        let addr = match ip.parse().unwrap() {
            IpAddr::V4(ip) => IfAddr::V4(Ifv4Addr {
                ip,
                netmask: Ipv4Addr::UNSPECIFIED,
                prefixlen: 0,
                broadcast: None,
            }),
            IpAddr::V6(ip) => IfAddr::V6(Ifv6Addr {
                ip,
                netmask: Ipv6Addr::UNSPECIFIED,
                prefixlen: 0,
                broadcast: None,
            }),
        };
        Interface {
            name: name.to_owned(),
            addr,
            index: None,
            oper_status: IfOperStatus::Up,
            is_p2p: false,
            #[cfg(windows)]
            adapter_name: name.to_owned(),
        }
    }

    #[test]
    fn a_vpn_default_route_still_offers_the_other_lan_interfaces() {
        let observed = "203.0.113.10:43000".parse().unwrap();
        let vpn = "10.8.0.2:5000".parse().unwrap();
        let mut out = vec![observed, vpn];
        let mut tunnel = iface("vpn", "10.8.0.2");
        tunnel.is_p2p = true;
        let interfaces = [
            tunnel,
            iface("ethernet", "192.168.50.4"),
            iface("ethernet", "::ffff:192.168.50.4"),
            iface("wifi", "fd00::4"),
            iface("wan", "203.0.113.10"),
        ];
        append(&mut out, "[::]:5000".parse().unwrap(), 1, &interfaces);
        assert_eq!(out[..2], [observed, vpn]);
        assert_eq!(out.len(), 5);
        for a in ["192.168.50.4:5000", "[fd00::4]:5000", "203.0.113.10:5000"] {
            assert!(out.contains(&a.parse().unwrap()));
        }
    }

    #[test]
    fn filters_inactive_interfaces_and_unusable_addresses_for_the_socket() {
        let mut down = iface("offline", "192.168.1.4");
        down.oper_status = IfOperStatus::Down;
        let mut interfaces = vec![down];
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "ff02::1",
            "fe80::1",
            "169.254.1.2",
            "fd00::2",
        ] {
            interfaces.push(iface("ethernet", ip));
        }
        let mut out = Vec::new();
        append(&mut out, "0.0.0.0:5000".parse().unwrap(), 1, &interfaces);
        let v4 = "169.254.1.2:5000".parse().unwrap();
        assert_eq!(out, [v4]);
        out.clear();
        append(&mut out, "[::]:5000".parse().unwrap(), 1, &interfaces);
        assert_eq!(out, [v4, "[fd00::2]:5000".parse().unwrap()]);
    }

    #[test]
    fn retries_rotate_overflow_without_filling_the_first_offer_with_aliases() {
        let mut interfaces: Vec<_> = (1..=12)
            .map(|n| iface("virtual", &format!("10.0.0.{n}")))
            .collect();
        let lan = "192.168.1.4:5000".parse().unwrap();
        interfaces.push(iface("ethernet", "192.168.1.4"));
        let observed = "203.0.113.7:45000".parse().unwrap();
        let route = "10.8.0.1:5000".parse().unwrap();
        let mut offered = BTreeSet::new();
        for round in 1..=3 {
            let mut out = vec![observed, route];
            append(&mut out, "[::]:5000".parse().unwrap(), round, &interfaces);
            assert_eq!(out.len(), MAX_DIRECT_ADDRS);
            assert_eq!(out[..2], [observed, route]);
            if round == 1 {
                assert!(out.contains(&lan));
            }
            offered.extend(out);
            interfaces.reverse();
        }
        assert_eq!(offered.len(), interfaces.len() + 2);
    }
}
