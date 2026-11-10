//! 控制端的单个会话：把画面、声音原样转交给界面（界面自己解码），转发输入与设置。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::secure::{FrameSealer, Negotiated};
use crate::transport::{self, FromHost, HostReceiver};
use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{
    ByeReason, Codec, ControllerMsg, DisplayInfo, HostMsg, Notice, Platform, SystemAction,
};
use anyhow::Result;
use bytes::Bytes;
use quinn::SendStream;
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::{CLIPBOARD_POLL, ClipboardSync, now_us, send_clipboard};
use crate::Problem;

/// 这么久没有收到被控端的任何消息（它每秒回 Pong），或者控制流这么久写不进去，才算失联。
/// 网络卡一下不断开：卡住期间排着的输入由命令队列丢掉（见 [`super::queue`]）
const HOST_TIMEOUT: Duration = Duration::from_secs(20);

/// 会话的画面和事件交给谁：桌面版是 Tauri 的 Channel，网页版是浏览器的 WebSocket，Android 是 Kotlin 取的队列。
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
    /// 会话开始了。`codecs` / `actions` 里只有本机认识的；本机不认识的平台（对方的版本更新）报成 `linux`
    Hello {
        hostname: String,
        os: String,
        version: String,
        displays: Vec<DisplayInfo>,
        current: u8,
        codecs: Vec<Codec>,
        platform: Platform,
        touch: bool,
        actions: Vec<SystemAction>,
    },
    /// 被控端在等它的用户同意这次连接（同意之后才会收到 `Hello` 和画面）
    AwaitingApproval,
    Encoder {
        codec: Codec,
        name: String,
        hardware: bool,
        width: u32,
        height: u32,
    },
    Displays {
        displays: Vec<DisplayInfo>,
        current: u8,
    },
    Stats {
        fps: f32,
        kbps: f32,
        rtt_ms: Option<f32>,
        width: u16,
        height: u16,
        frames: u64,
    },
    /// 被控端发来的提示，由控制台按自己的界面语言显示
    Notice { notice: NoticeView },
    /// 被控端复制了文本（只发给浏览器，见 [`super::Frontend::Browser`]；桌面版直接写入本机剪贴板）
    Clipboard { text: String },
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
    frames: u32,
    bytes: usize,
    total: u64,
    w: u16,
    h: u16,
    rtt: Option<f32>,
    last_msg: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            frames: 0,
            bytes: 0,
            total: 0,
            w: 0,
            h: 0,
            rtt: None,
            last_msg: Instant::now(),
        }
    }
}

pub(super) struct SessionIo {
    pub send: SendStream,
    /// 控制流上控制端 → 被控端的加密（已经发过确认，计数接着往下数）
    pub sealer: FrameSealer,
    /// 大块通道（剪贴板）：发送的一半、排着队的消息（编码好的）、加密
    pub bulk: (SendStream, mpsc::Receiver<Bytes>, FrameSealer),
    /// 往大块通道排消息（桌面版自己同步剪贴板时用）
    pub bulk_tx: mpsc::Sender<Bytes>,
    pub recv: HostReceiver,
    pub ui: Arc<dyn SessionUi>,
    pub mode: Mode,
    /// 端到端握手协商出来的会话版本和能力位（和被控端算出来的一样）：以后的可选功能按它决定发不发
    pub negotiated: Negotiated,
}

/// 见 [`super::Frontend`]
pub(super) enum Mode {
    Desktop {
        /// 远端→本机 与 本机→远端 共用同一个同步状态，用来抑制回声
        clip: Arc<tokio::sync::Mutex<Box<dyn ClipboardSync>>>,
        /// 是否同步剪贴板（界面可随时切换）
        enabled: Arc<AtomicBool>,
    },
    Browser,
}

