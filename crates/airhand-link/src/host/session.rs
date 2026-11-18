//! 被控端的一个会话：协议层的事都在这里（[`serve`]），各平台只实现 [`Platform`]（采集、编码、注入输入）。
//!
//! 桌面版和 Android 以前各写一份会话循环，Pong、失联检测、踢人时发 Bye、画质设置各有一份，
//! 慢慢就会不一样。现在它们只在这里：
//!
//! ```text
//! 端到端握手（配对过的、输对了配对码的）→ start：Hello、画面…… ─ 控制端确认了 → confirmed：显示它
//! ```
//!
//! * 会话不等控制端的确认就开始（省一个往返，编码器的初始化也和它重叠），确认了才显示它
//!   （[`Platform::confirmed`]）；[`CONFIRM_TIMEOUT`] 等不到确认就结束（见 `airhand_proto::handshake` 的"确认"）；
//! * 一直回应 Ping，[`PEER_TIMEOUT`] 收不到控制端的任何消息算失联。Ping 和画面确认（控制数据报）在读任务里
//!   就处理了（确认交给 [`Platform::acks`]），不等这里：这里可能正等着平台重建编码管线（见 [`super::HostStreams`]）；
//! * 本机用户断开所有控制端（[`Incoming::kick`]）时发 [`HostMsg::Bye`]，然后结束；
//! * 控制端的请求在会话的读任务里就换成了 [`Request`]（见 [`super::HostStreams`]）交给平台：画质设置做过范围校验，
//!   过时的输入已经换成了"释放所有按键"，对方版本新增的输入事件已经去掉了；
//! * 控制端麦克风的声音包和画面确认一样在读任务里就交给平台（[`Platform::mic`]），放不放进虚拟麦克风由平台决定
//!   （控制端打开了麦克风、[`Request::Mic`]，才放；会话开始之前的请求不会到平台）。
//!
//! 协议加了新的控制端消息，读任务里的 `match` 编译不过：在那里决定它是什么 [`Request`]（或者协议层自己处理），
//! 各平台跟着实现，不会一边做了、一边忘了。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::session::{ByeReason, HostMsg, InputEvent, QualitySettings, StreamView};
use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use super::queue::Outbox;
use super::streams::{Admitted, ControllerReceiver, HostStreams, Writer};

/// 画面确认交给谁（[`Platform::acks`]）：桌面版是每一路画面的 [`crate::flow::Flow`]（[`Flows`]），Android 是画面分发。
/// 在读任务里调用：要快，不能等什么。`stream` 是第几路画面（见 `airhand_proto::session` 的"多屏"），
/// 控制端给的，没检查过（不小于 `airhand_proto::MAX_STREAMS` 的已经去掉了）
pub trait Acks: Send + Sync + 'static {
    /// 控制端收到了第 `stream` 路的第 `seq` 帧（累积确认）；`arrived` 见 `ControllerDatagram::Ack`
    fn ack(&self, stream: u8, seq: u32, arrived: u64);

    /// 画面换了一条路（中转 ⇄ 直连，见 [`crate::direct`]）：时延的基准要从头量（每一路都是）。
    /// `lost`：旧路（直连）断了，在途的帧不会确认了，不要再等它们（会话随即要关键帧）
    fn path_changed(&self, lost: bool) {
        let _ = lost;
    }

    /// 控制端等不到第 `stream` 路的帧了（`ControllerDatagram::Lost`，见 `airhand_proto::session` 的"帧流"）：
    /// 它交给解码器的最后一帧是 `decoded`。返回 `true`：这一路这之后在途的帧作废了（下一帧从控制端有的帧恢复），
    /// 它们的帧流放弃掉；`false`：什么也不做（恢复帧已经在路上了；恢复只能编关键帧、等不到的帧还会重传、
    /// 这是同一处的第一次报告，见 `crate::refs` 的"只能编关键帧时"；默认：这个平台不恢复，控制端等久了会要关键帧）。
    fn lost(&self, stream: u8, decoded: u32) -> bool {
        let _ = (stream, decoded);
        false
    }

    /// 控制端定时上报每路收完和停顿的帧数。
    fn feedback(&self, stream: u8, fb: airhand_proto::session::VideoReport) {
        let _ = (stream, fb);
    }
}

