//! 被控端一个会话的链路：回答端到端握手、开本端的通道、读写任务、直连任务、结束。会话里收发什么、什么时候发，
//! 见 [`super::session`]。
//!
//! Ping 和画面确认在读任务里就处理掉（[`Express`]），不排进会话的主循环：主循环可能正等着平台做一件慢事
//! （换显示器时要等编码线程退出、建硬件编码器），这期间 Pong 和确认都晚了，控制端测到的往返时延、
//! 被控端的码率控制都会把它当成网络变慢。控制端的其它消息在读任务里就换成了 [`Request`]。
//!
//! 有直连（[`Incoming::direct`]）时会话开始之后还有一个直连任务：控制通道每一帧也从直连发一份，直连上控制端发来的
//! 控制通道和中转上的合并（[`crate::merge`]），画面、声音走哪条路由它决定。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use airhand_proto::SessionLane;
use airhand_proto::handshake::{Negotiated, PublicKey};
use airhand_proto::session::{
    ControllerMsg, HostMsg, InputEvent, MAX_CLIPBOARD_TEXT, QualitySettings,
};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc, watch};
use tracing::{info, warn};

use super::Incoming;
use super::clock::InputClock;
use super::queue::{self, Outbox};
use super::session::{Acks, Request};
use crate::direct::{self, Mirror, Route};
use crate::lanes::{Via, first_stream, open_lane};
use crate::merge::{Merge, Policy};
use crate::secure::{self, Admission, Gate, ResponderParams, STEP_TIMEOUT};
use crate::tasks::OwnedTask;
use crate::transport::{next_msg, write_audio, write_lane, write_video};

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
    /// 画面包（[`airhand_proto::media::frame`] 编好的）
    pub video: mpsc::Sender<Bytes>,
    /// 声音包（同上）
    pub audio: mpsc::Sender<Bytes>,
    /// 控制端的请求
    pub incoming: ControllerReceiver,
    /// 写任务：意外结束就是连接出了问题
    pub writer: Writer,
    reader: OwnedTask<Result<()>>,
    /// 直连任务（有直连时才有，见 [`crate::direct::host`]）：会话开始之后（[`HostStreams::started`]）才开始
    direct: Option<OwnedTask<()>>,
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
    express: Express,
}

impl ControllerReceiver {
    pub(crate) fn new(
        control: mpsc::Receiver<Request>,
        bulk: mpsc::Receiver<Request>,
        express: Express,
    ) -> Self {
        Self {
            control,
            bulk,
            bulk_open: true,
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

    /// 画面确认从现在起交给 `acks`（只能设一次）。之前收到的丢掉：那时还没发过画面
    pub fn set_acks(&self, acks: Arc<dyn Acks>) {
        let _ = self.express.acks.set(acks);
    }

    /// 多久没收到控制端的任何消息了（包括读任务自己处理掉的 Ping、确认）
    pub fn silent_for(&self) -> Duration {
        self.express.heard.lock().unwrap().elapsed()
    }
}

/// 读任务自己处理的消息（见模块说明）：
///
/// * Ping：马上回 Pong（只留最新值的槽位，见 [`super::queue`]）。同意之前也回：控制端不会把等待当成失联；
/// * 画面确认：马上交给平台的 [`Acks`]；
/// * 任何消息都记下收到的时刻，会话据此发现失联（[`super::PEER_TIMEOUT`]）。
#[derive(Clone)]
pub(crate) struct Express {
    out: Outbox,
    acks: Arc<OnceLock<Arc<dyn Acks>>>,
    /// 上次收到控制端消息的时刻（tokio 的时钟：测试里可以拨）
    heard: Arc<Mutex<tokio::time::Instant>>,
}

impl Express {
    pub(crate) fn new(out: Outbox) -> Self {
        Self {
            out,
            acks: Arc::default(),
            heard: Arc::new(Mutex::new(tokio::time::Instant::now())),
        }
    }

    /// 处理得了的就地处理、返回 `None`；其它的原样返回
    pub(crate) fn handle(&self, m: ControllerMsg) -> Option<ControllerMsg> {
        *self.heard.lock().unwrap() = tokio::time::Instant::now();
        match m {
            ControllerMsg::Ping { t } => {
                // 写任务不在了：会话从 Writer::failed 知道
                let _ = self.out.send(&HostMsg::Pong { t });
                None
            }
            ControllerMsg::Ack { seq, arrived } => {
                if let Some(acks) = self.acks.get() {
                    acks.ack(seq, arrived);
                }
                None
            }
            m => Some(m),
        }
    }
}

/// 控制端的一条消息交给会话时是什么请求（[`Express`] 处理掉的、直连的回答不在这里）。没什么可做的是 `None`
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
        ControllerMsg::RequestKeyframe { after } => Request::Keyframe(Some(after)),
        ControllerMsg::SelectDisplay { index } => Request::SelectDisplay(index),
        ControllerMsg::Clipboard { text } if text.len() > MAX_CLIPBOARD_TEXT => {
            warn!(len = text.len(), "ignoring an oversized clipboard text");
            return None;
        }
        ControllerMsg::Clipboard { text } => Request::Clipboard(text),
        ControllerMsg::Audio { enabled } => Request::Audio(enabled),
        ControllerMsg::Ping { .. } | ControllerMsg::Ack { .. } | ControllerMsg::Direct(_) => {
            return None;
        }
    })
}

