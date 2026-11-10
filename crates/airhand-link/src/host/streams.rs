//! 被控端一个会话的四条流：打开、端到端握手、读写任务、结束。会话里收发什么、什么时候发，见 [`super::session`]。
//!
//! Ping 和画面确认在读任务里就处理掉（[`Express`]），不排进会话的主循环：主循环可能正等着平台做一件慢事
//! （换显示器时要等编码线程退出、建硬件编码器），这期间 Pong 和确认都晚了，控制端测到的往返时延、
//! 被控端的码率控制都会把它当成网络变慢。
//!
//! 两端都支持直连时（见 [`crate::direct`]），会话开始之后还有一个直连任务：控制、大块通道每一帧也从直连发一份，
//! 直连上控制端发来的控制、大块通道和中转上的合并（[`crate::merge`]），画面、声音走哪条路由它决定。

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use airhand_proto::features;
use airhand_proto::handshake::{Negotiated, PublicKey};
use airhand_proto::session::{ControllerMsg, HostMsg, InputEvent, QualitySettings};
use airhand_proto::{Role, SessionLane};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use tokio::sync::{Notify, mpsc, watch};
use tracing::{info, warn};

use super::Incoming;
use super::clock::InputClock;
use super::queue::{self, Outbox};
use super::session::Acks;
use crate::direct::{self, Attach, Mirrors, Route};
use crate::merge::{Merge, Policy};
use crate::secure::{self, Admission, Gate, ResponderParams};
use crate::tasks::OwnedTask;
use crate::transport::{
    SessionStreams, next_msg, open_session, write_audio, write_lane, write_video,
};

/// 被控端一个会话的四条流。
///
/// 控制消息、大块消息（剪贴板、光标）排进 [`Outbox`]（它按消息挑通道，见 [`super::queue`]），画面包、声音包
/// 各排进一个有界队列，各自的写任务把它们加密后写进流（声音包优先发成数据报）。控制端的消息由读任务解密后交给
/// [`HostStreams::incoming`]，过时的输入在这里就换成了"释放所有按键"（见 [`InputClock`]）。
///
/// 结束时调用 [`HostStreams::close`]：在那之前要先停掉还拿着 `out` / `video` / `audio` 的东西
/// （编码管线、声音订阅），写任务才能把队列里剩下的（例如 Bye）发完、正常结束流。
pub struct HostStreams {
    /// 控制消息（控制通道、大块通道）
    pub out: Outbox,
    /// 画面包（[`airhand_proto::media::frame`] 编好的）
    pub video: mpsc::Sender<Bytes>,
    /// 声音包（同上）
    pub audio: mpsc::Sender<Bytes>,
    /// 控制端发来的消息
    pub incoming: ControllerReceiver,
    /// 写任务（四条流一起）：意外结束就是连接出了问题
    pub writer: Writer,
    reader: OwnedTask<Result<()>>,
    /// 直连任务（两端都支持直连时才有，见 [`crate::direct::host`]）：会话开始之后（[`HostStreams::started`]）才开始
    direct: Option<OwnedTask<()>>,
    started: Arc<Notify>,
}

/// 控制端发来的消息，交给会话的主循环（Ping、确认已经在读任务里处理掉了，见模块说明）。
/// 控制和大块消息使用独立信箱，批量剪贴板不能占满输入的接收位置。
pub struct ControllerReceiver {
    control: mpsc::Receiver<ControllerMsg>,
    bulk: mpsc::Receiver<ControllerMsg>,
    bulk_open: bool,
    express: Express,
}

impl ControllerReceiver {
    pub(crate) fn new(
        control: mpsc::Receiver<ControllerMsg>,
        bulk: mpsc::Receiver<ControllerMsg>,
        express: Express,
    ) -> Self {
        Self {
            control,
            bulk,
            bulk_open: true,
            express,
        }
    }

