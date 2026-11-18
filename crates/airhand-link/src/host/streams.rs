//! 被控端一个会话的链路：回答端到端握手、开本端的通道、读写任务、直连任务、结束。会话里收发什么、什么时候发，
//! 见 [`super::session`]。
//!
//! 发出握手的决定就开通道、交给会话，不等控制端的确认（见 `airhand_proto::handshake` 的"确认"）：确认是控制端
//! 在控制通道上发的第一帧，读任务解开它时告诉会话（[`ControllerReceiver::confirmation`]），解不开就结束。
//!
//! Ping 和画面确认是控制数据报（`airhand_proto::session::ControllerDatagram`），在读任务里就处理掉（[`Express`]），
//! 控制端麦克风的声音数据报也在这里解开、交给平台的 [`MicSink`]。它们都不排进会话的主循环：主循环可能正等着平台做一件慢事（换显示器时要等编码线程退出、建硬件编码器），这期间 Pong
//! 和确认都晚了，控制端测到的往返时延、被控端的码率控制都会把它当成网络变慢。控制端的其它消息在读任务里就换成了
//! [`Request`]。
//!
//! 有直连（[`Incoming::direct`]）时会话开始之后还有一个直连任务：直连通了就接到会话的 [`Paths`] 上
//! （控制通道每一帧、Pong 也从直连发一份），直连上控制端发来的控制通道和中转上的合并（[`crate::merge`]），
//! 画面、声音走哪条路由它按两条路上的探测决定（控制端的回声在读任务里转给它）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use airhand_proto::datagram;
use airhand_proto::framing;
use airhand_proto::handshake::{Confirm, PublicKey};
use airhand_proto::session::{
    ControllerDatagram, ControllerMsg, DirectOffer, HostDatagram, InputEvent, MAX_CLIPBOARD_TEXT,
    QualitySettings, StreamView,
};
use airhand_proto::{MAX_STREAMS, SessionLane};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{debug, info, warn};

use super::Incoming;
use super::clock::InputClock;
use super::queue::{self, Outbox};
use super::session::{Acks, MicSink, Request};
use super::write::{FrameStreams, Writers, write_all};
use crate::audio::Unbundler;
use crate::direct;
use crate::files::Files;
use crate::lanes::{Via, first_stream, open_lane};
use crate::merge::Merge;
use crate::paths::{DatagramSender, Paths};
use crate::secure::{self, Admission, Gate, Opener, ResponderParams, STEP_TIMEOUT};
use crate::tasks::OwnedTask;
use crate::transport::{next_msg, open_datagram};
use airhand_proto::files::FileMsg;
use airhand_proto::session::HostMsg;

/// 被控端一个会话的链路。
///
/// 控制消息、大块消息（剪贴板、光标）排进 [`Outbox`]（它按消息挑通道，见 [`super::queue`]），画面包、声音包
/// 各排进一个有界队列，各自的写任务把它们加密后写出去（声音发成数据报）。控制端的消息由读任务解密、换成 [`Request`]
/// 交给 [`HostStreams::incoming`]，过时的输入在这里就换成了"释放所有按键"（见 [`InputClock`]）。
///
/// 结束时调用 [`HostStreams::close`]：在那之前要先停掉还拿着 `out` / `video` / `audio` 的东西
/// （编码管线、声音订阅），写任务才能把队列里剩下的（例如 Bye）发完、正常结束通道。
pub struct HostStreams {
    /// 控制消息（控制通道、大块通道）
    pub out: Outbox,
    /// 画面包（[`airhand_proto::media::frame`] 编好的）：第 `i` 项是第 `i` 路（见 `airhand_proto::session` 的"多屏"）
    pub video: Vec<mpsc::Sender<Bytes>>,
    /// 声音包（同上）
    pub audio: mpsc::Sender<Bytes>,
    /// 控制端的请求、确认
    pub incoming: ControllerReceiver,
    /// 写任务：意外结束就是连接出了问题
    pub writer: Writer,
    reader: OwnedTask<Result<()>>,
    /// 直连任务（有直连时才有，见 [`crate::direct::host`]）：会话开始之后（[`HostStreams::started`]）才开始
    direct: Option<OwnedTask<()>>,
    /// 剪贴板文件处理端和收文件流的任务（见 [`crate::files`]）。
    pub files: Option<Files>,
    _files_task: Option<OwnedTask<()>>,
    started: Arc<Notify>,
    /// 在中转连接上的登记：会话结束时注销
    _registration: crate::lanes::Registration,
}

/// 控制端的请求，交给会话的主循环（Ping、确认已经在读任务里处理掉了，见模块说明）。
/// 控制和大块消息使用独立信箱，批量剪贴板不能占满输入的接收位置。
pub struct ControllerReceiver {
    control: mpsc::Receiver<Request>,
    bulk: mpsc::Receiver<Request>,
    bulk_open: bool,
    confirm: oneshot::Receiver<()>,
    express: Express,
}

impl ControllerReceiver {
    pub(crate) fn new(
        control: mpsc::Receiver<Request>,
        bulk: mpsc::Receiver<Request>,
        confirm: oneshot::Receiver<()>,
        express: Express,
    ) -> Self {
        Self {
            control,
            bulk,
            bulk_open: true,
            confirm,
            express,
        }
    }

