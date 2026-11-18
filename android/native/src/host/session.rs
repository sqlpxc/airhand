//! 被控端的单个远控会话：手机的 [`Platform`]（对应桌面版的 `client/src-tauri/src/host/session.rs`）。
//!
//! 协议层的事（Pong、失联检测、踢人时发 Bye、画质设置的校验）和桌面版一样在 [`airhand_link::host::serve`]。
//! 让不让这个控制端进来和桌面版一样看配对（[`Shared`] 的 `Gate`）：配对过的直接连上，控制端确认了握手之后
//! Kotlin 提醒手机前的人有人连上来了（[`Platform::confirmed`]）。
//!
//! 采集、编码、注入输入都在 Kotlin 那边：画面、声音由 [`Shared`] 的分发器交给会话，控制端的输入和剪贴板变成给 Kotlin 的事件，
//! 显示器、编码器、权限这些变化经广播（[`Shared::broadcast`]）发给所有会话。

use std::sync::Arc;

use airhand_link::host::{
    Acks, Admitted, Encoded, Outbox, Platform, Request, SendError, Senders, Start, serve,
};
use airhand_proto::SessionLane;
use airhand_proto::session::{HostHello, HostMsg, InputEvent, Notice};
use anyhow::{Result, bail};
use bytes::Bytes;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

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
            // 控制端确认了握手才在列表里显示它、提醒本机用户（Platform::confirmed）
            let mut phone = Phone::new(sh, id, senders);
            phone.publish = Some((ip, admitted.clone()));
            phone
        },
    )
    .await
}

/// 手机上的一个会话
pub(crate) struct Phone {
    shared: Arc<Shared>,
    id: u64,
    out: Outbox,
    video: mpsc::Sender<Bytes>,
    audio: mpsc::Sender<Bytes>,
    /// 发给所有会话的消息：会话开始时订阅（Hello 里带着那时的状态，之后的变化从这里来）
    broadcasts: Option<broadcast::Receiver<Encoded>>,
    /// 控制端确认了握手时在列表里显示的（[`Platform::confirmed`]）
    publish: Option<(std::net::IpAddr, Admitted)>,
}

impl Phone {
    pub(crate) fn new(shared: Arc<Shared>, id: u64, senders: Senders) -> Self {
        // 剪贴板里的文件（控制端也支持时）：手机分享给 Airhand 的文件公告给这个会话的控制端（见 crate::files）
        if let Some(f) = &senders.files {
            shared.files.lock().unwrap().insert(id, f.clone());
        }
        Self {
            shared,
            id,
            out: senders.out,
            // 手机只有一块屏幕：只有第 0 路画面
            video: senders
                .video
                .into_iter()
                .next()
                .expect("at least one video stream"),
            audio: senders.audio,
            broadcasts: None,
            publish: None,
        }
    }
}

/// 会话随时可能在某个 await 处被取消（断网、更换控制码、停止共享）：清理工作放在 Drop 里
impl Drop for Phone {
    fn drop(&mut self) {
        self.shared.files.lock().unwrap().remove(&self.id);
        self.shared.video.remove(self.id);
        self.shared.audio.unsubscribe(self.id);
        self.shared
            .event(json!({ "type": "input", "events": [InputEvent::ReleaseAll] }));
    }
}

/// 这个会话的画面确认交给画面分发（读任务里调用）
struct VideoAcks {
    shared: Arc<Shared>,
    id: u64,
}

/// 手机只有一块屏幕，只发第 0 路画面：别的路的不管
impl Acks for VideoAcks {
    fn ack(&self, stream: u8, seq: u32, arrived: u64) {
        if stream == 0 {
            self.shared.video.ack(self.id, seq, Some(arrived));
        }
    }

    /// 画面换了路（中转 ⇄ 直连）：这个会话的码率控制从头量时延；直连断了的话从关键帧重新加入
    fn path_changed(&self, lost: bool) {
        self.shared.video.path_changed(self.id, lost);
    }

    /// 控制端等不到帧了（帧流）：MediaCodec 不能控制长期参考帧，这个会话从关键帧重新加入（同一处报告第二次才这样做）
    fn lost(&self, stream: u8, decoded: u32) -> bool {
        stream == 0 && self.shared.video.lost(self.id, decoded)
    }

    /// 该观看者的帧流停顿报告交给自己的码率控制。
    fn feedback(&self, stream: u8, fb: airhand_proto::session::VideoReport) {
        if stream == 0 {
            self.shared.video.feedback(self.id, fb);
        }
    }
}

impl Platform for Phone {
    type Event = Result<Encoded, broadcast::error::RecvError>;

    fn acks(&self) -> Arc<dyn Acks> {
        Arc::new(VideoAcks {
            shared: self.shared.clone(),
            id: self.id,
        })
    }

    /// 控制端确认了握手：这时才在列表里显示它（Kotlin 随之提醒本机用户）。会话这时已经开始了（看得到画面）
    fn confirmed(&mut self) {
        if let Some((ip, admitted)) = self.publish.take() {
            self.shared.admitted(self.id, ip, &admitted);
        }
    }

