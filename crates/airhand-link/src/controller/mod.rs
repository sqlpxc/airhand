//! 控制端服务：维护与服务器的连接（自动重连），管理多个并发会话。
//!
//! 与界面无关。三种界面（桌面版的 WebView、网页控制台的浏览器、Android）用同一套接口：
//!
//! * 界面 → 服务：[`ControllerService::connect`] 开始会话（[`ConnectOptions`]），之后都是 [`Command`]（JSON 形状三边一样）；
//! * 服务 → 界面：会话的画面、光标、声音是媒体包（[`airhand_proto::media`]，原样转交），
//!   其它是 [`SessionEvent`]（JSON），都交给 [`SessionUi`]。
//!
//! 界面的 JSON 全都是 camelCase：字段名、枚举值（`"h264"`、`"android"`、`"quickSettings"`）、
//! 带字段的枚举用一个字段标明是哪一种（命令 `cmd`、事件和输入 `type`、问题和提示 `kind`）。
//! 样例见 `crates/airhand-link/tests/protocol.json`，三种界面的测试都读它。
//!
//! 界面和本服务可以在同一台电脑上（桌面版、Android），也可以是经网络连过来的浏览器（网页版），
//! 区别见 [`Frontend`]。
//!
//! # 被控端掉线后的重连
//!
//! 会话断了由控制台决定是否重连（再调用一次 [`ControllerService::connect`]）。连上时得到的重连凭证
//!（[`SessionInfo::resume`]，见 `airhand_proto::signal` 模块说明）由控制台保管，重连时带上：
//! 这样的重连不会被服务器当成猜码而限速。被控端不在线时 `connect` 返回 [`Problem::HostOffline`]，
//! 被控端重新上线时服务器通知，本服务调用 [`HostOnline`]，控制台这时再连一次即可，等待期间不需要反复尝试。
//! 与服务器的连接断过之后服务器不再记得谁在等谁，控制台在重新连上服务器时再连一次（还不在线的话会重新登记）。
//!
//! 凭证作废了（被控端的用户断开过所有控制端，或者换了控制码）时 `connect` 返回 [`Problem::Kicked`]：
//! 自动重连就此停下；用户自己要连的（例如从"我的设备"打开）就不带凭证再连一次。
//!
//! # 端到端加密
//!
//! `connect` 发起会话时就带着握手的第一条消息（[`crate::secure::initiate`]，协议见 `airhand_proto::handshake`），
//! 服务器批准之后在控制通道上做完（[`establish`]），之后每条通道的每一帧都是密文。本机的身份（[`Identity`]）由平台保存，
//! 启动服务时给进来；被控端的公钥由界面保管（"我的设备"，和重连凭证一样，见 [`ConnectOptions::host_key`]）：
//!
//! * 第一次连（界面没有它的公钥）：记下对方的公钥（[`SessionInfo::host_key`]），界面保存下来；
//! * 之后只认它：对不上返回 [`Problem::HostKeyChanged`]，不自动重连，由用户决定要不要相信新的身份
//!   （不带公钥再连一次）；
//! * 被控端要求配对时返回 [`Problem::PairingRequired`]：界面请用户输入对方托盘里的配对码，带着它再连一次
//!   （[`ConnectOptions::pairing`]）。配对之后被控端记住本机，以后不用再输；
//! * [`SessionInfo::safety`] 是安全码：被控端显示的是同一个，第一次连接时两边的人可以对一下。
//!
//! # 通道和数据报
//!
//! 一条服务器连接上可能有好几个会话：连接的 [`Router`] 按会话 ID 把被控端的通道（单向流）和数据报
//! （声音、探测）交给对应的会话（见 [`crate::lanes`]）。
//!
//! # 直连
//!
//! 会话开始之后被控端发来候选地址，控制端从连服务器的那个 endpoint 连过去（见 [`crate::direct`]）。
//! 连上了对界面没有什么不同，只是更快；画面从直连来的时候 [`SessionEvent::Stats`] 的 `direct` 是 true。

