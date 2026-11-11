//! 控制台（界面）和控制端服务之间的 WebSocket，桌面版和网页版共用：
//!
//! * 桌面版：控制台窗口（WebView）连本程序里只监听 127.0.0.1 的服务（客户端的 `console.rs`）；
//! * 网页版：浏览器连 airhand-web（`web/src/console.rs`）。
//!
//! 两边只是接受连接的方式不同（检查 Origin、令牌……），连上之后都交给 [`serve`]。每条连接就是一个控制台，
//! 有自己的控制端服务（[`ControllerService`]）和到中转服务器的连接，连接断了它的会话全部断开。
//!
//! 控制台那边，WebSocket、画面和声音的解码都在 Worker 里（`client/src/lib/console-worker.ts`），主线程只处理 JSON。
//!
//! # 协议
//!
//! 控制台 → 服务：
//! * 文本（JSON，camelCase）：`{"id": 1, "cmd": "connect", ...参数}`，命令见 [`Call`]：`appInfo`、`connect`、`ping`，
//!   加上对会话的操作 [`Command`]（和 Android 一样）。带 `id` 的会收到回复；不带的（输入事件这类高频消息）不回复。
//!   一条连接上的命令按收到的先后执行（例如按下和松开）；
//! * 二进制：画面确认每帧都有，不走 JSON：`1u8 | sid u32 LE | seq u32 LE`（[`parse_ack`]），不回复。
//!
//! 服务 → 控制台：
//! * 文本（JSON），见 [`Out`]：
//!   * `{"type": "reply", "id": 1, "ok": 结果}` 或 `{"type": "reply", "id": 1, "err": Problem}`
//!   * `{"type": "server", "state": ServerState}`：与中转服务器的连接状态，连上时和有变化时发
//!   * `{"type": "event", "ch": 3, "event": SessionEvent}`：会话事件
//!   * `{"type": "hostOnline", "code": 123456}`：在等的被控端上线了（见 [`HostOnline`]）
//! * 二进制：`ch u32 LE | 媒体包`（`airhand_proto::media`）
//!
//! `ch` 是控制台在 `connect` 时自己选的编号：会话的第一批数据可能比 `connect` 的回复先到，控制台靠它认出是哪个会话的。
//!
//! 控制台收到画面后发确认（累积确认，连着到的几帧只确认最后一帧），被控端的在途帧控制因此把这一段也算进去。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};
use tokio::task::JoinSet;
use tracing::debug;

use crate::controller::{
    ClipboardSync, Command, ConnectOptions, ControllerService, Frontend, HostOnline, SessionEvent,
    SessionUi, UiClosed,
};
use crate::net::Report;
use crate::secure::Identity;
use crate::{LinkConfig, Problem, ServerState};

/// 控制台发来的单条消息上限（剪贴板文本最多 1 MiB，JSON 转义后可能变大）
pub const MAX_MESSAGE: usize = 4 << 20;
/// 发往控制台的消息最多排这么多条、这么多字节。被控端的在途帧控制让画面不会积压，
/// 排满了说明控制台那边已经停止接收（例如电脑睡眠了），直接断开
const OUTBOX: usize = 1024;
const OUTBOX_BYTES: usize = 32 << 20;
/// WebSocket 心跳：浏览器会自动回应（后台标签页也一样）
const PING_EVERY: Duration = Duration::from_secs(15);
/// 这么久没收到控制台的任何消息（包括心跳回应）就断开
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// 同时进行中的 `connect`
const MAX_CONNECTING: usize = 64;

/// 二进制消息的类型：画面确认
const BINARY_ACK: u8 = 1;

/// WebSocket 的一条消息。两边用的 WebSocket 库不同，各自和它互相转换
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsMessage {
    Text(String),
    Binary(Bytes),
    /// 心跳（收到的 Ping、Pong 都是它）：只用来证明对方还在
    Ping,
    Close,
}