/// 写任务（几条通道一起）
pub struct Writer(pub(super) OwnedTask<Result<((), (), (), ())>>);

impl Writer {
    /// 等到写任务意外结束（可以放在 `select!` 里）
    pub async fn failed(&mut self) -> anyhow::Error {
        match (&mut self.0.0).await {
            Ok(Err(e)) => e.context("session writer failed"),
            r => anyhow!("session writer ended: {r:?}"),
        }
    }
}

/// 端到端握手之后，被控端知道的这个控制端（见 [`crate::secure::respond`]）。它已经确认过了：是真的在连，
/// 这时才在托盘、Android 的列表里显示它，开始采集
#[derive(Debug, Clone)]
pub struct Admitted {
    /// 控制端的公钥（它的身份）
    pub key: PublicKey,
    /// 控制端自己报的名称（空字符串表示不知道）和系统：在加密的握手里，服务器改不了
    pub name: String,
    pub os: String,
    /// 安全码：控制端显示的是同一个，两边的人可以对一下（见 `airhand_proto::handshake`）
    pub safety: String,
    /// 为什么让它进来；[`Admission::NeedsApproval`] 时会话要先请本机用户同意
    pub admission: Admission,
    /// 选定的会话版本、双方都支持的会话能力位
    pub negotiated: Negotiated,
    /// 开始时的画质（已经做过范围校验）：编码管线一开始就按它建
    pub settings: QualitySettings,
    /// 开始时要不要声音
    pub audio: bool,
}

