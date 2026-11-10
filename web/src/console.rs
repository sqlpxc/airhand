//! 浏览器控制台的 WebSocket（`/ws`）。每个连接就是一个控制台（一个浏览器标签页），
//! 有自己的控制端服务和到中转服务器的连接，标签页关掉时它的会话全部断开。
//!
//! # 协议
//!
//! 浏览器 → 服务：文本（JSON，camelCase），`{"id": 1, "cmd": "connect", ...参数}`，命令见 [`Call`]：网页版自己的几个，
//! 加上对会话的操作 [`Command`]（和桌面版、Android 一样）。
//! 带 `id` 的会收到回复；不带的（输入事件这类高频消息）不回复。
//! 画面确认每帧都有，不走 JSON：二进制 `1u8 | sid u32 LE | seq u32 LE`（[`parse_ack`]），不回复。
//!
//! 服务 → 浏览器：
//! * 文本（JSON），见 [`Out`]：
//!   * `{"type": "reply", "id": 1, "ok": 结果}` 或 `{"type": "reply", "id": 1, "err": Problem}`
//!   * `{"type": "server", "state": ServerState}`：与中转服务器的连接状态，有变化就发
//!   * `{"type": "event", "ch": 3, "event": SessionEvent}`：会话事件
//!   * `{"type": "hostOnline", "code": 123456}`：在等的被控端上线了（见 `airhand_link::controller::HostOnline`）
//! * 二进制：`ch u32 LE | 媒体包`，媒体包与桌面版相同（见 `airhand_proto::media`）
//!
//! `ch` 是浏览器在 `connect` 时自己选的编号（相当于桌面版的 Tauri Channel）：会话的第一批数据
//! 可能比 `connect` 的回复先到，浏览器靠它认出是哪个会话的。
//!
//! 浏览器收到画面后发确认（累积确认，连着到的几帧只确认最后一帧），被控端的在途帧控制因此把浏览器这段网络也算进去
//!（见 `airhand_link::controller::Frontend::Browser`）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use airhand_link::controller::{
    Command, ConnectOptions, ControllerService, Frontend, HostOnline, SessionEvent, SessionUi,
    UiClosed,
};
use airhand_link::net::Report;
use airhand_link::{Problem, ServerState};
use anyhow::{anyhow, bail};
use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::http::App;

/// 发往浏览器的消息最多排这么多条。被控端的在途帧控制让画面不会积压，
/// 排满了说明浏览器那边已经停止接收（例如电脑睡眠了），直接断开
const OUTBOX: usize = 1024;
/// WebSocket 心跳：浏览器会自动回应（后台标签页也一样）
const PING_EVERY: Duration = Duration::from_secs(15);
/// 这么久没收到浏览器的任何消息（包括心跳回应）就断开
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);
/// 浏览器发来的单条消息上限（剪贴板文本最多 1 MiB，JSON 转义后可能变大）
const MAX_MESSAGE: usize = 4 << 20;

/// 二进制消息的类型：画面确认
const BINARY_ACK: u8 = 1;

/// 浏览器发来的二进制画面确认 `1u8 | sid u32 LE | seq u32 LE` → `(sid, seq)`
fn parse_ack(b: &[u8]) -> Option<(u32, u32)> {
    let [BINARY_ACK, s0, s1, s2, s3, q0, q1, q2, q3] = *b else {
        return None;
    };
    Some((
        u32::from_le_bytes([s0, s1, s2, s3]),
        u32::from_le_bytes([q0, q1, q2, q3]),
    ))
}

