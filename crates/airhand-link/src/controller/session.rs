//! 控制端的单个会话：把画面、声音原样转交给界面（界面自己解码），转发输入与设置，
//! 把界面打好的麦克风声音包发成声音数据报，定时告诉被控端画面收得怎么样（[`Feedback::report`]）。
//!
//! 画面可能有好几路（多屏同时显示，见 `airhand_proto::session` 的"多屏"）：确认、等不到帧、报告、要关键帧都按路分开，
//! 画面包头里写着是第几路，界面据此交给那一路的解码器。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::receiver::{FromHost, HostReceiver};
use crate::audio::Bundler;
use crate::merge::Video;
use crate::paths::{DatagramSender, Paths};
use crate::secure::Sealer;
use crate::transport::write_lane;
use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{
    ByeReason, Codec, ControllerDatagram, ControllerMsg, DirectOffer, DisplayInfo, HostMsg,
    MicState, Notice, Platform, REPORT_EVERY, SystemAction, VideoReport,
};
use airhand_proto::{MAX_STREAMS, datagram};
use anyhow::Result;
use bytes::Bytes;
use quinn::SendStream;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{CLIPBOARD_POLL, Clip, ClipboardSync, send_clipboard};
use crate::Problem;
use crate::clock::now_us;
use crate::files::{Files, LocalFs, RemoteOffer};
use crate::tasks::OwnedTask;
use airhand_proto::files::{FileEntry, FileError};

/// 这么久没有收到被控端的任何消息（它每秒回 Pong），或者控制流这么久写不进去，才算失联。
/// 网络卡一下不断开：卡住期间排着的输入由命令队列丢掉（见 [`super::queue`]）
const HOST_TIMEOUT: Duration = Duration::from_secs(20);
/// 没有新的画面确认时，最近的一个隔这么久重发一次、一共重发这么多次（见 [`Feedback`]）
const ACK_REPEAT: Duration = Duration::from_millis(30);
const ACK_REPEATS: u8 = 3;

/// 发给被控端的控制数据报：每秒的 Ping、画面确认（`ControllerDatagram`）。丢了的确认怎么补见 [`AckState`]，
/// 画面的每一路各补各的
pub(super) struct Feedback {
    out: DatagramSender,
    /// 第 `i` 项是第 `i` 路
    acks: std::sync::Mutex<Vec<AckState>>,
    /// 发了新的确认：补发的定时器闲着的话叫醒它（见 [`Feedback::run_repeats`]）
    acked: tokio::sync::Notify,
}

impl Feedback {
    pub fn new(out: DatagramSender) -> Self {
        Self {
            out,
            acks: std::sync::Mutex::new(
                (0..MAX_STREAMS)
                    .map(|stream| AckState { stream, last: None })
                    .collect(),
            ),
            acked: tokio::sync::Notify::new(),
        }
    }

    /// 第 `stream` 路的第 `seq` 帧到了（界面收到了，或者界面反正解不了、这里替它确认），它的到达时刻是 `arrived`
    /// （见 `crate::merge` 的"到达时刻"）
    pub fn ack(&self, stream: u8, seq: u32, arrived: u64) {
        let m = self
            .acks
            .lock()
            .unwrap()
            .get_mut(stream as usize)
            .and_then(|a| a.ack(seq, arrived, Instant::now()));
        if let Some(m) = m {
            self.out.send(&m);
            self.acked.notify_one();
        }
    }

    /// 补发丢了的确认（见 [`AckState`]），一直跑到会话结束。每一路都补完了（画面停了、补够了次数）就不再定时醒来，
    /// 等到下一个确认：画面静止的会话不用每 [`ACK_REPEAT`] 醒一次
    async fn run_repeats(&self) {
        loop {
            let idle = !self.acks.lock().unwrap().iter().any(AckState::repeating);
            if idle {
                // 闲下来之前发的确认留下的通知只会让这里多转一圈
                self.acked.notified().await;
            }
            tokio::time::sleep(ACK_REPEAT).await;
            self.repeat();
        }
    }

    /// 好一阵没有新的确认了，最近的一个再发一次（每一路）
    fn repeat(&self) {
        let now = Instant::now();
        let repeats: Vec<_> = self
            .acks
            .lock()
            .unwrap()
            .iter_mut()
            .filter_map(|a| a.repeat(now))
            .collect();
        for m in repeats {
            self.out.send(&m);
        }
    }

    /// 第 `stream` 路等不到一帧了（见 `crate::merge`）：请被控端从 `decoded` 恢复
    fn lost(&self, stream: u8, decoded: u32) {
        self.out.send(&ControllerDatagram::Lost { decoded, stream });
    }

    /// 告诉被控端该路视频收完和等待重传的帧数。
    fn report(&self, stream: u8, report: VideoReport) {
        self.out
            .send(&ControllerDatagram::Report { report, stream });
    }

