//! 会话的写任务和控制端的接收端（两端共用的部分在 [`crate::lanes`]、[`crate::merge`]）。
//!
//! * 双方：[`write_lane`] 把排好队的帧加密后写进本端的一条通道（控制通道在直连通着时也从直连发一份）；
//!   [`DatagramSender`] 发控制数据报（Ping、Pong、画面确认）；
//! * 被控端：[`write_video`] 把画面写进现在走的那条路，[`write_audio`] 把声音包加密后发成数据报（同样走现在的那条路）；
//! * 控制端：[`HostReceiver`] 收被控端的控制、大块、画面通道和声音、控制数据报，解密之后交出来。
//!
//! 每条通道一条 QUIC 流，有自己的流量控制和丢包重传：画面堵住不影响控制消息，1 MiB 的剪贴板也不挡输入
//! （整条连接的拥塞控制还是共用的）。声音和控制数据报走 QUIC 数据报：不重传，也不排在丢了的包、流里别的消息后面等，
//! 在 QUIC 包里排在所有流的前面。
use crate::direct::{Mirror, Route};
use crate::lanes::open_lane;
use crate::merge::{Frame, Merge};
use airhand_proto::framing::{self, Decoded};
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::media::{self, AudioHeader, Kind};
use airhand_proto::minicbor::{Decode, Encode};
use airhand_proto::session::{HostDatagram, HostMsg};
use airhand_proto::{SessionLane, datagram};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, SendDatagramError, SendStream};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::secure::{Opener, Sealer};

/// 加密一帧（`u32 长度 | 明文`）。编码时多留了加密开销的位置，通常不用重新分配（[`framing::encode_with_spare`]、
/// [`media::frame`]）；同一份数据发给几个会话时（例如 Android 的广播）这里拷贝一次，每个会话的密钥不一样
pub fn seal(frame: Bytes, sealer: &mut Sealer) -> Result<Bytes> {
    let mut v = Vec::from(frame);
    v.reserve(SEAL_OVERHEAD);
    sealer.seal_frame(&mut v)?;
    Ok(Bytes::from(v))
}

/// 写任务从哪里取下一帧（明文，已经带长度前缀）：消息通道是 [`crate::queue`] 的队列，大块通道也可以是普通的 `mpsc`
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

async fn write_sealed(send: &mut SendStream, sealed: Bytes) -> Result<()> {
    tokio::time::timeout(WRITE_TIMEOUT, send.write_chunk(sealed))
        .await
        .context("session writer stalled")??;
    Ok(())
}

/// 把 `rx` 里排好队的帧（明文，已经带长度前缀）加密后依次写进本端的一条通道，`rx` 发完后正常结束这条流。
/// `mirror`：控制通道在直连通着时每一帧也从直连发一份（中转上照样是完整的一份，见 [`crate::direct`]）
pub async fn write_lane(
    mut send: SendStream,
    mut rx: impl Frames,
    mut sealer: Sealer,
    mirror: Option<Mirror>,
) -> Result<()> {
    while let Some(bytes) = rx.next().await {
        let sealed = seal(bytes, &mut sealer)?;
        if let Some(m) = &mirror {
            m.offer(&sealed);
        }
        write_sealed(&mut send, sealed).await?;
    }
    send.finish()?;
    Ok(())
}

/// 发控制数据报（`airhand_proto::datagram::CONTROL`：Ping、Pong、画面确认）。中转上发一份，直连通着时直连上也发
/// **同一份**（同一个计数，对方的重放窗口挡掉后到的那份，见 [`Mirror`]）：总是走更快的那条路。丢了就丢了
#[derive(Clone)]
pub struct DatagramSender(Arc<Datagrams>);

struct Datagrams {
    session_id: u64,
    relay: Connection,
    mirror: Mirror,
    sealer: Mutex<Sealer>,
}