/// 浏览器 → 服务的命令
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Call {
    AppInfo,
    ServerStatus,
    /// 连接被控端：`code`、`ch`，加上 [`ConnectOptions`] 的字段（重连凭证、记下的对方公钥、配对码、
    /// 开始时的画质和声音，和桌面版、Android 一样）
    Connect {
        code: String,
        ch: u32,
        #[serde(flatten)]
        opts: ConnectOptions,
    },
    /// 测量浏览器到这里的往返时间
    Ping,
    /// 对会话的操作（输入、画质、确认画面……）
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

/// 服务 → 浏览器的文本消息
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Out<'a> {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo<'a> {
    version: &'static str,
    server: &'a str,
    pinned: bool,
    /// 被控端看到的控制端名称
    hostname: &'a str,
}

pub async fn upgrade(
    ws: WebSocketUpgrade,
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !same_origin(&headers) {
        warn!(%peer, origin = ?headers.get(ORIGIN), "rejected a WebSocket opened by another website");
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| run(socket, app, peer))
}

/// 防止别的网站在用户的浏览器里偷偷连上来（浏览器总会带上 Origin）。
/// 经过反向代理时，代理需要保留 Host，或者设置 X-Forwarded-Host
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(ORIGIN) else {
        // 不是浏览器
        return true;
    };
    let Some(origin) = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, host)| host)
    else {
        return false;
    };
    [HOST.as_str(), "x-forwarded-host"]
        .into_iter()
        .filter_map(|h| headers.get(h)?.to_str().ok())
        .filter_map(|v| v.split(',').next())
        .any(|h| h.trim().eq_ignore_ascii_case(origin))
}

struct QueuedMessage {
    message: Message,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct Outbox {
    tx: mpsc::Sender<QueuedMessage>,
    overflow: Arc<Notify>,
    budget: Arc<Semaphore>,
}

impl Outbox {
    fn push(&self, m: Message) -> Result<(), UiClosed> {
        let len = match &m {
            Message::Text(t) => t.len(),
            Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
            Message::Close(_) => 128,
        };
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(len.max(1).min(u32::MAX as usize) as u32)
            .map_err(|_| {
                self.overflow.notify_one();
                UiClosed
            })?;
        match self.tx.try_send(QueuedMessage {
            message: m,
            _bytes: permit,
        }) {
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
        self.push(Message::Text(text.into()))
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

/// 一个会话的输出：带上浏览器选的 `ch` 发给浏览器
struct WebUi {
    ch: u32,
    out: Outbox,
}

impl SessionUi for WebUi {
    fn packet(&self, packet: Vec<u8>) -> Result<(), UiClosed> {
        let mut framed = Vec::with_capacity(4 + packet.len());
        framed.extend_from_slice(&self.ch.to_le_bytes());
        framed.extend_from_slice(&packet);
        self.out.push(Message::Binary(Bytes::from(framed)))
    }

    fn event(&self, event: SessionEvent) -> Result<(), UiClosed> {
        self.out.json(&Out::Event {
            ch: self.ch,
            event: &event,
        })
    }
}

struct Console {
    app: Arc<App>,
    svc: Arc<ControllerService>,
    out: Outbox,
    state: watch::Receiver<ServerState>,
    /// 进行中的 `connect`；控制台关闭时一起中止
    tasks: JoinSet<()>,
}

async fn run(socket: WebSocket, app: Arc<App>, peer: SocketAddr) {
    info!(%peer, "console opened");
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel(OUTBOX);
    let overflow = Arc::new(Notify::new());
    let out = Outbox {
        tx,
        overflow: overflow.clone(),
        budget: Arc::new(Semaphore::new(32 * 1024 * 1024)),
    };

    let writer = async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            let (msg, _permit) = tokio::select! {
                m = rx.recv() => match m {
                    Some(m) => (m.message, Some(m._bytes)),
                    None => break,
                },
                _ = ping.tick() => (Message::Ping(Bytes::new()), None),
            };
            if let Err(e) = sink.send(msg).await {
                debug!(%peer, "sending to the browser failed: {e}");
                break;
            }
        }
    };

    let (state_tx, state) = watch::channel(ServerState::Connecting);
    let _ = out.json(&Out::Server {
        state: &ServerState::Connecting,
    });
    let report: Report = {
        let out = out.clone();
        Arc::new(move |s: ServerState| {
            let changed = state_tx.send_if_modified(|cur| {
                let changed = *cur != s;
                *cur = s.clone();
                changed
            });
            if changed {
                let _ = out.json(&Out::Server { state: &s });
            }
        })
    };
    let host_online: HostOnline = {
        let out = out.clone();
        Arc::new(move |code| {
            let _ = out.json(&Out::HostOnline { code });
        })
    };
    let svc = Arc::new(ControllerService::start(
        app.link.clone(),
        Handle::current(),
        report,
        host_online,
        app.identity.clone(),
    ));
    let mut console = Console {
        app,
        svc,
        out,
        state,
        tasks: JoinSet::new(),
    };

    let reader = async {
        loop {
            let msg = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
                Err(_) => bail!("the browser stopped responding"),
                Ok(None) => return Ok(()),
                Ok(Some(Ok(m))) => m,
                // 多半是标签页被关掉、网络断了（没有正常的关闭握手），和正常关闭一样处理
                Ok(Some(Err(e))) => {
                    debug!(%peer, "receiving from the browser failed: {e}");
                    return Ok(());
                }
            };
            match msg {
                Message::Text(text) => console.handle(&text),
                Message::Binary(b) => {
                    if let Some((sid, seq)) = parse_ack(&b) {
                        let _ = console.svc.ack(sid, seq);
                    }
                }
                Message::Close(_) => return Ok(()),
                // 心跳回应等：只用来证明浏览器还在
                _ => {}
            }
        }
    };

