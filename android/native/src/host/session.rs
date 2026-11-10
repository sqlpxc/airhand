//! 被控端的单个远控会话：手机的 [`Platform`]（对应桌面版的 `client/src-tauri/src/host/session.rs`）。
//!
//! 协议层的事（请本机用户同意的流程、Pong、失联检测、踢人时发 Bye、画质设置的校验）和桌面版一样在
//! [`airhand_link::host::serve`]。手机默认要先请坐在它面前的人同意：认不认识这个控制端看的是它在端到端握手里的
//! 公钥（服务器冒充不了），要不要问由 [`Shared`] 的 `Gate` 决定，怎么问在这里（交给 Kotlin 显示，回答经
//! [`super::Op::Approve`] 回来）。
//!
//! 采集、编码、注入输入都在 Kotlin 那边：画面、声音由 [`Shared`] 的分发器交给会话，控制端的输入和剪贴板变成给 Kotlin 的事件，
//! 显示器、编码器、权限这些变化经广播（[`Shared::broadcast`]）发给所有会话。

use std::sync::Arc;

use airhand_link::host::{
    Admitted, Encoded, Outbox, Platform, Request, SendError, Senders, Start, serve,
};
use airhand_proto::SessionLane;
use airhand_proto::handshake::PublicKey;
use airhand_proto::session::{HostHello, HostMsg, InputEvent, Notice};
use anyhow::{Result, bail};
use bytes::Bytes;
use serde_json::json;
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{info, warn};

use super::{Shared, audio, video};
use airhand_link::host::Incoming;

pub async fn run(inc: Incoming, shared: Arc<Shared>) -> Result<()> {
    let id = inc.session_id;
    let ip = inc.peer.ip;
    let sh = shared.clone();
    serve(
        inc,
        &*shared,
        video::QUEUE,
        audio::QUEUE,
        move |admitted, senders| {
            // Confirm 验证之后才发布身份；pending/active 由下面的协议阶段更新。
            sh.admitted(id, ip, admitted);
            Phone::new(sh, id, admitted.key, senders)
        },
    )
    .await
}

/// 手机上的一个会话
pub(crate) struct Phone {
    shared: Arc<Shared>,
    id: u64,
    /// 控制端的公钥：本机用户同意了就记住它（这次共享里断线重连不再问）
    key: PublicKey,
    out: Outbox,
    video: mpsc::Sender<Bytes>,
    audio: mpsc::Sender<Bytes>,
    /// 发给所有会话的消息：会话开始之后才订阅（同意之前不转发，Hello 里带着那时的状态）
    broadcasts: Option<broadcast::Receiver<Encoded>>,
}

impl Phone {
    pub(crate) fn new(shared: Arc<Shared>, id: u64, key: PublicKey, senders: Senders) -> Self {
        Self {
            shared,
            id,
            key,
            out: senders.out,
            video: senders.video,
            audio: senders.audio,
            broadcasts: None,
        }
    }
}

/// 会话随时可能在某个 await 处被取消（断网、更换控制码、停止共享）：清理工作放在 Drop 里
impl Drop for Phone {
    fn drop(&mut self) {
        self.shared.take_pending(self.id);
        self.shared.video.remove(self.id);
        self.shared.audio.unsubscribe(self.id);
        self.shared
            .event(json!({ "type": "input", "events": [InputEvent::ReleaseAll] }));
    }
}

impl Platform for Phone {
    type Event = Result<Encoded, broadcast::error::RecvError>;