    async fn start(&mut self, start: Start) -> Result<()> {
        let sh = &self.shared;
        // 先订阅再读现在的状态：之后的变化都会收到，之前的都在 Hello 里
        self.broadcasts = Some(sh.broadcast.subscribe());
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
                // 只有一块屏幕：一路画面
                shown: vec![0],
                streams: 1,
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
            Request::Settings(s) => sh.video.set_settings(self.id, s),
            Request::Keyframe(Some((0, after))) => sh.video.request_keyframe(self.id, Some(after)),
            Request::Keyframe(None) => sh.video.request_keyframe(self.id, None),
            // 手机只有一块屏幕、一路画面
            Request::Keyframe(Some(_)) | Request::View(_) => {}
            Request::Clipboard(text) => sh.event(json!({ "type": "clipboard", "text": text })),
            // 控制端复制了文件：交给 Kotlin 放进手机的剪贴板（content URI，别的应用读的时候才取，见 crate::files）
            Request::ClipboardFiles(offer) => {
                let key = crate::files::register(offer.clone());
                sh.events.push(crate::files::event(key, &offer));
            }
            Request::Audio(true) => {
                sh.audio
                    .subscribe(self.id, self.audio.clone(), self.out.clone())
            }
            Request::Audio(false) => sh.audio.unsubscribe(self.id),
            // 手机不接控制端的麦克风：没有 Platform::mic，也不发 HostMsg::Mic，控制端不会显示麦克风
            Request::Mic(_) => {}
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

    fn video_stalled(&self) -> bool {
        self.shared.video.ack_stalled(self.id)
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
    async fn cancellation_releases_subscriptions() {
        let cfg: HostConfig = serde_json::from_value(json!({
            "server": "127.0.0.1:1", "fingerprint": "", "name": "test", "os": "test",
            "version": "test", "reclaim": null, "displays": [], "codecs": ["h264"],
            "input": false
        }))
        .unwrap();
        let host = Host::start(cfg).unwrap();
        let sh = host.shared.clone();
        let (video_tx, _video_rx) = mpsc::channel(video::QUEUE);
        let (audio_tx, _audio_rx) = mpsc::channel(audio::QUEUE);
        let (out, _control, _bulk) = queue::channel();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut phone = Phone::new(
                sh.clone(),
                7,
                Senders {
                    out,
                    video: vec![video_tx],
                    audio: audio_tx,
                    files: None,
                },
            );
            phone
                .start(Start {
                    settings: Default::default(),
                    audio: true,
                    view: Vec::new(),
                })
                .await
                .unwrap();
            // 一帧占在途字节上限的 2/5：发了两帧之后就放不下了
            let big = vec![0; airhand_link::flow::FlowState::default().window_bytes() * 2 / 5];
            for i in 0..3 {
                sh.video.push(
                    &big,
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

    /// 控制端确认了握手才出现在列表里（Kotlin 这时提醒本机用户），带着安全码，刚配对的标出来
    #[tokio::test]
    async fn confirmed_controllers_are_listed() {
        let cfg: HostConfig = serde_json::from_value(json!({
            "server": "127.0.0.1:1", "fingerprint": "", "name": "test", "os": "test",
            "version": "test", "reclaim": null, "displays": [], "codecs": ["h264"],
            "input": false
        }))
        .unwrap();
        let host = Host::start(cfg).unwrap();
        let (video_tx, _video_rx) = mpsc::channel(video::QUEUE);
        let (audio_tx, _audio_rx) = mpsc::channel(audio::QUEUE);
        let (out, _control, _bulk) = queue::channel();
        let mut phone = Phone::new(
            host.shared.clone(),
            9,
            Senders {
                out,
                video: vec![video_tx],
                audio: audio_tx,
                files: None,
            },
        );
        phone.publish = Some((
            "10.0.0.2".parse().unwrap(),
            Admitted {
                key: [0; 32],
                name: "PC".into(),
                os: "Windows".into(),
                safety: "482-913".into(),
                admission: airhand_link::host::Admission::Paired,
                settings: Default::default(),
                audio: false,
                view: Vec::new(),
            },
        ));
        let lists = |host: &Host| -> Vec<serde_json::Value> {
            std::iter::from_fn(|| host.poll_event(Duration::ZERO))
                .map(|e| serde_json::from_str::<serde_json::Value>(&e).unwrap())
                .filter(|v| v["type"] == "controllers")
                .collect()
        };
        assert!(lists(&host).is_empty());
        phone.confirmed();
        let got = lists(&host);
        assert_eq!(
            got[0]["list"],
            json!([{ "id": "9", "key": "00".repeat(32), "name": "PC", "ip": "10.0.0.2", "safety": "482-913", "justPaired": true }])
        );
        // 只发布一次
        phone.confirmed();
        assert!(lists(&host).is_empty());
    }
}
