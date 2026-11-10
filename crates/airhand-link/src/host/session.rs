//! 被控端的一个会话：协议层的事都在这里（[`serve`]），各平台只实现 [`Platform`]（采集、编码、注入输入、请本机用户同意）。
//!
//! 桌面版和 Android 以前各写一份会话循环，Pong、失联检测、踢人时发 Bye、画质设置、同意流程各有一份，
//! 慢慢就会不一样。现在它们只在这里：
//!
//! ```text
//! 端到端握手 ─┬─ 不用同意（配对过的、同意过的、谁都能连）──────────────────────────┐
//!            └─ NeedsApproval → AwaitingApproval，问本机用户（最多 60 秒）─┼─ 同意 → start：Hello、画面……
//!                                                                    └─ 拒绝 / 没人理 → Bye
//! ```
//!
//! * 同时最多有 [`MAX_PENDING_APPROVALS`] 个请求在等本机用户回答，再来的不问、直接回绝（[`ApprovalSlots`]）；
//! * 同意之前只回应 Ping、记下画质和声音设置（同意之后随 [`Start`] 一起生效），不发画面、不执行任何输入；
//! * 一直回应 Ping（[`HostMsg::Pong`]），[`PEER_TIMEOUT`] 收不到控制端的任何消息算失联；
//! * 本机用户断开所有控制端（[`Incoming::kick`]）时发 [`HostMsg::Bye`]，然后结束；
//! * 控制端的请求换成 [`Request`] 交给平台：画质设置做过范围校验，过时的输入已经换成了"释放所有按键"
//!   （见 [`super::HostStreams`]），对方版本新增的输入事件已经去掉了。
//!
//! 协议加了新的控制端消息，这里的 `match` 编译不过：在这里决定它是什么 [`Request`]（或者协议层自己处理），
//! 各平台跟着实现，不会一边做了、一边忘了。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::session::{
    ByeReason, ControllerMsg, HostMsg, InputEvent, MAX_CLIPBOARD_TEXT, QualitySettings,
};
use anyhow::{Result, bail};
use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};
use tokio::time::Instant;
use tracing::{info, warn};

use super::queue::Outbox;
use super::streams::{Admitted, ControllerReceiver, HostStreams, Writer};
use super::{Admission, Gate, Incoming};

/// 控制端每秒都会发 Ping，这么久没有任何消息说明对方已失联
pub const PEER_TIMEOUT: Duration = Duration::from_secs(30);
/// 本机用户这么久没有回应连接请求就当作没人理会（[`ByeReason::Unanswered`]）
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// 多久检查一次是不是失联了
const WATCHDOG: Duration = Duration::from_secs(5);
/// 同时最多有这么多个连接请求在等本机用户回答，再来的不问、直接回绝（[`ByeReason::TooManyRequests`]）。
///
/// 知道控制码的人可以反复发起连接：没有这个上限的话，每个请求都弹出一次"是否允许"、各占着服务器上这台被控端的
/// 一个会话槽位直到 [`APPROVAL_TIMEOUT`]。回绝的会话马上结束，槽位随之归还。
/// 配对过的、同意过的控制端不用问，不受影响
pub const MAX_PENDING_APPROVALS: usize = 2;

/// 在等本机用户回答的连接请求的名额（[`MAX_PENDING_APPROVALS`]）：一个被控端服务的所有会话共用
/// （[`Incoming::approvals`]）
#[derive(Clone)]
pub struct ApprovalSlots(Arc<Semaphore>);

impl Default for ApprovalSlots {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(MAX_PENDING_APPROVALS)))
    }
}

impl ApprovalSlots {
    /// 占一个名额（丢掉时归还）；满了为 `None`
    fn take(&self) -> Option<OwnedSemaphorePermit> {
        self.0.clone().try_acquire_owned().ok()
    }
}

/// 控制端要被控端做的事（会话开始之后才有，见模块说明）
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// 一批输入（不会是空的）。过时的已经换成了 [`InputEvent::ReleaseAll`]，不认识的事件已经去掉了
    Input(Vec<InputEvent>),
    /// 控制端收到了这一帧（累积确认，在途帧控制用）
    Ack(u32),
    /// 画质设置变了（做过范围校验）
    Settings(QualitySettings),
    /// 控制端要一个关键帧（例如解码器重置了）
    Keyframe,
    /// 改看另一个显示器（序号没检查过）
    SelectDisplay(u8),
    /// 控制端复制了文本（不会超过 [`MAX_CLIPBOARD_TEXT`]）
    Clipboard(String),
    /// 打开 / 关闭声音
    Audio(bool),
}