    fn ask(&mut self, admitted: &Admitted) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.shared.add_pending(self.id, tx);
        self.shared.update_controller(self.id, |c| c.pending = true);
        info!(session_id = self.id, safety = %admitted.safety, "asking the local user");
        rx
    }

    fn answered(&mut self, allowed: bool) {
        if allowed {
            self.shared.trust(self.key);
        }
    }

    async fn start(&mut self, start: Start) -> Result<()> {
        let sh = &self.shared;
        // 先订阅再读现在的状态：之后的变化都会收到，之前的都在 Hello 里
        self.broadcasts = Some(sh.broadcast.subscribe());
        sh.take_pending(self.id);
        sh.update_controller(self.id, |c| {
            c.pending = false;
            c.active = true;
        });
        {
            let info = sh.info.lock().unwrap();
            self.out.send(&HostMsg::Hello(HostHello {
                hostname: info.hostname.clone(),
                os: info.os.clone(),
                version: info.version.clone(),
                displays: info.displays.clone(),
                current_display: 0,
                codecs: info.codecs.clone(),
                platform: airhand_proto::session::Platform::Android,
                touch: info.touch,
                actions: info.actions.clone(),
            }))?;
            if !info.input {
                self.out
                    .send(&HostMsg::Notice(Some(Notice::MissingPermissions {
                        screen: false,
                        input: true,
                    })))?;
            }
            if let Some(e) = &info.encoder {
                self.out.send(&HostMsg::Encoder(e.clone()))?;
            }
            if let Some(detail) = &info.capture_failure {
                self.out.send(&HostMsg::Notice(Some(Notice::CaptureFailed {
                    detail: detail.clone(),
                })))?;
            }
        }
        sh.video
            .subscribe(self.id, self.video.clone(), start.settings);
        if start.audio {
            sh.audio
                .subscribe(self.id, self.audio.clone(), self.out.clone());
        }
        Ok(())
    }

    async fn request(&mut self, req: Request) -> Result<()> {
        let sh = &self.shared;
        match req {
            // 晚到太久的输入已经换成了"释放所有按键"
            Request::Input(events) => sh.event(json!({ "type": "input", "events": events })),
            Request::Ack(seq) => sh.video.ack(self.id, seq),
            Request::Settings(s) => sh.video.set_settings(self.id, s),
            Request::Keyframe => sh.video.request_keyframe(),
            // 手机只有一块屏幕
            Request::SelectDisplay(_) => {}
            Request::Clipboard(text) => sh.event(json!({ "type": "clipboard", "text": text })),
            Request::Audio(true) => {
                sh.audio
                    .subscribe(self.id, self.audio.clone(), self.out.clone())
            }
            Request::Audio(false) => sh.audio.unsubscribe(self.id),
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Self::Event {
        match &mut self.broadcasts {
            Some(rx) => rx.recv().await,
            None => std::future::pending().await,
        }
    }

    async fn event(&mut self, event: Self::Event) -> Result<()> {
        match event {
            Ok(msg) => {
                let lane = msg.lane();
                match self.out.send_encoded(msg) {
                    Ok(()) => {}
                    // 剪贴板：网络慢、前面排满了就丢掉这一次
                    Err(SendError::Full) if lane == SessionLane::Bulk => {
                        warn!(session_id = self.id, "dropped a clipboard update");
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!(session_id = self.id, n, "missed broadcast messages");
            }
            // 发送端在 Shared 里，会话拿着 Shared：不会发生
            Err(broadcast::error::RecvError::Closed) => bail!("the broadcast channel closed"),
        }
        Ok(())
    }

    /// 清理在 Drop 里（会话被取消时也要做）
    async fn stop(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, HostConfig};
    use airhand_link::host::queue;
    use std::time::Duration;

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
        let (out, _control, _bulk) = queue::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let (answer_tx, answer_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut phone = Phone::new(
                sh.clone(),
                7,
                [0; 32],
                Senders {
                    out,
                    video: video_tx,
                    audio: audio_tx,
                },
            );
            let admitted = Admitted {
                key: [0; 32],
                name: String::new(),
                os: String::new(),
                safety: String::new(),
                admission: airhand_link::host::Admission::NeedsApproval,
                negotiated: airhand_proto::handshake::Negotiated {
                    version: 1,
                    features: Default::default(),
                },
                settings: Default::default(),
                audio: true,
            };
            answer_tx.send(phone.ask(&admitted)).unwrap();
            phone
                .start(Start {
                    settings: Default::default(),
                    audio: true,
                })
                .await
                .unwrap();
            // 会话开始时去掉了等回答的登记：再登记一次，看取消时会不会清掉
            let (pending_tx, pending_rx) = oneshot::channel();
            sh.add_pending(7, pending_tx);
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
            ready_tx.send(pending_rx).unwrap();
            std::future::pending::<()>().await;
        });
        let mut asked = answer_rx.await.unwrap();
        // 会话开始时不再等回答
        assert!(asked.try_recv().is_err());
        let pending = ready_rx.await.unwrap();
        assert!(!host.shared.video.wait_window(Duration::ZERO));
        task.abort();
        let _ = task.await;
        assert!(pending.await.is_err());
        assert!(host.shared.video.wait_window(Duration::ZERO));
        let mut video_off = false;
        let mut audio_off = false;
        let mut released = false;
        while let Some(event) = host.poll_event(Duration::ZERO) {
            let v: serde_json::Value = serde_json::from_str(&event).unwrap();
            if v["active"] == false {
                video_off |= v["type"] == "video";
                audio_off |= v["type"] == "audio";
            }
            released |= v["type"] == "input" && v["events"][0]["type"] == "releaseAll";
        }
        assert!(video_off && audio_off && released);
    }
}