/// 一个控制台用到的、由宿主（桌面版 / 网页版）给的东西
pub struct Setup {
    pub link: Arc<LinkConfig>,
    /// 本机的身份（端到端加密，见 [`crate::controller`] 的"端到端加密"）
    pub identity: Identity,
    /// `appInfo` 的回答
    pub info: Value,
    /// 与中转服务器的连接状态除了告诉控制台，也告诉它（桌面版：托盘）
    pub report: Option<Report>,
    /// 桌面版：本服务同步这台电脑的剪贴板，每个会话一个（[`Frontend::Desktop`]）。
    /// `None`：控制台自己同步（浏览器，[`Frontend::Browser`]）
    pub clipboard: Option<fn() -> Box<dyn ClipboardSync>>,
}

/// 控制台发来的二进制画面确认 `1u8 | sid u32 LE | seq u32 LE` → `(sid, seq)`
pub fn parse_ack(b: &[u8]) -> Option<(u32, u32)> {
    let [BINARY_ACK, s0, s1, s2, s3, q0, q1, q2, q3] = *b else {
        return None;
    };
    Some((
        u32::from_le_bytes([s0, s1, s2, s3]),
        u32::from_le_bytes([q0, q1, q2, q3]),
    ))
}

/// 控制台 → 服务的命令
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Call {
    AppInfo,
    /// 连接被控端：`code`、`ch`、`clipboard`（桌面版：开始时是否同步剪贴板），加上 [`ConnectOptions`] 的字段
    /// （重连凭证、记下的对方公钥、配对码、开始时的画质和声音，和 Android 一样）
    Connect {
        code: String,
        ch: u32,
        #[serde(default)]
        clipboard: bool,
        #[serde(flatten)]
        opts: ConnectOptions,
    },
    /// 测量控制台到这里的往返时间
    Ping,
    /// 对会话的操作（输入、画质……）
    #[serde(untagged)]
    Session(Command),
}

#[derive(Debug, Deserialize)]
struct Request {
    #[serde(default)]
    id: Option<u32>,
    #[serde(flatten)]
    call: Call,
}

/// 服务 → 控制台的文本消息
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Out<'a> {
    Reply {
        id: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        err: Option<Problem>,
    },
    Server {
        state: &'a ServerState,
    },
    Event {
        ch: u32,
        event: &'a SessionEvent,
    },
    HostOnline {
        code: u32,
    },
}

/// 和一个控制台的会话：直到 WebSocket 断开（或者控制台停止响应、不再接收）才返回。
/// 返回时控制端服务随之停止，会话全部断开。正常关闭返回 `Ok`
pub async fn serve<S, E>(socket: S, setup: Setup) -> Result<()>
where
    S: Stream<Item = Result<WsMessage, E>> + Sink<WsMessage, Error = E> + Send,
    E: std::fmt::Display,
{
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Queued>(OUTBOX);
    let overflow = Arc::new(Notify::new());
    let out = Outbox {
        tx,
        overflow: overflow.clone(),
        budget: Arc::new(Semaphore::new(OUTBOX_BYTES)),
    };

    let writer = async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            let (msg, _permit) = tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => (m.message, Some(m.permit)),
                    None => return,
                },
                _ = ping.tick() => (WsMessage::Ping, None),
            };
            if let Err(e) = sink.send(msg).await {
                debug!("sending to the console failed: {e}");
                return;
            }
        }
    };

    let _ = out.json(&Out::Server {
        state: &ServerState::Connecting,
    });
    let report: Report = {
        let out = out.clone();
        let host = setup.report;
        let last = Mutex::new(ServerState::Connecting);
        Arc::new(move |s: ServerState| {
            if let Some(r) = &host {
                r(s.clone());
            }
            let mut last = last.lock().unwrap();
            if *last != s {
                let _ = out.json(&Out::Server { state: &s });
                *last = s;
            }
        })
    };
    let host_online: HostOnline = {
        let out = out.clone();
        Arc::new(move |code| {
            let _ = out.json(&Out::HostOnline { code });
        })
    };
    let mut console = Console {
        svc: Arc::new(ControllerService::start(
            setup.link,
            Handle::current(),
            report,
            host_online,
            setup.identity,
        )),
        out,
        info: setup.info,
        clipboard: setup.clipboard,
        tasks: JoinSet::new(),
    };

    let reader = async {
        loop {
            let msg = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Err(_) => bail!("the console stopped responding"),
                Ok(None) => return Ok(()),
                Ok(Some(Ok(m))) => m,
                // 多半是标签页被关掉、网络断了（没有正常的关闭握手），和正常关闭一样处理
                Ok(Some(Err(e))) => {
                    debug!("receiving from the console failed: {e}");
                    return Ok(());
                }
            };
            match msg {
                WsMessage::Text(text) => console.handle(&text),
                WsMessage::Binary(b) => {
                    if let Some((sid, seq)) = parse_ack(&b) {
                        let _ = console.svc.ack(sid, seq);
                    }
                }
                WsMessage::Close => return Ok(()),
                WsMessage::Ping => {}
            }
        }
    };

    tokio::select! {
        r = reader => r,
        () = writer => Ok(()),
        _ = overflow.notified() => Err(anyhow!("the console isn't receiving anymore")),
    }
    // console 在这里释放：中止进行中的 connect，断开所有会话和到中转服务器的连接
}