pub(super) async fn run(
    sid: u32,
    io: SessionIo,
    tx: super::queue::Sender,
    mut rx: super::queue::Receiver,
) {
    let SessionIo {
        mut send,
        mut sealer,
        bulk: (bulk_send, bulk_rx, bulk_sealer),
        bulk_tx,
        mut recv,
        ui,
        mode,
        negotiated,
    } = io;
    info!(sid, ?negotiated, "session started");
    let local_clip = match mode {
        Mode::Desktop { clip, enabled } => Some((clip, enabled)),
        Mode::Browser => None,
    };
    // Every frontend acknowledges after its last IPC/network hop, not here.
    let writer = async move {
        loop {
            let bytes = transport::seal(rx.recv().await?, &mut sealer)?;
            tokio::time::timeout(HOST_TIMEOUT, send.write_chunk(bytes)).await??;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    // 大块通道（剪贴板）：和画面一样的优先级，慢了不挡控制消息
    let bulk_writer = async move {
        transport::write_lane(bulk_send, bulk_rx, bulk_sealer).await?;
        // 只有会话结束时才会关闭（发送端在会话手里）
        std::future::pending::<()>().await;
        anyhow::Ok(())
    };

    // 统计数据由读循环写入、定时器读取（读循环本身绝不能被 select 取消，否则会破坏帧边界）
    let stats = std::sync::Arc::new(std::sync::Mutex::new(Stats::default()));

    let out = ui.clone();
    let st = stats.clone();
    let clip_in = local_clip.clone();
    let reader = async move {
        loop {
            let msg = match recv.recv().await? {
                // 画面、声音：只看一眼头做统计，原样交给界面
                FromHost::Video(p) => {
                    let h = VideoHeader::decode(&p)
                        .ok_or_else(|| anyhow::anyhow!("bad video packet header"))?;
                    {
                        let mut s = st.lock().unwrap();
                        s.last_msg = Instant::now();
                        s.frames += 1;
                        s.total += 1;
                        s.bytes += p.len();
                        s.w = h.width;
                        s.h = h.height;
                    }
                    out.packet(p).map_err(|_| ui_closed())?;
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
                FromHost::Control(m) => m,
            };
            st.lock().unwrap().last_msg = Instant::now();
            match msg {
                HostMsg::Hello(h) => {
                    // 对方版本新增的编码格式、系统操作这里用不了；新的平台当作一台普通的电脑
                    let _ = out.event(SessionEvent::Hello {
                        hostname: h.hostname,
                        os: h.os,
                        version: h.version,
                        displays: h.displays,
                        current: h.current_display,
                        codecs: h.codecs.into_iter().filter(|c| c.is_known()).collect(),
                        platform: h.platform.or_linux(),
                        touch: h.touch,
                        actions: h.actions.into_iter().filter(|a| a.is_known()).collect(),
                    });
                }
                HostMsg::AwaitingApproval => {
                    let _ = out.event(SessionEvent::AwaitingApproval);
                }
                HostMsg::Displays { displays, current } => {
                    let _ = out.event(SessionEvent::Displays { displays, current });
                }
                HostMsg::Pong { t } => {
                    let sample = now_us().saturating_sub(t) as f32 / 1000.0;
                    let mut s = st.lock().unwrap();
                    s.rtt = Some(match s.rtt {
                        Some(r) => r * 0.7 + sample * 0.3,
                        None => sample,
                    });
                }
                HostMsg::Notice(Some(n)) => {
                    let _ = out.event(SessionEvent::Notice { notice: n.into() });
                }
                // 对方版本新增的提示：不显示
                HostMsg::Notice(None) => debug!(sid, "skipping a notice from a newer host"),
                HostMsg::Clipboard { text } => match &clip_in {
                    Some((clip, enabled)) => {
                        if enabled.load(Ordering::Relaxed) {
                            clip.lock().await.apply(text);
                        }
                    }
                    None => {
                        let _ = out.event(SessionEvent::Clipboard { text });
                    }
                },
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
                HostMsg::Cursor(c) => {
                    out.packet(media::cursor_packet(&c))
                        .map_err(|_| ui_closed())?;
                }
                HostMsg::Encoder(e) => {
                    let _ = out.event(SessionEvent::Encoder {
                        codec: e.codec,
                        name: e.name,
                        hardware: e.hardware,
                        width: e.width,
                        height: e.height,
                    });
                }
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let out = ui.clone();
    let ping_tx = tx.clone();
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
                    fps: s.frames as f32 / dt,
                    kbps: s.bytes as f32 * 8.0 / 1000.0 / dt,
                    rtt_ms: s.rtt,
                    width: s.w,
                    height: s.h,
                    frames: s.total,
                };
                s.frames = 0;
                s.bytes = 0;
                e
            };
            out.event(event).map_err(|_| ui_closed())?;
            // 也是保活：被控端很久收不到控制端的消息就会结束会话
            ping_tx.send(ControllerMsg::Ping { t: now_us() })?;
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };

    let clipper = async move {
        let Some((clip, enabled)) = local_clip else {
            // 网页版：浏览器自己读剪贴板
            return std::future::pending().await;
        };
        let mut tick = tokio::time::interval(CLIPBOARD_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let mut c = clip.lock().await;
            // 关闭同步时也要消费变化，否则重新打开时会把旧内容发过去
            let Some(text) = c.poll().await else { continue };
            if enabled.load(Ordering::Relaxed) {
                send_clipboard(&bulk_tx, text);
            }
        }
    };

    let result: Result<()> = tokio::select! {
        r = reader => r,
        r = ticker => r,
        () = clipper => Ok(()),
        r = writer => r.and(Err(Problem::SessionEnded.into())),
        r = bulk_writer => r.and(Err(Problem::SessionEnded.into())),
    };
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

#[cfg(test)]
mod tests {
    use super::*;

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
            serde_json::to_value(SessionEvent::AwaitingApproval).unwrap(),
            json!({ "type": "awaitingApproval" })
        );
        let h = SessionEvent::Hello {
            hostname: "Pixel".into(),
            os: "Android 15".into(),
            version: "0.1.0".into(),
            displays: vec![],
            current: 0,
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
        let e = SessionEvent::Encoder {
            codec: Codec::AV1,
            name: "NVENC".into(),
            hardware: true,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({ "type": "encoder", "codec": "av1", "name": "NVENC", "hardware": true, "width": 1920, "height": 1080 })
        );
        let s = SessionEvent::Stats {
            fps: 30.0,
            kbps: 1.0,
            rtt_ms: None,
            width: 1,
            height: 1,
            frames: 2,
        };
        assert_eq!(serde_json::to_value(&s).unwrap()["rttMs"], json!(null));
        let c = SessionEvent::Closed {
            reason: Problem::from(airhand_proto::session::ByeReason::Declined),
            retry: false,
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({ "type": "closed", "reason": { "kind": "declined" }, "retry": false })
        );
    }
}