mod cursor;
mod link;
mod queue;
mod session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::direct::{self, Mirror};
use crate::lanes::Router;
use crate::secure::{self, Identity, InitiatorParams};
use crate::tasks::OwnedTask;
use crate::wire::{read_msg, write_msg};
use airhand_proto::framing;
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::session::{ControllerMsg, InputEvent, MAX_CLIPBOARD_TEXT, QualitySettings};
use airhand_proto::signal::{ClientMsg, ResumeToken, ServerMsg, SessionGrant, connect_outcome};
use airhand_proto::{MAX_SIGNAL_MSG, Role};
use anyhow::{Result, anyhow};
use bytes::Bytes;
use quinn::{Connection, Endpoint};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{debug, info, warn};

use crate::net::{self, LinkConfig, Reconnect, Report};
use crate::{Problem, ServerState};
pub use link::{Established, establish};
pub use session::{NoticeView, SessionEvent, SessionUi, UiClosed};

/// 桌面版检查本机剪贴板变化的间隔
pub const CLIPBOARD_POLL: Duration = Duration::from_millis(300);

/// 大块通道（剪贴板）最多排着这么多条：网络慢的时候再复制，丢掉的是这一次（之前排着的发完了就轮到下一次）
const BULK_QUEUE: usize = 4;

/// 本机剪贴板的双向同步（桌面版；实现见客户端的 `clipboard.rs`）。
pub trait ClipboardSync: Send + 'static {
    /// 每隔 [`CLIPBOARD_POLL`] 调用一次，只取已完成的读取结果；不得执行或等待系统剪贴板 IO。
    fn poll(&mut self) -> Option<String>;
    /// 写入被控端复制的文本
    fn apply(&self, text: String);
}

/// 控制台在哪里。
pub enum Frontend {
    /// 桌面控制台：本服务同步这台电脑的剪贴板
    Desktop {
        clipboard: Box<dyn ClipboardSync>,
        /// 是否同步剪贴板（之后可用 [`Command::ClipboardSync`] 切换）
        sync: bool,
    },
    /// 剪贴板由界面自己同步（网页控制台的浏览器、Android）：被控端复制的文本作为
    /// [`SessionEvent::Clipboard`] 交给界面，界面复制的文本用 [`Command::Clipboard`] 发给被控端。
    ///
    /// 不管哪种界面，画面都由界面在最后一段（IPC / 网络）之后确认（[`Command::Ack`]），
    /// 被控端的在途帧控制因此把这一段也算进去，这段慢的时候画面不会在中间越积越多。
    Browser,
}

/// 服务器通知：控制码为 `code` 的被控端重新上线了，在等它的会话（[`Problem::HostOffline`]）现在重连
pub type HostOnline = Arc<dyn Fn(u32) + Send + Sync>;

/// 界面对一个会话的操作。JSON：`{"cmd": "input", "sid": 1, "events": [{"type": "releaseAll"}]}`，三种界面一样
/// （桌面版经 Tauri 的 `ctrl` 命令，网页版经 WebSocket，Android 经 JNI）。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Command {
    /// 输入事件（同一批里相邻的鼠标移动会合并）
    Input {
        sid: u32,
        events: Vec<InputEvent>,
    },
    Settings {
        sid: u32,
        settings: QualitySettings,
    },
    /// 请被控端马上编一个关键帧（解码器出错、刚切回这个会话）
    Keyframe {
        sid: u32,
    },
    /// 切换到被控端的另一个显示器
    Display {
        sid: u32,
        index: u8,
    },
    /// 开启 / 关闭被控端的声音
    Audio {
        sid: u32,
        enabled: bool,
    },
    /// 把界面复制的文本发给被控端（网页版、Android：剪贴板由界面自己读）。走大块通道，网络慢、
    /// 前面还排着好几次没发完时丢掉这一次
    Clipboard {
        sid: u32,
        text: String,
    },
    /// 打开 / 关闭剪贴板同步（桌面版：由本服务同步这台电脑的剪贴板）
    ClipboardSync {
        sid: u32,
        enabled: bool,
    },
    /// 界面收到了第 `seq` 帧画面（累积确认），见 [`SessionUi::packet`]
    Ack {
        sid: u32,
        seq: u32,
    },
    /// 断开会话（先松开所有按着的键）
    Disconnect {
        sid: u32,
    },
}