    pub async fn recv(&mut self) -> Option<ControllerMsg> {
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

    /// 处理得了的就地处理、返回 `None`；其它的原样返回，交给会话
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

/// 四条流的写任务
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
    /// 打开会话的四条流，在控制流上做端到端握手（让不让这个控制端进来由 `gate` 决定，拒绝时返回错误）。
    /// 控制端确认了才返回（见 `airhand_proto::handshake` 的"确认"）：返回之前不要显示它、不要开始采集。
    /// `video_queue` / `audio_queue`：画面、声音的发送队列有多长（满了由发送方决定丢弃还是等待）
    pub async fn open(
        inc: &Incoming,
        video_queue: usize,
        audio_queue: usize,
        gate: &dyn Gate,
    ) -> Result<(Self, Admitted)> {
        let session_id = inc.session_id;
        let SessionStreams {
            control: (mut send, mut recv),
            video: (video_send, _),
            audio: (audio_send, _),
            bulk: (bulk_send, bulk_recv),
        } = open_session(&inc.conn, session_id, Role::Host).await?;
        let r = secure::respond(
            &mut recv,
            &mut send,
            ResponderParams {
                session_id,
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
        let (video_sealer, audio_sealer, bulk_sealer) = (
            r.secured.video.sealer,
            r.secured.audio.sealer,
            r.secured.bulk.sealer,
        );
        let control_sealer = r.sealer;
        let (out, ctl_rx, bulk_rx) = queue::channel();
        let (video_tx, video_rx) = mpsc::channel(video_queue);
        let (audio_tx, audio_rx) = mpsc::channel(audio_queue);
        let conn = inc.conn.clone();
        // 画面、声音走哪条路（直连任务决定，没有直连时一直是中转）；控制、大块通道在直连上的那一份
        let (route_tx, route) = watch::channel(Route::default());
        let mirrors = Mirrors::default();
        let broken = Arc::new(Notify::new());
        let writer = {
            let (mirrors, broken, route2) = (mirrors.clone(), broken.clone(), route.clone());
            OwnedTask(tokio::spawn(async move {
                tokio::try_join!(
                    write_lane(send, ctl_rx, control_sealer, mirrors.control),
                    write_lane(bulk_send, bulk_rx, bulk_sealer, mirrors.bulk),
                    write_video(video_send, video_rx, video_sealer, route2, broken),
                    write_audio(conn, session_id, audio_send, audio_rx, audio_sealer, route)
                )
            }))
        };
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过通道交给会话。
        // Ping、画面确认在这里就处理掉（见模块说明）；直连的回答交给直连任务
        let (mut control, attach_control) =
            Merge::new(SessionLane::Control, Policy::Complete, r.opener, recv);
        let (mut bulk, attach_bulk) = Merge::new(
            SessionLane::Bulk,
            Policy::Complete,
            r.secured.bulk.opener,
            bulk_recv,
        );
        let capacity = 4 * 1024 * 1024 / SessionLane::Control.max_frame_size();
        let (in_tx, control_in) = mpsc::channel(capacity);
        let (bulk_in, bulk_rx) = mpsc::channel(2);
        let express = Express::new(out.clone());
        let incoming = ControllerReceiver::new(control_in, bulk_rx, express.clone());
        let (answers_tx, answers) = mpsc::channel(4);
        let started = Arc::new(Notify::new());
        let direct = match &inc.direct {
            Some(link) if admitted.negotiated.has(features::session::DIRECT) => Some(OwnedTask(
                tokio::spawn(direct::host::run(direct::host::Session {
                    link: link.clone(),
                    session_id,
                    key: r.secured.direct,
                    probe: r.secured.probe,
                    out: out.clone(),
                    answers,
                    attach: Attach {
                        control: Some(attach_control),
                        bulk: Some(attach_bulk),
                        video: None,
                    },
                    mirrors,
                    route: route_tx,
                    broken,
                    acks: express.acks.clone(),
                    keyframe: in_tx.downgrade(),
                    started: started.clone(),
                })),
            )),
            _ => None,
        };
        let reader = OwnedTask(tokio::spawn(async move {
            let bulk_express = express.clone();
            // 大块通道（剪贴板）：结束了不算会话结束，控制流结束才是
            let bulk = async move {
                while let Some(m) = next_msg::<ControllerMsg>(&mut bulk, SessionLane::Bulk).await? {
                    let Some(m) = bulk_express.handle(m) else {
                        continue;
                    };
                    if bulk_in.send(m).await.is_err() {
                        break;
                    }
                }
                std::future::pending::<Result<()>>().await
            };
            let control = async move {
                let mut clock = InputClock::default();
                // 这一串过时的输入已经换成了一个"释放所有按键"
                let mut released = false;
                while let Some(m) =
                    next_msg::<ControllerMsg>(&mut control, SessionLane::Control).await?
                {
                    if let &ControllerMsg::Ping { t } = &m {
                        clock.stale(t, Instant::now());
                    }
                    let Some(m) = express.handle(m) else {
                        continue;
                    };
                    let m = match m {
                        // 直连任务的事（会话不支持直连时没人收，丢掉就是了）
                        ControllerMsg::Direct(offer) => {
                            let _ = answers_tx.try_send(offer);
                            continue;
                        }
                        ControllerMsg::Input { t, mut events } => {
                            // 对方版本新增的按钮、系统操作……：不执行
                            events.retain(InputEvent::understood);
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
                        m => m,
                    };
                    if in_tx.send(m).await.is_err() {
                        break;
                    }
                }
                Ok(())
            };
            tokio::select! {
                r = control => r,
                r = bulk => r,
            }
        }));
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
            bulk.try_send(ControllerMsg::Clipboard {
                text: "x".repeat(1 << 20),
            })
            .unwrap();
        }
        assert!(
            bulk.try_send(ControllerMsg::Clipboard {
                text: "more".into()
            })
            .is_err()
        );
        control
            .send(ControllerMsg::SelectDisplay { index: 7 })
            .await
            .unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(ControllerMsg::SelectDisplay { index: 7 })
        ));
        control
            .send(ControllerMsg::Settings(QualitySettings::default()))
            .await
            .unwrap();
        assert!(matches!(
            incoming.recv().await,
            Some(ControllerMsg::Settings(_))
        ));
        drop(control);
        assert!(incoming.recv().await.is_none());
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<(u32, Option<u64>)>>);

    impl Acks for Log {
        fn ack(&self, seq: u32, arrived: Option<u64>) {
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
        let ack = |seq| ControllerMsg::Ack {
            seq,
            arrived: Some(9),
        };
        assert!(express.handle(ack(1)).is_none());
        let log = Arc::new(Log::default());
        incoming.set_acks(log.clone());
        assert!(express.handle(ack(2)).is_none());
        assert_eq!(*log.0.lock().unwrap(), [(2, Some(9))]);

        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(incoming.silent_for(), Duration::from_secs(10));
        assert!(express.handle(ControllerMsg::Ping { t: 5 }).is_none());
        assert_eq!(incoming.silent_for(), Duration::ZERO);
        let pong = framing::decode_known(&control.try_recv().unwrap()[framing::PREFIX..]).unwrap();
        assert!(matches!(pong, HostMsg::Pong { t: 5 }));

        tokio::time::advance(Duration::from_secs(10)).await;
        let m = express.handle(ControllerMsg::RequestKeyframe);
        assert!(matches!(m, Some(ControllerMsg::RequestKeyframe)));
        assert_eq!(incoming.silent_for(), Duration::ZERO);
    }
}
