//! 一个会话的四条 QUIC 流（控制、画面、声音、大块，见 `airhand_proto` 的模块说明），端到端加密。
//!
//! 每条流有自己的流量控制和丢包重传，画面堵住不影响声音和控制消息，1 MiB 的剪贴板也不挡输入和确认
//! （整条连接的拥塞控制还是共用的，网络满了声音也会慢）。
//!
//! * 双方：[`open_session`] 打开四条流，之后在控制流上握手（[`crate::secure`]）；
//! * 双方：[`write_lane`] 把排好队的帧加密后写进一条流；被控端的 [`write_audio`] 把声音包加密后发成数据报
//!   （见 [`crate::host::HostStreams`]）；
//! * 控制端：[`HostReceiver`] 从四条流和数据报读被控端发来的东西，解密。
//!
//! 声音走 QUIC 数据报（`airhand_proto::datagram`）：不重传，也不排在丢了的包后面等重传，
//! 在 QUIC 包里排在所有流的前面。放不进一个数据报的包（很少见）才走声音流。
use crate::wire::{read_frame, read_packet};
use airhand_proto::framing::{self, Decoded};
use airhand_proto::handshake::TAG_LEN;
use airhand_proto::media::{self, AudioHeader, Kind};
use airhand_proto::session::HostMsg;
use airhand_proto::{MAX_CONTROL_MSG, Role, SessionHeader, SessionLane, datagram};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendDatagramError, SendStream};
use std::future::Future;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::debug;

use crate::secure::{DATAGRAM_OVERHEAD, DatagramOpener, DatagramSealer, FrameOpener, FrameSealer};

pub type Stream = (SendStream, RecvStream);

