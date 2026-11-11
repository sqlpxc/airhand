//! 给 QUIC 用的 UDP socket：中转服务器和客户端共用（客户端的打洞包装在 `direct::socket`）。
//!
//! 收发缓冲区要调大。系统默认的很小（Windows 几十 KB、Linux 约 208 KB），100 Mbps 的画面几毫秒就能填满：
//! 一次调度抖动就整批丢包，QUIC 随之重传、拥塞控制退让，画面卡一下、延迟跳一下。

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};

use socket2::{Domain, Protocol, Socket, Type};
use tracing::{debug, warn};

/// 想要的收发缓冲区大小。系统不给这么大时依次减半（macOS 的上限略小于 8 MiB）；
/// Linux 上超过 `net.core.rmem_max` / `wmem_max` 的部分会被悄悄截掉，见 [`bind`] 的警告
pub const BUFFER: usize = 8 << 20;
/// 减到这么小还设不上就算了，用系统的
const MIN_BUFFER: usize = 256 << 10;

/// 绑定一个 UDP socket，收发缓冲区尽量调大到 [`BUFFER`]。`dual`：IPv6 的 socket 同时收发 IPv4（关掉 `IPV6_V6ONLY`）
pub fn bind(addr: SocketAddr, dual: bool) -> io::Result<UdpSocket> {
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    if dual {
        s.set_only_v6(false)?;
    }
    enlarge(|n| s.set_recv_buffer_size(n));
    enlarge(|n| s.set_send_buffer_size(n));
    s.bind(&addr.into())?;
    let (recv, send) = (
        s.recv_buffer_size().unwrap_or(0),
        s.send_buffer_size().unwrap_or(0),
    );
    debug!(%addr, recv, send, "UDP socket buffers");
    // Linux 报告的是设上的两倍；被 rmem_max 截掉的话远小于想要的
    static WARNED: AtomicBool = AtomicBool::new(false);
    if recv.min(send) < BUFFER / 2 && !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            recv,
            send,
            "the system capped the UDP socket buffers (Linux: raise net.core.rmem_max and net.core.wmem_max to {BUFFER}); \
             high bitrates may drop packets"
        );
    }
    Ok(s.into())
}

/// 从 [`BUFFER`] 开始试，设不上就减半
fn enlarge(set: impl Fn(usize) -> io::Result<()>) {
    let mut n = BUFFER;
    while n >= MIN_BUFFER && set(n).is_err() {
        n /= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_enlarged() {
        let s = bind((std::net::Ipv4Addr::LOCALHOST, 0).into(), false).unwrap();
        let r = socket2::SockRef::from(&s);
        // 系统可能截掉一部分（Linux 的 rmem_max），但不会比默认的还小
        assert!(r.recv_buffer_size().unwrap() >= MIN_BUFFER.min(200 << 10));
        assert!(s.local_addr().unwrap().port() != 0);
    }
}
