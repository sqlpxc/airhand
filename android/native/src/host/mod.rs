//! 被控端（手机被别人控制）：保持与服务器的连接（自动重连、取回原来的控制码），为每个控制端请求启动会话。
//!
//! 和桌面版（`client/src-tauri/src/host/`）的分工不同：屏幕采集、编码、声音、注入输入都在 Kotlin 那边
//!（MediaProjection、MediaCodec、AccessibilityService），这里只管网络：
//!
//! * Kotlin → Rust：编码好的画面 / 声音（[`Host::video_hub`] / [`Host::audio_hub`]），状态变化（[`Op`]，JSON）；
//! * Rust → Kotlin：事件队列（JSON，见 [`Host::poll_event`]）：与服务器的连接状态、分配到的控制码、
//!   正在控制的设备、需不需要画面 / 声音、控制端发来的输入和剪贴板；
//! * 画面的节奏由 [`video::VideoHub`] 控制（Kotlin 的渲染线程每一轮调用 `tick` 和 `wait_window`）。
//!
//! 与服务器的连接和会话的各条通道和桌面版共用 [`airhand_link::host`]。控制码像桌面版一样固定
//!（`Register::keep`）：Kotlin 把服务器分配的控制码和密钥保存在应用自己的数据目录里，
//! 下次启动时带上（[`HostConfig::reclaim`]）取回同一个控制码。

pub mod audio;
mod session;
pub mod video;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_link::ServerState;
use airhand_link::host::{
    Admission, Encoded, Gate, HostCmd, HostEvents, Outbox, Pairing, PairingState, PairingStore,
};
use airhand_link::secure::{Identity, PairingAttempt};
use airhand_proto::handshake::PublicKey;
use airhand_proto::session::{Codec, DisplayInfo, EncoderInfo, HostMsg, Notice, SystemAction};
use airhand_proto::signal::Reclaim;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::ctl::LinkSettings;
use crate::queue::Queue;
use crate::rt::rt;
use crate::{hex, unhex};

/// Kotlin 传来的启动配置
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    #[serde(flatten)]
    pub link: LinkSettings,
    /// 应用版本，告诉控制端
    pub version: String,
    /// 上次分配到的控制码和密钥（32 位十六进制），用来取回同一个控制码
    pub reclaim: Option<SavedCode>,
    pub displays: Vec<DisplayInfo>,
    /// 手机能编码的格式
    pub codecs: Vec<Codec>,
    /// 能不能操作（无障碍服务开着没有）
    pub input: bool,
    /// 能接收多点触摸（Android 8+ 的连续手势）
    #[serde(default)]
    pub touch: bool,
    /// 能执行的系统操作（返回、主页……，随系统版本不同）
    #[serde(default)]
    pub actions: Vec<SystemAction>,
    /// 上次保存的配对状态（配对码、暂停了没有、配对过的控制端，见 [`airhand_link::host::pairing`]）。
    /// 没有时生成一个配对码，用 `pairing` 事件交给 Kotlin 保存
    #[serde(default)]
    pub pairing: Option<PairingState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SavedCode {
    pub code: u32,
    pub secret: String,
}

/// Kotlin 发来的状态变化 / 命令
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Op {
    /// 断开所有控制端
    KickAll,
    /// 断开所有控制端并更换控制码
    NewCode,
    /// 屏幕尺寸变了（旋转）
    Displays { displays: Vec<DisplayInfo> },
    /// 编码器（重新）创建了
    Encoder { info: EncoderInfo },
    /// 无障碍服务开关变了
    Permissions { input: bool },
    /// 屏幕采集失败（`None` 表示恢复了）
    Capture { failure: Option<String> },
    /// 声音采集失败（`None` 表示恢复了）
    Audio { failure: Option<String> },
    /// 手机的剪贴板有新文本（Android 10 起应用在后台读不到剪贴板，只有前台时才会有）
    Clipboard { text: String },
    /// 手机上分享给 Airhand 的文件（打开着的文件描述符）：公告给所有控制端，它们粘贴时才来取（见 crate::files）
    OfferFiles { files: Vec<crate::files::FdFile> },
    /// 换一个配对码（暂停了的配对随之恢复）
    NewPairingCode,
    /// 忘掉所有配对过的控制端
    ForgetPaired,
}

/// 会话开始时告诉控制端的信息，以及之后的变化
pub(crate) struct Info {
    pub hostname: String,
    pub os: String,
    pub version: String,
    pub displays: Vec<DisplayInfo>,
    pub codecs: Vec<Codec>,
    pub encoder: Option<EncoderInfo>,
    pub input: bool,
    pub touch: bool,
    pub actions: Vec<SystemAction>,
    pub capture_failure: Option<String>,
}