/// 每一路画面一个在途控制和码率（[`crate::flow::Flow`]，桌面版的 [`Acks`]）：一共 `airhand_proto::MAX_STREAMS` 个，
/// 会话一开始就建好（确认在读任务里就交给它们）。一路换了显示器（重建编码管线）时接着用它的码率估计
pub struct Flows(Vec<Arc<crate::flow::Flow>>);

impl Default for Flows {
    fn default() -> Self {
        Self(
            (0..airhand_proto::MAX_STREAMS)
                .map(|_| Arc::default())
                .collect(),
        )
    }
}

impl Flows {
    /// 第 `stream` 路的（不小于 `MAX_STREAMS` 的没有）
    pub fn get(&self, stream: u8) -> Option<&Arc<crate::flow::Flow>> {
        self.0.get(stream as usize)
    }

    /// 每一路的
    pub fn iter(&self) -> impl Iterator<Item = &Arc<crate::flow::Flow>> {
        self.0.iter()
    }
}

impl Acks for Flows {
    fn ack(&self, stream: u8, seq: u32, arrived: u64) {
        if let Some(f) = self.get(stream) {
            f.ack(seq, Some(arrived));
        }
    }

    fn path_changed(&self, lost: bool) {
        for f in self.iter() {
            f.path_changed(lost);
        }
    }

    fn lost(&self, stream: u8, decoded: u32) -> bool {
        self.get(stream).is_some_and(|f| f.lost(decoded))
    }

    fn feedback(&self, stream: u8, fb: airhand_proto::session::VideoReport) {
        if let Some(f) = self.get(stream) {
            f.feedback(fb);
        }
    }
}

/// 控制端麦克风的声音交给谁（[`Platform::mic`]，见 `airhand_proto::session` 的"麦克风"）。在读任务里调用：要快，不能等什么
pub trait MicSink: Send + Sync + 'static {
    /// 一个声音包（`airhand_proto::media` 格式：头 + Opus 包）。数据报里的副本已经去掉了、丢了的补上了，
    /// 按采集先后一个一个来（见 `crate::audio`）
    fn packet(&self, packet: Vec<u8>);
}

use super::{Admission, Gate, Incoming};

/// 控制端每秒都会发 Ping，这么久没有任何消息说明对方已失联
pub const PEER_TIMEOUT: Duration = Duration::from_secs(30);
/// 这么久等不到控制端的确认（见 `airhand_proto::handshake` 的"确认"）就结束会话
pub const CONFIRM_TIMEOUT: Duration = crate::secure::STEP_TIMEOUT;
/// 多久检查一次是不是失联了
const WATCHDOG: Duration = Duration::from_secs(5);

/// 控制端要被控端做的事（会话开始之后才有，见模块说明）
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// 一批输入（不会是空的）。过时的已经换成了 [`InputEvent::ReleaseAll`]，不认识的事件已经去掉了
    Input(Vec<InputEvent>),
    /// 画质设置变了（做过范围校验）
    Settings(QualitySettings),
    /// 要关键帧。控制端要的（例如解码器重置了）是 `Some((第几路, 它收到的这一路的最后一帧的序号))`：在那之后
    /// 发过关键帧就不用再编了（见 `airhand_proto::session` 的"关键帧"；第几路没检查过）；`None`：被控端自己要的
    /// （直连断了，断在上面的帧接收端等不到了），每一路都要编
    Keyframe(Option<(u8, u32)>),
    /// 看哪几个显示器：第 `i` 项是第 `i` 路画面（见 `airhand_proto::session` 的"多屏"）。显示器的序号、有几项都没检查过
    /// （[`resolve_view`]）。只看一个显示器（`ControllerMsg::SelectDisplay`）也是它
    View(Vec<StreamView>),
    /// 控制端复制了文本（不会超过 [`airhand_proto::session::MAX_CLIPBOARD_TEXT`]）
    Clipboard(String),
    /// 控制端复制了文件（见 [`crate::files`]）：放进本机剪贴板，本机有程序粘贴时才去取内容。
    /// 放好了（放不了也要）调用 [`crate::files::RemoteOffer::ready`]
    ClipboardFiles(crate::files::RemoteOffer),
    /// 打开 / 关闭声音
    Audio(bool),
    /// 打开 / 关闭控制端的麦克风（见 [`MicSink`]）
    Mic(bool),
}