    /// 每秒一次：Ping（也是保活），再带一次每一路最近的确认
    fn ping(&self) {
        self.out.send(&ControllerDatagram::Ping { t: now_us() });
        let last: Vec<_> = self
            .acks
            .lock()
            .unwrap()
            .iter()
            .filter_map(AckState::last)
            .collect();
        for m in last {
            self.out.send(&m);
        }
    }
}

/// 画面确认发什么、什么时候补发。
///
/// 确认走数据报，会丢。丢的要是最后一个（画面停了，或者被控端的在途帧满了、正等着它），被控端就一直等：
/// 所以 [`ACK_REPEAT`] 里没有新的确认时，最近的一个再发一次、最多 [`ACK_REPEATS`] 次，之后随每秒的 Ping 再发一次。
/// 画面一直在来时每帧都有新的确认，不补发。确认是累积的：比发过的还旧的（界面晚到的确认）不用发。
/// 一路画面一个
#[derive(Default)]
struct AckState {
    /// 第几路
    stream: u8,
    last: Option<LastAck>,
}

#[derive(Clone, Copy)]
struct LastAck {
    stream: u8,
    seq: u32,
    arrived: u64,
    /// 上次发出的时刻
    at: Instant,
    /// 还能补发几次
    left: u8,
}

impl LastAck {
    fn msg(&self) -> ControllerDatagram {
        ControllerDatagram::Ack {
            seq: self.seq,
            arrived: self.arrived,
            stream: self.stream,
        }
    }
}

impl AckState {
    /// 第 `seq` 帧到了：要发的确认（不比发过的新就不发）
    fn ack(&mut self, seq: u32, arrived: u64, now: Instant) -> Option<ControllerDatagram> {
        if self
            .last
            .is_some_and(|l| seq.wrapping_sub(l.seq) as i32 <= 0)
        {
            return None;
        }
        let l = LastAck {
            stream: self.stream,
            seq,
            arrived,
            at: now,
            left: ACK_REPEATS,
        };
        self.last = Some(l);
        Some(l.msg())
    }

    /// [`ACK_REPEAT`] 里没有发过确认了：最近的一个再发一次（最多 [`ACK_REPEATS`] 次）
    fn repeat(&mut self, now: Instant) -> Option<ControllerDatagram> {
        let l = self.last.as_mut()?;
        if l.left == 0 || now.saturating_duration_since(l.at) < ACK_REPEAT {
            return None;
        }
        l.left -= 1;
        l.at = now;
        Some(l.msg())
    }

    /// 最近的确认（随 Ping 再发一次）
    fn last(&self) -> Option<ControllerDatagram> {
        self.last.map(|l| l.msg())
    }

    /// 还要补发（发了确认之后还没补够 [`ACK_REPEATS`] 次）
    fn repeating(&self) -> bool {
        self.last.is_some_and(|l| l.left > 0)
    }
}

/// 会话的画面和事件交给谁：桌面版和网页版是控制台的 WebSocket（[`crate::console`]），Android 是 Kotlin 取的队列。
///
/// 返回 `Err` 表示界面已经不在了（窗口关了、浏览器断开了），会话随之结束。
pub trait SessionUi: Send + Sync + 'static {
    /// 画面 / 光标 / 声音包（[`airhand_proto::media`] 的格式：头 + 数据）。
    /// 界面收到画面包后确认（[`super::Command::Ack`]），被控端的在途帧控制因此把界面这一段也算进去
    fn packet(&self, packet: Vec<u8>) -> Result<(), UiClosed>;
    /// 会话事件（控制台那边是 JSON）
    fn event(&self, event: SessionEvent) -> Result<(), UiClosed>;
}

/// 控制台已经关闭
#[derive(Debug, Clone, Copy)]
pub struct UiClosed;