    pub async fn recv(&mut self) -> Option<Request> {
        loop {
            tokio::select! {
                biased;
                m = self.control.recv() => return m,
                m = self.bulk.recv(), if self.bulk_open => match m {
                    Some(m) => return Some(m),
                    None => self.bulk_open = false,
                }
            }
        }
    }

    /// 控制端的确认（控制通道上的第一帧，见 `airhand_proto::handshake` 的"确认"）：解开了就到，这时才算它真的在连；
    /// 读任务没等到确认就结束了（解不开：多半是重放的握手消息）就是 `Err`。只能取一次（再取的马上是 `Err`）
    pub fn confirmation(&mut self) -> oneshot::Receiver<()> {
        std::mem::replace(&mut self.confirm, oneshot::channel().1)
    }

    /// 画面确认从现在起交给 `acks`（只能设一次）。之前收到的丢掉：那时还没发过画面
    pub fn set_acks(&self, acks: Arc<dyn Acks>) {
        let _ = self.express.acks.set(acks);
    }

    /// 控制端麦克风的声音从现在起交给 `mic`（只能设一次）。之前收到的丢掉
    pub fn set_mic(&self, mic: Arc<dyn MicSink>) {
        let _ = self.express.mic.set(mic);
    }

    /// 多久没收到控制端的任何消息了（包括读任务自己处理掉的 Ping、确认）
    pub fn silent_for(&self) -> Duration {
        self.express.heard.lock().unwrap().elapsed()
    }
}

/// 读任务自己处理的事（见模块说明）：
///
/// * Ping：马上回 Pong（会话的主循环忙着的时候也回：控制端不会把它当成失联）；
/// * 画面确认：马上交给平台的 [`Acks`]（帧流：确认了的帧流不用管了）；
/// * 等不到帧了（帧流）：马上交给平台的 [`Acks`]，平台要恢复的话放弃之后的帧流；
/// * 帧流的接收报告直接交给平台的码率控制。
/// * 和画面有关的都带着是第几路（见 `airhand_proto::session` 的"多屏"），各交各的；没有的路不管；
/// * 控制端麦克风的声音包：马上交给平台的 [`MicSink`]；
/// * 任何消息都记下收到的时刻，会话据此发现失联（[`super::PEER_TIMEOUT`]）。
#[derive(Clone)]
pub(crate) struct Express {
    pong: Arc<dyn Fn(u64) + Send + Sync>,
    acks: Arc<OnceLock<Arc<dyn Acks>>>,
    mic: Arc<OnceLock<Arc<dyn MicSink>>>,
    /// 上次收到控制端消息的时刻（tokio 的时钟：测试里可以拨）
    heard: Arc<Mutex<tokio::time::Instant>>,
    /// 每一路在途的帧流
    frames: Vec<Arc<FrameStreams>>,
}

impl Express {
    /// `pong`：回一个 Pong（发成控制数据报）
    pub(crate) fn new(pong: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self {
            pong: Arc::new(pong),
            acks: Arc::default(),
            mic: Arc::default(),
            heard: Arc::new(Mutex::new(tokio::time::Instant::now())),
            frames: Vec::new(),
        }
    }

    /// 画面用帧流（每一路一个）：确认了的帧流不用管了，等不到的帧放弃
    fn with_frames(mut self, frames: Vec<Arc<FrameStreams>>) -> Self {
        self.frames = frames;
        self
    }

    /// 控制端麦克风的一个声音包（还没有人收时丢掉）
    pub(crate) fn mic(&self, packet: Vec<u8>) {
        if let Some(m) = self.mic.get() {
            m.packet(packet);
        }
    }

    /// 收到了控制端的消息（控制通道、大块通道、数据报上的都算）
    pub(crate) fn heard(&self) {
        *self.heard.lock().unwrap() = tokio::time::Instant::now();
    }

    /// 一个控制数据报
    pub(crate) fn datagram(&self, m: ControllerDatagram) {
        self.heard();
        match m {
            ControllerDatagram::Ping { t } => (self.pong)(t),
            // 没有这一路：不管
            ControllerDatagram::Ack { stream, .. }
            | ControllerDatagram::Lost { stream, .. }
            | ControllerDatagram::Report { stream, .. }
                if stream >= MAX_STREAMS => {}
            ControllerDatagram::Ack {
                seq,
                arrived,
                stream,
            } => {
                if let Some(acks) = self.acks.get() {
                    acks.ack(stream, seq, arrived);
                }
                if let Some(f) = self.frames.get(stream as usize) {
                    f.acked(seq);
                }
            }
            ControllerDatagram::Report { report, stream } => {
                if let Some(acks) = self.acks.get() {
                    acks.feedback(stream, report);
                }
            }
            // 平台开始恢复时，停止重传已作废的帧。
            ControllerDatagram::Lost { decoded, stream } => {
                let recover = self.acks.get().is_some_and(|a| a.lost(stream, decoded));
                if recover && let Some(f) = self.frames.get(stream as usize) {
                    let n = f.abandon(decoded);
                    debug!(
                        stream,
                        decoded,
                        abandoned = n,
                        "the controller lost a frame, recovering"
                    );
                }
            }
            // 直连任务的事（读任务转给它）
            ControllerDatagram::Echo { .. } => {}
        }
    }
}