/// 控制端要看的显示器（[`Request::View`]）落到这台被控端上（`displays` 个显示器，最多同时发 `max` 路）：
/// 去掉不存在的、重复的，多出来的不要；一个都不剩时看主显示器（第 0 个）。没有显示器时是空的
pub fn resolve_view(view: &[StreamView], displays: usize, max: u8) -> Vec<StreamView> {
    let mut out: Vec<StreamView> = Vec::with_capacity(view.len());
    let max = max.clamp(1, airhand_proto::MAX_STREAMS) as usize;
    for v in view {
        if out.len() == max {
            break;
        }
        if (v.display as usize) < displays && !out.iter().any(|o| o.display == v.display) {
            out.push(*v);
        }
    }
    if out.is_empty() && displays > 0 {
        out.push(StreamView::display(0));
    }
    out
}

/// 会话怎么开始：控制端在握手里给的画质、要不要声音、看哪几个显示器
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    /// 做过范围校验
    pub settings: QualitySettings,
    pub audio: bool,
    /// 看哪几个显示器（和 [`Request::View`] 一样没检查过；空的是只看主显示器）
    pub view: Vec<StreamView>,
}

/// 交给平台的发送端（[`serve`] 的 `platform` 拿到它们）
pub struct Senders {
    /// 控制消息：Hello、提示、显示器列表、光标、剪贴板……（按消息挑通道，见 [`super::queue`]）
    pub out: Outbox,
    /// 画面包：第 `i` 项是第 `i` 路（一共 `airhand_proto::MAX_STREAMS` 路，见 `airhand_proto::session` 的"多屏"）
    pub video: Vec<mpsc::Sender<Bytes>>,
    /// 声音包
    pub audio: mpsc::Sender<Bytes>,
    /// 剪贴板文件处理端（见 [`crate::files`]）。
    pub files: Option<crate::files::Files>,
}

/// 一个会话里各平台不同的事。方法都在会话的任务里依次调用，不会同时进来两个。
///
/// 会话随时可能在某个 `await` 处被取消（与服务器的连接断了、更换控制码……）：一定要做的清理放在 `Drop` 里，
/// [`Platform::stop`] 只做正常结束时要等的事（例如等编码线程退出，写任务才能把 Bye 发完）
pub trait Platform: Send {
    /// 平台自己的事（定时检查、别处来的广播……），见 [`Platform::next_event`]
    type Event: Send;

    /// 控制端确认了握手（见 `airhand_proto::handshake` 的"确认"）：是真的在连，这时才在托盘、列表里显示它、
    /// 提醒本机用户。会话这时已经开始了（[`Platform::start`] 在确认之前：重放的握手消息看不到画面，
    /// 只是让本机白编码一会儿）
    fn confirmed(&mut self) {}

    /// 画面确认交给谁。会话一开始取一次；确认在读任务里就交给它，不经过 [`Platform::request`]
    fn acks(&self) -> Arc<dyn Acks>;

    /// 控制端麦克风的声音交给谁。会话一开始取一次，和画面确认一样在读任务里就交给它。
    /// 默认没有（收到的丢掉）：这个平台不接麦克风
    fn mic(&self) -> Option<Arc<dyn MicSink>> {
        None
    }

    /// 会话开始：发 [`HostMsg::Hello`] 和开始时的提示，开始采集
    fn start(&mut self, start: Start) -> impl Future<Output = Result<()>> + Send;

    /// 执行控制端的请求。返回错误时会话结束
    fn request(&mut self, req: Request) -> impl Future<Output = Result<()>> + Send;

    /// 等平台自己的下一件事（会话开始之后才等）。会和控制端的消息一起放在 `select!` 里：
    /// 必须能随时取消、不丢东西（等定时器、等通道可以；真正做事放在 [`Platform::event`] 里）
    fn next_event(&mut self) -> impl Future<Output = Self::Event> + Send;

    /// 处理 [`Platform::next_event`] 等到的事。返回错误时会话结束
    fn event(&mut self, event: Self::Event) -> impl Future<Output = Result<()>> + Send;

    /// 视频确认超过期限，控制心跳仍正常也应结束会话，避免永久冻结画面。
    fn video_stalled(&self) -> bool {
        false
    }