impl HostStreams {
    /// 回答控制端的端到端握手（让不让这个控制端进来由 `gate` 决定，拒绝时返回错误），开好本端的通道。
    /// 控制端确认了才返回（见 `airhand_proto::handshake` 的"确认"）：返回之前不要显示它、不要开始采集。
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
            negotiated: r.negotiated,
            settings: r.intro.settings.sanitized(),
            audio: r.intro.audio,
        };
        info!(
            session_id,
            admission = ?admitted.admission,
            controller = %secure::hex(&admitted.key),
            safety = %admitted.safety,
            negotiated = ?admitted.negotiated,
            "end-to-end session established"
        );
        let secured = r.secured;
        let bulk_send = open_lane(conn, session_id, SessionLane::Bulk).await?;
        let video_send = open_lane(conn, session_id, SessionLane::Video).await?;
        let (out, ctl_rx, bulk_rx) = queue::channel();
        let (video_tx, video_rx) = mpsc::channel(video_queue);
        let (audio_tx, audio_rx) = mpsc::channel(audio_queue);
        // 画面、声音走哪条路（直连任务决定，没有直连时一直是中转）；控制通道在直连上的那一份
        let (route_tx, route) = watch::channel(Route::default());
        let mirror = Mirror::default();
        let broken = Arc::new(Notify::new());
        let writer = OwnedTask(tokio::spawn(write_all(Writers {
            conn: conn.clone(),
            session_id,
            control: (send, ctl_rx, r.sealer, mirror.clone()),
            bulk: (bulk_send, bulk_rx, secured.bulk.sealer),
            video: (video_send, video_rx, secured.video.sealer, broken.clone()),
            audio: (audio_rx, secured.audio.sealer),
            route,
        })));
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过信箱交给会话
        let mut control = Merge::new(
            SessionLane::Control,
            Policy::Complete,
            r.opener,
            inbox.control,
        );
        control.add(control_in, Via::Relay)?;
        let bulk = Merge::new(
            SessionLane::Bulk,
            Policy::Complete,
            secured.bulk.opener,
            inbox.bulk,
        );
        let capacity = 4 * 1024 * 1024 / SessionLane::Control.max_frame_size();
        let (control_tx, control_rx) = mpsc::channel(capacity);
        let (bulk_tx, bulk_rx) = mpsc::channel(2);
        let express = Express::new(out.clone());
        let incoming = ControllerReceiver::new(control_rx, bulk_rx, express.clone());
        let (answers_tx, answers) = mpsc::channel(4);
        let started = Arc::new(Notify::new());
        let direct = inc.direct.as_ref().map(|link| {
            OwnedTask(tokio::spawn(direct::host::run(direct::host::Session {
                link: link.clone(),
                session_id,
                probe: secured.probe,
                probes: inbox.probes,
                feed: inbox.feed.clone(),
                out: out.clone(),
                answers,
                mirror,
                route: route_tx,
                broken,
                acks: express.acks.clone(),
                keyframe: control_tx.downgrade(),
                started: started.clone(),
            })))
        });
        let reader = OwnedTask(tokio::spawn(read_all(
            session_id, control, bulk, express, control_tx, bulk_tx, answers_tx,
        )));
        Ok((
            Self {
                out,
                video: video_tx,
                audio: audio_tx,
                incoming,
                writer: Writer(writer),
                reader,
                direct,
                started,
                _registration: registration,
            },
            admitted,
        ))
    }

    /// 会话开始了（发了 Hello）时通知它（`notify_one`）：这时才开始建直连，不把本机的地址告诉还没被同意的控制端
    pub fn started(&self) -> Arc<Notify> {
        self.started.clone()
    }

    /// 结束会话：等写任务把排好队的数据（例如 Bye）发完（最多 1 秒），返回会话的结果
    ///（`result` 是会话自己的结果；会话正常结束时，读任务的错误也算进来）
    pub async fn close(self, session_id: u64, result: Result<()>) -> Result<()> {
        let HostStreams {
            out,
            video,
            audio,
            incoming,
            mut writer,
            mut reader,
            direct,
            started: _,
            _registration,
        } = self;
        // 直连任务手里也有一个 Outbox（发候选地址）：先停掉它，写任务才知道没有人再发了
        if let Some(mut d) = direct {
            d.abort();
            let _ = (&mut d.0).await;
        }
        drop((out, video, audio, incoming));
        reader.abort();
        // 读任务手里也有一个 Outbox（回 Pong）：等它真的结束，写任务才知道没有人再发了
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

/// 本端各条通道的写任务要的东西
struct Writers {
    conn: quinn::Connection,
    session_id: u64,
    control: (
        quinn::SendStream,
        queue::LaneReceiver,
        secure::Sealer,
        Mirror,
    ),
    bulk: (quinn::SendStream, queue::LaneReceiver, secure::Sealer),
    video: (
        quinn::SendStream,
        mpsc::Receiver<Bytes>,
        secure::Sealer,
        Arc<Notify>,
    ),
    audio: (mpsc::Receiver<Bytes>, secure::Sealer),
    route: watch::Receiver<Route>,
}

async fn write_all(w: Writers) -> Result<((), (), (), ())> {
    let Writers {
        conn,
        session_id,
        control: (control, ctl_rx, ctl_sealer, mirror),
        bulk: (bulk, bulk_rx, bulk_sealer),
        video: (video, video_rx, video_sealer, broken),
        audio: (audio_rx, audio_sealer),
        route,
    } = w;
    tokio::try_join!(
        write_lane(control, ctl_rx, ctl_sealer, Some(mirror)),
        write_lane(bulk, bulk_rx, bulk_sealer, None),
        write_video(
            video,
            video_rx,
            video_sealer,
            session_id,
            route.clone(),
            broken
        ),
        write_audio(conn, session_id, audio_rx, audio_sealer, route)
    )
}

/// 读控制端的控制、大块通道：Ping、确认就地处理（[`Express`]），直连的回答交给直连任务，其它的换成请求交给会话
async fn read_all(
    session_id: u64,
    mut control: Merge,
    mut bulk: Merge,
    express: Express,
    control_tx: mpsc::Sender<Request>,
    bulk_tx: mpsc::Sender<Request>,
    answers: mpsc::Sender<airhand_proto::session::DirectOffer>,
) -> Result<()> {
    let bulk_express = express.clone();
    // 大块通道（剪贴板）：结束了不算会话结束，控制通道结束才是
    let bulk = async move {
        while let Some(m) = next_msg::<ControllerMsg>(&mut bulk, SessionLane::Bulk).await? {
            let Some(r) = bulk_express.handle(m).and_then(request) else {
                continue;
            };
            if bulk_tx.send(r).await.is_err() {
                break;
            }
        }
        std::future::pending::<Result<()>>().await
    };
    let control = async move {
        let mut clock = InputClock::default();
        // 这一串过时的输入已经换成了一个"释放所有按键"
        let mut released = false;
        while let Some(m) = next_msg::<ControllerMsg>(&mut control, SessionLane::Control).await? {
            if let &ControllerMsg::Ping { t } = &m {
                clock.stale(t, Instant::now());
            }
            let m = match express.handle(m) {
                // 直连任务的事（没有直连时没人收，丢掉就是了）
                Some(ControllerMsg::Direct(offer)) => {
                    let _ = answers.try_send(offer);
                    continue;
                }
                Some(ControllerMsg::Input { t, events }) => {
                    if !clock.stale(t, Instant::now()) {
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
                Some(m) => m,
                None => continue,
            };
            let Some(r) = request(m) else { continue };
            if control_tx.send(r).await.is_err() {
                break;
            }
        }
        Ok(())
    };
    tokio::select! {
        r = control => r,
        r = bulk => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::framing;

    #[tokio::test]
    async fn bulk_backpressure_cannot_delay_control_or_keep_a_closed_session_alive() {
        let (control, control_rx) = mpsc::channel(1);
        let (bulk, bulk_rx) = mpsc::channel(2);
        let (out, _control, _bulk) = queue::channel();
        let mut incoming = ControllerReceiver::new(control_rx, bulk_rx, Express::new(out));
        for _ in 0..2 {
            bulk.try_send(Request::Clipboard("x".repeat(1 << 20)))
                .unwrap();
        }
        assert!(bulk.try_send(Request::Clipboard("more".into())).is_err());
        control.send(Request::SelectDisplay(7)).await.unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(Request::SelectDisplay(7))
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
            request(ControllerMsg::RequestKeyframe { after: 7 }),
            Some(Request::Keyframe(Some(7)))
        );
        let wild = QualitySettings {
            fps: 0,
            ..QualitySettings::default()
        };
        assert_eq!(
            request(ControllerMsg::Settings(wild)),
            Some(Request::Settings(wild.sanitized()))
        );
        assert_eq!(request(ControllerMsg::Ping { t: 1 }), None);
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<(u32, u64)>>);

    impl Acks for Log {
        fn ack(&self, seq: u32, arrived: u64) {
            self.0.lock().unwrap().push((seq, arrived));
        }
    }

    /// Ping 马上回 Pong、确认马上交给平台，都不进会话的主循环；会话开始之前的确认丢掉。
    /// 任何消息都算控制端还在
    #[tokio::test(start_paused = true)]
    async fn pings_and_acks_take_the_express_lane() {
        let (out, mut control, _bulk) = queue::channel();
        let (_tx, rx) = mpsc::channel(1);
        let incoming = ControllerReceiver::new(rx, mpsc::channel(1).1, Express::new(out));
        let express = incoming.express.clone();
        let ack = |seq| ControllerMsg::Ack { seq, arrived: 9 };
        assert!(express.handle(ack(1)).is_none());
        let log = Arc::new(Log::default());
        incoming.set_acks(log.clone());
        assert!(express.handle(ack(2)).is_none());
        assert_eq!(*log.0.lock().unwrap(), [(2, 9)]);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(incoming.silent_for(), Duration::from_secs(10));
        assert!(express.handle(ControllerMsg::Ping { t: 5 }).is_none());
        assert_eq!(incoming.silent_for(), Duration::ZERO);
        let pong = framing::decode_known(&control.try_recv().unwrap()[framing::PREFIX..]).unwrap();
        assert!(matches!(pong, HostMsg::Pong { t: 5 }));

        tokio::time::advance(Duration::from_secs(10)).await;
        let m = express.handle(ControllerMsg::RequestKeyframe { after: 0 });
        assert!(matches!(m, Some(ControllerMsg::RequestKeyframe { .. })));
        assert_eq!(incoming.silent_for(), Duration::ZERO);
    }
}