/// 控制端的一条消息交给会话时是什么请求（直连的回答不在这里）。没什么可做的是 `None`
pub(crate) fn request(m: ControllerMsg) -> Option<Request> {
    Some(match m {
        ControllerMsg::Input { mut events, .. } => {
            // 对方版本新增的按钮、系统操作……：不执行
            events.retain(InputEvent::understood);
            if events.is_empty() {
                return None;
            }
            Request::Input(events)
        }
        ControllerMsg::Settings(s) => Request::Settings(s.sanitized()),
        ControllerMsg::RequestKeyframe { after, stream } => {
            Request::Keyframe(Some((stream, after)))
        }
        ControllerMsg::SelectDisplay { index } => Request::View(vec![StreamView::display(index)]),
        ControllerMsg::View { mut streams } => {
            // 太长的不要（平台再按自己有几个显示器、能发几路去掉不要的，见 resolve_view）
            streams.truncate(MAX_STREAMS as usize);
            Request::View(streams)
        }
        ControllerMsg::Clipboard { text } if text.len() > MAX_CLIPBOARD_TEXT => {
            warn!(len = text.len(), "ignoring an oversized clipboard text");
            return None;
        }
        ControllerMsg::Clipboard { text } => Request::Clipboard(text),
        ControllerMsg::Audio { enabled } => Request::Audio(enabled),
        ControllerMsg::Mic { enabled } => Request::Mic(enabled),
        // 直连的回答交给直连任务，剪贴板里的文件交给 Files（都在读任务里）
        ControllerMsg::Direct(_) | ControllerMsg::Files(_) => return None,
    })
}

/// 发给控制端的文件消息（见 [`crate::files::Peer`]）：经会话的 `Outbox`。会话结束时（[`Files::close`]）放开它，
/// 写任务才知道没有人再发了
struct HostPeer(Mutex<Option<Outbox>>);

impl crate::files::Peer for HostPeer {
    fn send(&self, msg: FileMsg) -> bool {
        let out = self.0.lock().unwrap().clone();
        out.is_some_and(|o| o.send(&HostMsg::Files(Some(msg))).is_ok())
    }

    fn close(&self) {
        self.0.lock().unwrap().take();
    }
}

/// 写任务（几条通道一起）
pub struct Writer(pub(super) OwnedTask<Result<()>>);

impl Writer {
    /// 等到写任务意外结束（可以放在 `select!` 里）
    pub async fn failed(&mut self) -> anyhow::Error {
        match (&mut self.0.0).await {
            Ok(Err(e)) => e.context("session writer failed"),
            r => anyhow!("session writer ended: {r:?}"),
        }
    }
}

/// 端到端握手之后，被控端知道的这个控制端（见 [`crate::secure::respond`]）。它还没有确认
/// （[`ControllerReceiver::confirmation`]）：确认之前不要在托盘、Android 的列表里显示它，不要提醒本机用户
#[derive(Debug, Clone)]
pub struct Admitted {
    /// 控制端的公钥（它的身份）
    pub key: PublicKey,
    /// 控制端自己报的名称（空字符串表示不知道）和系统：在加密的握手里，服务器改不了
    pub name: String,
    pub os: String,
    /// 安全码：控制端显示的是同一个，两边的人可以对一下（见 `airhand_proto::handshake`）
    pub safety: String,
    /// 为什么让它进来（刚配对的还是配对过的）
    pub admission: Admission,
    /// 开始时的画质（已经做过范围校验）：编码管线一开始就按它建
    pub settings: QualitySettings,
    /// 开始时要不要声音
    pub audio: bool,
    /// 开始时看哪几个显示器（没检查过，见 [`super::resolve_view`]；空的是只看主显示器）
    pub view: Vec<StreamView>,
}

