//! 一个会话的四条 QUIC 流（控制、画面、声音、大块，见 `airhand_proto` 的模块说明），端到端加密。
//!
//! 每条流有自己的流量控制和丢包重传，画面堵住不影响声音和控制消息，1 MiB 的剪贴板也不挡输入和确认
//! （整条连接的拥塞控制还是共用的，网络满了声音也会慢）。
//!
//! * 双方：[`open_session`] 打开四条流，之后在控制流上握手（[`crate::secure`]）；
//! * 双方：[`write_lane`] 把排好队的帧加密后写进一条流（直连通着的话也从直连发一份）；
//!   被控端的 [`write_video`] 把画面写进现在走的那条路，[`write_audio`] 把声音包加密后发成数据报
//!   （见 [`crate::host::HostStreams`]）；
//! * 控制端：[`HostReceiver`] 从四条流和数据报读被控端发来的东西，解密。直连上的流通了之后加进来，
//!   和中转上的按计数合并（[`crate::merge`]）。
//!
//! 声音走 QUIC 数据报（`airhand_proto::datagram`）：不重传，也不排在丢了的包后面等重传，
//! 在 QUIC 包里排在所有流的前面。放不进一个数据报的包（很少见）才走声音流，和数据报用同一把密钥、同一串计数。
use crate::direct::{Attach, Mirror, Route};
use crate::merge::{Merge, Policy};
use crate::wire::read_packet;
use airhand_proto::framing::{self, Decoded};
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::media::{self, AudioHeader, Kind};
use airhand_proto::session::HostMsg;
use airhand_proto::{Role, SessionHeader, SessionLane, datagram};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;
use tracing::debug;

use crate::secure::{Opener, Sealer};

pub type Stream = (SendStream, RecvStream);

/// 加密一帧（`u32 长度 | 明文`）。编码时多留了加密开销的位置，通常不用重新分配（[`framing::encode_with_spare`]、
/// [`media::frame`]）；同一份数据发给几个会话时（例如 Android 的广播）这里拷贝一次，每个会话的密钥不一样
pub fn seal(frame: Bytes, sealer: &mut Sealer) -> Result<Bytes> {
    let mut v = Vec::from(frame);
    v.reserve(SEAL_OVERHEAD);
    sealer.seal_frame(&mut v)?;
    Ok(Bytes::from(v))
}

/// 写任务从哪里取下一帧（明文，已经带长度前缀）：控制端的大块通道是普通的 `mpsc` 队列，
/// 被控端的两条消息通道是 [`crate::host::queue`]（有只留最新值的槽位）
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

/// 把 `rx` 里排好队的帧（明文，已经带长度前缀）加密后依次写进一条流，`rx` 发完后正常结束这条流。
/// 控制、大块通道：直连通着的话每一帧也交给 `mirror` 从直连发一份（中转上照样是完整的一份，见 [`crate::direct`]）
pub async fn write_lane(
    mut send: SendStream,
    mut rx: impl Frames,
    mut sealer: Sealer,
    mirror: Mirror,
) -> Result<()> {
    while let Some(bytes) = rx.next().await {
        let sealed = seal(bytes, &mut sealer)?;
        mirror.offer(&sealed);
        write_sealed(&mut send, sealed).await?;
    }
    send.finish()?;
    Ok(())
}

