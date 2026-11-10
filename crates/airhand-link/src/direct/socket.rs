//! 连中转用的 UDP socket：quinn 照常用它收发；打洞时从**同一个** socket 发几个字节（同一个本地端口，
//! 也就是服务器看到的那个 NAT 映射），对方的 NAT 才认得出来、放行之后的直连。

use std::io::{self, IoSliceMut};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, Runtime, UdpPoller};

/// 打洞包：几个字节，第一个字节是 0。不是 QUIC 包（QUIC 的短包头后面至少还有 8 字节的连接 ID），
/// 对方的 quinn 收到了直接丢掉，也不会回一个无状态重置
const PUNCH: [u8; 4] = [0, b'a', b'h', b'p'];

/// 包了一层的 socket：除了打洞，什么都原样交给 quinn 自己的实现
#[derive(Debug)]
struct PunchSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    /// 测试用：只往这个地址发，发给别处的悄悄丢掉（模拟直连断了）
    #[cfg(feature = "test-hooks")]
    only_to: std::sync::Mutex<Option<SocketAddr>>,
}

impl AsyncUdpSocket for PunchSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        #[cfg(feature = "test-hooks")]
        if self
            .only_to
            .lock()
            .unwrap()
            .is_some_and(|a| a != transmit.destination)
        {
            return Ok(());
        }
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.inner.poll_recv(cx, bufs, meta)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

/// 从连中转的那个 socket 发打洞包
#[derive(Clone, Debug)]
pub struct Punch(Arc<PunchSocket>);

impl Punch {
    /// 往 `to` 发一个打洞包（打开本端 NAT 上通往它的映射）。发不出去就算了：过一会儿还会再发
    pub(crate) fn send(&self, to: SocketAddr) {
        let _ = self.0.try_send(&Transmit {
            destination: to,
            ecn: None,
            contents: &PUNCH,
            segment_size: None,
            src_ip: None,
        });
    }

    /// 测试用：这个 socket 只往 `addr` 发包（`None`：恢复正常），发给别处的悄悄丢掉。
    /// 让被控端只能和中转说话，就模拟出直连突然断了
    #[cfg(feature = "test-hooks")]
    pub fn only_to(&self, addr: Option<SocketAddr>) {
        *self.0.only_to.lock().unwrap() = addr;
    }
}

/// [`bind`] 的结果：交给 quinn 的 socket、打洞用的句柄、quinn 的运行时
pub(crate) type Bound = (Arc<dyn AsyncUdpSocket>, Punch, Arc<dyn Runtime>);

/// 绑定一个 UDP socket（`addr` 通常是 `0.0.0.0:0` / `[::]:0`），给 quinn 用、也能打洞
pub(crate) fn bind(addr: SocketAddr) -> io::Result<Bound> {
    let runtime =
        quinn::default_runtime().ok_or_else(|| io::Error::other("no async runtime found"))?;
    let socket = std::net::UdpSocket::bind(addr)?;
    let socket = Arc::new(PunchSocket {
        inner: runtime.wrap_udp_socket(socket)?,
        #[cfg(feature = "test-hooks")]
        only_to: Default::default(),
    });
    Ok((socket.clone(), Punch(socket), runtime))
}
