//! 控制台窗口（WebView）和控制端服务之间的 WebSocket。
//!
//! 协议和网页版一样（见 `airhand_link::console`）：画面、声音从 WebSocket 直接进到 WebView 的 Worker 里解码、绘制，
//! 命令按顺序到达，都不经过 Tauri 的 IPC（它的命令和 Channel 都要在主线程上来回，画面多了会挡住输入和确认）。
//!
//! * 只监听 127.0.0.1，控制台第一次要地址（`console_url` 命令）时才开始监听；
//! * 地址里带着随机的令牌，只有控制台页面拿得到；还只接受控制台页面的 Origin：本机的其它程序、浏览器里的网页都连不上来；
//! * 每条连接就是一个控制台，有自己的控制端服务和到中转服务器的连接（状态也报给托盘）；切回被控端时一起断开。

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use airhand_link::console::{self, MAX_MESSAGE, Setup, WsMessage};
use futures_util::{SinkExt, StreamExt, future};
use tauri::{AppHandle, Manager};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header::ORIGIN;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{info, warn};

use crate::status::{self, Mode};
use crate::{AppState, clipboard, config};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 打包进程序的控制台页面的 Origin：Tauri 的自定义协议（Windows 上是 `http://tauri.localhost`，
/// macOS / Linux 上是 `tauri://localhost`）
const CONSOLE_ORIGINS: [&str; 3] = [
    "http://tauri.localhost",
    "https://tauri.localhost",
    "tauri://localhost",
];

/// 谁能连上来
struct Gate {
    /// 地址里的随机令牌
    token: String,
    /// 调试构建：Vite 开发服务器的 Origin（tauri.conf.json 的 `devUrl`）
    dev_origin: Option<String>,
}

impl Gate {
    /// 握手请求是控制台页面发来的：地址里的令牌对，Origin 是控制台页面的
    fn allows(&self, req: &Request) -> bool {
        let token_ok = req
            .uri()
            .path()
            .strip_prefix('/')
            .is_some_and(|t| same(t.as_bytes(), self.token.as_bytes()));
        let origin = req.headers().get(ORIGIN).and_then(|o| o.to_str().ok());
        token_ok
            && origin.is_some_and(|o| {
                CONSOLE_ORIGINS.contains(&o) || self.dev_origin.as_deref() == Some(o)
            })
    }
}

/// 开始监听，返回控制台连过来的地址 `ws://127.0.0.1:端口/令牌`
pub async fn listen(app: AppHandle) -> std::io::Result<String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let gate = Arc::new(Gate {
        token: rand::random::<[u8; 16]>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        dev_origin: cfg!(debug_assertions)
            .then(|| app.config().build.dev_url.as_ref())
            .flatten()
            .map(|u| u.origin().ascii_serialization()),
    });
    let url = format!("ws://{}/{}", listener.local_addr()?, gate.token);
    tauri::async_runtime::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((tcp, _)) => {
                    tauri::async_runtime::spawn(connection(tcp, gate.clone(), app.clone()));
                }
                Err(e) => {
                    warn!("accepting a console connection failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(url)
}

async fn connection(tcp: TcpStream, gate: Arc<Gate>, app: AppHandle) {
    // 输入事件、画面确认都是小消息，不能被 Nagle 算法攒着（本机回环也一样）
    let _ = tcp.set_nodelay(true);
    // 拒绝时的回应是 tungstenite 定的类型
    #[allow(clippy::result_large_err)]
    let check = |req: &Request, res: Response| {
        if gate.allows(req) {
            return Ok(res);
        }
        warn!(origin = ?req.headers().get(ORIGIN), "rejected a console connection");
        let mut forbidden = ErrorResponse::new(None);
        *forbidden.status_mut() = StatusCode::FORBIDDEN;
        Err(forbidden)
    };
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE));
    let accepted = tokio_tungstenite::accept_hdr_async_with_config(tcp, check, Some(config));
    let Ok(Ok(ws)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, accepted).await else {
        return;
    };

    let state = app.state::<AppState>();
    // 控制端模式才有控制台的身份（见 switch_services）
    let Some(identity) = state.services.lock().unwrap().controller.clone() else {
        return;
    };
    let mut mode = state.hub.subscribe();
    let setup = Setup {
        link: config::link(),
        identity,
        info: serde_json::to_value(crate::app_info()).unwrap_or_default(),
        report: Some(status::controller_reporter(&state.hub)),
        clipboard: Some(|| Box::new(clipboard::ClipSync::new())),
    };
    let socket = ws
        .with(|m| future::ready(Ok::<_, tungstenite::Error>(to_tungstenite(m))))
        .map(|m| m.map(from_tungstenite));
    info!("console connected");
    tokio::select! {
        r = console::serve(socket, setup) => match r {
            Ok(()) => info!("console disconnected"),
            Err(e) => info!("console disconnected: {e:#}"),
        },
        // 切回被控端：控制端服务和它的会话马上停下
        _ = mode.wait_for(|s| s.mode != Mode::Controller) => {}
    }
}

/// 比较令牌：花的时间和哪个字节不同无关
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

fn from_tungstenite(m: Message) -> WsMessage {
    match m {
        Message::Text(t) => WsMessage::Text(t.as_str().to_owned()),
        Message::Binary(b) => WsMessage::Binary(b),
        Message::Close(_) => WsMessage::Close,
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => WsMessage::Ping,
    }
}

fn to_tungstenite(m: WsMessage) -> Message {
    match m {
        WsMessage::Text(t) => Message::Text(t.into()),
        WsMessage::Binary(b) => Message::Binary(b),
        WsMessage::Ping => Message::Ping(Default::default()),
        WsMessage::Close => Message::Close(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }

    /// 只有带着令牌、从控制台页面发来的握手才接受
    #[test]
    fn only_the_console_page_gets_in() {
        let gate = Gate {
            token: "0123abcd".into(),
            dev_origin: Some("http://localhost:1420".into()),
        };
        let req = |path: &str, origin: Option<&str>| {
            let mut b = Request::builder().uri(path);
            if let Some(o) = origin {
                b = b.header(ORIGIN, o);
            }
            b.body(()).unwrap()
        };
        for origin in CONSOLE_ORIGINS.into_iter().chain(["http://localhost:1420"]) {
            assert!(gate.allows(&req("/0123abcd", Some(origin))), "{origin}");
        }
        // 令牌不对
        assert!(!gate.allows(&req("/0123abce", Some("tauri://localhost"))));
        assert!(!gate.allows(&req("/", Some("tauri://localhost"))));
        assert!(!gate.allows(&req("/0123abcd/x", Some("tauri://localhost"))));
        // 别的网页、不带 Origin 的（不是浏览器）
        assert!(!gate.allows(&req("/0123abcd", Some("https://evil.example"))));
        assert!(!gate.allows(&req("/0123abcd", Some("null"))));
        assert!(!gate.allows(&req("/0123abcd", None)));
        // 发布版没有开发服务器
        let release = Gate {
            dev_origin: None,
            ..gate
        };
        assert!(!release.allows(&req("/0123abcd", Some("http://localhost:1420"))));
    }

    #[test]
    fn message_conversion() {
        for m in [
            WsMessage::Text("{}".into()),
            WsMessage::Binary(vec![1, 2].into()),
            WsMessage::Ping,
            WsMessage::Close,
        ] {
            assert_eq!(from_tungstenite(to_tungstenite(m.clone())), m);
        }
    }
}