/// 直连上一帧画面这么久写不进去，就当它断了（这一帧改从中转发）
const DIRECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// 被控端的画面：`rx` 里排好队的画面包加密后写进现在走的那条路（`route`，由直连任务按两条路的时延决定）。
/// 往直连上写不进去（断了、堵死了）就放弃直连上的这条流，这一帧改写进中转，并通知 `broken`；
/// 接收端按计数排好（见 [`crate::merge`]）。中转上写不进去是错误：会话结束
pub async fn write_video(
    mut relay: SendStream,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    mut route: watch::Receiver<Route>,
    broken: Arc<Notify>,
) -> Result<()> {
    // 直连上画面的流：哪条连接上的（`stable_id`）
    let mut direct: Option<(usize, SendStream)> = None;
    while let Some(frame) = rx.recv().await {
        let sealed = seal(frame, &mut sealer)?;
        let media = route.borrow_and_update().media.clone();
        if let Some(conn) = media {
            match write_direct(&conn, &mut direct, sealed.clone(), &mut route).await {
                Ok(()) => continue,
                Err(e) => {
                    debug!("writing video to the direct path failed: {e:#}");
                    if let Some((_, mut s)) = direct.take() {
                        let _ = s.reset(0u32.into());
                    }
                    broken.notify_one();
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

/// 往直连上写一帧画面（这条连接上还没有画面的流就开一条）。写的时候换了路也算失败：这一帧改从中转发
async fn write_direct(
    conn: &Connection,
    direct: &mut Option<(usize, SendStream)>,
    sealed: Bytes,
    route: &mut watch::Receiver<Route>,
) -> Result<()> {
    if direct
        .as_ref()
        .is_none_or(|(id, _)| *id != conn.stable_id())
    {
        let s = crate::direct::open_lane(conn, SessionLane::Video).await?;
        *direct = Some((conn.stable_id(), s));
    }
    let (_, s) = direct.as_mut().expect("opened above");
    tokio::select! {
        r = tokio::time::timeout(DIRECT_WRITE_TIMEOUT, s.write_chunk(sealed)) => {
            r.context("the direct path stalled")??;
            Ok(())
        }
        _ = route.changed() => anyhow::bail!("switched paths while writing"),
    }
}

/// 被控端的声音：`rx` 里排好队的声音包（[`media::frame`] 编好的，带长度前缀）加密后发成数据报，
/// 发在画面走的那条路上（`route`）。放不进一个数据报的、或者连接不支持数据报的加密后写进中转上的声音流 `send`。
/// `rx` 关闭后正常结束声音流
pub async fn write_audio(
    relay: Connection,
    session_id: u64,
    mut send: SendStream,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    route: watch::Receiver<Route>,
) -> Result<()> {
    while let Some(framed) = rx.recv().await {
        let packet = framed.get(framing::PREFIX..).unwrap_or_default();
        let size = datagram::HEADER + packet.len() + SEAL_OVERHEAD;
        let direct = route.borrow().media.clone();
        let conn = direct.as_ref().unwrap_or(&relay);
        let fits = conn
            .max_datagram_size()
            .is_some_and(|max| size <= max.min(datagram::MAX));
        if fits {
            let d = sealer.seal_datagram(session_id, datagram::AUDIO, packet)?;
            match conn.send_datagram(Bytes::from(d)) {
                Ok(()) => continue,
                // 直连断了：这一包丢了就丢了（直连任务会把路改回中转）
                Err(SendDatagramError::ConnectionLost(_)) if direct.is_some() => continue,
                Err(SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                // 对方不收数据报、太大了：走声音流（这个计数没用上，接收端本来就容许跳过）
                Err(_) => {}
            }
        }
        write_sealed(&mut send, seal(framed, &mut sealer)?).await?;
    }
    send.finish()?;
    Ok(())
}

/// 连接上发给会话 `session_id` 的声音数据报（整个数据报，还是密文）。连接上只有一个会话的控制端用
///（测试、调试工具）；[`crate::controller::ControllerService`] 自己按会话分发
pub fn session_datagrams(conn: &Connection, session_id: u64) -> mpsc::Receiver<Bytes> {
    let (tx, rx) = mpsc::channel(16);
    let conn = conn.clone();
    tokio::spawn(async move {
        while let Ok(d) = conn.read_datagram().await {
            if datagram::session(&d) == Some(session_id)
                && datagram::kind(&d) == Some(datagram::AUDIO)
                && let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(d)
            {
                break;
            }
        }
    });
    rx
}

pub struct SessionStreams {
    pub control: Stream,
    pub video: Stream,
    pub audio: Stream,
    /// 大块通道（剪贴板、光标图像），双向
    pub bulk: Stream,
}

pub async fn open_session(
    conn: &Connection,
    session_id: u64,
    role: Role,
) -> Result<SessionStreams> {
    async fn lane(
        conn: &Connection,
        session_id: u64,
        role: Role,
        lane: SessionLane,
    ) -> Result<Stream> {
        let (mut send, recv) = conn.open_bi().await?;
        send.set_priority(lane.priority() as i32)?;
        send.write_all(&SessionHeader::new(session_id, role, lane).encode())
            .await?;
        Ok((send, recv))
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let control = lane(conn, session_id, role, SessionLane::Control).await?;
        let video = lane(conn, session_id, role, SessionLane::Video).await?;
        let audio = lane(conn, session_id, role, SessionLane::Audio).await?;
        let bulk = lane(conn, session_id, role, SessionLane::Bulk).await?;
        Ok(SessionStreams {
            control,
            video,
            audio,
            bulk,
        })
    })
    .await
    .context("timed out opening session streams")?
}

/// 被控端发来的一样东西（已经解密）
#[derive(Debug)]
pub enum FromHost {
    /// 控制流、大块通道上的消息
    Control(HostMsg),
    /// 画面包（[`media`] 格式：头 + 码流），原样交给界面。`arrived`：开始从网络上到达的时刻
    /// （控制端的时钟，微秒，见 [`crate::wire::read_packet_timed`]），确认时带给被控端（见 `ControllerMsg::Ack`）。
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

/// 控制端读被控端的四条流和声音数据报的解密端
pub struct Openers {
    pub control: Opener,
    pub bulk: Opener,
    pub video: Opener,
    /// 声音流和声音数据报共用
    pub audio: Opener,
}

/// 控制端读被控端的四条流和声音数据报。
///
/// 每条流一个读任务，读任务活到流结束：`select!` / 超时只会取消通道上的接收，不会切断半帧。
/// 几个有界的信箱互相独立，画面、剪贴板再多也挡不住 Pong 和 Bye。
/// 直连通了之后，被控端在直连上开的控制、大块、画面通道经 [`HostReceiver::attach`] 加进来。
pub struct HostReceiver {
    control: mpsc::Receiver<Result<FromHost>>,
    bulk: mpsc::Receiver<Result<FromHost>>,
    video: mpsc::Receiver<Result<FromHost>>,
    audio: mpsc::Receiver<Result<FromHost>>,
    bulk_open: bool,
    video_open: bool,
    audio_open: bool,
    audio_order: AudioOrder,
    attach: Attach,
    _tasks: JoinSet<()>,
    // 画面流和声音流反方向的一半用不到，但要留到会话结束
    _reverse: (SendStream, SendStream),
}

impl HostReceiver {
    /// `control` / `bulk`：控制流、大块通道收的一半（发的一半由控制端的会话自己用）；
    /// `datagrams`：发给这个会话的声音数据报（整个数据报，还是密文）
    pub fn new(
        control: RecvStream,
        bulk: RecvStream,
        video: Stream,
        audio: Stream,
        datagrams: Option<mpsc::Receiver<Bytes>>,
        openers: Openers,
    ) -> Self {
        let mut tasks = JoinSet::new();
        let (audio_tx, audio_rx) = mpsc::channel(16);
        let audio_opener = Arc::new(Mutex::new(openers.audio));
        if let Some(d) = datagrams {
            datagram_reader(&mut tasks, d, audio_opener.clone(), audio_tx.clone());
        }
        audio_reader(&mut tasks, audio.1, audio_opener, audio_tx);
        let (video_tx, video_rx) = mpsc::channel(2);
        let (video_send, video_recv) = video;
        let (video_merge, attach_video) =
            Merge::new(SessionLane::Video, Policy::Video, openers.video, video_recv);
        video_reader(&mut tasks, video_merge, video_tx);
        let (control, attach_control) = Merge::new(
            SessionLane::Control,
            Policy::Complete,
            openers.control,
            control,
        );
        let (bulk, attach_bulk) =
            Merge::new(SessionLane::Bulk, Policy::Complete, openers.bulk, bulk);
        Self {
            control: message_reader(&mut tasks, control, SessionLane::Control),
            bulk: message_reader(&mut tasks, bulk, SessionLane::Bulk),
            video: video_rx,
            audio: audio_rx,
            bulk_open: true,
            video_open: true,
            audio_open: true,
            audio_order: AudioOrder::default(),
            attach: Attach {
                control: Some(attach_control),
                bulk: Some(attach_bulk),
                video: Some(attach_video),
            },
            _tasks: tasks,
            _reverse: (video_send, audio.0),
        }
    }

    /// 直连上被控端开的通道交给谁（见 [`crate::direct`]）
    pub(crate) fn attach(&self) -> Attach {
        self.attach.clone()
    }

    /// 下一样东西。控制消息优先；控制流结束了就是会话结束了（其它流先结束不算）
    pub async fn recv(&mut self) -> Result<FromHost> {
        loop {
            tokio::select! {
                biased;
                m = self.control.recv() => return m.context("control stream ended")?,
                m = self.bulk.recv(), if self.bulk_open => match m {
                    Some(m) => return m,
                    None => self.bulk_open = false,
                },
                m = self.audio.recv(), if self.audio_open => match m {
                    Some(Ok(FromHost::Audio(p))) => {
                        if self.audio_order.accept(&p) {
                            return Ok(FromHost::Audio(p));
                        }
                    }
                    Some(m) => return m,
                    None => self.audio_open = false,
                },
                m = self.video.recv(), if self.video_open => match m {
                    Some(m) => return m,
                    None => self.video_open = false,
                },
            }
        }
    }
}

/// 数据报和音频流可能乱序：采集时刻在一个会话内不倒退，即使采集器重启、序号从头算也一样。
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

/// 把读到的东西放进信箱。返回 false 表示这条流读完了（结束、出错，或者没人要了）
async fn forward(tx: &mpsc::Sender<Result<FromHost>>, r: Result<Option<FromHost>>) -> bool {
    let r = match r {
        Ok(Some(m)) => Ok(m),
        Ok(None) => return false,
        Err(e) => Err(e),
    };
    let more = r.is_ok();
    tx.send(r).await.is_ok() && more
}

/// 一条消息通道（控制、大块）上的消息：解码、检查通道；对方版本新增的消息跳过。通道结束时为 `None`。
/// 被控端和控制端共用（`M` 是对方发来的消息）
pub(crate) async fn next_msg<M>(merge: &mut Merge, lane: SessionLane) -> Result<Option<M>>
where
    M: for<'b> airhand_proto::minicbor::Decode<'b, ()> + LaneOf,
{
    loop {
        let Some(frame) = merge.next().await? else {
            return Ok(None);
        };
        match framing::decode::<M>(&frame.data)? {
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

/// 读一条消息通道（控制、大块）
fn message_reader(
    tasks: &mut JoinSet<()>,
    mut merge: Merge,
    lane: SessionLane,
) -> mpsc::Receiver<Result<FromHost>> {
    let capacity = (4 * 1024 * 1024 / lane.max_frame_size()).min(16);
    let (tx, rx) = mpsc::channel(capacity);
    tasks.spawn(async move {
        loop {
            let r = next_msg::<HostMsg>(&mut merge, lane)
                .await
                .map(|m| m.map(FromHost::Control));
            if !forward(&tx, r).await {
                break;
            }
        }
    });
    rx
}

/// 解开一个声音包（数据报或者声音流上的一帧）。数据报本来就可能丢：解不开的（被篡改、重放的）丢掉这一包
fn open_audio(opener: &Mutex<Opener>, mut p: Vec<u8>) -> Option<Vec<u8>> {
    if let Err(e) = opener.lock().unwrap().open(&mut p) {
        debug!("dropped an audio packet: {e:#}");
        return None;
    }
    (Kind::of(&p) == Some(Kind::Audio) && p.len() <= media::MAX_AUDIO_PACKET).then_some(p)
}

/// 声音数据报放进声音的信箱。信箱满了（界面跟不上）也丢掉这一包，不等
fn datagram_reader(
    tasks: &mut JoinSet<()>,
    mut rx: mpsc::Receiver<Bytes>,
    opener: Arc<Mutex<Opener>>,
    tx: mpsc::Sender<Result<FromHost>>,
) {
    tasks.spawn(async move {
        while let Some(d) = rx.recv().await {
            if datagram::kind(&d) != Some(datagram::AUDIO) {
                continue;
            }
            let Some(p) = open_audio(&opener, d[datagram::HEADER..].to_vec()) else {
                continue;
            };
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(Ok(FromHost::Audio(p))) {
                break;
            }
        }
    });
}

/// 声音流（放不进数据报的声音包）
fn audio_reader(
    tasks: &mut JoinSet<()>,
    mut recv: RecvStream,
    opener: Arc<Mutex<Opener>>,
    tx: mpsc::Sender<Result<FromHost>>,
) {
    let max = SessionLane::Audio.max_frame_size();
    tasks.spawn(async move {
        loop {
            let r = match read_packet(&mut recv, max).await {
                Ok(Some(p)) => match open_audio(&opener, p) {
                    Some(p) => Ok(Some(FromHost::Audio(p))),
                    None => continue,
                },
                Ok(None) => Ok(None),
                Err(e) => Err(e.into()),
            };
            if !forward(&tx, r).await {
                break;
            }
        }
    });
}

/// 画面：解不开、不是画面包都是错误（流是可靠的）。中转和直连上的帧按计数排好（见 [`crate::merge`]）
fn video_reader(tasks: &mut JoinSet<()>, mut merge: Merge, tx: mpsc::Sender<Result<FromHost>>) {
    tasks.spawn(async move {
        loop {
            let r = merge.next().await.map(|f| {
                f.map(|f| FromHost::Video {
                    packet: f.data,
                    arrived: f.arrived,
                    direct: f.direct,
                    gap: f.gap,
                })
            });
            if !forward(&tx, r).await {
                break;
            }
        }
    });
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