    /// 会话正常结束：停掉还拿着发送端的东西（编码线程、声音订阅），写任务才能把排着的（例如 Bye）发完
    fn stop(self) -> impl Future<Output = ()> + Send
    where
        Self: Sized;
}

/// 一个会话，从端到端握手到结束。
///
/// * `gate`：让不让这个控制端进来（拒绝时返回错误，还没有调用 `platform`）；
/// * `video_queue` / `audio_queue`：画面、声音的发送队列有多长（见 [`HostStreams::open`]）；
/// * `platform`：握手通过之后建起这个会话的平台部分（控制端还没确认：确认了才调用 [`Platform::confirmed`]）。
pub async fn serve<P, F>(
    inc: Incoming,
    gate: &dyn Gate,
    video_queue: usize,
    audio_queue: usize,
    platform: F,
) -> Result<()>
where
    P: Platform,
    F: FnOnce(&Admitted, Senders) -> P,
{
    let session_id = inc.session_id;
    let mut kick = inc.kick.clone();
    let (mut io, admitted) = match HostStreams::open(&inc, video_queue, audio_queue, gate).await {
        Ok(r) => r,
        Err(e) => {
            warn!(session_id, "controller not admitted: {e:#}");
            return Err(e);
        }
    };
    if admitted.admission == Admission::Known {
        info!(session_id, "a known controller connected");
    }
    let out = io.out.clone();
    let started = io.started();
    let mut platform = platform(
        &admitted,
        Senders {
            out: io.out.clone(),
            video: io.video.clone(),
            audio: io.audio.clone(),
            files: io.files.clone(),
        },
    );
    let result = drive(
        session_id,
        &admitted,
        &mut platform,
        &out,
        &mut io.incoming,
        &mut kick,
        &mut io.writer,
        &|| started.notify_one(),
    )
    .await;
    // 先停掉还拿着发送端的东西，写任务才能把排着的（例如 Bye）发完
    platform.stop().await;
    drop(out);
    io.close(session_id, result).await
}