/// 加密一帧（`u32 长度 | 明文`）。编码时多留了标签的位置，通常不用重新分配（[`framing::encode_with_spare`]、
/// [`media::frame`]）；同一份数据发给几个会话时（例如 Android 的广播）这里拷贝一次，每个会话的密钥不一样
pub fn seal(frame: Bytes, sealer: &mut FrameSealer) -> Result<Bytes> {
    let mut v = Vec::from(frame);
    v.reserve(TAG_LEN);
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

/// 把 `rx` 里排好队的帧（明文，已经带长度前缀）加密后依次写进一条流，`rx` 发完后正常结束这条流。
/// 十秒钟写不进去一帧算对方失联
pub async fn write_lane(
    mut send: SendStream,
    mut rx: impl Frames,
    mut sealer: FrameSealer,
) -> Result<()> {
    while let Some(bytes) = rx.next().await {
        let sealed = seal(bytes, &mut sealer)?;
        tokio::time::timeout(Duration::from_secs(10), send.write_chunk(sealed))
            .await
            .context("session writer stalled")??;
    }
    send.finish()?;
    Ok(())
}

/// 被控端的声音：`rx` 里排好队的声音包（[`media::frame`] 编好的，带长度前缀）加密后发成数据报，
/// 放不进一个数据报的、或者连接不支持数据报的加密后写进声音流 `send`。`rx` 关闭后正常结束声音流
pub async fn write_audio(
    conn: Connection,
    session_id: u64,
    mut send: SendStream,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: FrameSealer,
    mut datagrams: DatagramSealer,
) -> Result<()> {
    while let Some(framed) = rx.recv().await {
        let packet = framed.get(4..).unwrap_or_default();
        let size = datagram::HEADER + DATAGRAM_OVERHEAD + packet.len();
        let fits = conn
            .max_datagram_size()
            .is_some_and(|max| size <= max.min(datagram::MAX));
        if fits {
            let d = datagrams.seal(session_id, packet)?;
            match conn.send_datagram(Bytes::from(d)) {
                Ok(()) => continue,
                Err(SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                // 对方不收数据报、太大了：走声音流
                Err(_) => {}
            }
        }
        let sealed = seal(framed, &mut sealer)?;
        tokio::time::timeout(Duration::from_secs(10), send.write_chunk(sealed))
            .await
            .context("session writer stalled")??;
    }
    send.finish()?;
    Ok(())
}

/// 连接上发给会话 `session_id` 的数据报（去掉会话 ID，还是密文）。连接上只有一个会话的控制端用
///（测试、调试工具）；[`crate::controller::ControllerService`] 自己按会话分发
pub fn session_datagrams(conn: &Connection, session_id: u64) -> mpsc::Receiver<Bytes> {
    let (tx, rx) = mpsc::channel(16);
    let conn = conn.clone();
    tokio::spawn(async move {
        while let Ok(d) = conn.read_datagram().await {
            if datagram::session(&d) == Some(session_id)
                && let Err(mpsc::error::TrySendError::Closed(_)) =
                    tx.try_send(d.slice(datagram::HEADER..))
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
    /// 大块通道（剪贴板、光标），双向
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
    /// 画面包（[`media`] 格式：头 + 码流），原样交给界面
    Video(Vec<u8>),
    /// 声音包（[`media`] 格式：头 + Opus 包），原样交给界面
    Audio(Vec<u8>),
}

/// 控制端读被控端的四条流和声音数据报的解密端
pub struct Openers {
    pub control: FrameOpener,
    pub bulk: FrameOpener,
    pub video: FrameOpener,
    pub audio: FrameOpener,
    pub datagrams: DatagramOpener,
}

/// 控制端读被控端的四条流和声音数据报。
///
/// 每条流一个读任务，读任务活到流结束：`select!` / 超时只会取消通道上的接收，不会切断半帧。
/// 几个有界的信箱互相独立，画面、剪贴板再多也挡不住 Pong 和 Bye。
pub struct HostReceiver {
    control: mpsc::Receiver<Result<FromHost>>,
    bulk: mpsc::Receiver<Result<FromHost>>,
    video: mpsc::Receiver<Result<FromHost>>,
    audio: mpsc::Receiver<Result<FromHost>>,
    bulk_open: bool,
    video_open: bool,
    audio_open: bool,
    audio_order: AudioOrder,
    _tasks: JoinSet<()>,
    // 画面流和声音流反方向的一半用不到，但要留到会话结束
    _reverse: (SendStream, SendStream),
}

impl HostReceiver {
    /// `control` / `bulk`：控制流、大块通道收的一半（发的一半由控制端的会话自己用）；
    /// `datagrams`：发给这个会话的数据报（去掉了会话 ID），里面是加密的声音包
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
        if let Some(d) = datagrams {
            datagram_reader(&mut tasks, d, openers.datagrams, audio_tx.clone());
        }
        media_reader(&mut tasks, audio.1, Kind::Audio, openers.audio, audio_tx);
        let (video_tx, video_rx) = mpsc::channel(2);
        media_reader(&mut tasks, video.1, Kind::Video, openers.video, video_tx);
        Self {
            control: message_reader(&mut tasks, control, openers.control),
            bulk: message_reader(&mut tasks, bulk, openers.bulk),
            video: video_rx,
            audio: audio_rx,
            bulk_open: true,
            video_open: true,
            audio_open: true,
            audio_order: AudioOrder::default(),
            _tasks: tasks,
            _reverse: (video.0, audio.0),
        }
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
                        if self.audio_order.accept(&p, Instant::now()) {
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

/// 数据报可能乱序：比已经交出去的包还早的声音包（迟到的）丢掉，不倒着播放。
/// 停顿后的第一包（`start`）、隔了一阵子才来的包照收：被控端重新开始发声音时序号会从头算
#[derive(Default)]
struct AudioOrder {
    last: Option<(u32, Instant)>,
}

impl AudioOrder {
    /// 这么久以内才算乱序
    const WINDOW: Duration = Duration::from_millis(500);

    fn accept(&mut self, packet: &[u8], now: Instant) -> bool {
        let Some(h) = AudioHeader::decode(packet) else {
            return true;
        };
        if let Some((last, at)) = self.last
            && !h.start
            && now.duration_since(at) < Self::WINDOW
            && h.seq.wrapping_sub(last) as i32 <= 0
        {
            return false;
        }
        self.last = Some((h.seq, now));
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

/// 读一条被控端的控制消息（解密、解码）；对方版本新增的消息跳过。流在消息边界处结束时为 `None`
async fn next_host_msg(
    recv: &mut RecvStream,
    opener: &mut FrameOpener,
    buf: &mut Vec<u8>,
) -> Result<Option<HostMsg>> {
    loop {
        if !read_frame(recv, MAX_CONTROL_MSG, buf).await? {
            return Ok(None);
        }
        opener.open(buf)?;
        match framing::decode(buf)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown(variant) => debug!(variant, "skipping a message from a newer host"),
        }
    }
}

/// 读一条消息流（控制流、大块通道）
fn message_reader(
    tasks: &mut JoinSet<()>,
    mut recv: RecvStream,
    mut opener: FrameOpener,
) -> mpsc::Receiver<Result<FromHost>> {
    let (tx, rx) = mpsc::channel(16);
    tasks.spawn(async move {
        let mut buf = Vec::new();
        loop {
            let r = next_host_msg(&mut recv, &mut opener, &mut buf)
                .await
                .map(|m| m.map(FromHost::Control));
            if !forward(&tx, r).await {
                break;
            }
        }
    });
    rx
}

/// 声音数据报放进声音的信箱。数据报本来就可能丢：解不开的（被篡改、重放的）、信箱满了（界面跟不上）都丢掉这一包，不等
fn datagram_reader(
    tasks: &mut JoinSet<()>,
    mut rx: mpsc::Receiver<Bytes>,
    mut opener: DatagramOpener,
    tx: mpsc::Sender<Result<FromHost>>,
) {
    tasks.spawn(async move {
        while let Some(d) = rx.recv().await {
            let Some(p) = opener.open(&d) else {
                debug!("dropped an audio datagram that failed authentication");
                continue;
            };
            if Kind::of(&p) != Some(Kind::Audio) || p.len() > media::MAX_AUDIO_PACKET {
                continue;
            }
            if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(Ok(FromHost::Audio(p))) {
                break;
            }
        }
    });
}

fn media_reader(
    tasks: &mut JoinSet<()>,
    mut recv: RecvStream,
    kind: Kind,
    mut opener: FrameOpener,
    tx: mpsc::Sender<Result<FromHost>>,
) {
    let max = TAG_LEN
        + match kind {
            Kind::Audio => media::MAX_AUDIO_PACKET,
            _ => media::MAX_VIDEO_PACKET,
        };
    tasks.spawn(async move {
        loop {
            let r = match read_packet(&mut recv, max).await {
                Ok(Some(mut p)) => match opener.open(&mut p) {
                    Ok(()) if Kind::of(&p) == Some(kind) => Ok(Some(match kind {
                        Kind::Audio => FromHost::Audio(p),
                        _ => FromHost::Video(p),
                    })),
                    Ok(()) => Err(anyhow!("unexpected packet on the {kind:?} stream")),
                    Err(e) => Err(e),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(seq: u32, start: bool) -> Vec<u8> {
        let h = AudioHeader {
            seq,
            start,
            capture_us: 0,
        };
        h.encode().to_vec()
    }

    #[test]
    fn late_audio_datagrams_are_dropped() {
        let mut o = AudioOrder::default();
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        assert!(o.accept(&audio(1, true), ms(0)));
        assert!(o.accept(&audio(3, false), ms(20)));
        // 迟到的 2、重复的 3
        assert!(!o.accept(&audio(2, false), ms(25)));
        assert!(!o.accept(&audio(3, false), ms(26)));
        assert!(o.accept(&audio(4, false), ms(40)));
        // 序号从头算：停顿后的第一包
        assert!(o.accept(&audio(1, true), ms(60)));
        assert!(o.accept(&audio(2, false), ms(80)));
        // 停顿后的第一包丢了：隔了一阵子之后照收
        assert!(o.accept(&audio(7, false), ms(1000)));
        assert!(o.accept(&audio(2, false), ms(2000)));
        // 序号绕回
        let mut o = AudioOrder::default();
        assert!(o.accept(&audio(u32::MAX, false), ms(0)));
        assert!(o.accept(&audio(0, false), ms(20)));
        assert!(!o.accept(&audio(u32::MAX, false), ms(30)));
    }
}