/// 发往控制台的会话事件（JSON，camelCase，以 `type` 区分）。
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SessionEvent {
    /// 会话开始了。`codecs` / `actions` 里只有本机认识的；本机不认识的平台（对方的版本更新）报成 `linux`。
    /// `current`：第 0 路画面是哪个显示器；`shown`：每一路画面是哪个显示器（见 [`SessionEvent::Displays`]）；
    /// `max_streams`：对方最多能同时发几路画面（同时看几个显示器，至少 1）
    Hello {
        hostname: String,
        os: String,
        version: String,
        displays: Vec<DisplayInfo>,
        current: u8,
        shown: Vec<u8>,
        max_streams: u8,
        codecs: Vec<Codec>,
        platform: Platform,
        touch: bool,
        actions: Vec<SystemAction>,
    },
    /// 第 `stream` 路画面的编码器（重新）建好了
    Encoder {
        stream: u8,
        codec: Codec,
        name: String,
        hardware: bool,
        width: u32,
        height: u32,
    },
    /// 显示器列表、看着哪几个显示器变了。`current`：第 0 路画面是哪个显示器（只看一个显示器的界面只管它）；
    /// `shown`：第 `i` 项是第 `i` 路画面的显示器（见 `airhand_proto::session` 的"多屏"）
    Displays {
        displays: Vec<DisplayInfo>,
        current: u8,
        shown: Vec<u8>,
    },
    /// 每秒一次。`fps`：最快的那一路的帧率；`kbps`：所有的画面和声音；`width` / `height`：第 0 路画面的大小；
    /// `frames`：一共收到了几帧（所有的路）
    Stats {
        fps: f32,
        kbps: f32,
        rtt_ms: Option<f32>,
        width: u16,
        height: u16,
        frames: u64,
        /// 画面是从直连来的（不经过中转，见 [`crate::direct`]）
        direct: bool,
    },
    /// 被控端发来的提示，由控制台按自己的界面语言显示
    Notice { notice: NoticeView },
    /// 被控端复制了文本（只发给浏览器，见 [`super::Frontend::Browser`]；桌面版直接写入本机剪贴板）
    Clipboard { text: String },
    /// 被控端复制了文件（只发给浏览器、Android，见 [`super::Frontend::Browser`]；桌面版直接放进本机剪贴板）：
    /// 用户选中的那几项。界面要内容时从 [`super::ControllerService::remote_files`] 取（编号是 `clip`）
    ClipboardFiles { clip: u32, items: Vec<FileEntry> },
    /// 剪贴板里的文件传不了（被控端那边的、本机这边的）：`name` 是哪个文件，`error` 是为什么
    /// （例如只能走中转、又超过了服务器允许的大小），界面按自己的语言显示
    FileError { name: String, error: FileError },
    /// 被控端现在能不能接本机的麦克风（见 [`super`] 的"麦克风"）：会话开始时一次，之后有变化再来。
    /// 被控端不支持（版本旧）时不会有。`detail`：出错时给排查问题用的技术信息（英文）
    Mic {
        state: MicState,
        detail: Option<String>,
    },
    /// `retry=false` 表示对方主动断开，不应自动重连
    Closed { reason: Problem, retry: bool },
}

/// [`Notice`] 发给控制台时的 JSON 形状：`{ "kind": "captureFailed", "detail": "..." }`
///（协议里的 `Notice` 是 CBOR，和界面的 JSON 分开定义，各改各的）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum NoticeView {
    NoDisplays,
    Wayland,
    CaptureFailed {
        detail: String,
    },
    /// 不显示：撤下之前的 `captureFailed`
    CaptureRecovered,
    AudioFailed {
        detail: String,
    },
    /// 不显示：撤下之前的 `audioFailed`
    AudioRecovered,
    /// 两项都为 false 表示权限已经全部授予，控制台撤下之前的权限提示
    MissingPermissions {
        screen: bool,
        input: bool,
    },
}

impl From<Notice> for NoticeView {
    fn from(n: Notice) -> Self {
        match n {
            Notice::NoDisplays => NoticeView::NoDisplays,
            Notice::Wayland => NoticeView::Wayland,
            Notice::CaptureFailed { detail } => NoticeView::CaptureFailed { detail },
            Notice::CaptureRecovered => NoticeView::CaptureRecovered,
            Notice::AudioFailed { detail } => NoticeView::AudioFailed { detail },
            Notice::AudioRecovered => NoticeView::AudioRecovered,
            Notice::MissingPermissions { screen, input } => {
                NoticeView::MissingPermissions { screen, input }
            }
        }
    }
}

struct Stats {
    /// 这一秒每一路收到的帧
    frames: [u32; MAX_STREAMS as usize],
    bytes: usize,
    total: u64,
    /// 第 0 路画面的大小
    w: u16,
    h: u16,
    rtt: Option<f32>,
    last_msg: Instant,
    /// 最近一帧画面是从直连来的
    direct: bool,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frames: [0; MAX_STREAMS as usize],
            bytes: 0,
            total: 0,
            w: 0,
            h: 0,
            rtt: None,
            last_msg: Instant::now(),
            direct: false,
        }
    }
}

/// 每一路画面的 [`Arrivals`]（第 `i` 项是第 `i` 路）
pub(super) fn arrivals() -> Vec<Arrivals> {
    (0..MAX_STREAMS).map(|_| Arrivals::default()).collect()
}

/// 一路画面交给了界面、还没确认的帧是什么时候开始从网络上到达的（控制端的时钟，见 [`now_us`]）：界面确认时带给被控端
/// （`ControllerDatagram::Ack::arrived`）。确认是累积的：确认了一帧，它和它前面的都不用记了。
/// 在途的帧不超过 `flow::MAX_WINDOW` 个，最多记 [`Arrivals::MAX`] 个就够了
#[derive(Default)]
pub(super) struct Arrivals {
    pending: VecDeque<(u32, u64)>,
    /// 交给界面的最后一帧
    last: u32,
}

impl Arrivals {
    const MAX: usize = 128;

    fn record(&mut self, seq: u32, at: u64) {
        if self.pending.len() == Self::MAX {
            self.pending.pop_front();
        }
        self.pending.push_back((seq, at));
        self.last = seq;
    }

