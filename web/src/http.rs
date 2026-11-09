//! HTTP(S) 服务：控制台网页（静态文件）+ WebSocket（`/ws`）。
//!
//! 同一个端口同时支持 HTTP 和 HTTPS：看连接的第一个字节，是 TLS 握手（0x16）就按 HTTPS 处理。
//! 本机用 `http://localhost:端口`（浏览器把 localhost 当作安全页面），其它电脑用 `https://地址:端口`。

use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use airhand_link::LinkConfig;
use axum::Router;
use axum::extract::{ConnectInfo, Request};
use axum::http::HeaderValue;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;
use tower_http::services::ServeDir;
use tracing::{debug, warn};

use crate::auth::{self, Auth};
use crate::console;

pub struct App {
    pub link: Arc<LinkConfig>,
    pub auth: Option<Auth>,
    /// 网页控制台的身份（端到端加密，见 [`crate::identity`]）：所有标签页共用，配对过的电脑靠它认出这个控制台
    pub identity: airhand_link::secure::Identity,
}

/// 连接信息（放在请求的 extensions 里）
#[derive(Debug, Clone, Copy)]
pub struct Conn {
    /// 是否是 HTTPS
    pub tls: bool,
}

/// 与桌面版（`client/src-tauri/tauri.conf.json`）相同的内容安全策略；`connect-src 'self'` 包括同源的 WebSocket
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; font-src 'self' data:; worker-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn router(app: Arc<App>, root: &Path) -> Router {
    Router::new()
        .route("/ws", get(console::upgrade))
        .fallback_service(ServeDir::new(root))
        .layer(middleware::from_fn_with_state(app.clone(), auth::check))
        .layer(middleware::from_fn(headers))
        .with_state(app)
}

async fn headers(req: Request, next: Next) -> Response {
    // Vite 输出的 assets/ 文件名里带内容哈希，可以一直缓存；其它（index.html）每次都检查
    let immutable = req.uri().path().contains("/assets/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    h.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    res
}

pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: TlsAcceptor,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    loop {
        let (tcp, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(c) => c,
                Err(e) => {
                    // 例如文件描述符用完了：稍等再试，不要空转
                    warn!("accepting a connection failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            _ = &mut shutdown => return,
        };
        tokio::spawn(connection(tcp, peer, router.clone(), tls.clone()));
    }
}

async fn connection(tcp: TcpStream, peer: SocketAddr, router: Router, tls: TlsAcceptor) {
    // 输入事件、画面确认都是小消息，不能被 Nagle 算法攒着
    let _ = tcp.set_nodelay(true);
    let mut first = [0u8; 1];
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, tcp.peek(&mut first)).await {
        Ok(Ok(1..)) => {}
        _ => return,
    }
    if first[0] == 0x16 {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, tls.accept(tcp)).await {
            Ok(Ok(stream)) => http1(stream, peer, true, router).await,
            // 多半是浏览器还不信任自签名证书，用户确认之后会重新连接
            Ok(Err(e)) => debug!(%peer, "TLS handshake failed: {e}"),
            Err(_) => debug!(%peer, "TLS handshake timed out"),
        }
    } else {
        http1(tcp, peer, false, router).await;
    }
}

async fn http1<I>(io: I, peer: SocketAddr, tls: bool, router: Router)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = router.map_request(move |mut req: Request<Incoming>| {
        req.extensions_mut().insert(ConnectInfo(peer));
        req.extensions_mut().insert(Conn { tls });
        req
    });
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .serve_connection(TokioIo::new(io), TowerToHyperService::new(service))
        .with_upgrades();
    if let Err(e) = conn.await {
        debug!(%peer, "HTTP connection ended: {e}");
    }
}