    let result = tokio::select! {
        r = reader => r,
        () = writer => Ok(()),
        _ = overflow.notified() => Err(anyhow!("the browser isn't receiving anymore")),
    };
    match result {
        Ok(()) => info!(%peer, "console closed"),
        Err(e) => info!(%peer, "console closed: {e:#}"),
    }
    // console 在这里释放：中止进行中的 connect，断开所有会话和到中转服务器的连接
}

fn unit(r: Result<(), Problem>) -> Result<Value, Problem> {
    r.map(|()| Value::Null)
}

fn to_value(v: impl Serialize) -> Result<Value, Problem> {
    serde_json::to_value(v).map_err(Problem::failed)
}

impl Console {
    fn handle(&mut self, text: &str) {
        let req: Request = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                debug!("bad message from the browser: {e}");
                // 能认出 id 的话告诉浏览器，免得它一直等
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
        let svc = &self.svc;
        let result = match req.call {
            Call::AppInfo => to_value(AppInfo {
                version: env!("CARGO_PKG_VERSION"),
                server: &self.app.link.server,
                pinned: self.app.link.pinned(),
                hostname: &self.app.link.name,
            }),
            Call::ServerStatus => to_value(&*self.state.borrow()),
            Call::Connect { code, ch, opts } => {
                while self.tasks.try_join_next().is_some() {}
                if self.tasks.len() >= 64 {
                    self.out
                        .reply(req.id, Err(Problem::failed("too many pending connections")));
                    return;
                }
                let (svc, out) = (self.svc.clone(), self.out.clone());
                self.tasks.spawn(async move {
                    let ui = Arc::new(WebUi {
                        ch,
                        out: out.clone(),
                    });
                    let r = svc.connect(&code, opts, ui, Frontend::Browser).await;
                    out.reply(req.id, r.and_then(to_value));
                });
                // 顺便回收已经结束的
                while self.tasks.try_join_next().is_some() {}
                return;
            }
            Call::Ping => Ok(Value::Null),
            Call::Session(c) => unit(svc.command(c)),
        };
        self.out.reply(req.id, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::{Codec, InputEvent, QualitySettings};
    use axum::http::HeaderValue;
    use serde_json::json;

    /// 前端（client/src/lib/backend/web.ts 的 ackMessage）发的二进制确认
    #[test]
    fn binary_acks() {
        assert_eq!(parse_ack(&[1, 3, 0, 0, 0, 0x2a, 1, 0, 0]), Some((3, 298)));
        assert_eq!(parse_ack(&[2, 3, 0, 0, 0, 0x2a, 1, 0, 0]), None);
        assert_eq!(parse_ack(&[1, 3, 0, 0, 0]), None);
        assert_eq!(parse_ack(&[]), None);
    }

    #[tokio::test]
    async fn outbox_limits_bytes_and_releases_budget_on_drop() {
        let (tx, mut rx) = mpsc::channel(8);
        let out = Outbox {
            tx,
            overflow: Arc::new(Notify::new()),
            budget: Arc::new(Semaphore::new(10)),
        };
        out.push(Message::Binary(Bytes::from_static(b"123456")))
            .unwrap();
        assert!(
            out.push(Message::Binary(Bytes::from_static(b"123456")))
                .is_err()
        );
        drop(rx.recv().await);
        out.push(Message::Binary(Bytes::from_static(b"123456")))
            .unwrap();
    }

    #[test]
    fn parse_requests() {
        let r: Request = serde_json::from_value(json!({
            "id": 7, "cmd": "connect", "code": "123 456", "ch": 3, "audio": true,
            "settings": { "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "av1" }
        }))
        .unwrap();
        assert_eq!(r.id, Some(7));
        let Call::Connect { code, ch, opts } = r.call else {
            panic!("{:?}", r.call)
        };
        assert_eq!((code.as_str(), ch, opts.resume), ("123 456", 3, None));
        assert_eq!((opts.host_key, opts.pairing), (None, None));
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

        // 重连：带上凭证。和桌面版、Android 共用的连接选项样例（crates/airhand-link/tests/protocol.json）
        let fixture: Value = serde_json::from_str(include_str!(
            "../../crates/airhand-link/tests/protocol.json"
        ))
        .unwrap();
        let mut connect = fixture["connectOptions"].clone();
        connect["id"] = json!(8);
        connect["cmd"] = json!("connect");
        connect["code"] = json!("123456");
        connect["ch"] = json!(4);
        let r: Request = serde_json::from_value(connect).unwrap();
        let Call::Connect { opts, .. } = r.call else {
            panic!("{:?}", r.call)
        };
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

        // 和桌面版、Android 共用的命令样例
        for c in fixture["commands"].as_array().unwrap() {
            let mut with_id = c.clone();
            with_id["id"] = json!(9);
            let r: Request = serde_json::from_value(with_id).unwrap_or_else(|e| panic!("{c}: {e}"));
            assert_eq!(r.id, Some(9));
            assert!(matches!(r.call, Call::Session(_)), "{c}");
        }
    }

    /// 前端（client/src/lib/backend/web.ts）依赖这个 JSON 形状
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
        assert_eq!(
            v(&Out::Reply {
                id: 3,
                ok: None,
                err: Some(Problem::HostOffline)
            }),
            json!({ "type": "reply", "id": 3, "err": { "kind": "hostOffline" } })
        );
    }

    #[test]
    fn origin_check() {
        let h = |pairs: &[(&str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            m
        };
        assert!(same_origin(&h(&[("host", "nas:8080")])));
        assert!(same_origin(&h(&[
            ("host", "nas:8080"),
            ("origin", "https://nas:8080")
        ])));
        assert!(same_origin(&h(&[
            ("host", "NAS:8080"),
            ("origin", "http://nas:8080")
        ])));
        assert!(!same_origin(&h(&[
            ("host", "localhost:8080"),
            ("origin", "https://evil.example")
        ])));
        assert!(!same_origin(&h(&[
            ("host", "localhost:8080"),
            ("origin", "http://localhost:8081")
        ])));
        assert!(!same_origin(&h(&[
            ("host", "nas:8080"),
            ("origin", "null")
        ])));
        // 反向代理改了 Host，但带了 X-Forwarded-Host
        assert!(same_origin(&h(&[
            ("host", "127.0.0.1:8080"),
            ("x-forwarded-host", "airhand.example.com"),
            ("origin", "https://airhand.example.com")
        ])));
    }
}