impl Command {
    /// 这个命令针对的会话
    pub fn sid(&self) -> u32 {
        match self {
            Command::Input { sid, .. }
            | Command::Settings { sid, .. }
            | Command::Keyframe { sid }
            | Command::Display { sid, .. }
            | Command::Audio { sid, .. }
            | Command::Clipboard { sid, .. }
            | Command::ClipboardSync { sid, .. }
            | Command::Ack { sid, .. }
            | Command::Disconnect { sid } => *sid,
        }
    }
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Result<SessionGrant, Problem>>>>>;

/// 怎样连这台被控端（除了控制码），开始时的画质和声音。界面保管这些，下次连时带上。
///
/// JSON 是 camelCase，三种界面一样：`connect` 命令是它的字段加上 `code`（和各自的通道需要的，例如网页版的 `ch`）
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectOptions {
    /// 重连凭证：之前连上这个控制码时得到的（[`SessionInfo::resume`]），第一次连接时为 `None`
    #[serde(default)]
    pub resume: Option<ResumeToken>,
    /// 之前记下的被控端公钥（[`SessionInfo::host_key`]，64 位十六进制）：只认它（见模块说明的"端到端加密"）
    #[serde(default)]
    pub host_key: Option<String>,
    /// 配对码（被控端要求配对时用户输入的，6 位数字）
    #[serde(default)]
    pub pairing: Option<String>,
    /// 开始时的画质：在端到端握手里交给被控端，它一开始就按这个建编码器
    #[serde(default)]
    pub settings: QualitySettings,
    /// 开始时要不要声音（同上）
    #[serde(default)]
    pub audio: bool,
}

#[derive(Clone)]
struct Live {
    conn: Connection,
    /// 连服务器的 endpoint：直连也从它发起（同一个 socket，见 [`crate::direct`]）
    endpoint: Endpoint,
    /// 被控端的通道、数据报按会话交给会话
    router: Arc<Router>,
    tx: mpsc::Sender<ClientMsg>,
    pending: Pending,
}

struct SessionHandle {
    tx: queue::Sender,
    /// 大块通道（剪贴板）：编码好的消息
    bulk: mpsc::Sender<Bytes>,
    task: AbortHandle,
    /// 桌面版：是否同步剪贴板
    clipboard: Option<Arc<AtomicBool>>,
    /// 画面帧什么时候到的：确认时带上（见 [`session::Arrivals`]）
    arrivals: Arc<Mutex<session::Arrivals>>,
}

struct Inner {
    cfg: Arc<LinkConfig>,
    /// 本机的身份（端到端握手里的静态密钥）：被控端据此认出配对过、同意过的控制端
    identity: Identity,
    rt: Handle,
    live: Mutex<Option<Live>>,
    sessions: Mutex<HashMap<u32, SessionHandle>>,
    next_req: AtomicU32,
    next_sid: AtomicU32,
    host_online: HostOnline,
}

pub struct ControllerService {
    inner: Arc<Inner>,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub sid: u32,
    pub code: String,
    pub host_name: String,
    pub host_os: String,
    /// 重连凭证：这个会话断了之后重连时交给 [`ControllerService::connect`]
    pub resume: ResumeToken,
    /// 被控端的公钥（64 位十六进制）：界面保存下来，之后连它时带上（[`ConnectOptions::host_key`]）
    pub host_key: String,
    /// 安全码（见模块说明的"端到端加密"）
    pub safety: String,
}

impl ControllerService {
    /// 在 `rt` 上启动服务：连接服务器并在掉线后自动重连，连接状态的变化通过 `report` 通知，
    /// 在等的被控端上线了通过 `host_online` 通知。`identity` 是本机的身份（平台保存，见模块说明的"端到端加密"）。
    /// 服务被 drop 时断开所有会话和连接。
    pub fn start(
        cfg: Arc<LinkConfig>,
        rt: Handle,
        report: Report,
        host_online: HostOnline,
        identity: Identity,
    ) -> Self {
        let inner = Arc::new(Inner {
            cfg,
            identity,
            rt: rt.clone(),
            live: Mutex::default(),
            sessions: Mutex::default(),
            next_req: AtomicU32::new(0),
            next_sid: AtomicU32::new(0),
            host_online,
        });
        let task = rt.spawn(run(inner.clone(), report));
        Self { inner, task }
    }