/// 会话之间共享的东西
pub(crate) struct Shared {
    pub info: Mutex<Info>,
    pub events: Arc<Queue<String>>,
    pub video: video::VideoHub,
    pub audio: audio::AudioHub,
    /// 发给所有会话的消息（显示器变化、编码器信息、提示、剪贴板……），已编码（同一条消息只编码一次），
    /// 带着走哪条通道（见 `airhand_link::host::queue`）
    pub broadcast: broadcast::Sender<Encoded>,
    controllers: Mutex<Vec<ControllerView>>,
    /// 配对码和配对过的控制端（认的是它们在端到端握手里的公钥，服务器冒充不了），由 Kotlin 保存
    pairing: Pairing,
    /// 本机的端到端加密身份
    identity: Identity,
    /// 每个会话的剪贴板文件（控制端也支持时才有）：会话 id → Files
    pub files: Mutex<HashMap<u64, airhand_link::files::Files>>,
}

/// 配对状态交给 Kotlin：`{"type":"pairing","state":{code, locked, paired}}`，它保存下来、显示在"共享本机"页，
/// 下次开始共享时放在 [`HostConfig::pairing`] 里传回来
struct KotlinStore(Arc<Queue<String>>);

impl PairingStore for KotlinStore {
    fn changed(&self, state: &PairingState) {
        self.0
            .push(json!({ "type": "pairing", "state": state }).to_string());
    }
}

fn as_string<S: serde::Serializer>(v: &u64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(v)
}

/// 告诉 Kotlin 的控制端列表里的一项
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ControllerView {
    /// 会话 id。是随机的 u64，转成字符串（Kotlin 的 JSON 解析会把超过 Long 的数字变成浮点数）
    #[serde(serialize_with = "as_string")]
    id: u64,
    /// 控制端的公钥（64 位十六进制）：同一台设备断线重连也是它，Kotlin 按它更新同一条提醒
    key: String,
    /// 控制端在加密握手里确认过的名称
    pub name: String,
    ip: String,
    /// 安全码：控制端显示的是同一个，两边的人可以对一下（见 `airhand_proto::handshake`）
    pub safety: String,
    /// 这次刚用配对码配对（不是之前配对过的）
    #[serde(rename = "justPaired")]
    pub just_paired: bool,
}

impl Shared {
    pub fn event(&self, v: serde_json::Value) {
        self.events.push(v.to_string());
    }

    fn broadcast(&self, msg: &HostMsg) {
        // 剪贴板（最多 1 MiB）走大块通道，不挡控制消息：Outbox 按消息挑通道
        let _ = self.broadcast.send(Outbox::encode(msg));
    }

    fn notice(&self, n: Notice) {
        self.broadcast(&HostMsg::Notice(Some(n)));
    }

    fn report_controllers(&self) {
        let list = self.controllers.lock().unwrap().clone();
        self.event(json!({ "type": "controllers", "list": list }));
    }

    /// 控制端确认了握手：在列表里显示它（Kotlin 随之提醒本机用户有人连上来了）
    pub fn admitted(
        &self,
        session_id: u64,
        ip: std::net::IpAddr,
        admitted: &airhand_link::host::Admitted,
    ) {
        self.controllers.lock().unwrap().push(ControllerView {
            id: session_id,
            key: hex(&admitted.key),
            name: admitted.name.clone(),
            ip: ip.to_string(),
            safety: admitted.safety.clone(),
            just_paired: admitted.admission == Admission::Paired,
        });
        self.report_controllers();
    }
}

/// 和桌面版一样：没配对过的控制端都要输入配对码（显示在"共享本机"页），配对过的直接连上。
/// 只凭控制码进不来：服务器知道所有控制码
impl Gate for Shared {
    fn identity(&self) -> Identity {
        self.identity.clone()
    }

    fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        self.pairing.begin()
    }

    fn is_known(&self, key: &PublicKey) -> bool {
        self.pairing.is_known(key)
    }
}

pub struct Host {
    shared: Arc<Shared>,
    cmd: mpsc::UnboundedSender<HostCmd>,
    task: JoinHandle<()>,
}

