//! 被控端一个会话的链路：回答端到端握手、开本端的通道、读写任务、直连任务、结束。会话里收发什么、什么时候发，
//! 见 [`super::session`]。
//!
//! 发出握手的决定就开通道、交给会话，不等控制端的确认（见 `airhand_proto::handshake` 的"确认"）：确认是控制端
//! 在控制通道上发的第一帧，读任务解开它时告诉会话（[`ControllerReceiver::confirmation`]），解不开就结束。
//!
//! Ping 和画面确认是控制数据报（`airhand_proto::session::ControllerDatagram`），在读任务里就处理掉（[`Express`]），
//! 不排进会话的主循环：主循环可能正等着平台做一件慢事（换显示器时要等编码线程退出、建硬件编码器），这期间 Pong
//! 和确认都晚了，控制端测到的往返时延、被控端的码率控制都会把它当成网络变慢。控制端的其它消息在读任务里就换成了
//! [`Request`]。
//!
//! 有直连（[`Incoming::direct`]）时会话开始之后还有一个直连任务：直连通了就接到会话的 [`Paths`] 上
//! （控制通道每一帧、Pong 也从直连发一份），直连上控制端发来的控制通道和中转上的合并（[`crate::merge`]），
//! 画面、声音走哪条路由它按两条路上的探测决定（控制端的回声在读任务里转给它）。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use airhand_proto::SessionLane;
use airhand_proto::framing;
use airhand_proto::handshake::{Confirm, Negotiated, PublicKey};
use airhand_proto::session::{
    ControllerDatagram, ControllerMsg, DirectOffer, HostDatagram, InputEvent, MAX_CLIPBOARD_TEXT,
    QualitySettings,
};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::{info, warn};

use super::Incoming;
use super::clock::InputClock;
use super::queue::{self, Outbox};
use super::session::{Acks, Request};
use super::write::{Writers, write_all};
use crate::direct;
use crate::lanes::{Via, first_stream, open_lane};
use crate::merge::{Merge, Policy};
use crate::paths::{DatagramSender, Paths};
use crate::secure::{self, Admission, Gate, Opener, ResponderParams, STEP_TIMEOUT};
use crate::tasks::OwnedTask;
use crate::transport::{next_msg, open_datagram};

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
    /// 控制端的请求、确认
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

    /// 多久没收到控制端的任何消息了（包括读任务自己处理掉的 Ping、确认）
    pub fn silent_for(&self) -> Duration {
        self.express.heard.lock().unwrap().elapsed()
    }
}

/// 读任务自己处理的事（见模块说明）：
///
/// * Ping：马上回 Pong。同意之前也回：控制端不会把等待当成失联；
/// * 画面确认：马上交给平台的 [`Acks`]；
/// * 任何消息都记下收到的时刻，会话据此发现失联（[`super::PEER_TIMEOUT`]）。
#[derive(Clone)]
pub(crate) struct Express {
    pong: Arc<dyn Fn(u64) + Send + Sync>,
    acks: Arc<OnceLock<Arc<dyn Acks>>>,
    /// 上次收到控制端消息的时刻（tokio 的时钟：测试里可以拨）
    heard: Arc<Mutex<tokio::time::Instant>>,
}

