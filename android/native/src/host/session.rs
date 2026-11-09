//! 被控端的单个远控会话（对应桌面版的 `client/src-tauri/src/host/session.rs`，三条流的收发用
//! [`airhand_link::host::HostSession`]，两边一样）。
//!
//! 手机默认要先请坐在它面前的人同意（见 `airhand_proto::session` 的模块说明）：
//!
//! ```text
//! 等控制端的 Hello（最多 2 秒）─┬─ 本次共享里同意过这台控制端 ──────────────┐
//!                              └─ AwaitingApproval → 问本机用户（最多 60 秒）─┼─ 同意 → Hello、画面……
//!                                                                          └─ 拒绝 / 没人理 → Bye
//! ```
//!
//! 同意之前只回应 Ping、记下画质和声音设置，不发画面、不执行任何输入。

use std::sync::Arc;
use std::time::{Duration, Instant};

use airhand_link::host::{HostSession, PEER_TIMEOUT};
use airhand_proto::session::{
    ByeReason, ControllerMsg, HostHello, HostMsg, Notice, Platform, QualitySettings,
};
use anyhow::Result;
use quinn::Connection;
use serde_json::json;
use tokio::sync::{broadcast, oneshot, watch};
use tracing::{info, warn};

use super::{Shared, audio, video};

/// 等控制端报上自己的标识（它是会话流上的第一条消息，通常马上就到）
const HELLO_WAIT: Duration = Duration::from_secs(2);
/// 本机用户这么久没有回应连接请求就当作拒绝
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 本机用户同意之前会话所处的阶段
enum Gate {
    /// 等控制端的 Hello，好知道是不是同意过的设备
    WaitHello,
    /// 正在问本机用户
    Asking(oneshot::Receiver<bool>),
    /// 已同意（或者不需要同意）
    Open,
}

async fn decided(gate: &mut Gate) -> bool {
    match gate {
        Gate::Asking(rx) => rx.await.unwrap_or(false),
        _ => std::future::pending().await,
    }
}

async fn until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// 上层随时可能在某个 await 处取消会话（断网、更换控制码、停止共享）：
/// 清理工作放在 Drop 里，而不是主循环后面的语句里
struct SessionGuard {
    shared: Arc<Shared>,
    id: u64,
}
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.shared.take_pending(self.id);
        self.shared.video.remove(self.id);
        self.shared.audio.unsubscribe(self.id);
        self.shared
            .event(json!({ "type": "input", "events": ["ReleaseAll"] }));
    }
}