impl Host {
    pub fn start(cfg: HostConfig) -> Result<Arc<Host>> {
        let events = Arc::new(Queue::new(1024));
        let link = Arc::new(cfg.link.link(events.clone())?);
        let identity = cfg.link.identity(&events);
        let pairing = Pairing::new(cfg.pairing, KotlinStore(events.clone()));
        // 现在的配对状态给"共享本机"页显示（新生成的配对码上面已经发过一次，再发一次也没关系）
        KotlinStore(events.clone()).changed(&pairing.state());
        let (broadcast, _) = broadcast::channel(32);
        let shared = Arc::new(Shared {
            info: Mutex::new(Info {
                hostname: cfg.link.name.clone(),
                os: cfg.link.os.clone(),
                version: cfg.version,
                displays: cfg.displays,
                codecs: cfg.codecs,
                encoder: None,
                input: cfg.input,
                touch: cfg.touch,
                actions: cfg.actions,
                capture_failure: None,
            }),
            video: video::VideoHub::new(events.clone()),
            audio: audio::AudioHub::new(events.clone()),
            events,
            broadcast,
            controllers: Mutex::default(),
            pairing,
            identity,
            files: Mutex::default(),
        });
        let reclaim = cfg.reclaim.and_then(|s| {
            Some(Reclaim {
                code: s.code,
                secret: unhex::<16>(&s.secret)?,
            })
        });
        let (cmd, rx) = mpsc::unbounded_channel();
        let sh = shared.clone();
        let task = rt().spawn(airhand_link::host::run(
            link,
            true,
            reclaim,
            shared.clone(),
            move |inc| {
                let sh = sh.clone();
                async move {
                    let _ = session::run(inc, sh).await;
                }
            },
            rx,
        ));
        Ok(Arc::new(Host { shared, cmd, task }))
    }

    pub fn poll_event(&self, timeout: Duration) -> Option<String> {
        self.shared.events.pop(timeout)
    }

    pub fn video_hub(&self) -> &video::VideoHub {
        &self.shared.video
    }

    pub fn audio_hub(&self) -> &audio::AudioHub {
        &self.shared.audio
    }

    pub fn call(&self, json: &str) -> Result<()> {
        let op: Op = serde_json::from_str(json)?;
        let sh = &self.shared;
        match op {
            Op::KickAll => {
                let _ = self.cmd.send(HostCmd::KickAll);
            }
            Op::NewCode => {
                let _ = self.cmd.send(HostCmd::NewCode);
            }
            Op::Displays { displays } => {
                let mut info = sh.info.lock().unwrap();
                if info.displays != displays {
                    info.displays = displays.clone();
                    drop(info);
                    sh.broadcast(&HostMsg::Displays {
                        displays,
                        current: 0,
                        shown: vec![0],
                    });
                }
            }
            Op::Encoder { info: e } => {
                sh.video.reset_config();
                sh.info.lock().unwrap().encoder = Some(e.clone());
                sh.broadcast(&HostMsg::Encoder(e));
            }
            Op::Permissions { input } => {
                let mut info = sh.info.lock().unwrap();
                if info.input != input {
                    info.input = input;
                    drop(info);
                    sh.notice(Notice::MissingPermissions {
                        screen: false,
                        input: !input,
                    });
                }
            }
            Op::Capture { failure } => {
                let mut info = sh.info.lock().unwrap();
                if info.capture_failure != failure {
                    info.capture_failure = failure.clone();
                    drop(info);
                    sh.notice(match failure {
                        Some(detail) => Notice::CaptureFailed { detail },
                        None => Notice::CaptureRecovered,
                    });
                }
            }
            Op::Audio { failure } => sh.audio.set_failure(failure),
            Op::Clipboard { text } => {
                if !text.is_empty() && text.len() <= airhand_proto::session::MAX_CLIPBOARD_TEXT {
                    sh.broadcast(&HostMsg::Clipboard { text });
                }
            }
            Op::OfferFiles { files } => {
                let provider = Arc::new(crate::files::FdFiles::new(files));
                if !provider.is_empty() {
                    for f in sh.files.lock().unwrap().values() {
                        f.offer(provider.clone());
                    }
                }
            }
            Op::NewPairingCode => sh.pairing.new_code(),
            Op::ForgetPaired => sh.pairing.forget(),
        }
        Ok(())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // 中止任务会依次释放会话、QUIC 连接（自动向服务器发送关闭帧）
        self.task.abort();
        self.shared.events.close();
    }
}

/// 被控端服务发生的事变成给 Kotlin 的事件（控制码由 Kotlin 保存）
impl HostEvents for Shared {
    fn server(&self, state: ServerState) {
        self.event(json!({ "type": "server", "state": state }));
    }