impl HostStreams {
    /// 回答控制端的端到端握手（让不让这个控制端进来由 `gate` 决定，拒绝时返回错误），开好本端的通道。
    /// 发出决定就返回，不等控制端的确认（见模块说明）。
    /// `video_queue` / `audio_queue`：画面、声音的发送队列有多长（满了由发送方决定丢弃还是等待）
    pub async fn open(
        inc: &Incoming,
        video_queue: usize,
        audio_queue: usize,
        gate: &dyn Gate,
    ) -> Result<(Self, Admitted)> {
        let session_id = inc.session_id;
        let conn = &inc.conn;
        let (mut inbox, registration) = inc.router.register(session_id);
        // 握手的回答写进本端的控制通道；控制端要等收到回答才开它的，用到时再等
        let mut send = open_lane(conn, session_id, SessionLane::Control).await?;
        let (r, control_in) = secure::respond(
            &inc.init,
            &mut send,
            first_stream(&mut inbox.control, STEP_TIMEOUT),
            ResponderParams {
                code: inc.code,
                gate,
            },
        )
        .await?;
        let admitted = Admitted {
            key: r.secured.peer,
            name: r.intro.name.chars().take(64).collect(),
            os: r.intro.os.chars().take(64).collect(),
            safety: r.secured.safety.clone(),
            admission: r.admission,
            settings: r.intro.settings.sanitized(),
            audio: r.intro.audio,
            view: r
                .intro
                .view
                .into_iter()
                .take(MAX_STREAMS as usize)
                .collect(),
        };
        info!(
            session_id,
            admission = ?admitted.admission,
            controller = %secure::hex(&admitted.key),
            safety = %admitted.safety,
            "end-to-end session established"
        );
        let mut secured = r.secured;
        let bulk_send = open_lane(conn, session_id, SessionLane::Bulk).await?;
        let frames: Vec<Arc<FrameStreams>> =
            (0..secured.video.len()).map(|_| Arc::default()).collect();
        let (out, ctl_rx, bulk_rx) = queue::channel();
        let (video_tx, video): (Vec<_>, Vec<_>) = std::mem::take(&mut secured.video)
            .into_iter()
            .zip(&frames)
            .map(|(keys, streams)| {
                let (tx, rx) = mpsc::channel(video_queue);
                (tx, (streams.clone(), rx, keys.sealer))
            })
            .unzip();
        let (audio_tx, audio_rx) = mpsc::channel(audio_queue);
        // 两条路：直连通了由直连任务接上，画面、声音走哪条也由它决定（没有直连时一直是中转）
        let paths = Paths::new(conn.clone());
        let writer = OwnedTask(tokio::spawn(write_all(Writers {
            session_id,
            paths: paths.clone(),
            control: (send, ctl_rx, r.sealer),
            bulk: (bulk_send, bulk_rx, secured.bulk.sealer),
            video,
            audio: (audio_rx, secured.audio.sealer),
        })));
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过信箱交给会话。
        // 控制端的控制通道握手时已经等到了的（Noise_XX）先放进去，没等到的（Noise_IK）之后从信箱来
        let mut control = Merge::new(SessionLane::Control, r.opener, inbox.control);
        if let Some(s) = control_in {
            control.add(s, Via::Relay)?;
        }
        let bulk = Merge::new(SessionLane::Bulk, secured.bulk.opener, inbox.bulk);
        let capacity = 4 * 1024 * 1024 / SessionLane::Control.max_frame_size();
        let (control_tx, control_rx) = mpsc::channel(capacity);
        let (bulk_tx, bulk_rx) = mpsc::channel(2);
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let datagrams = DatagramSender::new(session_id, paths.clone(), secured.datagrams.sealer);
        let pong = datagrams.clone();
        let express =
            Express::new(move |t| pong.send(&HostDatagram::Pong { t })).with_frames(frames);
        let incoming = ControllerReceiver::new(control_rx, bulk_rx, confirm_rx, express.clone());
        let (answers_tx, answers) = mpsc::channel(4);
        let (echoes_tx, echoes) = mpsc::channel(ECHO_QUEUE);
        let started = Arc::new(Notify::new());
        let (files, files_task) = Files::start(
            session_id,
            paths.clone(),
            secured.files,
            inc.file_limit,
            HostPeer(Mutex::new(Some(out.clone()))),
            inbox.files,
        );
        let direct = inc.direct.as_ref().map(|link| {
            OwnedTask(tokio::spawn(direct::host::run(direct::host::Session {
                link: link.clone(),
                session_id,
                paths,
                datagrams,
                echoes,
                feed: inbox.feed.clone(),
                out: out.clone(),
                answers,
                acks: express.acks.clone(),
                keyframe: control_tx.downgrade(),
                started: started.clone(),
            })))
        });
        let reader = OwnedTask(tokio::spawn(read_all(Readers {
            session_id,
            control,
            bulk,
            files: Some(files.clone()),
            datagrams: (inbox.datagrams, secured.datagrams.opener),
            mic: (inbox.audio, secured.audio.opener),
            express,
            confirm: confirm_tx,
            control_tx,
            bulk_tx,
            answers: answers_tx,
            echoes: echoes_tx,
        })));
        Ok((
            Self {
                out,
                video: video_tx,
                audio: audio_tx,
                incoming,
                writer: Writer(writer),
                reader,
                direct,
                files: Some(files),
                _files_task: Some(files_task),
                started,
                _registration: registration,
            },
            admitted,
        ))
    }

    /// 会话开始了（发了 Hello）时通知它（`notify_one`）：这时才开始建直连
    pub fn started(&self) -> Arc<Notify> {
        self.started.clone()
    }

    /// 结束会话：等写任务把排好队的数据（例如 Bye）发完（最多 1 秒），返回会话的结果
    ///（`result` 是会话自己的结果；会话正常结束时，读任务的错误也算进来：例如控制端的确认解不开）
    pub async fn close(self, session_id: u64, result: Result<()>) -> Result<()> {
        let HostStreams {
            out,
            video,
            audio,
            incoming,
            mut writer,
            mut reader,
            direct,
            files,
            _files_task,
            started: _,
            _registration,
        } = self;
        // 剪贴板里的文件手里也有一个 Outbox：停下给到一半的、取到一半的，放开它
        if let Some(f) = files {
            f.close();
        }
        // 直连任务手里也有一个 Outbox（发候选地址）：先停掉它，写任务才知道没有人再发了
        if let Some(mut d) = direct {
            d.abort();
            let _ = (&mut d.0).await;
        }
        drop((out, video, audio, incoming));
        reader.abort();
        let read = (&mut reader.0).await;
        if !writer.0.is_finished() {
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer.0.0).await;
        }
        let result = match result {
            Ok(()) => match read {
                Ok(Err(e)) => Err(e),
                _ => Ok(()),
            },
            Err(e) => Err(e),
        };
        match &result {
            Ok(()) => info!(session_id, "session ended"),
            Err(e) => warn!(session_id, "session failed: {e:#}"),
        }
        result
    }
}