/// 会话的主循环（见模块说明）。正常结束（控制端断开、发了 Bye）时为 `Ok`。
/// `started`：会话开始了（发了 Hello）之后调用：这时才开始建直连（见 [`crate::direct`]）
#[allow(clippy::too_many_arguments)]
async fn drive<P: Platform>(
    session_id: u64,
    admitted: &Admitted,
    platform: &mut P,
    out: &Outbox,
    incoming: &mut ControllerReceiver,
    kick: &mut watch::Receiver<ByeReason>,
    writer: &mut Writer,
    started: &(dyn Fn() + Sync),
) -> Result<()> {
    // 本机用户可能在端到端握手期间已经点了断开，不能先开始采集再处理这个通知。
    if kick.has_changed().unwrap_or(false) {
        out.send(&HostMsg::Bye(*kick.borrow_and_update()))?;
        return Ok(());
    }
    incoming.set_acks(platform.acks());
    if let Some(mic) = platform.mic() {
        incoming.set_mic(mic);
    }
    // 不等确认就开始（见模块说明）
    platform
        .start(Start {
            settings: admitted.settings,
            audio: admitted.audio,
            view: admitted.view.clone(),
        })
        .await?;
    info!(session_id, "session started");
    started();
    let mut confirm = incoming.confirmation();
    let mut confirmed = false;
    let confirm_by = Instant::now() + CONFIRM_TIMEOUT;
    let mut watchdog = tokio::time::interval(WATCHDOG);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 被控端服务还在（踢人的发送端还在）
    let mut kickable = true;

    loop {
        if kick.has_changed().unwrap_or(false) {
            out.send(&HostMsg::Bye(*kick.borrow_and_update()))?;
            return Ok(());
        }
        tokio::select! {
            c = &mut confirm, if !confirmed => {
                // 读任务没等到确认就结束了：会话的结果是读任务的错误（见 HostStreams::close）
                if c.is_err() {
                    return Ok(());
                }
                confirmed = true;
                info!(session_id, "the controller confirmed the handshake");
                platform.confirmed();
            }
            () = tokio::time::sleep_until(confirm_by), if !confirmed => {
                bail!("the controller didn't confirm the handshake");
            }
            req = incoming.recv() => {
                let Some(req) = req else { return Ok(()) };
                platform.request(req).await?;
            }
            changed = kick.changed(), if kickable => {
                // 发送端不在了：被控端服务正在结束，会话随之被取消
                if changed.is_err() {
                    kickable = false;
                    continue;
                }
                let reason = *kick.borrow_and_update();
                info!(session_id, ?reason, "the local user disconnected the controller");
                out.send(&HostMsg::Bye(reason))?;
                return Ok(());
            }
            event = platform.next_event() => platform.event(event).await?,
            e = writer.failed() => return Err(e),
            _ = watchdog.tick() => {
                if platform.video_stalled() {
                    bail!("the controller stopped acknowledging video");
                }
                if incoming.silent_for() > PEER_TIMEOUT {
                    bail!("the controller stopped responding");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::queue::{self, LaneReceiver};
    use crate::host::streams::{Express, request};
    use crate::tasks::OwnedTask;
    use airhand_proto::framing;
    use airhand_proto::session::{
        Codec, ControllerDatagram, ControllerMsg, MAX_CLIPBOARD_TEXT, MouseButton,
    };
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;

    /// 平台那边发生了什么
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Confirmed,
        Start(Start),
        Request(Request),
        Event(u32),
        Ack(u32, u64),
    }

    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<Call>>>);

    impl Log {
        fn push(&self, c: Call) {
            self.0.lock().unwrap().push(c);
        }
        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut self.0.lock().unwrap())
        }
    }

    struct Fake {
        log: Log,
        events: mpsc::Receiver<u32>,
    }

    impl Acks for Log {
        fn ack(&self, stream: u8, seq: u32, arrived: u64) {
            assert_eq!(stream, 0);
            self.push(Call::Ack(seq, arrived));
        }
    }

    impl Platform for Fake {
        type Event = Option<u32>;
        fn acks(&self) -> Arc<dyn Acks> {
            Arc::new(self.log.clone())
        }
        fn confirmed(&mut self) {
            self.log.push(Call::Confirmed);
        }
        async fn start(&mut self, start: Start) -> Result<()> {
            self.log.push(Call::Start(start));
            Ok(())
        }
        async fn request(&mut self, req: Request) -> Result<()> {
            self.log.push(Call::Request(req));
            Ok(())
        }
        async fn next_event(&mut self) -> Option<u32> {
            self.events.recv().await
        }
        async fn event(&mut self, e: Option<u32>) -> Result<()> {
            match e {
                Some(e) => self.log.push(Call::Event(e)),
                None => std::future::pending().await,
            }
            Ok(())
        }
        async fn stop(self) {}
    }

    /// 一个跑着的会话：往里发控制端的消息、踢人，看平台和控制端收到了什么
    struct Harness {
        log: Log,
        to_host: mpsc::Sender<Request>,
        /// 读任务自己处理的事（Ping、确认）在这里处理，和真的读任务一样
        express: Express,
        /// 回了哪些 Pong
        pongs: Arc<Mutex<Vec<u64>>>,
        /// 控制端的确认（`None`：已经确认了）
        confirm: Option<oneshot::Sender<()>>,
        events: mpsc::Sender<u32>,
        kick: watch::Sender<ByeReason>,
        control: LaneReceiver,
        task: tokio::task::JoinHandle<Result<()>>,
    }

    fn admitted(admission: Admission) -> Admitted {
        Admitted {
            key: [1; 32],
            name: "ctl".into(),
            os: "test".into(),
            safety: "000-000".into(),
            admission,
            settings: QualitySettings::default(),
            audio: true,
            view: Vec::new(),
        }
    }

    /// 控制端一开始就确认了的会话（真的控制端收到决定马上确认）
    fn session(admission: Admission) -> Harness {
        let mut h = unconfirmed(admission);
        h.confirm();
        h
    }

    /// 还没确认的会话（[`Harness::confirm`]）
    fn unconfirmed(admission: Admission) -> Harness {
        let log = Log::default();
        let (to_host, control_in) = mpsc::channel(16);
        let (events_tx, events) = mpsc::channel(16);
        let (kick, mut kick_rx) = watch::channel(ByeReason::Kicked);
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let (out, control, _bulk) = queue::channel();
        let pongs = Arc::new(Mutex::new(Vec::new()));
        let p = pongs.clone();
        let express = Express::new(move |t| p.lock().unwrap().push(t));
        let mut incoming =
            ControllerReceiver::new(control_in, mpsc::channel(1).1, confirm_rx, express.clone());
        let mut platform = Fake {
            log: log.clone(),
            events,
        };
        let task = tokio::spawn(async move {
            let _bulk = _bulk;
            let mut writer = Writer(OwnedTask(tokio::spawn(std::future::pending())));
            drive(
                7,
                &admitted(admission),
                &mut platform,
                &out,
                &mut incoming,
                &mut kick_rx,
                &mut writer,
                &|| {},
            )
            .await
        });
        Harness {
            log,
            to_host,
            express,
            pongs,
            confirm: Some(confirm_tx),
            events: events_tx,
            kick,
            control,
            task,
        }
    }

    impl Harness {
        /// 控制端的确认到了（读任务解开了控制通道上的第一帧）
        fn confirm(&mut self) {
            let _ = self.confirm.take().unwrap().send(());
        }

        /// 和真的读任务一样：控制通道上的消息换成请求交给会话
        async fn send(&self, m: ControllerMsg) {
            self.express.heard();
            if let Some(r) = request(m) {
                self.to_host.send(r).await.unwrap();
            }
        }

        /// 控制数据报（Ping、确认）：读任务就地处理
        fn datagram(&self, m: ControllerDatagram) {
            self.express.datagram(m);
        }

        /// 让会话把手里的消息处理完
        async fn settle(&self) {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        }

        /// 控制端收到的
        async fn received(&mut self) -> Vec<HostMsg> {
            self.settle().await;
            std::iter::from_fn(|| self.control.try_recv())
                .map(|b| framing::decode_known(&b[framing::PREFIX..]).unwrap())
                .collect()
        }
    }

    /// 从记录里拿掉 `c`（和别的谁先谁后不一定的），没有就失败
    fn remove(log: &mut Vec<Call>, c: Call) {
        let i = log
            .iter()
            .position(|x| *x == c)
            .unwrap_or_else(|| panic!("{c:?} not in {log:?}"));
        log.remove(i);
    }

    fn click() -> Vec<InputEvent> {
        vec![InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        }]
    }

    fn fast() -> QualitySettings {
        QualitySettings {
            fps: 250,
            codec: Codec::AV1,
            ..QualitySettings::default()
        }
    }

    #[tokio::test]
    async fn a_kick_during_the_handshake_prevents_start() {
        // current-thread runtime: send before yielding, so drive first sees the pending kick.
        for admission in [Admission::Known, Admission::Paired] {
            let mut h = session(admission);
            h.kick.send_replace(ByeReason::Kicked);
            assert!(matches!(
                h.received().await[..],
                [HostMsg::Bye(ByeReason::Kicked)]
            ));
            assert!(h.log.take().is_empty());
            assert!(h.task.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn requests_reach_the_platform_and_pings_are_answered() {
        let mut h = session(Admission::Paired);
        // 会话开始了才把确认交给平台
        h.settle().await;
        h.datagram(ControllerDatagram::Ping { t: 5 });
        h.send(ControllerMsg::Input {
            t: 0,
            events: click(),
        })
        .await;
        // 空的一批输入不交给平台
        h.send(ControllerMsg::Input {
            t: 0,
            events: vec![],
        })
        .await;
        h.send(ControllerMsg::Settings(fast())).await;
        h.datagram(ControllerDatagram::Ack {
            seq: 3,
            arrived: 9,
            stream: 0,
        });
        h.send(ControllerMsg::Clipboard {
            text: "x".repeat(MAX_CLIPBOARD_TEXT + 1),
        })
        .await;
        h.send(ControllerMsg::Clipboard { text: "hi".into() }).await;
        h.events.send(9).await.unwrap();
        assert!(h.received().await.is_empty());
        assert_eq!(*h.pongs.lock().unwrap(), [5]);
        let mut log = h.log.take();
        // 确认、平台自己的事、读任务直接交给平台的画面确认，和控制端的其它消息谁先谁后不一定
        for c in [Call::Confirmed, Call::Event(9), Call::Ack(3, 9)] {
            remove(&mut log, c);
        }
        assert_eq!(
            log,
            [
                Call::Start(Start {
                    settings: QualitySettings::default(),
                    audio: true,
                    view: Vec::new(),
                }),
                Call::Request(Request::Input(click())),
                Call::Request(Request::Settings(fast().sanitized())),
                Call::Request(Request::Clipboard("hi".into())),
            ]
        );
        // 控制端断开：正常结束
        drop(h.to_host);
        assert!((&mut h.task).await.unwrap().is_ok());
    }

    /// 会话不等确认就开始，确认了才告诉平台（显示控制端、提醒本机用户）
    #[tokio::test]
    async fn the_session_starts_before_the_confirmation() {
        let mut h = unconfirmed(Admission::Known);
        h.settle().await;
        assert!(matches!(h.log.take()[..], [Call::Start(_)]));
        h.confirm();
        h.settle().await;
        assert_eq!(h.log.take(), [Call::Confirmed]);
    }

    /// 等不到确认（重放的握手消息）：会话结束，不显示；读任务没等到确认就结束了也一样
    #[tokio::test(start_paused = true)]
    async fn no_confirmation_ends_the_session() {
        let h = unconfirmed(Admission::Known);
        let err = h.task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("confirm"), "{err}");
        assert!(!h.log.take().contains(&Call::Confirmed));

        let mut h = unconfirmed(Admission::Known);
        h.settle().await;
        drop(h.confirm.take());
        assert!((&mut h.task).await.unwrap().is_ok());
        assert!(!h.log.take().contains(&Call::Confirmed));
    }

    #[tokio::test]
    async fn kicked() {
        for admission in [Admission::Known, Admission::Paired] {
            let mut h = session(admission);
            h.settle().await;
            h.kick.send_replace(ByeReason::CodeChanged);
            assert!((&mut h.task).await.unwrap().is_ok());
            let got = h.received().await;
            assert!(
                matches!(got.last(), Some(HostMsg::Bye(ByeReason::CodeChanged))),
                "{got:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_controller_times_out() {
        let h = session(Admission::Known);
        let err = h.task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("stopped responding"), "{err}");
    }

    /// 控制端要看的显示器：不存在的、重复的去掉，最多能发几路就几路；一个都不剩（或者没说）就看主显示器
    #[test]
    fn views_resolve_against_the_displays() {
        let v = |ds: &[u8]| {
            ds.iter()
                .copied()
                .map(StreamView::display)
                .collect::<Vec<_>>()
        };
        assert_eq!(resolve_view(&v(&[2, 0]), 3, 4), v(&[2, 0]));
        assert_eq!(resolve_view(&v(&[2, 7, 2, 1]), 3, 4), v(&[2, 1]));
        assert_eq!(resolve_view(&v(&[0, 1, 2]), 3, 2), v(&[0, 1]));
        // 只能发一路（0 也当作 1）
        assert_eq!(resolve_view(&v(&[1, 0]), 3, 0), v(&[1]));
        assert_eq!(resolve_view(&[], 3, 4), v(&[0]));
        assert_eq!(resolve_view(&v(&[9]), 3, 4), v(&[0]));
        assert_eq!(resolve_view(&v(&[0]), 0, 4), v(&[]));
        // 每一路的大小跟着它走
        let sized = [StreamView {
            display: 1,
            max_short: 720,
        }];
        assert_eq!(resolve_view(&sized, 2, 4), sized);
    }

    /// 桌面版的确认按路交给那一路的码率控制；没有的路不管
    #[test]
    fn flows_route_acks_by_stream() {
        let flows = Flows::default();
        let seq = flows.get(1).unwrap().sent(1000);
        assert_eq!(flows.get(1).unwrap().inflight_for_test(), 1);
        flows.ack(0, seq, 5);
        assert_eq!(flows.get(1).unwrap().inflight_for_test(), 1);
        flows.ack(1, seq, 5);
        assert_eq!(flows.get(1).unwrap().inflight_for_test(), 0);
        flows.ack(airhand_proto::MAX_STREAMS, seq, 5);
        assert!(flows.get(airhand_proto::MAX_STREAMS).is_none());
        assert!(!flows.lost(airhand_proto::MAX_STREAMS, 0));
    }
}