    /// 连接控制码为 `code` 的被控端（用户输入的样子，允许空格 / 短横线）。
    /// `opts`：重连凭证、记下的被控端公钥、配对码、开始时的画质和声音（见 [`ConnectOptions`]）
    pub async fn connect(
        &self,
        code: &str,
        opts: ConnectOptions,
        ui: Arc<dyn SessionUi>,
        frontend: Frontend,
    ) -> Result<SessionInfo, Problem> {
        let code = airhand_proto::parse_code(code).ok_or(Problem::InvalidCode)?;
        let pairing = match opts.pairing.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(p) => Some(airhand_proto::parse_pairing(p).ok_or(Problem::PairingFailed)?),
        };
        let host_key = match opts.host_key.as_deref() {
            None | Some("") => None,
            Some(k) => {
                Some(secure::unhex32(k).ok_or_else(|| Problem::failed("invalid saved host key"))?)
            }
        };
        // 端到端握手的第一条消息随请求一起发给服务器，它转给被控端（见模块说明的"端到端加密"）。
        // 开始时的画质和声音在介绍里：被控端一开始就按它建编码器，不用先按默认设置建一个再重建
        let intro = secure::controller_intro(
            &self.inner.cfg.name,
            &self.inner.cfg.os,
            opts.settings,
            opts.audio,
        );
        let (initiator, init) = secure::initiate(InitiatorParams {
            code,
            me: &self.inner.identity,
            host_key,
            pairing: pairing.as_deref(),
            intro: &intro,
        })
        .map_err(Problem::failed)?;
        let resume = opts.resume;
        let live = self
            .inner
            .live
            .lock()
            .unwrap()
            .clone()
            .ok_or(Problem::NotConnected)?;
        let req_id = self.inner.next_req.fetch_add(1, Ordering::Relaxed);
        let (otx, orx) = oneshot::channel();
        {
            let mut pending = live.pending.lock().unwrap();
            if pending.len() >= 64 {
                return Err(Problem::failed("too many pending connections"));
            }
            pending.insert(req_id, otx);
        }
        // Also remove cancelled requests, not just requests that time out.
        struct RequestGuard(Pending, u32);
        impl Drop for RequestGuard {
            fn drop(&mut self) {
                self.0.lock().unwrap().remove(&self.1);
            }
        }
        let _request = RequestGuard(live.pending.clone(), req_id);
        live.tx
            .try_send(ClientMsg::Connect {
                req_id,
                code,
                resume,
                init,
            })
            .map_err(|_| server_gone())?;
        let grant = match tokio::time::timeout(Duration::from_secs(10), orx).await {
            Ok(Ok(Ok(g))) => g,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => return Err(server_gone()),
            Err(_) => {
                live.pending.lock().unwrap().remove(&req_id);
                return Err(Problem::ServerTimeout);
            }
        };

        // 在控制通道上做完握手：确认对方的身份、让对方确认本机
        let link = establish(&live.router, &live.conn, grant.session_id, initiator)
            .await
            .map_err(|e| Problem::from_error(e, Problem::failed))?;
        let (peer, safety) = (link.peer, link.safety.clone());
        let arrivals = Arc::new(Mutex::new(session::Arrivals::default()));

