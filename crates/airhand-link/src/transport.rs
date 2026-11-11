//! 会话通道的收发，两端共用：[`write_lane`] 把排好队的帧加密后写进本端的一条通道（控制通道在直连通着时也从直连发一份，
//! 见 [`crate::paths`]），[`next_msg`] 从一条消息通道上取下一条消息，[`open_datagram`] 解开一个控制数据报。
//!
//! 各端自己的在别处：被控端的画面、声音写任务在 `host::write`，控制端的接收端在 [`crate::controller::HostReceiver`]；
//! 通道和数据报从哪里来在 [`crate::lanes`]，几条路上的帧怎么合并在 [`crate::merge`]。
//!
//! 每条通道一条 QUIC 流，有自己的流量控制和丢包重传：画面堵住不影响控制消息，1 MiB 的剪贴板也不挡输入
//! （整条连接的拥塞控制还是共用的）。声音和控制数据报走 QUIC 数据报：不重传，也不排在丢了的包、流里别的消息后面等，
//! 在 QUIC 包里排在所有流的前面。
use crate::merge::{Frame, Merge};
use crate::paths::Paths;
use airhand_proto::SessionLane;
use airhand_proto::datagram;
use airhand_proto::framing::{self, Decoded};
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::minicbor::Decode;
use airhand_proto::session::{ControllerMsg, HostMsg};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::SendStream;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use crate::secure::{Opener, Sealer};

/// 加密一帧（`u32 长度 | 明文`）。编码时多留了加密开销的位置，通常不用重新分配（[`framing::encode_with_spare`]、
/// `airhand_proto::media::frame`）；同一份数据发给几个会话时（例如 Android 的广播）这里拷贝一次，每个会话的密钥不一样
pub fn seal(frame: Bytes, sealer: &mut Sealer) -> Result<Bytes> {
    let mut v = Vec::from(frame);
    v.reserve(SEAL_OVERHEAD);
    sealer.seal_frame(&mut v)?;
    Ok(Bytes::from(v))
}

/// 写任务从哪里取下一帧（明文，已经带长度前缀）：消息通道是 `crate::queue` 的队列，大块通道也可以是普通的 `mpsc`
pub trait Frames: Send {
    /// 下一帧；`None` 表示发完了，正常结束这条流
    fn next(&mut self) -> impl Future<Output = Option<Bytes>> + Send;
}

impl Frames for mpsc::Receiver<Bytes> {
    async fn next(&mut self) -> Option<Bytes> {
        self.recv().await
    }
}

/// 十秒钟写不进去一帧算对方失联
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// 往中转上的一条流写一帧密文：写不进去是错误（会话结束）
pub(crate) async fn write_sealed(send: &mut SendStream, sealed: Bytes) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, send.write_chunk(sealed))
        .await
        .context("session writer stalled")??;
    Ok(())
}

/// 把 `rx` 里排好队的帧（明文，已经带长度前缀）加密后依次写进本端的一条通道，`rx` 发完后正常结束这条流。
/// `mirror`：控制通道在直连通着时每一帧也从直连发一份（中转上照样是完整的一份，见 [`Paths::mirror`]）
pub async fn write_lane(
    mut send: SendStream,
    mut rx: impl Frames,
    mut sealer: Sealer,
    mirror: Option<Paths>,
) -> Result<()> {
    while let Some(bytes) = rx.next().await {
        let sealed = seal(bytes, &mut sealer)?;
        if let Some(p) = &mirror {
            p.mirror(&sealed);
        }
        write_sealed(&mut send, sealed).await?;
    }
    send.finish()?;
    Ok(())
}

/// 解开一个数据报（整个数据报：会话 ID、种类、密封单元）里的消息。解不开（被改过、重放的、另一条路上后到的那一份）、
/// 不认识（对方版本新增的）的都是 `None`：数据报本来就会丢
pub(crate) fn open_datagram<T: for<'b> Decode<'b, ()>>(opener: &mut Opener, d: &[u8]) -> Option<T> {
    let mut body = d.get(datagram::HEADER..)?.to_vec();
    opener.open(&mut body).ok()?;
    match framing::decode(&body) {
        Ok(Decoded::Msg(m)) => Some(m),
        _ => None,
    }
}

/// 一条消息通道（控制、大块）上的消息：解码、检查通道；对方版本新增的消息跳过。通道结束时为 `None`。
/// 被控端和控制端共用（`M` 是对方发来的消息）。可以放在 `select!` 里（取消不丢消息）
pub(crate) async fn next_msg<M>(merge: &mut Merge, lane: SessionLane) -> Result<Option<M>>
where
    M: for<'b> Decode<'b, ()> + LaneOf,
{
    loop {
        let Some(Frame { data, .. }) = merge.next().await? else {
            return Ok(None);
        };
        match framing::decode::<M>(&data)? {
            Decoded::Msg(m) => {
                anyhow::ensure!(m.lane() == lane, "a message on the wrong lane");
                return Ok(Some(m));
            }
            Decoded::Unknown(variant) => debug!(variant, "skipping a message from a newer peer"),
        }
    }
}

/// 消息走哪条通道（[`next_msg`] 检查用）
pub(crate) trait LaneOf {
    fn lane(&self) -> SessionLane;
}

impl LaneOf for HostMsg {
    fn lane(&self) -> SessionLane {
        HostMsg::lane(self)
    }
}

impl LaneOf for ControllerMsg {
    fn lane(&self) -> SessionLane {
        ControllerMsg::lane(self)
    }
}