    async fn registered(&self, code: Reclaim, _changed: bool) {
        self.event(json!({ "type": "registered", "code": code.code, "secret": hex(&code.secret) }));
    }

    async fn released(&self) {
        self.event(json!({ "type": "released" }));
    }

    fn ended(&self, session_id: u64) {
        self.controllers
            .lock()
            .unwrap()
            .retain(|c| c.id != session_id);
        self.report_controllers();
    }

    fn offline(&self) {
        self.controllers.lock().unwrap().clear();
        self.report_controllers();
    }

    fn kicked(&self, cmd: HostCmd) {
        // 更换控制码：配对码也换一个、配对过的全部忘掉（和桌面版一样）；只是断开所有控制端的话，配对过的还能连回来
        if cmd == HostCmd::NewCode {
            self.pairing.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_ids_are_strings() {
        let v = ControllerView {
            id: u64::MAX,
            key: "ab".repeat(32),
            name: "PC".into(),
            ip: "1.2.3.4".into(),
            safety: "482-913".into(),
            just_paired: true,
        };
        assert_eq!(
            serde_json::to_value(&v).unwrap(),
            json!({ "id": "18446744073709551615", "key": "ab".repeat(32), "name": "PC", "ip": "1.2.3.4", "safety": "482-913", "justPaired": true })
        );
    }

    #[test]
    fn config_and_ops_parse() {
        let cfg: HostConfig = serde_json::from_str(
            r#"{"server":"127.0.0.1:4433","fingerprint":"","name":"Pixel 8","os":"Android 15",
                "version":"0.1.0","reclaim":{"code":123456,"secret":"00112233445566778899aabbccddeeff"},
                "displays":[{"index":0,"name":"Screen","x":0,"y":0,"width":1080,"height":2400,"primary":true}],
                "codecs":["h264","h265"],"input":false,"touch":true,"actions":["back","home","volumeUp"],
                "pairing":{"code":"482913","locked":false,"paired":[{"key":"ab","name":"PC","since":1}]}}"#,
        )
        .unwrap();
        assert_eq!(cfg.link.name, "Pixel 8");
        assert_eq!(cfg.codecs, [Codec::H264, Codec::H265]);
        assert_eq!(cfg.reclaim.unwrap().code, 123456);
        assert!(cfg.touch);
        assert_eq!(
            cfg.actions,
            [
                SystemAction::Back,
                SystemAction::Home,
                SystemAction::VolumeUp
            ]
        );
        let pairing = cfg.pairing.unwrap();
        assert_eq!((pairing.code.as_str(), pairing.paired.len()), ("482913", 1));

        for op in [
            r#"{"op":"kickAll"}"#,
            r#"{"op":"newCode"}"#,
            r#"{"op":"permissions","input":true}"#,
            r#"{"op":"capture","failure":null}"#,
            r#"{"op":"audio","failure":"no RECORD_AUDIO permission"}"#,
            r#"{"op":"clipboard","text":"hi"}"#,
            r#"{"op":"encoder","info":{"codec":"h264","name":"c2.qti.avc.encoder","hardware":true,"width":1080,"height":2400}}"#,
            r#"{"op":"displays","displays":[]}"#,
            r#"{"op":"newPairingCode"}"#,
            r#"{"op":"forgetPaired"}"#,
        ] {
            serde_json::from_str::<Op>(op).unwrap_or_else(|e| panic!("{op}: {e}"));
        }
    }

    /// 配对状态交给 Kotlin：开始时一次（没有配对码就生成一个），之后每次变化一次；更换控制码时配对也重来
    #[test]
    fn pairing_state_goes_to_kotlin() {
        let events = Arc::new(Queue::new(16));
        let pairing = Pairing::new(None, KotlinStore(events.clone()));
        let next = |events: &Queue<String>| -> PairingState {
            let v: serde_json::Value =
                serde_json::from_str(&events.pop(Duration::ZERO).unwrap()).unwrap();
            assert_eq!(v["type"], "pairing");
            serde_json::from_value(v["state"].clone()).unwrap()
        };
        assert_eq!(next(&events), pairing.state());
        pairing.new_code();
        let s = next(&events);
        assert_eq!(s, pairing.state());
        assert_eq!(s.code.len(), airhand_proto::PAIRING_DIGITS);
        let key = *Identity::generate().public();
        assert!(pairing.begin().unwrap().complete(key, "PC"));
        assert_eq!(next(&events).paired[0].name, "PC");
        assert!(events.pop(Duration::ZERO).is_none());
    }
}