impl DatagramSender {
    /// `relay`：中转连接；`mirror`：这个会话在直连上的那一份；`sealer`：`Secured::datagrams` 的发送端
    pub fn new(session_id: u64, relay: Connection, mirror: Mirror, sealer: Sealer) -> Self {
        Self(Arc::new(Datagrams {
            session_id,
            relay,
            mirror,
            sealer: Mutex::new(sealer),
        }))
    }

    pub fn send<T: Encode<()>>(&self, msg: &T) {
        let d = &self.0;
        let sealed = d.sealer.lock().unwrap().seal_datagram(
            d.session_id,
            datagram::CONTROL,
            &framing::to_vec(msg),
        );
        let sealed = match sealed {
            Ok(s) => Bytes::from(s),
            Err(e) => return debug!("can't seal a control datagram: {e:#}"),
        };
        d.mirror.datagram(&sealed);
        if let Err(e) = d.relay.send_datagram(sealed) {
            debug!("dropped a control datagram: {e}");
        }
    }
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

/// 直连上一帧画面这么久写不进去，就当它断了（这一帧改从中转发）
const DIRECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// 被控端的画面：`rx` 里排好队的画面包加密后写进现在走的那条路（`route`，由直连任务按两条路的时延决定）。
/// 换路只在两帧之间生效，写到一半的帧照样写完：直连上已经写进去的帧照样送到，接收端按计数排好（见 [`crate::merge`]）。
/// 往直连上写不进去（断了、堵死了）就放弃直连上的这条流（流里还没送到的帧随之丢了），这一帧改写进中转，并在
/// `broken` 里报告是哪条直连：直连任务当它断了（要关键帧、码率控制不再等那些帧）。中转上写不进去是错误：会话结束
pub async fn write_video(
    mut relay: SendStream,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    session_id: u64,
    mut route: watch::Receiver<Route>,
    broken: watch::Sender<Option<Connection>>,
) -> Result<()> {
    // 直连上画面的流：哪条连接上的（`stable_id`）
    let mut direct: Option<(usize, SendStream)> = None;
    while let Some(frame) = rx.recv().await {
        let sealed = seal(frame, &mut sealer)?;
        let media = route.borrow_and_update().media.clone();
        if let Some(conn) = media {
            match write_direct(&conn, &mut direct, session_id, sealed.clone()).await {
                Ok(()) => continue,
                Err(e) => {
                    debug!("writing video to the direct path failed: {e:#}");
                    if let Some((_, mut s)) = direct.take() {
                        let _ = s.reset(0u32.into());
                    }
                    broken.send_replace(Some(conn));
                }
            }
        }
        write_sealed(&mut relay, sealed).await?;
    }
    relay.finish()?;
    if let Some((_, mut s)) = direct {
        let _ = s.finish();
    }
    Ok(())
}

/// 往直连上写一帧画面（这条连接上还没有画面的流就开一条）
async fn write_direct(
    conn: &Connection,
    direct: &mut Option<(usize, SendStream)>,
    session_id: u64,
    sealed: Bytes,
) -> Result<()> {
    if direct
        .as_ref()
        .is_none_or(|(id, _)| *id != conn.stable_id())
    {
        let s = open_lane(conn, session_id, SessionLane::Video).await?;
        *direct = Some((conn.stable_id(), s));
    }
    let (_, s) = direct.as_mut().expect("opened above");
    tokio::time::timeout(DIRECT_WRITE_TIMEOUT, s.write_chunk(sealed))
        .await
        .context("the direct path stalled")??;
    Ok(())
}

/// 被控端的声音：`rx` 里排好队的声音包（[`media::frame`] 编好的，带长度前缀）加密后发成数据报，
/// 发在画面走的那条路上（`route`）。放不进一个数据报的（[`media::MAX_AUDIO_PACKET`]）丢掉
pub async fn write_audio(
    relay: Connection,
    session_id: u64,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    route: watch::Receiver<Route>,
) -> Result<()> {
    while let Some(framed) = rx.recv().await {
        let packet = framed.get(framing::PREFIX..).unwrap_or_default();
        if packet.len() > media::MAX_AUDIO_PACKET {
            debug!(
                len = packet.len(),
                "dropped an audio packet too large for a datagram"
            );
            continue;
        }
        let direct = route.borrow().media.clone();
        let conn = direct.as_ref().unwrap_or(&relay);
        let d = sealer.seal_datagram(session_id, datagram::AUDIO, packet)?;
        match conn.send_datagram(Bytes::from(d)) {
            Ok(()) => {}
            Err(SendDatagramError::ConnectionLost(e)) if direct.is_none() => return Err(e.into()),
            // 直连断了（直连任务会把路改回中转）、发送缓冲满了：这一包丢了就丢了
            Err(e) => debug!("dropped an audio packet: {e}"),
        }
    }
    Ok(())
}

/// 被控端发来的一样东西（已经解密）
#[derive(Debug)]
pub enum FromHost {
    /// 控制通道、大块通道上的消息
    Control(HostMsg),
    /// 控制数据报（Pong）
    Datagram(HostDatagram),
    /// 画面包（[`media`] 格式：头 + 码流），原样交给界面。`arrived`：开始从网络上到达的时刻
    /// （控制端的时钟，微秒，见 [`crate::wire::read_packet_timed`]），确认时带给被控端（见 `ControllerDatagram::Ack`）。
    /// `direct`：从直连来的；`gap`：前面有帧没等到（断在了直连上），要一个关键帧（见 [`crate::merge`]）
    Video {
        packet: Vec<u8>,
        arrived: u64,
        direct: bool,
        gap: bool,
    },
    /// 声音包（[`media`] 格式：头 + Opus 包），原样交给界面
    Audio(Vec<u8>),
}

/// 一种数据报（声音、控制）的信箱和它的解密端
pub type DatagramInbox = (mpsc::Receiver<Bytes>, Opener);

/// 控制端收被控端的三条通道、声音数据报和控制数据报。
///
/// 每条通道的流由 [`Merge`] 的读任务读（读任务活到流结束，取消 [`HostReceiver::recv`] 不会切断半帧），
/// 这里只从几个互相独立的信箱里取：画面、剪贴板再多也挡不住控制消息和 Pong
pub struct HostReceiver {
    control: Merge,
    bulk: Merge,
    video: Merge,
    audio: DatagramInbox,
    datagrams: DatagramInbox,
    bulk_open: bool,
    video_open: bool,
    audio_open: bool,
    datagrams_open: bool,
    audio_order: AudioOrder,
}

impl HostReceiver {
    /// `control` / `bulk` / `video`：三条通道的接收端；`audio` / `datagrams`：发给这个会话的声音、控制数据报
    /// （整个数据报，还是密文）和它们的解密端
    pub fn new(
        control: Merge,
        bulk: Merge,
        video: Merge,
        audio: DatagramInbox,
        datagrams: DatagramInbox,
    ) -> Self {
        Self {
            control,
            bulk,
            video,
            audio,
            datagrams,
            bulk_open: true,
            video_open: true,
            audio_open: true,
            datagrams_open: true,
            audio_order: AudioOrder::default(),
        }
    }

