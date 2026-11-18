//! Airhand 客户端一侧的链路，桌面客户端（`client/src-tauri`）、网页控制台（`web/`）和 Android（`android/native`）共用：
//!
//! * [`net`]：QUIC 连接、证书固定、版本握手、断线重连；
//! * [`wire`]：从流里读写长度前缀帧（帧的格式和消息的编解码在 `airhand_proto::framing`，那边不碰 IO）；
//! * [`secure`]：端到端加密：设备的身份密钥、握手、每一帧的加解密；
//! * [`lanes`]：会话的通道（每端为自己要发的开单向流）和数据报从哪里来，中转和直连共用；
//!   [`paths`]：往哪条路发（中转、直连，控制帧和 Ping / 确认两条路都发，画面、声音只走一条）；
//!   [`merge`]：几条路上的帧按计数排好；[`transport`]：两端共用的通道写任务和消息读取；
//! * [`udp`]：收发缓冲区调大了的 UDP socket（中转服务器也用）；
//! * [`direct`]：中转之外两端之间的直连（打洞、TLS 只认对方的证书指纹、画面走哪条路）；
//! * [`controller`]：控制端服务与会话。与界面无关：界面的命令是 [`controller::Command`]，
//!   画面和事件交给 [`controller::SessionUi`]；
//! * [`console`]（feature `console`）：控制台和控制端服务之间的 WebSocket 协议，桌面版和网页版共用；
//! * [`host`]：被控端与服务器的连接和会话（协议层的事在这里，采集、编码、注入输入由各平台实现 [`host::Platform`]）；
//! * [`files`]：剪贴板里的文件（复制时只发清单，粘贴时才传内容），两端共用；
//! * [`flow`]：被控端的在途控制（按字节）和码率控制（按排队时延和带宽估计）；[`refs`]：每一帧参考哪一帧、
//!   控制端等不到帧时从哪一帧恢复（长期参考帧、参考帧失效）；
//! * [`Problem`]：显示给用户的问题，以类型而不是文字的形式传给界面。
//!
//! 两端的消息通道用同一种发送队列（`queue`：只留最新值的槽位 + 有界的队列），时钟在 `clock`。
//! 声音数据报的副本在 `audio` 里处理。
//!
//! # feature
//!
//! 除了 [`wire`] 和 [`udp`]，都在默认打开的 `client` 里。中转服务器关掉它（`default-features = false`）、只用这两个：
//! 读写几帧不用把 Noise、SPAKE2 这些它用不到的库也编译进去。

#[cfg(feature = "client")]
mod audio;
#[cfg(feature = "client")]
mod clock;
#[cfg(feature = "console")]
pub mod console;
#[cfg(feature = "client")]
pub mod controller;
#[cfg(feature = "client")]
pub mod direct;
#[cfg(feature = "client")]
pub mod files;
#[cfg(feature = "client")]
pub mod flow;
#[cfg(feature = "client")]
pub mod host;
#[cfg(feature = "client")]
pub mod lanes;
#[cfg(feature = "client")]
pub mod merge;
#[cfg(feature = "client")]
pub mod net;
#[cfg(feature = "client")]
pub mod paths;
#[cfg(feature = "client")]
mod problem;
#[cfg(feature = "client")]
mod queue;
#[cfg(feature = "client")]
pub mod refs;
#[cfg(feature = "client")]
pub mod secure;
#[cfg(feature = "client")]
pub mod tasks;
#[cfg(feature = "client")]
pub mod transport;
pub mod udp;
pub mod wire;

#[cfg(feature = "client")]
pub use net::{LinkConfig, ServerState};

#[cfg(all(test, feature = "client"))]
mod tests {
    /// 进程内的一对 QUIC 连接（本机回环，直连那套只认证书指纹的 TLS）：要真的 QUIC 流的测试用
    pub(crate) async fn quic_pair() -> (quinn::Connection, quinn::Connection) {
        use crate::direct::tls::{Cert, SERVER_NAME, client_config, server_config};
        let (server_cert, client_cert) = (Cert::generate().unwrap(), Cert::generate().unwrap());
        let localhost = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let server =
            quinn::Endpoint::server(server_config(&server_cert).unwrap(), localhost).unwrap();
        let client = quinn::Endpoint::client(localhost).unwrap();
        let config = client_config(&client_cert, server_cert.fingerprint).unwrap();
        let connecting = client
            .connect_with(config, server.local_addr().unwrap(), SERVER_NAME)
            .unwrap();
        let (c, s) = tokio::join!(connecting, async { server.accept().await.unwrap().await });
        (c.unwrap(), s.unwrap())
    }