pub async fn run(
    conn: Connection,
    session_id: u64,
    mut kick: watch::Receiver<ByeReason>,
    shared: Arc<Shared>,
) -> Result<()> {
    let guard = SessionGuard {
        shared: shared.clone(),
        id: session_id,
    };
    let mut io = HostSession::open(&conn, session_id, video::QUEUE, audio::QUEUE).await?;
    let ctl = io.ctl.clone();
    let mut bcast = shared.broadcast.subscribe();

    // 同意之前收到的设置，同意之后再生效
    let mut settings: Option<QualitySettings> = None;
    let mut want_audio = false;
    let mut device: Option<[u8; 16]> = None;
    let mut gate = Gate::WaitHello;
    let mut deadline = Some(tokio::time::Instant::now() + HELLO_WAIT);
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let mut last_msg = Instant::now();

    // 同意之后（或者不需要同意时）：开始正常的会话
    let open = |gate: &mut Gate,
                deadline: &mut Option<tokio::time::Instant>,
                settings: Option<QualitySettings>,
                want_audio: bool|
     -> Result<()> {
        *gate = Gate::Open;
        *deadline = None;
        shared.take_pending(session_id);
        shared.update_controller(session_id, |c| {
            c.pending = false;
            c.active = true;
        });
        let info = shared.info.lock().unwrap();
        ctl.send(&HostMsg::Hello(HostHello {
            hostname: info.hostname.clone(),
            os: info.os.clone(),
            version: info.version.clone(),
            displays: info.displays.clone(),
            current_display: 0,
            codecs: info.codecs.clone(),
            platform: Platform::Android,
            touch: info.touch,
            actions: info.actions.clone(),
        }))?;
        if !info.input {
            ctl.send(&HostMsg::Notice(Notice::MissingPermissions {
                screen: false,
                input: true,
            }))?;
        }
        if let Some(e) = &info.encoder {
            ctl.send(&HostMsg::Encoder(e.clone()))?;
        }
        if let Some(detail) = &info.capture_failure {
            ctl.send(&HostMsg::Notice(Notice::CaptureFailed {
                detail: detail.clone(),
            }))?;
        }
        drop(info);
        if let Some(s) = settings {
            shared.video.set_settings(s);
        }
        shared.video.add(session_id, io.video.clone());
        if want_audio {
            shared
                .audio
                .subscribe(session_id, io.audio.clone(), ctl.clone());
        }
        info!(session_id, "session started");
        Ok(())
    };

    if !shared.approval_required() {
        open(&mut gate, &mut deadline, None, false)?;
    }

    // 问本机用户：告诉控制端在等，等结果
    let ask = |gate: &mut Gate, deadline: &mut Option<tokio::time::Instant>| -> Result<()> {
        let (tx, rx) = oneshot::channel();
        shared.add_pending(session_id, tx);
        shared.update_controller(session_id, |c| c.pending = true);
        *gate = Gate::Asking(rx);
        *deadline = Some(tokio::time::Instant::now() + APPROVAL_TIMEOUT);
        info!(session_id, "asking the local user to allow the connection");
        ctl.send(&HostMsg::AwaitingApproval)
    };

    let result: Result<()> = async {
        loop {
            let is_open = matches!(gate, Gate::Open);
            tokio::select! {
                msg = io.incoming.recv() => {
                    let Some(msg) = msg else { break };
                    last_msg = Instant::now();
                    match msg {
                        ControllerMsg::Hello(h) => {
                            device = Some(h.device);
                            if matches!(gate, Gate::WaitHello) {
                                if shared.is_trusted(&h.device) {
                                    info!(session_id, "this controller was allowed before during this sharing");
                                    open(&mut gate, &mut deadline, settings.take(), want_audio)?;
                                } else {
                                    ask(&mut gate, &mut deadline)?;
                                }
                            }
                        }
                        ControllerMsg::Ping { t } => ctl.send(&HostMsg::Pong { t })?,
                        ControllerMsg::Settings(s) if !is_open => settings = Some(s),
                        ControllerMsg::Audio { enabled } if !is_open => want_audio = enabled,
                        // 同意之前不执行任何操作、不发画面
                        _ if !is_open => {}
                        // 晚到太久的输入已经在 HostSession 里换成了"释放所有按键"
                        ControllerMsg::Input { events, .. } => {
                            shared.event(json!({ "type": "input", "events": events }));
                        }
                        ControllerMsg::Ack { seq } => shared.video.ack(session_id, seq),
                        ControllerMsg::Settings(s) => shared.video.set_settings(s),
                        ControllerMsg::RequestKeyframe => shared.video.request_keyframe(),
                        // 手机只有一块屏幕
                        ControllerMsg::SelectDisplay { .. } => {}
                        ControllerMsg::Clipboard { text } => {
                            shared.event(json!({ "type": "clipboard", "text": text }));
                        }
                        ControllerMsg::Audio { enabled } => {
                            if enabled {
                                shared.audio.subscribe(session_id, io.audio.clone(), ctl.clone());
                            } else {
                                shared.audio.unsubscribe(session_id);
                            }
                        }
                    }
                }
                allow = decided(&mut gate) => {
                    if allow {
                        info!(session_id, "the local user allowed the connection");
                        if let Some(d) = device {
                            shared.trust(d);
                        }
                        open(&mut gate, &mut deadline, settings.take(), want_audio)?;
                    } else {
                        info!(session_id, "the local user declined the connection");
                        ctl.send(&HostMsg::Bye(ByeReason::Declined))?;
                        break;
                    }
                }
                _ = until(deadline) => match gate {
                    // 控制端没报上标识（不应该发生）：当作没见过的设备
                    Gate::WaitHello => ask(&mut gate, &mut deadline)?,
                    _ => {
                        info!(session_id, "nobody answered the connection request");
                        ctl.send(&HostMsg::Bye(ByeReason::Unanswered))?;
                        break;
                    }
                },
                _ = kick.changed() => {
                    let reason = *kick.borrow_and_update();
                    info!(session_id, ?reason, "the local user disconnected the controller");
                    ctl.send(&HostMsg::Bye(reason))?;
                    break;
                }
                b = bcast.recv() => match b {
                    // 同意之前不转发（Hello 里会带上那时的状态）
                    Ok(b) => if is_open {
                        ctl.send_encoded(b)?;
                    },
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(session_id, n, "missed broadcast messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                e = io.writer.failed() => return Err(e),
                _ = watchdog.tick() => {
                    if last_msg.elapsed() > PEER_TIMEOUT {
                        anyhow::bail!("the controller stopped responding");
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    // 先让画面 / 声音分发放开这个会话的发送端，写任务才能把排着队的（例如 Bye）发完
    drop(guard);
    drop(ctl);
    io.close(session_id, result).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, HostConfig};
    use airhand_link::host::Control;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn cancellation_releases_subscriptions_and_pending_approval() {
        let cfg: HostConfig = serde_json::from_value(json!({
            "server": "127.0.0.1:1", "fingerprint": "", "name": "test", "os": "test",
            "version": "test", "reclaim": null, "displays": [], "codecs": ["H264"],
            "input": false, "approval": true
        }))
        .unwrap();
        let host = Host::start(cfg).unwrap();
        let sh = host.shared.clone();
        let (video_tx, _video_rx) = mpsc::channel(video::QUEUE);
        let (audio_tx, _audio_rx) = mpsc::channel(audio::QUEUE);
        let (ctl_tx, _ctl_rx) = mpsc::channel(8);
        let (approval_tx, approval_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = SessionGuard {
                shared: sh.clone(),
                id: 7,
            };
            sh.video.add(7, video_tx);
            sh.audio.subscribe(7, audio_tx, Control::new(ctl_tx));
            sh.add_pending(7, approval_tx);
            for i in 0..3 {
                sh.video.push(
                    &[1],
                    i == 0,
                    false,
                    0,
                    airhand_proto::session::Codec::AV1,
                    8,
                    8,
                );
            }
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        assert!(!host.shared.video.wait_window(Duration::ZERO));
        task.abort();
        let _ = task.await;
        assert!(approval_rx.await.is_err());
        assert!(host.shared.video.wait_window(Duration::ZERO));
        let mut video_off = false;
        let mut audio_off = false;
        while let Some(event) = host.poll_event(Duration::ZERO) {
            let v: serde_json::Value = serde_json::from_str(&event).unwrap();
            if v["active"] == false {
                video_off |= v["type"] == "video";
                audio_off |= v["type"] == "audio";
            }
        }
        assert!(video_off && audio_off);
    }
}