    /// 下一样东西。控制消息优先；控制通道结束了就是会话结束了（其它通道先结束不算）。可以放在 `select!` 里
    pub async fn recv(&mut self) -> Result<FromHost> {
        loop {
            tokio::select! {
                biased;
                m = next_msg::<HostMsg>(&mut self.control, SessionLane::Control) => {
                    return Ok(FromHost::Control(m?.context("control stream ended")?));
                }
                d = self.datagrams.0.recv(), if self.datagrams_open => match d {
                    Some(d) => {
                        if let Some(m) = open_datagram(&mut self.datagrams.1, &d) {
                            return Ok(FromHost::Datagram(m));
                        }
                    }
                    None => self.datagrams_open = false,
                },
                m = next_msg::<HostMsg>(&mut self.bulk, SessionLane::Bulk), if self.bulk_open => match m? {
                    Some(m) => return Ok(FromHost::Control(m)),
                    None => self.bulk_open = false,
                },
                d = self.audio.0.recv(), if self.audio_open => match d {
                    Some(d) => {
                        if let Some(p) = self.open_audio(&d) {
                            return Ok(FromHost::Audio(p));
                        }
                    }
                    None => self.audio_open = false,
                },
                f = self.video.next(), if self.video_open => match f? {
                    Some(f) => {
                        return Ok(FromHost::Video {
                            packet: f.data,
                            arrived: f.arrived,
                            direct: f.direct,
                            gap: f.gap,
                        });
                    }
                    None => self.video_open = false,
                },
            }
        }
    }