/// 会话怎么开始：握手里的画质和要不要声音，同意之前又收到的设置也算进来
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Start {
    /// 做过范围校验
    pub settings: QualitySettings,
    pub audio: bool,
}

/// 交给平台的发送端（[`serve`] 的 `platform` 拿到它们）
pub struct Senders {
    /// 控制消息：Hello、提示、显示器列表、光标、剪贴板……（按消息挑通道，见 [`super::queue`]）
    pub out: Outbox,
    /// 画面包
    pub video: mpsc::Sender<Bytes>,
    /// 声音包
    pub audio: mpsc::Sender<Bytes>,
}

/// 一个会话里各平台不同的事。方法都在会话的任务里依次调用，不会同时进来两个。
///
/// 会话随时可能在某个 `await` 处被取消（与服务器的连接断了、更换控制码……）：一定要做的清理放在 `Drop` 里，
/// [`Platform::stop`] 只做正常结束时要等的事（例如等编码线程退出，写任务才能把 Bye 发完）
pub trait Platform: Send {
    /// 平台自己的事（定时检查、别处来的广播……），见 [`Platform::next_event`]
    type Event: Send;

    /// 请本机用户同意这次连接（握手时 [`Gate`] 要求的，[`Admission::NeedsApproval`]）。
    /// 回答从返回的通道来，通道关了算拒绝；最多等 [`APPROVAL_TIMEOUT`]。同时在等回答的请求最多
    /// [`MAX_PENDING_APPROVALS`] 个，再来的不会问到这里。
    /// 默认直接拒绝：给不会要求同意的 [`Gate`] 用
    fn ask(&mut self, admitted: &Admitted) -> oneshot::Receiver<bool> {
        let _ = admitted;
        warn!("this host can't ask its user to allow a connection, declining");
        oneshot::channel().1
    }

    /// 本机用户回答了连接请求（没人理会、会话先结束了的不调用）。同意了的话接着就是 [`Platform::start`]
    fn answered(&mut self, allowed: bool) {
        let _ = allowed;
    }