    /// 同上，但客户端 → 服务器的包经过一个会丢包、有时延的 UDP 中继：每个包晚 `delay` 到，按 `loss` 的比例丢掉
    /// （握手完了才开始丢）。服务器 → 客户端的只加时延。返回的任务丢掉时中继停下。
    ///
    /// 不能每个包各睡各的：Windows 的定时器十几毫秒一跳，各睡各的包醒来的先后会乱，乱序也会让 QUIC 的流读到一半停下来。
    /// 所以每个方向排一个队，按到达的先后放出去（定时器晚了就晚一批，不乱序）。
    /// 三个 socket 的收发缓冲区都调大（见 crate::udp）：不然一个大帧的一串包就把它们溢出了，"不丢包"的路也丢包
    pub(crate) async fn lossy_quic_pair(
        loss: f64,
        delay: std::time::Duration,
    ) -> (
        quinn::Connection,
        quinn::Connection,
        crate::tasks::OwnedTask<()>,
    ) {
        use crate::direct::tls::{Cert, SERVER_NAME, client_config, server_config};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::mpsc;
        use tokio::time::Instant;
        let (server_cert, client_cert) = (Cert::generate().unwrap(), Cert::generate().unwrap());
        let localhost = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        let endpoint = |server: Option<quinn::ServerConfig>| {
            let socket = crate::udp::bind(localhost, false).unwrap();
            quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                server,
                socket,
                Arc::new(quinn::TokioRuntime),
            )
            .unwrap()
        };
        let server = endpoint(Some(server_config(&server_cert).unwrap()));
        let server_addr = server.local_addr().unwrap();
        let proxy = Arc::new(
            tokio::net::UdpSocket::from_std({
                let s = crate::udp::bind(localhost, false).unwrap();
                s.set_nonblocking(true).unwrap();
                s
            })
            .unwrap(),
        );
        let proxy_addr = proxy.local_addr().unwrap();
        let lossy = Arc::new(AtomicBool::new(false));
        // 一个方向的队：按到达的先后、晚 `delay` 放出去
        let queue = move |proxy: Arc<tokio::net::UdpSocket>| {
            let (tx, mut rx) =
                mpsc::unbounded_channel::<(Instant, Vec<u8>, std::net::SocketAddr)>();
            let task = tokio::spawn(async move {
                while let Some((at, packet, to)) = rx.recv().await {
                    tokio::time::sleep_until(at + delay).await;
                    let _ = proxy.send_to(&packet, to).await;
                }
            });
            (tx, task)
        };
        let relay = {
            let (proxy, lossy) = (proxy.clone(), lossy.clone());
            tokio::spawn(async move {
                let (to_server, _up) = queue(proxy.clone());
                let (to_client, _down) = queue(proxy.clone());
                let _tasks = (crate::tasks::OwnedTask(_up), crate::tasks::OwnedTask(_down));
                let mut client_addr = None;
                let mut buf = vec![0u8; 65536];
                // 固定的伪随机数：每次跑丢的包一样多
                let mut seed = 0x2545_f491_4f6c_dd1du64;
                while let Ok((n, from)) = proxy.recv_from(&mut buf).await {
                    let now = Instant::now();
                    if from == server_addr {
                        if let Some(to) = client_addr {
                            let _ = to_client.send((now, buf[..n].to_vec(), to));
                        }
                        continue;
                    }
                    client_addr = Some(from);
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let roll = (seed >> 11) as f64 / (1u64 << 53) as f64;
                    if !(lossy.load(Ordering::Relaxed) && roll < loss) {
                        let _ = to_server.send((now, buf[..n].to_vec(), server_addr));
                    }
                }
            })
        };
        let client = endpoint(None);
        let config = client_config(&client_cert, server_cert.fingerprint).unwrap();
        let connecting = client
            .connect_with(config, proxy_addr, SERVER_NAME)
            .unwrap();
        let (c, s) = tokio::join!(connecting, async { server.accept().await.unwrap().await });
        lossy.store(true, Ordering::Relaxed);
        (c.unwrap(), s.unwrap(), crate::tasks::OwnedTask(relay))
    }
}
#[cfg(feature = "client")]
pub use problem::Problem;