struct Queued {
    message: WsMessage,
    permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct Outbox {
    tx: mpsc::Sender<Queued>,
    overflow: Arc<Notify>,
    /// 排着的字节数
    budget: Arc<Semaphore>,
}

impl Outbox {
    fn push(&self, message: WsMessage) -> Result<(), UiClosed> {
        let len = match &message {
            WsMessage::Text(t) => t.len(),
            WsMessage::Binary(b) => b.len(),
            WsMessage::Ping | WsMessage::Close => 0,
        };
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(len.clamp(1, u32::MAX as usize) as u32)
            .map_err(|_| {
                self.overflow.notify_one();
                UiClosed
            })?;
        match self.tx.try_send(Queued { message, permit }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.overflow.notify_one();
                Err(UiClosed)
            }
            Err(TrySendError::Closed(_)) => Err(UiClosed),
        }
    }

    fn json(&self, v: &impl Serialize) -> Result<(), UiClosed> {
        let text = serde_json::to_string(v).map_err(|_| UiClosed)?;
        self.push(WsMessage::Text(text))
    }

    fn reply(&self, id: Option<u32>, result: Result<Value, Problem>) {
        let Some(id) = id else { return };
        let (ok, err) = match result {
            Ok(v) => (Some(v), None),
            Err(e) => (None, Some(e)),
        };
        let _ = self.json(&Out::Reply { id, ok, err });
    }
}

/// 一个会话的输出：带上控制台选的 `ch` 发给控制台
struct ConsoleUi {
    ch: u32,
    out: Outbox,
}

impl SessionUi for ConsoleUi {
    fn packet(&self, mut packet: Vec<u8>) -> Result<(), UiClosed> {
        // 解密后原缓冲还留着标签和计数（24 字节）的容量，直接用它放通道号，避免再分配整包
        let len = packet.len();
        packet.resize(len + 4, 0);
        packet.copy_within(..len, 4);
        packet[..4].copy_from_slice(&self.ch.to_le_bytes());
        self.out.push(WsMessage::Binary(Bytes::from(packet)))
    }

    fn event(&self, event: SessionEvent) -> Result<(), UiClosed> {
        self.out.json(&Out::Event {
            ch: self.ch,
            event: &event,
        })
    }
}

struct Console {
    svc: Arc<ControllerService>,
    out: Outbox,
    info: Value,
    clipboard: Option<fn() -> Box<dyn ClipboardSync>>,
    /// 进行中的 `connect`；控制台关闭时一起中止
    tasks: JoinSet<()>,
}