impl Express {
    /// `pong`：回一个 Pong（发成控制数据报）
    pub(crate) fn new(pong: impl Fn(u64) + Send + Sync + 'static) -> Self {
        Self {
            pong: Arc::new(pong),
            acks: Arc::default(),
            heard: Arc::new(Mutex::new(tokio::time::Instant::now())),
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
            ControllerDatagram::Ack { seq, arrived } => {
                if let Some(acks) = self.acks.get() {
                    acks.ack(seq, arrived);
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
        ControllerMsg::RequestKeyframe { after } => Request::Keyframe(Some(after)),
        ControllerMsg::SelectDisplay { index } => Request::SelectDisplay(index),
        ControllerMsg::Clipboard { text } if text.len() > MAX_CLIPBOARD_TEXT => {
            warn!(len = text.len(), "ignoring an oversized clipboard text");
            return None;
        }
        ControllerMsg::Clipboard { text } => Request::Clipboard(text),
        ControllerMsg::Audio { enabled } => Request::Audio(enabled),
        ControllerMsg::Direct(_) => return None,
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

/// 端到端握手之后，被控端知道的这个控制端（见 [`crate::secure::respond`]）。它还没有确认
/// （[`ControllerReceiver::confirmation`]）：确认之前不要在托盘、Android 的列表里显示它，不要请本机用户同意
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
        // 两条路：直连通了由直连任务接上，画面、声音走哪条也由它决定（没有直连时一直是中转）
        let paths = Paths::new(conn.clone());
        let writer = OwnedTask(tokio::spawn(write_all(Writers {
            session_id,
            paths: paths.clone(),
            control: (send, ctl_rx, r.sealer),
            bulk: (bulk_send, bulk_rx, secured.bulk.sealer),
            video: (video_send, video_rx, secured.video.sealer),
            audio: (audio_rx, secured.audio.sealer),
        })));
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过信箱交给会话。
        // 控制端的控制通道握手时已经等到了的（Noise_XX）先放进去，没等到的（Noise_IK）之后从信箱来
        let mut control = Merge::new(
            SessionLane::Control,
            Policy::Complete,
            r.opener,
            inbox.control,
        );
        if let Some(s) = control_in {
            control.add(s, Via::Relay)?;
        }
        let bulk = Merge::new(
            SessionLane::Bulk,
            Policy::Complete,
            secured.bulk.opener,
            inbox.bulk,
        );
        let capacity = 4 * 1024 * 1024 / SessionLane::Control.max_frame_size();
        let (control_tx, control_rx) = mpsc::channel(capacity);
        let (bulk_tx, bulk_rx) = mpsc::channel(2);
        let (confirm_tx, confirm_rx) = oneshot::channel();
        let datagrams = DatagramSender::new(session_id, paths.clone(), secured.datagrams.sealer);
        let pong = datagrams.clone();
        let express = Express::new(move |t| pong.send(&HostDatagram::Pong { t }));
        let incoming = ControllerReceiver::new(control_rx, bulk_rx, confirm_rx, express.clone());
        let (answers_tx, answers) = mpsc::channel(4);
        let (echoes_tx, echoes) = mpsc::channel(ECHO_QUEUE);
        let started = Arc::new(Notify::new());
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
            datagrams: (inbox.datagrams, secured.datagrams.opener),
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
    /// 控制数据报（Ping、确认、回声）：从哪条路来、整个数据报，和它们的解密端
    datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
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

/// 读控制端的控制通道（第一帧是确认）、大块通道和控制数据报：Ping、确认就地处理（[`Express`]），
/// 直连的回答、探测的回声交给直连任务，其它的换成请求交给会话。三样各读各的：会话的主循环慢了（请求的信箱满了），
/// Pong 和确认照样不等
async fn read_all(r: Readers) -> Result<()> {
    let Readers {
        session_id,
        mut control,
        mut bulk,
        datagrams: (mut datagrams, mut opener),
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
                ControllerDatagram::Ack { .. } => {}
            }
            express.datagram(m);
        }
        std::future::pending::<Result<()>>().await
    };
    // 大块通道（剪贴板）：结束了不算会话结束，控制通道结束才是
    let bulk = async {
        while let Some(m) = next_msg::<ControllerMsg>(&mut bulk, SessionLane::Bulk).await? {
            express.heard();
            let Some(r) = request(m) else { continue };
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
        r = dgrams => r,
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
        let ack = |seq| ControllerDatagram::Ack { seq, arrived: 9 };
        express.datagram(ack(1));
        let log = Arc::new(Log::default());
        incoming.set_acks(log.clone());
        express.datagram(ack(2));
        assert_eq!(*log.0.lock().unwrap(), [(2, 9)]);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(incoming.silent_for(), Duration::from_secs(10));
        express.datagram(ControllerDatagram::Ping { t: 5 });
        assert_eq!(incoming.silent_for(), Duration::ZERO);
        assert_eq!(*pongs.lock().unwrap(), [5]);

        tokio::time::advance(Duration::from_secs(10)).await;
        express.heard();
        assert_eq!(incoming.silent_for(), Duration::ZERO);
    }

    /// 读任务：控制通道上的第一帧必须是确认，解得开才告诉会话；之后的消息照常
    #[tokio::test]
    async fn the_first_control_frame_must_be_the_confirmation() {
        use crate::secure::test_lane;
        for genuine in [true, false] {
            let (mut sealer, opener) = test_lane(true);
            let (frames_tx, frames) = mpsc::unbounded_channel();
            let control = Merge::new(SessionLane::Control, Policy::Complete, opener, frames);
            let (_bulk_frames, bulk_frames) = mpsc::unbounded_channel();
            let bulk = Merge::new(
                SessionLane::Bulk,
                Policy::Complete,
                test_lane(true).1,
                bulk_frames,
            );
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
                .send((server.accept_uni().await.unwrap(), Via::Relay))
                .ok();

            let (control_tx, mut control_rx) = mpsc::channel(4);
            let (confirm, mut confirmed) = oneshot::channel();
            let (_dgram_tx, dgram_rx) = mpsc::channel(1);
            let reader = tokio::spawn(read_all(Readers {
                session_id: 1,
                control,
                bulk,
                datagrams: (dgram_rx, test_lane(false).1),
                express: Express::new(|_| {}),
                confirm,
                control_tx,
                bulk_tx: mpsc::channel(1).0,
                answers: mpsc::channel(1).0,
                echoes: mpsc::channel(1).0,
            }));
            if genuine {
                (&mut confirmed).await.unwrap();
                assert_eq!(control_rx.recv().await, Some(Request::SelectDisplay(3)));
                reader.abort();
            } else {
                assert!(reader.await.unwrap().is_err());
                assert!(confirmed.await.is_err());
                assert!(control_rx.recv().await.is_none());
            }
        }
    }
}