/// 两条路上弹回来的探测最多排着这么多个没交给直连任务（每条路每 250 ms 一个）
const ECHO_QUEUE: usize = 8;

/// 读任务要的东西
struct Readers {
    session_id: u64,
    control: Merge,
    bulk: Merge,
    /// 剪贴板文件处理端。
    files: Option<Files>,
    /// 控制数据报（Ping、确认、回声）：从哪条路来、整个数据报，和它们的解密端
    datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
    /// 控制端麦克风的声音数据报（整个数据报，还是密文）和它们的解密端
    mic: (mpsc::Receiver<Bytes>, Opener),
    express: Express,
    /// 解开了控制端的确认
    confirm: oneshot::Sender<()>,
    control_tx: mpsc::Sender<Request>,
    bulk_tx: mpsc::Sender<Request>,
    /// 直连的回答交给直连任务
    answers: mpsc::Sender<DirectOffer>,
    /// 探测的回声（从哪条路回来的、探测发出的时刻）也交给它
    echoes: mpsc::Sender<(Via, u64)>,
}

/// 读控制端的控制通道（第一帧是确认）、大块通道、控制数据报和声音数据报：Ping、确认、麦克风的声音就地处理
/// （[`Express`]），直连的回答、探测的回声交给直连任务，其它的换成请求交给会话。四样各读各的：会话的主循环慢了
/// （请求的信箱满了），Pong、确认和声音照样不等
async fn read_all(r: Readers) -> Result<()> {
    let Readers {
        session_id,
        mut control,
        mut bulk,
        files,
        datagrams: (mut datagrams, mut opener),
        mic: (mut mic_rx, mut mic_opener),
        express,
        confirm,
        control_tx,
        bulk_tx,
        answers,
        echoes,
    } = r;
    // Ping 和输入都是"晚到了多久"的样本（见 InputClock）
    let clock = Mutex::new(InputClock::default());
    let dgrams = async {
        while let Some((via, d)) = datagrams.recv().await {
            let Some(m) = open_datagram::<ControllerDatagram>(&mut opener, &d) else {
                continue;
            };
            match m {
                ControllerDatagram::Ping { t } => {
                    clock.lock().unwrap().stale(t, Instant::now());
                }
                // 没有直连时没人收（也不会有回声），丢掉就是了
                ControllerDatagram::Echo { t } => {
                    let _ = echoes.try_send((via, t));
                }
                ControllerDatagram::Ack { .. }
                | ControllerDatagram::Lost { .. }
                | ControllerDatagram::Report { .. } => {}
            }
            express.datagram(m);
        }
        std::future::pending::<Result<()>>().await
    };
    // 控制端的麦克风：解开、拆出没收过的包（副本、迟到的去掉，见 crate::audio），一个一个交给平台。
    // 解不开的（被改过、重放的）整个丢掉
    let mic = async {
        let mut unbundler = Unbundler::default();
        let mut packets = VecDeque::new();
        while let Some(d) = mic_rx.recv().await {
            let Some(sealed) = d.get(datagram::HEADER..) else {
                continue;
            };
            let mut plain = sealed.to_vec();
            if mic_opener.open(&mut plain).is_err() {
                continue;
            }
            express.heard();
            unbundler.unbundle(&plain, &mut packets);
            for p in packets.drain(..) {
                express.mic(p);
            }
        }
        std::future::pending::<Result<()>>().await
    };
    // 剪贴板里的文件的消息：交给 Files；控制端的公告变成请求（平台放进本机剪贴板）。文字取代对方之前的公告
    let files_msg = |m: &ControllerMsg| -> Option<Option<Request>> {
        match m {
            ControllerMsg::Files(f) => Some(
                files
                    .as_ref()
                    .zip(f.clone())
                    .and_then(|(files, f)| files.handle(f))
                    .map(Request::ClipboardFiles),
            ),
            ControllerMsg::Clipboard { .. } => {
                if let Some(f) = &files {
                    f.remote_cleared();
                }
                None
            }
            _ => None,
        }
    };
    // 大块通道（剪贴板）：结束了不算会话结束，控制通道结束才是
    let bulk = async {
        while let Some(m) = next_msg::<ControllerMsg>(&mut bulk, SessionLane::Bulk).await? {
            express.heard();
            let r = match files_msg(&m) {
                Some(r) => r,
                None => request(m),
            };
            let Some(r) = r else { continue };
            if bulk_tx.send(r).await.is_err() {
                break;
            }
        }
        std::future::pending::<Result<()>>().await
    };
    let control = async {
        // 第一帧是控制端的确认（见 airhand_proto::handshake 的"确认"）：解得开它，这个控制端才是真的在连
        let Some(first) = control.next().await? else {
            return Ok(());
        };
        let _: Confirm = framing::decode_known(&first.data)
            .context("the controller's first frame isn't a handshake confirmation")?;
        express.heard();
        let _ = confirm.send(());
        // 这一串过时的输入已经换成了一个"释放所有按键"
        let mut released = false;
        while let Some(m) = next_msg::<ControllerMsg>(&mut control, SessionLane::Control).await? {
            express.heard();
            let m = match m {
                // 直连任务的事（没有直连时没人收，丢掉就是了）
                ControllerMsg::Direct(offer) => {
                    let _ = answers.try_send(offer);
                    continue;
                }
                ControllerMsg::Input { t, events } => {
                    let stale = clock.lock().unwrap().stale(t, Instant::now());
                    if !stale {
                        released = false;
                        ControllerMsg::Input { t, events }
                    } else if released {
                        continue;
                    } else {
                        // 不在网络恢复后重放过时的操作；丢掉的里面可能有松开按键，所以松开所有按键
                        warn!(session_id, "dropping input that arrived too late");
                        released = true;
                        ControllerMsg::Input {
                            t,
                            events: vec![InputEvent::ReleaseAll],
                        }
                    }
                }
                m => m,
            };
            let r = match files_msg(&m) {
                Some(r) => r,
                None => request(m),
            };
            let Some(r) = r else { continue };
            if control_tx.send(r).await.is_err() {
                break;
            }
        }
        Ok(())
    };
    tokio::select! {
        r = control => r,
        r = bulk => r,
        r = dgrams => r,
        r = mic => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bulk_backpressure_cannot_delay_control_or_keep_a_closed_session_alive() {
        let (control, control_rx) = mpsc::channel(1);
        let (bulk, bulk_rx) = mpsc::channel(2);
        let mut incoming = ControllerReceiver::new(
            control_rx,
            bulk_rx,
            oneshot::channel().1,
            Express::new(|_| {}),
        );
        for _ in 0..2 {
            bulk.try_send(Request::Clipboard("x".repeat(1 << 20)))
                .unwrap();
        }
        assert!(bulk.try_send(Request::Clipboard("more".into())).is_err());
        control.send(Request::Keyframe(None)).await.unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(Request::Keyframe(None))
        ));
        control
            .send(Request::Settings(QualitySettings::default()))
            .await
            .unwrap();
        assert!(matches!(incoming.recv().await, Some(Request::Settings(_))));
        drop(control);
        assert!(incoming.recv().await.is_none());
    }

    /// 控制端的消息变成什么请求：不认识的输入事件去掉，空了就不交；太大的剪贴板不交；要关键帧带着序号
    #[test]
    fn controller_messages_become_requests() {
        use airhand_proto::session::{MouseButton, QualitySettings};
        let unknown = InputEvent::MouseButton {
            button: MouseButton::Unknown(9),
            down: true,
        };
        assert_eq!(
            request(ControllerMsg::Input {
                t: 0,
                events: vec![unknown.clone(), InputEvent::ReleaseAll],
            }),
            Some(Request::Input(vec![InputEvent::ReleaseAll]))
        );
        assert_eq!(
            request(ControllerMsg::Input {
                t: 0,
                events: vec![unknown]
            }),
            None
        );
        assert_eq!(
            request(ControllerMsg::Clipboard {
                text: "x".repeat(MAX_CLIPBOARD_TEXT + 1)
            }),
            None
        );
        assert_eq!(
            request(ControllerMsg::RequestKeyframe {
                after: 7,
                stream: 2
            }),
            Some(Request::Keyframe(Some((2, 7))))
        );
        // 只看一个显示器是看哪几个显示器的简便说法；太长的不要
        assert_eq!(
            request(ControllerMsg::SelectDisplay { index: 3 }),
            Some(Request::View(vec![StreamView::display(3)]))
        );
        let many: Vec<_> = (0..9).map(StreamView::display).collect();
        assert_eq!(
            request(ControllerMsg::View {
                streams: many.clone()
            }),
            Some(Request::View(many[..MAX_STREAMS as usize].to_vec()))
        );
        assert_eq!(
            request(ControllerMsg::Mic { enabled: true }),
            Some(Request::Mic(true))
        );
        let wild = QualitySettings {
            fps: 0,
            ..QualitySettings::default()
        };
        assert_eq!(
            request(ControllerMsg::Settings(wild)),
            Some(Request::Settings(wild.sanitized()))
        );
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<(u8, u32, u64)>>);

    impl Acks for Log {
        fn ack(&self, stream: u8, seq: u32, arrived: u64) {
            self.0.lock().unwrap().push((stream, seq, arrived));
        }
    }

    /// Ping 马上回 Pong、确认马上交给平台，都不进会话的主循环；会话开始之前的确认丢掉。
    /// 任何消息都算控制端还在
    #[tokio::test(start_paused = true)]
    async fn pings_and_acks_take_the_express_lane() {
        let pongs = Arc::new(Mutex::new(Vec::new()));
        let p = pongs.clone();
        let express = Express::new(move |t| p.lock().unwrap().push(t));
        let (_tx, rx) = mpsc::channel(1);
        let incoming = ControllerReceiver::new(
            rx,
            mpsc::channel(1).1,
            oneshot::channel().1,
            express.clone(),
        );
        let ack = |seq, stream| ControllerDatagram::Ack {
            seq,
            arrived: 9,
            stream,
        };
        express.datagram(ack(1, 0));
        let log = Arc::new(Log::default());
        incoming.set_acks(log.clone());
        express.datagram(ack(2, 0));
        // 每一路各自确认；没有的路不管
        express.datagram(ack(7, 1));
        express.datagram(ack(8, MAX_STREAMS));
        assert_eq!(*log.0.lock().unwrap(), [(0, 2, 9), (1, 7, 9)]);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(incoming.silent_for(), Duration::from_secs(10));
        express.datagram(ControllerDatagram::Ping { t: 5 });
        assert_eq!(incoming.silent_for(), Duration::ZERO);
        assert_eq!(*pongs.lock().unwrap(), [5]);

        tokio::time::advance(Duration::from_secs(10)).await;
        express.heard();
        assert_eq!(incoming.silent_for(), Duration::ZERO);
    }

    /// 每一路的交付报告交给平台的码率控制。
    #[tokio::test(start_paused = true)]
    async fn reports_feed_the_rate_control() {
        use airhand_proto::session::VideoReport;
        #[derive(Default)]
        struct Fb(Mutex<Vec<(u8, VideoReport)>>);
        impl Acks for Fb {
            fn ack(&self, _: u8, _: u32, _: u64) {}
            fn feedback(&self, stream: u8, fb: VideoReport) {
                self.0.lock().unwrap().push((stream, fb));
            }
        }
        let express = Express::new(|_| {});
        let incoming = ControllerReceiver::new(
            mpsc::channel(1).1,
            mpsc::channel(1).1,
            oneshot::channel().1,
            express.clone(),
        );
        let platform = Arc::new(Fb::default());
        incoming.set_acks(platform.clone());
        express.datagram(ControllerDatagram::Report {
            report: VideoReport {
                frames: 30,
                stalled: 3,
            },
            stream: 1,
        });
        assert_eq!(
            *platform.0.lock().unwrap(),
            [(
                1,
                VideoReport {
                    frames: 30,
                    stalled: 3
                }
            )]
        );
    }

    /// 控制端等不到帧了：读任务马上交给平台；平台要恢复的话，之后的帧流放弃，确认了的帧流忘掉
    #[tokio::test]
    async fn lost_frames_abandon_their_streams_when_the_platform_recovers() {
        struct Recover(Mutex<Vec<(u8, u32)>>, bool);
        impl Acks for Recover {
            fn ack(&self, _: u8, _: u32, _: u64) {}
            fn lost(&self, stream: u8, decoded: u32) -> bool {
                self.0.lock().unwrap().push((stream, decoded));
                self.1
            }
        }
        let lost = |decoded, stream| ControllerDatagram::Lost { decoded, stream };
        let (client, _server) = crate::tests::quic_pair().await;
        for recover in [false, true] {
            // 两路画面：第 0 路的帧流不受第 1 路的影响
            let (other, frames) = (
                Arc::new(FrameStreams::default()),
                Arc::new(FrameStreams::default()),
            );
            for seq in 1..=4 {
                frames.add_for_test(seq, client.open_uni().await.unwrap());
                other.add_for_test(seq, client.open_uni().await.unwrap());
            }
            let express = Express::new(|_| {}).with_frames(vec![other.clone(), frames.clone()]);
            // 平台还没接上：什么也不做
            express.datagram(lost(1, 1));
            assert_eq!(frames.len_for_test(), 4);
            let platform = Arc::new(Recover(Mutex::default(), recover));
            let incoming = ControllerReceiver::new(
                mpsc::channel(1).1,
                mpsc::channel(1).1,
                oneshot::channel().1,
                express.clone(),
            );
            incoming.set_acks(platform.clone());
            express.datagram(ControllerDatagram::Ack {
                seq: 1,
                arrived: 0,
                stream: 1,
            });
            assert_eq!(frames.len_for_test(), 3);
            express.datagram(lost(2, 1));
            assert_eq!(*platform.0.lock().unwrap(), [(1, 2)]);
            assert_eq!(frames.len_for_test(), if recover { 1 } else { 3 });
            assert_eq!(other.len_for_test(), 4);
        }
    }

    /// 控制端麦克风的声音数据报：读任务解开、去掉副本，一包一包按先后交给平台；丢了的从下一个数据报里补上。
    /// 解不开的、重放的丢掉；平台还没设好接收时收到的也丢掉
    #[tokio::test]
    async fn mic_datagrams_reach_the_platform_once_each() {
        use crate::audio::Bundler;
        use crate::secure::test_lane;
        use airhand_proto::media::{self, AudioHeader};

        struct Sink(Arc<Mutex<Vec<u32>>>);
        impl MicSink for Sink {
            fn packet(&self, p: Vec<u8>) {
                self.0
                    .lock()
                    .unwrap()
                    .push(AudioHeader::decode(&p).unwrap().seq);
            }
        }

        // 控制通道一直没有流：读任务只处理数据报
        let (_control_streams, frames) = mpsc::unbounded_channel();
        let control = Merge::new(SessionLane::Control, test_lane(true).1, frames);
        let (_bulk_streams, frames) = mpsc::unbounded_channel();
        let bulk = Merge::new(SessionLane::Bulk, test_lane(true).1, frames);
        let (mic_tx, mic_rx) = mpsc::channel(16);
        let (_dgram_tx, dgram_rx) = mpsc::channel(1);
        let express = Express::new(|_| {});
        let reader = tokio::spawn(read_all(Readers {
            session_id: 1,
            control,
            bulk,
            files: None,
            datagrams: (dgram_rx, test_lane(false).1),
            mic: (mic_rx, test_lane(false).1),
            express: express.clone(),
            confirm: oneshot::channel().0,
            control_tx: mpsc::channel(1).0,
            bulk_tx: mpsc::channel(1).0,
            answers: mpsc::channel(1).0,
            echoes: mpsc::channel(1).0,
        }));
        let (mut sealer, _) = test_lane(false);
        let mut bundler = Bundler::default();
        let mut sealed = |seq: u32| {
            let h = AudioHeader {
                seq,
                start: seq == 1,
                capture_us: seq as u64 * 20_000,
            };
            let plain = bundler
                .bundle(Bytes::from(media::frame(&h.encode(), &[&[seq as u8; 50]])))
                .unwrap();
            Bytes::from(sealer.seal_datagram(1, datagram::AUDIO, &plain).unwrap())
        };
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        };

        // 平台还没接上：丢掉
        mic_tx.send(sealed(1)).await.unwrap();
        settle().await;
        let got = Arc::new(Mutex::new(Vec::new()));
        let receiver = ControllerReceiver::new(
            mpsc::channel(1).1,
            mpsc::channel(1).1,
            oneshot::channel().1,
            express.clone(),
        );
        receiver.set_mic(Arc::new(Sink(got.clone())));
        // 第 3 个数据报丢了；一个伪造的、一个重放的
        let (d2, _d3, d4) = (sealed(2), sealed(3), sealed(4));
        for d in [d2, Bytes::from(vec![0u8; 60]), d4.clone(), d4] {
            mic_tx.send(d).await.unwrap();
        }
        settle().await;
        assert_eq!(*got.lock().unwrap(), [2, 3, 4]);
        reader.abort();
    }

    /// 读任务：控制通道上的第一帧必须是确认，解得开才告诉会话；之后的消息照常
    #[tokio::test]
    async fn the_first_control_frame_must_be_the_confirmation() {
        use crate::secure::test_lane;
        for genuine in [true, false] {
            let (mut sealer, opener) = test_lane(true);
            let (frames_tx, frames) = mpsc::unbounded_channel();
            let control = Merge::new(SessionLane::Control, opener, frames);
            let (_bulk_frames, bulk_frames) = mpsc::unbounded_channel();
            let bulk = Merge::new(SessionLane::Bulk, test_lane(true).1, bulk_frames);
            // 控制端的控制通道：一条真的单向流（Merge 只认 QUIC 的流），用进程内的两个 QUIC 端点
            let (client, server) = crate::tests::quic_pair().await;
            let mut send = client.open_uni().await.unwrap();
            let first = if genuine {
                let mut f = framing::encode(&Confirm::default());
                sealer.seal_frame(&mut f).unwrap();
                f
            } else {
                // 重放握手消息的一方没有会话密钥：拿不出确认
                framing::frame(&[0u8; 40])
            };
            send.write_all(&first).await.unwrap();
            let mut next = framing::encode(&ControllerMsg::SelectDisplay { index: 3 });
            sealer.seal_frame(&mut next).unwrap();
            send.write_all(&next).await.unwrap();
            // 对方写了东西，这边才接得到这条流
            frames_tx
                .send(crate::lanes::LaneStream {
                    recv: server.accept_uni().await.unwrap(),
                    via: Via::Relay,
                })
                .ok();

            let (control_tx, mut control_rx) = mpsc::channel(4);
            let (confirm, mut confirmed) = oneshot::channel();
            let (_dgram_tx, dgram_rx) = mpsc::channel(1);
            let (_mic_tx, mic_rx) = mpsc::channel(1);
            let reader = tokio::spawn(read_all(Readers {
                session_id: 1,
                control,
                bulk,
                files: None,
                datagrams: (dgram_rx, test_lane(false).1),
                mic: (mic_rx, test_lane(false).1),
                express: Express::new(|_| {}),
                confirm,
                control_tx,
                bulk_tx: mpsc::channel(1).0,
                answers: mpsc::channel(1).0,
                echoes: mpsc::channel(1).0,
            }));
            if genuine {
                (&mut confirmed).await.unwrap();
                assert_eq!(
                    control_rx.recv().await,
                    Some(Request::View(vec![StreamView::display(3)]))
                );
                reader.abort();
            } else {
                assert!(reader.await.unwrap().is_err());
                assert!(confirmed.await.is_err());
                assert!(control_rx.recv().await.is_none());
            }
        }
    }
}
