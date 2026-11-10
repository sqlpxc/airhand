//! 被控端一个会话的四条流：打开、端到端握手、读写任务、结束。会话里收发什么、什么时候发，见 [`super::session`]。

use std::time::{Duration, Instant};

use airhand_proto::framing::{self, Decoded};
use airhand_proto::handshake::{Negotiated, PublicKey};
use airhand_proto::session::{ControllerMsg, InputEvent, QualitySettings};
use airhand_proto::{MAX_CONTROL_MSG, Role, SessionLane};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::Incoming;
use super::clock::InputClock;
use super::queue::{self, Outbox};
use crate::secure::{self, Admission, Gate, ResponderParams};
use crate::tasks::OwnedTask;
use crate::transport::{SessionStreams, open_session, write_audio, write_lane};
use crate::wire::read_frame;

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
    pub incoming: mpsc::Receiver<ControllerMsg>,
    /// 写任务（四条流一起）：意外结束就是连接出了问题
    pub writer: Writer,
    reader: OwnedTask<Result<()>>,
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

/// 读一条控制端的消息（解密、解码）；对方版本新增的消息跳过。流在消息边界处结束时为 `None`
async fn next_controller_msg(
    recv: &mut quinn::RecvStream,
    opener: &mut secure::FrameOpener,
    buf: &mut Vec<u8>,
) -> Result<Option<ControllerMsg>> {
    loop {
        if !read_frame(recv, MAX_CONTROL_MSG, buf).await? {
            return Ok(None);
        }
        opener.open(buf)?;
        match framing::decode(buf)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown(variant) => {
                debug!(variant, "skipping a message from a newer controller")
            }
        }
    }
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
            bulk: (bulk_send, mut bulk_recv),
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
        let (video_sealer, audio_sealer, bulk_sealer, datagrams) = (
            r.secured.sealer(SessionLane::Video),
            r.secured.sealer(SessionLane::Audio),
            r.secured.sealer(SessionLane::Bulk),
            r.secured.datagram_sealer(),
        );
        let mut opener = r.opener;
        let mut bulk_opener = r.secured.opener(SessionLane::Bulk);
        let control_sealer = r.sealer;
        let (out, ctl_rx, bulk_rx) = queue::channel();
        let (video_tx, video_rx) = mpsc::channel(video_queue);
        let (audio_tx, audio_rx) = mpsc::channel(audio_queue);
        let conn = inc.conn.clone();
        let writer = OwnedTask(tokio::spawn(async move {
            tokio::try_join!(
                write_lane(send, ctl_rx, control_sealer),
                write_lane(bulk_send, bulk_rx, bulk_sealer),
                write_lane(video_send, video_rx, video_sealer),
                write_audio(
                    conn,
                    session_id,
                    audio_send,
                    audio_rx,
                    audio_sealer,
                    datagrams
                )
            )
        }));
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过通道交给会话
        let (in_tx, incoming) = mpsc::channel(64);
        let bulk_in = in_tx.clone();
        let reader = OwnedTask(tokio::spawn(async move {
            // 大块通道（剪贴板）：结束了不算会话结束，控制流结束才是
            let bulk = async move {
                let mut buf = Vec::new();
                while let Some(m) =
                    next_controller_msg(&mut bulk_recv, &mut bulk_opener, &mut buf).await?
                {
                    if matches!(m, ControllerMsg::Input { .. }) {
                        // 输入只走控制流（过时的输入要在那里挡掉）
                        warn!(session_id, "ignoring input sent on the bulk lane");
                        continue;
                    }
                    if bulk_in.send(m).await.is_err() {
                        break;
                    }
                }
                std::future::pending::<Result<()>>().await
            };
            let control = async move {
                let mut buf = Vec::with_capacity(1024);
                let mut clock = InputClock::default();
                // 这一串过时的输入已经换成了一个"释放所有按键"
                let mut released = false;
                while let Some(m) = next_controller_msg(&mut recv, &mut opener, &mut buf).await? {
                    let m = match m {
                        ControllerMsg::Ping { t } => {
                            clock.stale(t, Instant::now());
                            m
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
            },
            admitted,
        ))
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
        } = self;
        drop((out, video, audio, incoming));
        reader.abort();
        if !writer.0.is_finished() {
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer.0.0).await;
        }
        let result = match result {
            Ok(()) => match (&mut reader.0).await {
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