    /// 界面确认了第 `seq` 帧：它的到达时刻（不是交给过界面的帧、已经确认过的为 `None`）
    pub(super) fn take(&mut self, seq: u32) -> Option<u64> {
        let at = self.pending.iter().find(|(s, _)| *s == seq)?.1;
        self.pending.retain(|(s, _)| s.wrapping_sub(seq) as i32 > 0);
        Some(at)
    }

    /// 交给界面的最后一帧的序号（还没有时为 0）：要关键帧时告诉被控端（见 `ControllerMsg::RequestKeyframe`）
    pub(super) fn last(&self) -> u32 {
        self.last
    }
}

pub(super) struct SessionIo {
    /// 控制通道：发送的一半、加密（已经发过确认，计数接着往下数）
    pub control: (SendStream, Sealer),
    /// 大块通道（剪贴板）：发送的一半、排着队的消息（编码好的）、加密
    pub bulk: (SendStream, mpsc::Receiver<Bytes>, Sealer),
    /// 往大块通道排消息（桌面版自己同步剪贴板时用）
    pub bulk_tx: mpsc::Sender<Bytes>,
    pub recv: HostReceiver,
    pub ui: Arc<dyn SessionUi>,
    pub mode: Mode,
    /// 每一路画面的（第 `i` 项是第 `i` 路）
    pub arrivals: Arc<std::sync::Mutex<Vec<Arrivals>>>,
    /// Ping、画面确认（控制数据报）
    pub feedback: Arc<Feedback>,
    /// 会话的两条路：控制通道在直连通着时也从直连发一份
    pub paths: Paths,
    /// 直连任务
    pub direct: Direct,
    /// 麦克风：会话 ID、界面交过来的声音包、声音数据报的加密端
    pub mic: (u64, mpsc::Receiver<Bytes>, Sealer),
    /// 剪贴板文件会话和收文件流的任务。
    pub files: (Files, OwnedTask<()>),
    /// 被控端剪贴板里最新的文件（网页版、Android 从这里取）
    pub remote: Arc<std::sync::Mutex<Option<RemoteOffer>>>,
}

/// 会话的直连任务（[`crate::direct::controller`]）：被控端发来的候选地址交给它
pub(super) struct Direct {
    pub offers: mpsc::Sender<DirectOffer>,
    pub task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
}

/// 见 [`super::Frontend`]
pub(super) enum Mode {
    Desktop {
        /// 远端→本机 与 本机→远端 共用同一个同步状态，用来抑制回声
        clip: Arc<std::sync::Mutex<Box<dyn ClipboardSync>>>,
        /// 是否同步剪贴板（界面可随时切换）
        enabled: Arc<AtomicBool>,
    },
    Browser,
}

