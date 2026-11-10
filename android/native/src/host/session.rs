//! 被控端的单个远控会话（对应桌面版的 `client/src-tauri/src/host/session.rs`，各条通道的收发用
//! [`airhand_link::host::HostSession`]，两边一样）。
//!
//! 手机默认要先请坐在它面前的人同意（见 `airhand_proto::session` 的模块说明）。认不认识这个控制端看的是它在
//! 端到端握手里的公钥（服务器冒充不了），握手由 [`HostSession::open`] 做，让不让进来由 [`Shared`] 的 `Gate` 决定：
//!
//! ```text
//! 端到端握手 ─┬─ 本次共享里同意过这台控制端 / 不用同意 ─────────────────────────┐
//!            └─ AwaitingApproval → 问本机用户（显示安全码，最多 60 秒）─┼─ 同意 → Hello、画面……
//!                                                                    └─ 拒绝 / 没人理 → Bye
//! ```
//!
//! 同意之前只回应 Ping、记下画质和声音设置，不发画面、不执行任何输入。

use std::sync::Arc;
use std::time::{Duration, Instant};

use airhand_link::host::{Admission, HostSession, Incoming, PEER_TIMEOUT};
use airhand_proto::session::{
    ByeReason, ControllerMsg, HostHello, HostMsg, InputEvent, Notice, Platform, QualitySettings,
};
use anyhow::Result;
use serde_json::json;
use tokio::sync::{broadcast, oneshot};
use tracing::{info, warn};

use super::{Broadcast, Shared, audio, video};
/// 本机用户这么久没有回应连接请求就当作拒绝
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// 本机用户同意之前会话所处的阶段
enum Phase {
    /// 正在问本机用户
    Asking(oneshot::Receiver<bool>),
    /// 已同意（或者不需要同意）
    Open,
}

async fn decided(phase: &mut Phase) -> bool {
    match phase {
        Phase::Asking(rx) => rx.await.unwrap_or(false),
        Phase::Open => std::future::pending().await,
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
            .event(json!({ "type": "input", "events": [InputEvent::ReleaseAll] }));
    }
}

pub async fn run(inc: Incoming, shared: Arc<Shared>) -> Result<()> {
    let session_id = inc.session_id;
    let guard = SessionGuard {
        shared: shared.clone(),
        id: session_id,
    };
    // 端到端握手：控制端是谁（公钥）、要不要问本机用户，由 Shared 的 Gate 决定。
    // 返回的时候控制端已经确认过了（重放的握手消息走不到这里），这时才在列表里显示它、开始采集
    let (mut io, admitted) = HostSession::open(&inc, video::QUEUE, audio::QUEUE, &*shared).await?;
    let mut kick = inc.kick;
    // 列表里换成控制端在加密握手里报的名称，加上安全码（问本机用户时一起显示）
    shared.update_controller(session_id, |c| {
        if !admitted.name.is_empty() {
            c.name = admitted.name.clone();
        }
        c.safety = Some(admitted.safety.clone());
    });
    let ctl = io.ctl.clone();
    let bulk = io.bulk.clone();
    let mut bcast = shared.broadcast.subscribe();

    // 开始时的画质和声音在握手里；同意之前收到的变化也记下来，同意之后一起生效
    let mut settings: Option<QualitySettings> = Some(admitted.settings);
    let mut want_audio = admitted.audio;
    let mut phase = Phase::Open;
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut watchdog = tokio::time::interval(Duration::from_secs(5));
    let mut last_msg = Instant::now();

    // 同意之后（或者不需要同意时）：开始正常的会话
    let open = |phase: &mut Phase,
                deadline: &mut Option<tokio::time::Instant>,
                settings: Option<QualitySettings>,
                want_audio: bool|
     -> Result<()> {
        *phase = Phase::Open;
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
            ctl.send(&HostMsg::Notice(Some(Notice::MissingPermissions {
                screen: false,
                input: true,
            })))?;
        }
        if let Some(e) = &info.encoder {
            ctl.send(&HostMsg::Encoder(e.clone()))?;
        }
        if let Some(detail) = &info.capture_failure {
            ctl.send(&HostMsg::Notice(Some(Notice::CaptureFailed {
                detail: detail.clone(),
            })))?;
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

    if admitted.admission == Admission::NeedsApproval {
        // 问本机用户：告诉控制端在等，等结果
        let (tx, rx) = oneshot::channel();
        shared.add_pending(session_id, tx);
        shared.update_controller(session_id, |c| c.pending = true);
        phase = Phase::Asking(rx);
        deadline = Some(tokio::time::Instant::now() + APPROVAL_TIMEOUT);
        info!(session_id, safety = %admitted.safety, "asking the local user to allow the connection");
        ctl.send(&HostMsg::AwaitingApproval)?;
    } else {
        if admitted.admission == Admission::Known {
            info!(
                session_id,
                "this controller was allowed before during this sharing"
            );
        }
        open(&mut phase, &mut deadline, settings.take(), want_audio)?;
    }

    let result: Result<()> = async {
        loop {
            let is_open = matches!(phase, Phase::Open);
            tokio::select! {
                msg = io.incoming.recv() => {
                    let Some(msg) = msg else { break };
                    last_msg = Instant::now();
                    match msg {
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
                allow = decided(&mut phase) => {
                    if allow {
                        info!(session_id, "the local user allowed the connection");
                        shared.trust(admitted.key);
                        open(&mut phase, &mut deadline, settings.take(), want_audio)?;
                    } else {
                        info!(session_id, "the local user declined the connection");
                        ctl.send(&HostMsg::Bye(ByeReason::Declined))?;
                        break;
                    }
                }
                _ = until(deadline) => {
                    info!(session_id, "nobody answered the connection request");
                    ctl.send(&HostMsg::Bye(ByeReason::Unanswered))?;
                    break;
                }
                _ = kick.changed() => {
                    let reason = *kick.borrow_and_update();
                    info!(session_id, ?reason, "the local user disconnected the controller");
                    ctl.send(&HostMsg::Bye(reason))?;
                    break;
                }
                b = bcast.recv() => match b {
                    // 同意之前不转发（Hello 里会带上那时的状态）
                    Ok(_) if !is_open => {}
                    Ok(Broadcast::Control(b)) => ctl.send_encoded(b)?,
                    // 剪贴板：网络慢、前面排满了就丢掉这一次
                    Ok(Broadcast::Bulk(b)) => if let Err(e) = bulk.send_encoded(b) {
                        warn!(session_id, "dropped a clipboard update: {e:#}");
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
    drop((ctl, bulk));
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
            "version": "test", "reclaim": null, "displays": [], "codecs": ["h264"],
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