    /// 声音数据报：解开、检查是声音包、不是迟到的。解不开的（被篡改、重放的）、迟到的都丢掉这一包
    fn open_audio(&mut self, d: &[u8]) -> Option<Vec<u8>> {
        let mut p = d.get(datagram::HEADER..)?.to_vec();
        if let Err(e) = self.audio.1.open(&mut p) {
            debug!("dropped an audio packet: {e:#}");
            return None;
        }
        (Kind::of(&p) == Some(Kind::Audio) && self.audio_order.accept(&p)).then_some(p)
    }
}

/// 数据报可能乱序：采集时刻在一个会话内不倒退，即使采集器重启、序号从头算也一样。
/// `start` 只描述静音段边界，不能让迟到的首包重置顺序。相同采集时刻用序号消歧（允许绕回）。
#[derive(Default)]
struct AudioOrder {
    last: Option<AudioHeader>,
}

impl AudioOrder {
    fn accept(&mut self, packet: &[u8]) -> bool {
        let Some(h) = AudioHeader::decode(packet) else {
            return false;
        };
        if let Some(last) = self.last
            && (h.capture_us < last.capture_us
                || (h.capture_us == last.capture_us && h.seq.wrapping_sub(last.seq) as i32 <= 0))
        {
            return false;
        }
        self.last = Some(h);
        true
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

impl LaneOf for airhand_proto::session::ControllerMsg {
    fn lane(&self) -> SessionLane {
        airhand_proto::session::ControllerMsg::lane(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(seq: u32, start: bool, capture_us: u64) -> Vec<u8> {
        AudioHeader {
            seq,
            start,
            capture_us,
        }
        .encode()
        .to_vec()
    }

    #[test]
    fn late_audio_cannot_rewind_playback_even_when_it_starts_a_segment() {
        let mut o = AudioOrder::default();
        // 首包丢了或迟到：先收到了第三包。
        assert!(o.accept(&audio(3, false, 60_000)));
        assert!(!o.accept(&audio(1, true, 20_000)));
        assert!(!o.accept(&audio(2, false, 40_000)));
        assert!(!o.accept(&audio(3, false, 60_000)));
        assert!(o.accept(&audio(4, false, 80_000)));
        // 采集器重启，首包也可能丢失：用采集时钟分辨新流与迟到的旧流。
        assert!(o.accept(&audio(2, false, 1_000_000)));
        assert!(!o.accept(&audio(1, true, 980_000)));
        assert!(!o.accept(&audio(5, false, 100_000)));
        assert!(o.accept(&audio(3, false, 1_020_000)));
        // 相同采集时刻（时钟精度不够）仍能按序号丢重复、识别绕回。
        let mut o = AudioOrder::default();
        assert!(o.accept(&audio(u32::MAX, true, 0)));
        assert!(o.accept(&audio(0, false, 0)));
        assert!(!o.accept(&audio(u32::MAX, true, 0)));
        assert!(!o.accept(&audio(0, true, 0)));
        assert!(!o.accept(&[]));
    }
}