pub(super) async fn run(
    sid: u32,
    io: SessionIo,
    tx: super::queue::Sender,
    rx: super::queue::Receiver,
) {
    let SessionIo {
        control: (send, sealer),
        bulk: (bulk_send, bulk_rx, bulk_sealer),
        bulk_tx,
        mut recv,
        ui,
        mode,
        arrivals,
        feedback,
        paths,
        direct: Direct {
            offers,
            task: direct_task,
        },
        mic: (session_id, mut mic_rx, mut mic_sealer),
        files: (files, _files_task),
        remote,
    } = io;
    let mut file_notices = files.notices();
    let notices_ui = ui.clone();
    let notices = async move {
        while let Some(n) = file_notices.recv().await {
            let _ = notices_ui.event(SessionEvent::FileError {
                name: n.name,
                error: n.error,
            });
        }
        std::future::pending::<()>().await
    };
    let files_in = files.clone();
    let files_out = files.clone();
    let mic_paths = paths.clone();
    // 画面最近是从直连来的：麦克风的声音也走直连（被控端按两条路的时延选的，见 crate::direct）
    let video_direct = Arc::new(AtomicBool::new(false));
    info!(sid, "session started");
    let local_clip = match mode {
        Mode::Desktop { clip, enabled } => Some((clip, enabled)),
        Mode::Browser => None,
    };
    // 控制通道：每一帧写进中转（完整的一份），直连通着的话也从直连发一份（见 crate::direct）。
    // 队列失败了（排满了不能丢的命令）写任务就结束
    let overflow = tx.clone();
    let writer = async move {
        write_lane(send, rx, sealer, Some(paths)).await?;
        anyhow::ensure!(!overflow.failed(), "controller command queue overflow");
        anyhow::Ok(())
    };
    // 大块通道（剪贴板）：和画面一样的优先级，慢了不挡控制消息
    let bulk_writer = async move {
        write_lane(bulk_send, bulk_rx, bulk_sealer, None).await?;
        // 只有会话结束时才会关闭（发送端在会话手里）
        std::future::pending::<()>().await;
        anyhow::Ok(())
    };

    // 统计数据由读循环写入、定时器读取（读循环本身绝不能被 select 取消，否则会破坏帧边界）
    let stats = std::sync::Arc::new(std::sync::Mutex::new(Stats::default()));

    let out = ui.clone();
    let st = stats.clone();
    let clip_in = local_clip.clone();
    let keyframe_tx = tx.clone();
    let acker = feedback.clone();
    let on_direct = video_direct.clone();
    let video_stats = recv.video_stats();
    let reporter_feedback = feedback.clone();
    let reader = async move {
        let mut cursor = super::cursor::CursorReceiver::default();
        loop {
            let msg = match recv.recv().await? {
                // 画面、声音：只看一眼头做统计，原样交给界面。交出来的画面都解得了（缺了参考帧的在
                // crate::merge 里就挑出来了：硬件解码器多半不报错、而是解出花屏，三种界面都不用知道有过缺口）
                FromHost::Video {
                    stream,
                    packet: p,
                    arrived,
                    direct,
                } => {
                    // 未知编码仍交给界面确认并丢弃，不能把合法的新格式当成损坏而断开。
                    let h = VideoHeader::decode_any(&p)
                        .ok_or_else(|| anyhow::anyhow!("bad video packet header"))?;
                    // 界面按头里写的路交给解码器：和来的通道对不上的不能交出去
                    anyhow::ensure!(
                        h.stream == stream,
                        "a video packet of stream {} came on stream {stream}",
                        h.stream
                    );
                    {
                        let mut s = st.lock().unwrap();
                        s.last_msg = Instant::now();
                        s.frames[stream as usize] += 1;
                        s.total += 1;
                        s.bytes += p.len();
                        if stream == 0 {
                            s.w = h.width;
                            s.h = h.height;
                        }
                        s.direct = direct;
                    }
                    on_direct.store(direct, Ordering::Relaxed);
                    arrivals.lock().unwrap()[stream as usize].record(h.seq, arrived);
                    out.packet(p).map_err(|_| ui_closed())?;
                    continue;
                }
                FromHost::VideoEvent(stream, v) => {
                    match v {
                        // 解不了的增量帧不交给界面，这里替它确认：不然被控端的在途窗口停住，恢复的帧也发不过来
                        Video::Skipped { seq, arrived } => {
                            st.lock().unwrap().last_msg = Instant::now();
                            acker.ack(stream, seq, arrived);
                        }
                        // 等不到一帧了：请被控端恢复这一路
                        Video::Lost { decoded } => acker.lost(stream, decoded),
                        // 被控端在这一帧之后发过关键帧就不用再编了
                        Video::Keyframe { after } => {
                            keyframe_tx.send(ControllerMsg::RequestKeyframe { after, stream })?;
                        }
                        Video::Frame(_) => unreachable!("delivered as FromHost::Video"),
                    }
                    continue;
                }
                FromHost::Audio(p) => {
                    {
                        let mut s = st.lock().unwrap();
                        s.last_msg = Instant::now();
                        s.bytes += p.len();
                    }
                    out.packet(p).map_err(|_| ui_closed())?;
                    continue;
                }
                FromHost::Pong(t) => {
                    let sample = now_us().saturating_sub(t) as f32 / 1000.0;
                    let rtt = {
                        let mut s = st.lock().unwrap();
                        s.last_msg = Instant::now();
                        let rtt = match s.rtt {
                            Some(r) => r * 0.7 + sample * 0.3,
                            None => sample,
                        };
                        s.rtt = Some(rtt);
                        rtt
                    };
                    // 缺着的画面等多久按它算（见 crate::merge）
                    recv.set_rtt(rtt as f64);
                    continue;
                }
                FromHost::Control(m) => m,
            };
            st.lock().unwrap().last_msg = Instant::now();
            match msg {
                HostMsg::Hello(h) => {
                    let max_streams = h.max_streams();
                    let shown = shown_or_current(h.shown, h.current_display);
                    // 对方版本新增的编码格式、系统操作这里用不了；新的平台当作一台普通的电脑
                    let _ = out.event(SessionEvent::Hello {
                        hostname: h.hostname,
                        os: h.os,
                        version: h.version,
                        displays: h.displays,
                        current: h.current_display,
                        shown,
                        max_streams,
                        codecs: h.codecs.into_iter().filter(|c| c.is_known()).collect(),
                        platform: h.platform.or_linux(),
                        touch: h.touch,
                        actions: h.actions.into_iter().filter(|a| a.is_known()).collect(),
                    });
                }
                HostMsg::Displays {
                    displays,
                    current,
                    shown,
                } => {
                    let shown = shown_or_current(shown, current);
                    let _ = out.event(SessionEvent::Displays {
                        displays,
                        current,
                        shown,
                    });
                }
                HostMsg::Notice(Some(n)) => {
                    let _ = out.event(SessionEvent::Notice { notice: n.into() });
                }
                // 对方版本新增的提示：不显示
                HostMsg::Notice(None) => debug!(sid, "skipping a notice from a newer host"),
                HostMsg::Clipboard { text } => {
                    // 文字取代被控端之前公告的文件
                    files_in.remote_cleared();
                    match &clip_in {
                        Some((clip, enabled)) => {
                            if enabled.load(Ordering::Relaxed) {
                                clip.lock().unwrap().apply(text);
                            }
                        }
                        None => {
                            let _ = out.event(SessionEvent::Clipboard { text });
                        }
                    }
                }
                HostMsg::Files(Some(m)) => {
                    if let Some(offer) = files_in.handle(m) {
                        *remote.lock().unwrap() = Some(offer.clone());
                        match &clip_in {
                            Some((clip, enabled)) => {
                                if enabled.load(Ordering::Relaxed) {
                                    clip.lock().unwrap().apply_files(offer);
                                } else {
                                    // 不同步剪贴板：不放进本机剪贴板，但别让被控端一直等
                                    offer.ready();
                                }
                            }
                            None => {
                                let _ = out.event(SessionEvent::ClipboardFiles {
                                    clip: offer.clip(),
                                    items: offer.items().to_vec(),
                                });
                                offer.ready();
                            }
                        }
                    }
                }
                // 对方版本新增的文件消息：跳过
                HostMsg::Files(None) => {}
                HostMsg::Bye(reason) => {
                    if let ByeReason::Unknown(n) = reason {
                        info!(
                            sid,
                            reason = n,
                            "the host ended the session for a reason this version doesn't know"
                        );
                    }
                    return Err(Problem::from(reason).into());
                }
                HostMsg::Cursor { visibility, shape } => {
                    let state = cursor.update(visibility, shape);
                    out.packet(media::cursor_packet(&state))
                        .map_err(|_| ui_closed())?;
                }
                HostMsg::CursorImage { reference, image } => {
                    if let Some(state) = cursor.image(reference, image) {
                        out.packet(media::cursor_packet(&state))
                            .map_err(|_| ui_closed())?;
                    }
                }
                HostMsg::Encoder(e) => {
                    let _ = out.event(SessionEvent::Encoder {
                        stream: e.stream,
                        codec: e.codec,
                        name: e.name,
                        hardware: e.hardware,
                        width: e.width,
                        height: e.height,
                    });
                }
                // 直连的候选地址：交给直连任务
                HostMsg::Direct(offer) => {
                    let _ = offers.try_send(offer);
                }
                HostMsg::Mic { state, detail } => {
                    // 对方版本新增的状态：不知道能不能接，当作不支持（界面不显示麦克风）
                    let state = if state.is_known() {
                        state
                    } else {
                        MicState::Unsupported
                    };
                    let _ = out.event(SessionEvent::Mic { state, detail });
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let out = ui.clone();
    let pinger = feedback.clone();
    let ticker = async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = Instant::now();
        loop {
            tick.tick().await;
            let dt = last.elapsed().as_secs_f32().max(0.001);
            last = Instant::now();
            let event = {
                let mut s = stats.lock().unwrap();
                if s.last_msg.elapsed() > HOST_TIMEOUT {
                    return Err(Problem::HostUnresponsive.into());
                }
                let e = SessionEvent::Stats {
                    fps: s.frames.iter().copied().max().unwrap_or(0) as f32 / dt,
                    kbps: s.bytes as f32 * 8.0 / 1000.0 / dt,
                    rtt_ms: s.rtt,
                    width: s.w,
                    height: s.h,
                    frames: s.total,
                    direct: s.direct,
                };
                s.frames = [0; MAX_STREAMS as usize];
                s.bytes = 0;
                e
            };
            out.event(event).map_err(|_| ui_closed())?;
            // 也是保活：被控端很久收不到控制端的消息就会结束会话
            pinger.ping();
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    // 画面收得怎么样（见 crate::merge 的"报告"）：每 REPORT_EVERY 告诉被控端一次，这段时间什么画面都没收到就不报
    let reporter = async move {
        let mut tick = tokio::time::interval(REPORT_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            for (stream, stats) in video_stats.iter().enumerate() {
                let r = stats.take();
                if !r.is_empty() {
                    reporter_feedback.report(stream as u8, r);
                }
            }
        }
    };
    // 丢了的确认（见 Feedback）
    let repeater = async move { feedback.run_repeats().await };

    let clipper = async move {
        let Some((clip, enabled)) = local_clip else {
            // 网页版：浏览器自己读剪贴板
            return std::future::pending().await;
        };
        let mut tick = tokio::time::interval(CLIPBOARD_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            // poll 只取后台读任务已完成的结果，锁内不执行或等待剪贴板 IO。
            let got = clip.lock().unwrap().poll();
            // 关闭同步时也要消费变化，否则重新打开时会把旧内容发过去
            let Some(got) = got else { continue };
            if !enabled.load(Ordering::Relaxed) {
                continue;
            }
            match got {
                Clip::Text(text) => {
                    files_out.withdraw();
                    send_clipboard(&bulk_tx, text);
                }
                // 复制时只发清单，读取文件元数据可能阻塞。
                Clip::Files(paths) => {
                    let local = tokio::task::spawn_blocking(move || LocalFs::new(&paths)).await;
                    if let Ok(local) = local
                        && !local.is_empty()
                    {
                        files_out.offer(Arc::new(local));
                    }
                }
            }
        }
    };

    // 直连任务一直跑到会话结束（它不再试了：一直等着）
    let direct = async move {
        direct_task.await;
        std::future::pending::<()>().await
    };
    // 麦克风：界面打好的声音包带上之前几包的副本（见 crate::audio），加密之后发成数据报。
    // 和画面走同一条路；丢了就丢了（下一个数据报里有副本）
    let mic = async move {
        let mut bundler = Bundler::default();
        while let Some(packet) = mic_rx.recv().await {
            let Some(plain) = bundler.bundle_packet(packet) else {
                continue;
            };
            let d = match mic_sealer.seal_datagram(session_id, datagram::AUDIO, &plain) {
                Ok(d) => Bytes::from(d),
                Err(e) => {
                    debug!(sid, "can't seal a microphone packet: {e:#}");
                    continue;
                }
            };
            let direct = video_direct
                .load(Ordering::Relaxed)
                .then(|| mic_paths.direct())
                .flatten();
            let conn = direct.as_ref().unwrap_or(mic_paths.relay());
            if let Err(e) = conn.send_datagram(d) {
                debug!(sid, "dropped a microphone packet: {e}");
            }
        }
        std::future::pending::<()>().await
    };
    let result: Result<()> = tokio::select! {
        r = reader => r,
        r = ticker => r,
        _ = repeater => Ok(()),
        () = reporter => Ok(()),
        () = clipper => Ok(()),
        () = direct => Ok(()),
        () = mic => Ok(()),
        () = notices => Ok(()),
        r = writer => r.and(Err(Problem::SessionEnded.into())),
        r = bulk_writer => r.and(Err(Problem::SessionEnded.into())),
    };
    // 停下给到一半的、取到一半的（本机剪贴板里留着的公告之后再读也只是失败）
    files.close();
    let reason = match result {
        Ok(()) => Problem::SessionEnded,
        Err(e) => Problem::from_error(e, Problem::failed),
    };
    // 对方有意结束（断开所有控制端 / 更换控制码）时不自动重连
    let retry = !reason.is_final();
    info!(sid, retry, "session ended: {reason}");
    if ui.event(SessionEvent::Closed { reason, retry }).is_err() {
        warn!(sid, "couldn't tell the console that the session ended");
    }
}

/// 控制台已经关了（窗口关闭 / 浏览器断开），会话也就没必要继续了
fn ui_closed() -> anyhow::Error {
    anyhow::anyhow!("the console was closed")
}

/// 每一路画面是哪个显示器；被控端没说（版本旧）时只有第 0 路，就是 `current`
fn shown_or_current(shown: Vec<u8>, current: u8) -> Vec<u8> {
    if shown.is_empty() {
        vec![current]
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ack(seq: u32) -> Option<ControllerDatagram> {
        Some(ControllerDatagram::Ack {
            seq,
            arrived: 0,
            stream: 0,
        })
    }

    /// 画面一直在来时不补发；停了之后每隔 ACK_REPEAT 补一次、最多 ACK_REPEATS 次；旧的、重复的确认不发
    #[test]
    fn acks_are_repeated_only_when_no_new_ones_follow() {
        let mut a = AckState::default();
        let t0 = Instant::now();
        assert_eq!(a.repeat(t0), None);
        // 还没发过确认：补发的定时器闲着
        assert!(!a.repeating());
        // 60 fps：每帧都有新的确认，定时器不补
        let frame = Duration::from_micros(16_667);
        for i in 1..=10u32 {
            let now = t0 + frame * i;
            assert_eq!(a.ack(i, 0, now), ack(i));
            assert_eq!(a.repeat(now + frame / 2), None, "frame {i}");
        }
        // 界面晚到的旧确认、重复的：不发，也不把最近的换成旧的
        let last = t0 + frame * 10;
        assert_eq!(a.ack(7, 0, last), None);
        assert_eq!(a.ack(10, 0, last), None);
        assert_eq!(a.last(), ack(10));
        // 画面停了：补发三次，每次隔 ACK_REPEAT
        let mut now = last;
        let mut repeats = 0;
        for _ in 0..20 {
            now += Duration::from_millis(10);
            if a.repeat(now).is_some() {
                repeats += 1;
            }
        }
        assert_eq!(repeats, ACK_REPEATS);
        assert_eq!(a.last(), ack(10));
        // 补够了：定时器闲下来，直到下一个确认
        assert!(!a.repeating());
        assert_eq!(a.ack(11, 0, now), ack(11));
        assert!(a.repeating());
        // 序号绕回
        let mut a = AckState::default();
        assert_eq!(a.ack(u32::MAX, 0, t0), ack(u32::MAX));
        assert_eq!(a.ack(0, 0, t0), ack(0));
    }

    /// 补完了就闲下来（不再定时醒）；闲下来之后再来确认，照样补发 ACK_REPEATS 次
    #[tokio::test]
    async fn repeats_resume_after_going_idle() {
        let (client, server) = crate::tests::quic_pair().await;
        let (sealer, _opener) = crate::secure::test_lane(false);
        let feedback = Arc::new(Feedback::new(DatagramSender::new(
            1,
            Paths::new(client),
            sealer,
        )));
        let _repeater = {
            let f = feedback.clone();
            OwnedTask(tokio::spawn(async move { f.run_repeats().await }))
        };
        for seq in 1..=2u32 {
            feedback.ack(0, seq, 0);
            let until = tokio::time::Instant::now() + Duration::from_millis(400);
            let mut sent = 0;
            while let Ok(Ok(_)) = tokio::time::timeout_at(until, server.read_datagram()).await {
                sent += 1;
            }
            // 确认本身 + 补发
            assert_eq!(sent, 1 + ACK_REPEATS as usize, "frame {seq}");
            assert!(!feedback.acks.lock().unwrap()[0].repeating());
        }
    }

    /// 控制台（client/src/lib/api.ts 的 SessionEvent / Notice 类型）依赖这个 JSON 形状
    #[test]
    fn event_json_shape() {
        use serde_json::json;
        let n = SessionEvent::Notice {
            notice: Notice::CaptureFailed {
                detail: "0x887A0026".into(),
            }
            .into(),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({ "type": "notice", "notice": { "kind": "captureFailed", "detail": "0x887A0026" } })
        );
        let n = SessionEvent::Notice {
            notice: Notice::Wayland.into(),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({ "type": "notice", "notice": { "kind": "wayland" } })
        );
        let n = SessionEvent::Notice {
            notice: Notice::MissingPermissions {
                screen: false,
                input: true,
            }
            .into(),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({ "type": "notice", "notice": { "kind": "missingPermissions", "screen": false, "input": true } })
        );
        let n = SessionEvent::Notice {
            notice: Notice::AudioRecovered.into(),
        };
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({ "type": "notice", "notice": { "kind": "audioRecovered" } })
        );
        let c = SessionEvent::Closed {
            reason: Problem::from(airhand_proto::session::ByeReason::Kicked),
            retry: false,
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({ "type": "closed", "reason": { "kind": "kicked" }, "retry": false })
        );
        let c = SessionEvent::Clipboard { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({ "type": "clipboard", "text": "hi" })
        );
        assert_eq!(
            serde_json::to_value(SessionEvent::Mic {
                state: MicState::NoDevice,
                detail: None
            })
            .unwrap(),
            json!({ "type": "mic", "state": "noDevice", "detail": null })
        );
        assert_eq!(
            serde_json::to_value(SessionEvent::Mic {
                state: MicState::Failed,
                detail: Some("0x8889000A".into())
            })
            .unwrap(),
            json!({ "type": "mic", "state": "failed", "detail": "0x8889000A" })
        );
        let h = SessionEvent::Hello {
            hostname: "Pixel".into(),
            os: "Android 15".into(),
            version: "0.1.0".into(),
            displays: vec![],
            current: 0,
            shown: vec![0],
            max_streams: 1,
            codecs: vec![Codec::H265],
            platform: Platform::Android,
            touch: true,
            actions: vec![SystemAction::Back, SystemAction::QuickSettings],
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["platform"], json!("android"));
        assert_eq!(v["touch"], json!(true));
        assert_eq!(v["actions"], json!(["back", "quickSettings"]));
        assert_eq!(v["codecs"], json!(["h265"]));
        assert_eq!((&v["shown"], &v["maxStreams"]), (&json!([0]), &json!(1)));
        let d = SessionEvent::Displays {
            displays: vec![],
            current: 1,
            shown: vec![1, 0],
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({ "type": "displays", "displays": [], "current": 1, "shown": [1, 0] })
        );
        let e = SessionEvent::Encoder {
            stream: 1,
            codec: Codec::AV1,
            name: "NVENC".into(),
            hardware: true,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({ "type": "encoder", "stream": 1, "codec": "av1", "name": "NVENC", "hardware": true, "width": 1920, "height": 1080 })
        );
        let s = SessionEvent::Stats {
            fps: 30.0,
            kbps: 1.0,
            rtt_ms: None,
            width: 1,
            height: 1,
            frames: 2,
            direct: true,
        };
        assert_eq!(serde_json::to_value(&s).unwrap()["rttMs"], json!(null));
        assert_eq!(serde_json::to_value(&s).unwrap()["direct"], json!(true));
        let c = SessionEvent::Closed {
            reason: Problem::from(airhand_proto::session::ByeReason::CodeChanged),
            retry: false,
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({ "type": "closed", "reason": { "kind": "codeChanged" }, "retry": false })
        );
    }
}