        let sid = self.inner.next_sid.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = queue::channel();
        let (bulk_tx, bulk_rx) = mpsc::channel(BULK_QUEUE);
        // 直连：等被控端发来候选地址
        let mirror = Mirror::default();
        let (offers_tx, offers) = mpsc::channel(4);
        let answer = tx.clone();
        let direct = direct::controller::run(direct::controller::Session {
            endpoint: live.endpoint.clone(),
            relay: live.conn.clone(),
            session_id: grant.session_id,
            observed: grant.observed,
            probe: link.probe,
            probes: link.probes,
            feed: link.feed,
            offers,
            answer: Box::new(move |offer| {
                let _ = answer.send(ControllerMsg::Direct(offer));
            }),
            mirror: mirror.clone(),
        });
        let (mode, clipboard) = match frontend {
            Frontend::Desktop { clipboard, sync } => {
                let enabled = Arc::new(AtomicBool::new(sync));
                let mode = session::Mode::Desktop {
                    clip: Arc::new(Mutex::new(clipboard)),
                    enabled: enabled.clone(),
                };
                (mode, Some(enabled))
            }
            Frontend::Browser => (session::Mode::Browser, None),
        };
        let io = session::SessionIo {
            control: link.control,
            bulk: (link.bulk.0, bulk_rx, link.bulk.1),
            bulk_tx: bulk_tx.clone(),
            recv: link.receiver,
            ui,
            mode,
            negotiated: link.negotiated,
            arrivals: arrivals.clone(),
            mirror,
            direct: session::Direct {
                offers: offers_tx,
                task: Box::pin(direct),
            },
        };
        let registration = link.registration;
        let inner = self.inner.clone();
        let tx2 = tx.clone();
        // Hold the registry lock until insertion; a fast EOF must not remove a
        // not-yet-inserted entry and leave a dead handle behind.
        let mut sessions = self.inner.sessions.lock().unwrap();
        let handle = self.inner.rt.spawn(async move {
            // 会话结束或者被中止时随之注销
            let _registration = registration;
            session::run(sid, io, tx2, rx).await;
            inner.sessions.lock().unwrap().remove(&sid);
        });
        sessions.insert(
            sid,
            SessionHandle {
                tx,
                bulk: bulk_tx,
                task: handle.abort_handle(),
                clipboard,
                arrivals,
            },
        );
        info!(sid, code, host = %grant.host_name, %safety, "session established");
        Ok(SessionInfo {
            sid,
            code: airhand_proto::format_code(code),
            host_name: grant.host_name,
            host_os: grant.host_os,
            resume: grant.resume,
            host_key: secure::hex(&peer),
            safety,
        })
    }

    /// 执行界面的命令
    pub fn command(&self, cmd: Command) -> Result<(), Problem> {
        let msg = match cmd {
            Command::Input { sid, events } => (sid, queue::input(events)),
            Command::Settings { sid, settings } => {
                (sid, ControllerMsg::Settings(settings.sanitized()))
            }
            Command::Keyframe { sid } => {
                let sessions = self.inner.sessions.lock().unwrap();
                let s = sessions.get(&sid).ok_or(Problem::NoSession)?;
                let after = s.arrivals.lock().unwrap().last();
                return s.tx.send(ControllerMsg::RequestKeyframe { after });
            }
            Command::Display { sid, index } => (sid, ControllerMsg::SelectDisplay { index }),
            Command::Audio { sid, enabled } => (sid, ControllerMsg::Audio { enabled }),
            Command::Clipboard { sid, text } => {
                let sessions = self.inner.sessions.lock().unwrap();
                let s = sessions.get(&sid).ok_or(Problem::NoSession)?;
                send_clipboard(&s.bulk, text);
                return Ok(());
            }
            Command::ClipboardSync { sid, enabled } => {
                let sessions = self.inner.sessions.lock().unwrap();
                let s = sessions.get(&sid).ok_or(Problem::NoSession)?;
                // 网页版、Android 没有这个开关：剪贴板由界面自己决定发不发
                if let Some(c) = &s.clipboard {
                    c.store(enabled, Ordering::Relaxed);
                }
                return Ok(());
            }
            Command::Ack { sid, seq } => return self.ack(sid, seq),
            Command::Disconnect { sid } => {
                self.disconnect(sid);
                return Ok(());
            }
        };
        self.send(msg.0, msg.1)
    }

    fn send(&self, sid: u32, msg: ControllerMsg) -> Result<(), Problem> {
        let sessions = self.inner.sessions.lock().unwrap();
        let s = sessions.get(&sid).ok_or(Problem::NoSession)?;
        s.tx.send(msg)
    }

    /// 界面收到了这一帧（[`Command::Ack`]；Android 每帧走一次 JNI，不经过 JSON）。确认是累积的，
    /// 带着这一帧开始从网络上到达的时刻（见 `ControllerMsg::Ack`）
    pub fn ack(&self, sid: u32, seq: u32) -> Result<(), Problem> {
        let sessions = self.inner.sessions.lock().unwrap();
        let s = sessions.get(&sid).ok_or(Problem::NoSession)?;
        let Some(arrived) = s.arrivals.lock().unwrap().take(seq) else {
            // 不是这个会话交出去的帧（界面的毛病），或者已经确认过了：确认是累积的，不用再发
            debug!(sid, seq, "ignoring an acknowledgement for an unknown frame");
            return Ok(());
        };
        s.tx.send(ControllerMsg::Ack { seq, arrived })
    }