impl Console {
    fn handle(&mut self, text: &str) {
        let req: Request = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                debug!("bad message from the console: {e}");
                // 能认出 id 的话告诉控制台，免得它一直等
                #[derive(Deserialize)]
                struct Id {
                    id: Option<u32>,
                }
                if let Ok(Id { id }) = serde_json::from_str(text) {
                    self.out.reply(id, Err(Problem::failed(e)));
                }
                return;
            }
        };
        let result = match req.call {
            Call::AppInfo => Ok(self.info.clone()),
            Call::Connect {
                code,
                ch,
                clipboard,
                opts,
            } => {
                // 顺便回收已经结束的
                while self.tasks.try_join_next().is_some() {}
                if self.tasks.len() >= MAX_CONNECTING {
                    self.out
                        .reply(req.id, Err(Problem::failed("too many pending connections")));
                    return;
                }
                let frontend = match self.clipboard {
                    Some(new) => Frontend::Desktop {
                        clipboard: new(),
                        sync: clipboard,
                    },
                    None => Frontend::Browser,
                };
                let (svc, out) = (self.svc.clone(), self.out.clone());
                self.tasks.spawn(async move {
                    let ui = Arc::new(ConsoleUi {
                        ch,
                        out: out.clone(),
                    });
                    let r = svc.connect(&code, opts, ui, frontend).await;
                    out.reply(
                        req.id,
                        r.and_then(|info| serde_json::to_value(info).map_err(Problem::failed)),
                    );
                });
                return;
            }
            Call::Ping => Ok(Value::Null),
            Call::Session(c) => self.svc.command(c).map(|()| Value::Null),
        };
        self.out.reply(req.id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::{Codec, InputEvent, QualitySettings};
    use serde_json::json;
    use std::convert::Infallible;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// 前端（client/src/lib/ack.ts 的 ackMessage）发的二进制确认
    #[test]
    fn binary_acks() {
        assert_eq!(parse_ack(&[1, 3, 0, 0, 0, 0x2a, 1, 0, 0]), Some((3, 298)));
        assert_eq!(parse_ack(&[2, 3, 0, 0, 0, 0x2a, 1, 0, 0]), None);
        assert_eq!(parse_ack(&[1, 3, 0, 0, 0]), None);
        assert_eq!(parse_ack(&[]), None);
    }

    fn outbox(messages: usize, bytes: usize) -> (Outbox, mpsc::Receiver<Queued>) {
        let (tx, rx) = mpsc::channel(messages);
        let out = Outbox {
            tx,
            overflow: Arc::new(Notify::new()),
            budget: Arc::new(Semaphore::new(bytes)),
        };
        (out, rx)
    }

    #[tokio::test]
    async fn media_envelope_reuses_the_decrypted_buffer() {
        let (out, mut rx) = outbox(1, 1024);
        let ui = ConsoleUi { ch: 7, out };
        let mut packet = Vec::with_capacity(3 + airhand_proto::handshake::SEAL_OVERHEAD);
        packet.extend_from_slice(&[1, 2, 3]);
        let ptr = packet.as_ptr();
        ui.packet(packet).unwrap();
        let WsMessage::Binary(bytes) = rx.recv().await.unwrap().message else {
            panic!()
        };
        assert_eq!(bytes.as_ref(), &[7, 0, 0, 0, 1, 2, 3]);
        assert_eq!(bytes.as_ptr(), ptr);
    }

    #[tokio::test]
    async fn outbox_limits_bytes_and_releases_budget_on_drop() {
        let (out, mut rx) = outbox(8, 10);
        let six = || WsMessage::Binary(Bytes::from_static(b"123456"));
        out.push(six()).unwrap();
        assert!(out.push(six()).is_err());
        drop(rx.recv().await);
        out.push(six()).unwrap();
    }

    #[test]
    fn parse_requests() {
        let r: Request = serde_json::from_value(json!({
            "id": 7, "cmd": "connect", "code": "123 456", "ch": 3, "audio": true, "clipboard": true,
            "settings": { "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "av1" }
        }))
        .unwrap();
        assert_eq!(r.id, Some(7));
        let Call::Connect {
            code,
            ch,
            clipboard,
            opts,
        } = r.call
        else {
            panic!("{:?}", r.call)
        };
        assert_eq!((code.as_str(), ch, clipboard), ("123 456", 3, true));
        assert_eq!(
            (opts.resume, opts.host_key, opts.pairing),
            (None, None, None)
        );
        assert_eq!(
            opts.settings,
            QualitySettings {
                fps: 60,
                bitrate_kbps: 8000,
                max_short: 1080,
                adaptive: true,
                codec: Codec::AV1
            }
        );
        assert!(opts.audio);

        // 重连：带上凭证。和 Android 共用的连接选项样例（tests/protocol.json）
        let fixture: Value = serde_json::from_str(include_str!("../tests/protocol.json")).unwrap();
        let mut connect = fixture["connectOptions"].clone();
        connect["id"] = json!(8);
        connect["cmd"] = json!("connect");
        connect["code"] = json!("123456");
        connect["ch"] = json!(4);
        let r: Request = serde_json::from_value(connect).unwrap();
        let Call::Connect {
            opts, clipboard, ..
        } = r.call
        else {
            panic!("{:?}", r.call)
        };
        assert!(!clipboard);
        assert_eq!(
            opts,
            serde_json::from_value::<ConnectOptions>(fixture["connectOptions"].clone()).unwrap()
        );
        assert!(opts.resume.is_some() && opts.host_key.is_some() && opts.pairing.is_some());

        // 不带 id：不回复
        let r: Request = serde_json::from_value(json!({
            "cmd": "input", "sid": 1,
            "events": [{ "type": "mouseMove", "x": 0.5, "y": 0.25 }, { "type": "mouseDelta", "dx": -2, "dy": 3 }, { "type": "releaseAll" }]
        }))
        .unwrap();
        assert_eq!(r.id, None);
        let Call::Session(Command::Input { sid: 1, events }) = r.call else {
            panic!("{:?}", r.call)
        };
        assert_eq!(
            events,
            vec![
                InputEvent::MouseMove { x: 0.5, y: 0.25 },
                InputEvent::MouseDelta { dx: -2, dy: 3 },
                InputEvent::ReleaseAll
            ]
        );

        let r: Request = serde_json::from_str(r#"{"id":1,"cmd":"appInfo"}"#).unwrap();
        assert!(matches!(r.call, Call::AppInfo));
        let r: Request = serde_json::from_str(r#"{"cmd":"ack","sid":2,"seq":99}"#).unwrap();
        assert!(matches!(
            r.call,
            Call::Session(Command::Ack { sid: 2, seq: 99 })
        ));
        let r: Request = serde_json::from_str(r#"{"id":4,"cmd":"ping"}"#).unwrap();
        assert!(matches!(r.call, Call::Ping));
        assert!(serde_json::from_str::<Request>(r#"{"id":1,"cmd":"nope"}"#).is_err());

        // 和 Android 共用的命令样例
        for c in fixture["commands"].as_array().unwrap() {
            let mut with_id = c.clone();
            with_id["id"] = json!(9);
            let r: Request = serde_json::from_value(with_id).unwrap_or_else(|e| panic!("{c}: {e}"));
            assert_eq!(r.id, Some(9));
            assert!(matches!(r.call, Call::Session(_)), "{c}");
        }
    }

    /// 前端（client/src/lib/console.ts）依赖这个 JSON 形状
    #[test]
    fn output_shape() {
        let v = |o: &Out| serde_json::to_value(o).unwrap();
        assert_eq!(
            v(&Out::Reply {
                id: 1,
                ok: Some(Value::Null),
                err: None
            }),
            json!({ "type": "reply", "id": 1, "ok": null })
        );
        assert_eq!(
            v(&Out::Reply {
                id: 2,
                ok: None,
                err: Some(Problem::CodeNotFound)
            }),
            json!({ "type": "reply", "id": 2, "err": { "kind": "codeNotFound" } })
        );
        assert_eq!(
            v(&Out::Server {
                state: &ServerState::Connected
            }),
            json!({ "type": "server", "state": { "state": "connected" } })
        );
        assert_eq!(
            v(&Out::Event {
                ch: 3,
                event: &SessionEvent::Clipboard { text: "x".into() }
            }),
            json!({ "type": "event", "ch": 3, "event": { "type": "clipboard", "text": "x" } })
        );
        assert_eq!(
            v(&Out::HostOnline { code: 123_456 }),
            json!({ "type": "hostOnline", "code": 123_456 })
        );
    }

    /// 内存里的 WebSocket：测试往 `tx` 里放控制台发的，从 `rx` 里取服务发的
    struct Pipe {
        rx: mpsc::UnboundedReceiver<WsMessage>,
        tx: mpsc::UnboundedSender<WsMessage>,
    }

    impl Stream for Pipe {
        type Item = Result<WsMessage, Infallible>;
        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.rx.poll_recv(cx).map(|m| m.map(Ok))
        }
    }

    impl Sink<WsMessage> for Pipe {
        type Error = Infallible;
        fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(self: Pin<&mut Self>, m: WsMessage) -> Result<(), Infallible> {
            let _ = self.tx.send(m);
            Ok(())
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
            Poll::Ready(Ok(()))
        }
    }

    /// 一条连接从头到尾：先告诉控制台服务器的状态，命令按顺序回复，坏消息能认出 id 的也回复，关闭时 serve 返回
    #[tokio::test]
    async fn serves_a_console() {
        let (to_console, mut from_service) = mpsc::unbounded_channel();
        let (to_service, from_console) = mpsc::unbounded_channel();
        let pipe = Pipe {
            rx: from_console,
            tx: to_console,
        };
        // 连不上的服务器：只会一直重试，不影响命令
        let link = Arc::new(LinkConfig::new("127.0.0.1:9", "", "test", "test").unwrap());
        let setup = Setup {
            link,
            identity: Identity::generate(),
            info: json!({ "version": "1" }),
            report: None,
            clipboard: None,
        };
        let served = tokio::spawn(serve(pipe, setup));
        let mut next = async || match from_service.recv().await.unwrap() {
            WsMessage::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
            m => panic!("{m:?}"),
        };
        assert_eq!(
            next().await,
            json!({ "type": "server", "state": { "state": "connecting" } })
        );
        for m in [
            r#"{"id":1,"cmd":"appInfo"}"#,
            r#"{"id":2,"cmd":"ping"}"#,
            r#"{"id":3,"cmd":"keyframe","sid":9}"#,
            r#"{"id":4,"cmd":"nope"}"#,
            r#"{"cmd":"input","sid":9,"events":[]}"#,
            r#"{"id":5,"cmd":"ping"}"#,
        ] {
            to_service.send(WsMessage::Text(m.into())).unwrap();
        }
        to_service
            .send(WsMessage::Binary(Bytes::from_static(&[
                1, 9, 0, 0, 0, 1, 0, 0, 0,
            ])))
            .unwrap();
        let mut replies = Vec::new();
        while replies.len() < 5 {
            let m = next().await;
            if m["type"] == "reply" {
                replies.push(m);
            }
        }
        assert_eq!(
            replies[0],
            json!({ "type": "reply", "id": 1, "ok": { "version": "1" } })
        );
        assert_eq!(replies[1], json!({ "type": "reply", "id": 2, "ok": null }));
        assert_eq!(
            replies[2],
            json!({ "type": "reply", "id": 3, "err": { "kind": "noSession" } })
        );
        assert_eq!(
            (&replies[3]["id"], &replies[3]["err"]["kind"]),
            (&json!(4), &json!("failed"))
        );
        // 不带 id 的输入没有回复
        assert_eq!(replies[4], json!({ "type": "reply", "id": 5, "ok": null }));
        to_service.send(WsMessage::Close).unwrap();
        served.await.unwrap().unwrap();
    }
}