    /// 会话开始（不用同意，或者本机用户同意了）：发 [`HostMsg::Hello`] 和开始时的提示，开始采集
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
/// * `platform`：控制端进来了（已经确认过：这时才在托盘、列表里显示它）之后建起这个会话的平台部分。
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
    let mut platform = platform(
        &admitted,
        Senders {
            out: io.out.clone(),
            video: io.video.clone(),
            audio: io.audio.clone(),
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
        &inc.approvals,
    )
    .await;
    // 先停掉还拿着发送端的东西，写任务才能把排着的（例如 Bye）发完
    platform.stop().await;
    drop(out);
    io.close(session_id, result).await
}

/// 会话处在哪一步
enum Phase {
    /// 在等本机用户回答
    Asking {
        answer: oneshot::Receiver<bool>,
        deadline: Instant,
        /// 占着的名额（[`ApprovalSlots`]）：不再等了就归还
        _slot: OwnedSemaphorePermit,
    },
    /// 开始了
    Open,
}

enum Answer {
    Allowed,
    Declined,
    Unanswered,
}

/// 等本机用户回答（会话开始之后一直等下去）
async fn answer(phase: &mut Phase) -> Answer {
    match phase {
        Phase::Asking {
            answer, deadline, ..
        } => match tokio::time::timeout_at(*deadline, answer).await {
            Ok(Ok(true)) => Answer::Allowed,
            Ok(_) => Answer::Declined,
            Err(_) => Answer::Unanswered,
        },
        Phase::Open => std::future::pending().await,
    }
}

/// 会话的主循环（见模块说明）。正常结束（控制端断开、发了 Bye）时为 `Ok`
#[allow(clippy::too_many_arguments)]
async fn drive<P: Platform>(
    session_id: u64,
    admitted: &Admitted,
    platform: &mut P,
    out: &Outbox,
    incoming: &mut ControllerReceiver,
    kick: &mut watch::Receiver<ByeReason>,
    writer: &mut Writer,
    approvals: &ApprovalSlots,
) -> Result<()> {
    // 本机用户可能在端到端握手期间已经点了断开，不能先开始采集再处理这个通知。
    if kick.has_changed().unwrap_or(false) {
        out.send(&HostMsg::Bye(*kick.borrow_and_update()))?;
        return Ok(());
    }
    let mut start = Start {
        settings: admitted.settings,
        audio: admitted.audio,
    };
    let mut phase = if admitted.admission == Admission::NeedsApproval {
        // 在等本机用户回答的请求已经够多了：这一个不问（见 MAX_PENDING_APPROVALS）
        let Some(slot) = approvals.take() else {
            info!(
                session_id,
                "too many connection requests are waiting for the local user, turning this one away"
            );
            out.send(&HostMsg::Bye(ByeReason::TooManyRequests))?;
            return Ok(());
        };
        let answer = platform.ask(admitted);
        info!(session_id, safety = %admitted.safety, "asking the local user to allow the connection");
        out.send(&HostMsg::AwaitingApproval)?;
        Phase::Asking {
            answer,
            deadline: Instant::now() + APPROVAL_TIMEOUT,
            _slot: slot,
        }
    } else {
        platform.start(start).await?;
        info!(session_id, "session started");
        Phase::Open
    };
    let mut watchdog = tokio::time::interval(WATCHDOG);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_msg = Instant::now();
    // 被控端服务还在（踢人的发送端还在）
    let mut kickable = true;

    loop {
        if kick.has_changed().unwrap_or(false) {
            out.send(&HostMsg::Bye(*kick.borrow_and_update()))?;
            return Ok(());
        }
        let open = matches!(phase, Phase::Open);
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { return Ok(()) };
                last_msg = Instant::now();
                let req = match msg {
                    // 同意之前也回应：控制端不会把等待当成失联
                    ControllerMsg::Ping { t } => {
                        out.send(&HostMsg::Pong { t })?;
                        continue;
                    }
                    ControllerMsg::Input { events, .. } if events.is_empty() => continue,
                    ControllerMsg::Input { events, .. } => Request::Input(events),
                    ControllerMsg::Ack { seq } => Request::Ack(seq),
                    ControllerMsg::Settings(s) => Request::Settings(s.sanitized()),
                    ControllerMsg::RequestKeyframe => Request::Keyframe,
                    ControllerMsg::SelectDisplay { index } => Request::SelectDisplay(index),
                    ControllerMsg::Clipboard { text } if text.len() > MAX_CLIPBOARD_TEXT => {
                        warn!(session_id, len = text.len(), "ignoring an oversized clipboard text");
                        continue;
                    }
                    ControllerMsg::Clipboard { text } => Request::Clipboard(text),
                    ControllerMsg::Audio { enabled } => Request::Audio(enabled),
                };
                match req {
                    req if open => platform.request(req).await?,
                    // 同意之前：记下画质和声音设置，同意之后一起生效；别的（输入、剪贴板……）都不执行
                    Request::Settings(s) => start.settings = s,
                    Request::Audio(a) => start.audio = a,
                    _ => {}
                }
            }
            a = answer(&mut phase) => {
                if let Answer::Allowed | Answer::Declined = a {
                    platform.answered(matches!(a, Answer::Allowed));
                }
                let reason = match a {
                    Answer::Allowed => {
                        info!(session_id, "the local user allowed the connection");
                        phase = Phase::Open;
                        platform.start(start).await?;
                        info!(session_id, "session started");
                        continue;
                    }
                    Answer::Declined => {
                        info!(session_id, "the local user declined the connection");
                        ByeReason::Declined
                    }
                    Answer::Unanswered => {
                        info!(session_id, "nobody answered the connection request");
                        ByeReason::Unanswered
                    }
                };
                out.send(&HostMsg::Bye(reason))?;
                return Ok(());
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
            event = platform.next_event(), if open => platform.event(event).await?,
            e = writer.failed() => return Err(e),
            _ = watchdog.tick() => {
                if open && platform.video_stalled() {
                    bail!("the controller stopped acknowledging video");
                }
                if last_msg.elapsed() > PEER_TIMEOUT {
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
    use crate::tasks::OwnedTask;
    use airhand_proto::framing;
    use airhand_proto::handshake::Negotiated;
    use airhand_proto::session::{Codec, MouseButton};
    use std::sync::{Arc, Mutex};

    /// 平台那边发生了什么
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Ask,
        Answered(bool),
        Start(Start),
        Request(Request),
        Event(u32),
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
        answer: Option<oneshot::Receiver<bool>>,
        events: mpsc::Receiver<u32>,
    }

    impl Platform for Fake {
        type Event = Option<u32>;
        fn ask(&mut self, _: &Admitted) -> oneshot::Receiver<bool> {
            self.log.push(Call::Ask);
            self.answer.take().unwrap()
        }
        fn answered(&mut self, allowed: bool) {
            self.log.push(Call::Answered(allowed));
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
        to_host: mpsc::Sender<ControllerMsg>,
        events: mpsc::Sender<u32>,
        kick: watch::Sender<ByeReason>,
        answer: Option<oneshot::Sender<bool>>,
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
            negotiated: Negotiated {
                version: 1,
                features: Default::default(),
            },
            settings: QualitySettings::default(),
            audio: true,
        }
    }

    fn session(admission: Admission) -> Harness {
        session_with(admission, ApprovalSlots::default())
    }

    /// `approvals`：和别的会话共用的名额（同一个被控端服务的会话）
    fn session_with(admission: Admission, approvals: ApprovalSlots) -> Harness {
        let log = Log::default();
        let (to_host, control_in) = mpsc::channel(16);
        let mut incoming = ControllerReceiver::new(control_in, mpsc::channel(1).1);
        let (events_tx, events) = mpsc::channel(16);
        let (kick, mut kick_rx) = watch::channel(ByeReason::Kicked);
        let (answer_tx, answer) = oneshot::channel();
        let (out, control, _bulk) = queue::channel();
        let mut platform = Fake {
            log: log.clone(),
            answer: Some(answer),
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
                &approvals,
            )
            .await
        });
        Harness {
            log,
            to_host,
            events: events_tx,
            kick,
            answer: Some(answer_tx),
            control,
            task,
        }
    }

    impl Harness {
        async fn send(&self, m: ControllerMsg) {
            self.to_host.send(m).await.unwrap();
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
    async fn a_kick_during_the_handshake_prevents_start_and_approval() {
        // current-thread runtime: send before yielding, so drive first sees the pending kick.
        for admission in [Admission::Known, Admission::NeedsApproval] {
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
        h.send(ControllerMsg::Ping { t: 5 }).await;
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
        h.send(ControllerMsg::Ack { seq: 3 }).await;
        h.send(ControllerMsg::Clipboard {
            text: "x".repeat(MAX_CLIPBOARD_TEXT + 1),
        })
        .await;
        h.send(ControllerMsg::Clipboard { text: "hi".into() }).await;
        h.events.send(9).await.unwrap();
        assert!(matches!(h.received().await[..], [HostMsg::Pong { t: 5 }]));
        let mut log = h.log.take();
        // 平台自己的事和控制端的消息谁先谁后不一定
        let event = log.iter().position(|c| *c == Call::Event(9)).unwrap();
        log.remove(event);
        assert_eq!(
            log,
            [
                Call::Start(Start {
                    settings: QualitySettings::default(),
                    audio: true
                }),
                Call::Request(Request::Input(click())),
                Call::Request(Request::Settings(fast().sanitized())),
                Call::Request(Request::Ack(3)),
                Call::Request(Request::Clipboard("hi".into())),
            ]
        );
        // 控制端断开：正常结束
        drop(h.to_host);
        assert!((&mut h.task).await.unwrap().is_ok());
    }

    /// 同意之前只回应 Ping、记下设置；同意之后带着记下的设置开始
    #[tokio::test]
    async fn nothing_happens_before_the_local_user_allows_it() {
        let mut h = session(Admission::NeedsApproval);
        h.send(ControllerMsg::Input {
            t: 0,
            events: click(),
        })
        .await;
        h.send(ControllerMsg::Settings(fast())).await;
        h.send(ControllerMsg::Audio { enabled: false }).await;
        h.send(ControllerMsg::Clipboard { text: "hi".into() }).await;
        h.send(ControllerMsg::Ping { t: 1 }).await;
        h.events.send(1).await.unwrap();
        let got = h.received().await;
        assert!(
            matches!(got[..], [HostMsg::Pong { t: 1 }, HostMsg::AwaitingApproval]),
            "{got:?}"
        );
        assert_eq!(h.log.take(), [Call::Ask]);

        h.answer.take().unwrap().send(true).unwrap();
        // 和回答同时到的消息算同意之前的（控制端收到 Hello 之前发的本来就是）：等会话开始了再发
        h.settle().await;
        h.send(ControllerMsg::Input {
            t: 0,
            events: click(),
        })
        .await;
        h.settle().await;
        let log = h.log.take();
        assert_eq!(
            log[..2],
            [
                Call::Answered(true),
                Call::Start(Start {
                    settings: fast().sanitized(),
                    audio: false
                }),
            ]
        );
        // 开始之后才处理平台自己的事（之前排着的那一件也是）
        assert!(log.contains(&Call::Event(1)), "{log:?}");
        assert!(log.contains(&Call::Request(Request::Input(click()))));
    }

    #[tokio::test]
    async fn declined() {
        let mut h = session(Admission::NeedsApproval);
        h.answer.take().unwrap().send(false).unwrap();
        assert!((&mut h.task).await.unwrap().is_ok());
        let got = h.received().await;
        assert!(matches!(
            got[..],
            [HostMsg::AwaitingApproval, HostMsg::Bye(ByeReason::Declined)]
        ));
        assert_eq!(h.log.take(), [Call::Ask, Call::Answered(false)]);
    }

    #[tokio::test(start_paused = true)]
    async fn unanswered() {
        let mut h = session(Admission::NeedsApproval);
        // 控制端一直在发 Ping：不算失联，但没人理会
        let ping = h.to_host.clone();
        let _pinger = OwnedTask(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = ping.send(ControllerMsg::Ping { t: 0 }).await;
            }
        }));
        assert!((&mut h.task).await.unwrap().is_ok());
        // 最新的 Pong 在槽位里，排在最前面
        let got = h.received().await;
        assert!(
            matches!(
                got[..],
                [
                    HostMsg::Pong { .. },
                    HostMsg::AwaitingApproval,
                    HostMsg::Bye(ByeReason::Unanswered)
                ]
            ),
            "{got:?}"
        );
        assert_eq!(h.log.take(), [Call::Ask]);
    }

    /// 同时在等本机用户回答的请求有上限：再来的不问、直接回绝；有一个不再等了（回答了、走了）才问下一个
    #[tokio::test]
    async fn only_a_few_requests_wait_for_the_local_user_at_once() {
        let slots = ApprovalSlots::default();
        let mut waiting = Vec::new();
        for _ in 0..MAX_PENDING_APPROVALS {
            let mut h = session_with(Admission::NeedsApproval, slots.clone());
            assert!(matches!(
                h.received().await[..],
                [HostMsg::AwaitingApproval]
            ));
            assert_eq!(h.log.take(), [Call::Ask]);
            waiting.push(h);
        }
        // 再来一个：本机用户看不到它，会话马上结束
        let mut extra = session_with(Admission::NeedsApproval, slots.clone());
        assert!((&mut extra.task).await.unwrap().is_ok());
        let got = extra.received().await;
        assert!(
            matches!(got[..], [HostMsg::Bye(ByeReason::TooManyRequests)]),
            "{got:?}"
        );
        assert!(extra.log.take().is_empty());
        // 不用问的（同意过的、配对过的）不受影响
        let known = session_with(Admission::Known, slots.clone());
        known.settle().await;
        assert!(matches!(known.log.take()[..], [Call::Start(_)]));

        // 本机用户同意了一个：它不再占着名额，下一个请求又会问
        let mut allowed = waiting.pop().unwrap();
        allowed.answer.take().unwrap().send(true).unwrap();
        allowed.settle().await;
        let next = session_with(Admission::NeedsApproval, slots.clone());
        next.settle().await;
        assert_eq!(next.log.take(), [Call::Ask]);
        // 控制端自己走了的也一样
        let gone = waiting.pop().unwrap();
        drop(gone.to_host);
        assert!(gone.task.await.unwrap().is_ok());
        let another = session_with(Admission::NeedsApproval, slots);
        another.settle().await;
        assert_eq!(another.log.take(), [Call::Ask]);
    }

    #[tokio::test]
    async fn kicked() {
        for admission in [Admission::Open, Admission::NeedsApproval] {
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
        let h = session(Admission::Open);
        let err = h.task.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("stopped responding"), "{err}");
    }
}