    /// 断开会话（[`Command::Disconnect`]）
    pub fn disconnect(&self, sid: u32) {
        if let Some(s) = self.inner.sessions.lock().unwrap().remove(&sid) {
            let _ = s.tx.send(queue::input(vec![InputEvent::ReleaseAll]));
            // 给写任务一点时间把"释放按键"发出去
            let task = s.task;
            self.inner.rt.spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                task.abort();
            });
        }
    }
}

/// 把剪贴板文本排进大块通道（见 [`Command::Clipboard`]）。太大的、空的不发；前面还排着好几次没发完时丢掉这一次
fn send_clipboard(bulk: &mpsc::Sender<Bytes>, text: String) {
    if text.is_empty() || text.len() > MAX_CLIPBOARD_TEXT {
        return;
    }
    let msg = framing::encode_with_spare(&ControllerMsg::Clipboard { text }, SEAL_OVERHEAD);
    if let Err(mpsc::error::TrySendError::Full(_)) = bulk.try_send(Bytes::from(msg)) {
        warn!("the clipboard lane is busy (slow network), dropped this clipboard update");
    }
}

/// 等待连接结果时与服务器的连接断了
fn server_gone() -> Problem {
    Problem::ServerLost {
        detail: "the connection closed while waiting for the answer".into(),
    }
}

impl Drop for ControllerService {
    fn drop(&mut self) {
        for (_, s) in self.inner.sessions.lock().unwrap().drain() {
            s.task.abort();
        }
        self.task.abort();
    }
}

async fn run(inner: Arc<Inner>, report: Report) {
    let mut reconnect = Reconnect::new(report.clone());
    loop {
        reconnect.begin();
        // 控制端不会主动要求重连：控制流正常结束也算掉线
        let result = connect_once(&inner, &report)
            .await
            .and_then(|()| Err(anyhow!("the server closed the control stream")));
        let live = inner.live.lock().unwrap().take();
        if let Some(live) = live {
            // 让所有等待中的连接请求立即失败
            live.pending.lock().unwrap().clear();
        }
        reconnect.end("controller", result).await;
    }
}

async fn connect_once(inner: &Arc<Inner>, report: &Report) -> Result<()> {
    let sig = net::connect_signal(&inner.cfg, Role::Controller).await?;
    let (tx, mut rx) = mpsc::channel::<ClientMsg>(64);
    let pending: Pending = Arc::default();
    // 被控端的通道、数据报按会话 ID 交给会话（见模块说明的"通道和数据报"）
    let (router, _router): (_, OwnedTask<()>) = Router::start(&sig.conn);
    *inner.live.lock().unwrap() = Some(Live {
        conn: sig.conn.clone(),
        endpoint: sig.endpoint.clone(),
        router,
        tx,
        pending: pending.clone(),
    });
    report(ServerState::Connected);
    info!("controller connected to the server");

    let mut send = sig.send;
    let mut recv = sig.recv;
    let inner = inner.clone();
    let writer = async move {
        while let Some(m) = rx.recv().await {
            write_msg(&mut send, &m).await?;
        }
        anyhow::Ok(())
    };
    let reader = async move {
        let mut buf = Vec::new();
        while let Some(m) = read_msg::<_, ServerMsg>(&mut recv, MAX_SIGNAL_MSG, &mut buf).await? {
            match m {
                ServerMsg::ConnectResult {
                    req_id,
                    grant,
                    error,
                } => {
                    if let Some(tx) = pending.lock().unwrap().remove(&req_id) {
                        let _ = tx.send(connect_outcome(grant, error).map_err(Problem::from));
                    }
                }
                ServerMsg::HostOnline { code } => {
                    info!(code, "a host we were waiting for is back");
                    (inner.host_online)(code);
                }
                other => warn!("ignoring server message: {other:?}"),
            }
        }
        anyhow::Ok(())
    };
    tokio::select! {
        r = reader => r,
        r = writer => r,
        e = sig.conn.closed() => Err(anyhow!(e)),
    }
}
