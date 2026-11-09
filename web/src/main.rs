//! Airhand 网页控制台：控制端跑在服务器 / 容器里，在浏览器里使用。
//!
//! 浏览器打开这个服务的端口就是控制台（与桌面版同一套界面，见 `client/src`）。每个浏览器标签页
//! 通过 WebSocket 连到这里，各自有一个独立的控制端（见 [`console`]）：它用 QUIC 连接中转服务器，
//! 把画面原样转给浏览器，由浏览器用 WebCodecs 解码。
//!
//! 环境变量：
//! * `AIRHAND_SERVER`          中转服务器地址 `host:port`，默认 `127.0.0.1:4433`
//! * `AIRHAND_SERVER_FP`       中转服务器的证书指纹（服务器日志里打印的那个）；不设置则第一次连上时记下服务器的指纹，
//!   之后只认它（见 [`trust`]）
//! * `AIRHAND_SERVER_TRUST`    没有设置 `AIRHAND_SERVER_FP` 时，记下的指纹存在这个文件里。默认放在 `AIRHAND_WEB_CERT`
//!   所在的目录（`server-trust.txt`）；都没有设置时只记在内存里，重启后重新记
//! * `AIRHAND_NAME`            被控端托盘里显示的控制端名称，默认 `Airhand Web`
//! * `AIRHAND_WEB_LISTEN`      监听地址（TCP），默认 `0.0.0.0:8080`。同一个端口同时支持 HTTP 和 HTTPS
//! * `AIRHAND_WEB_PASSWORD`    访问密码（浏览器弹出登录框，用户名随意）；不设置则不需要密码
//! * `AIRHAND_WEB_CERT` / `AIRHAND_WEB_KEY`
//!   HTTPS 证书和私钥（PEM）。文件不存在时生成一张自签名证书写到这里；不设置则每次启动临时生成
//! * `AIRHAND_WEB_CERT_NAMES`  自动生成证书时写入的域名 / IP，逗号分隔，默认 `localhost`
//! * `AIRHAND_WEB_ROOT`        控制台网页所在的目录（`client/` 里 `npm run build` 的输出），默认 `../client/dist`
//!   （在 `web/` 里 `cargo run` 时正好是它；Docker 镜像里另有设置）
//! * `RUST_LOG`                日志级别，默认 `info`

mod auth;
mod console;
mod http;
mod tls;
mod trust;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use airhand_link::LinkConfig;
use anyhow::{Context, Result, bail, ensure};
use tracing::{info, warn};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_or(key: &str, default: &str) -> String {
    env(key).unwrap_or_else(|| default.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,quinn=warn,quinn_proto=warn".into()),
        )
        .init();

    let server = env_or("AIRHAND_SERVER", "127.0.0.1:4433");
    let fingerprint = env("AIRHAND_SERVER_FP").unwrap_or_default();
    let name = env_or("AIRHAND_NAME", "Airhand Web");
    let os = format!("{} (web console)", std::env::consts::OS);
    let mut link = LinkConfig::new(server.trim(), &fingerprint, name, os)
        .context("invalid AIRHAND_SERVER_FP")?;
    if !link.pinned() {
        let trust_file = env("AIRHAND_SERVER_TRUST").map(PathBuf::from).or_else(|| {
            env("AIRHAND_WEB_CERT")
                .and_then(|c| Some(Path::new(&c).parent()?.join("server-trust.txt")))
        });
        match trust_file {
            Some(path) => {
                info!(
                    "AIRHAND_SERVER_FP is not set: the relay server's certificate is remembered on the first connection ({}) and a different one is refused later",
                    path.display()
                );
                link = link.with_store(Arc::new(trust::FileTrust::new(path)));
            }
            None => warn!(
                "AIRHAND_SERVER_FP is not set: the relay server's certificate is remembered on the first connection, but only until restart (set AIRHAND_SERVER_TRUST to keep it, or AIRHAND_SERVER_FP to pin it)"
            ),
        }
    }

    let listen: SocketAddr = env_or("AIRHAND_WEB_LISTEN", "0.0.0.0:8080")
        .parse()
        .context("invalid AIRHAND_WEB_LISTEN")?;
    let root = PathBuf::from(env_or("AIRHAND_WEB_ROOT", "../client/dist"));
    ensure!(
        root.join("index.html").is_file(),
        "the console isn't built: {} has no index.html (run `npm run build` in client/, or set AIRHAND_WEB_ROOT)",
        root.display()
    );

    let names: Vec<String> = env_or("AIRHAND_WEB_CERT_NAMES", "localhost")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let tls = match (env("AIRHAND_WEB_CERT"), env("AIRHAND_WEB_KEY")) {
        (Some(cert), Some(key)) => {
            tls::load_or_generate(Path::new(&cert), Path::new(&key), &names)?
        }
        (None, None) => tls::generate(&names)?,
        _ => bail!("set both AIRHAND_WEB_CERT and AIRHAND_WEB_KEY, or neither"),
    };
    info!("HTTPS certificate SHA-256 fingerprint: {}", tls.fingerprint);

    let password = env("AIRHAND_WEB_PASSWORD");
    if password.is_none() {
        info!(
            "AIRHAND_WEB_PASSWORD is not set: anyone who can reach this port can use the console (a control code is still needed for each computer)"
        );
    }
    let app = Arc::new(http::App {
        link: Arc::new(link),
        auth: password.map(auth::Auth::new),
    });
    let router = http::router(app.clone(), &root);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("can't listen on {listen}"))?;
    info!(
        relay = %app.link.server,
        pinned = app.link.pinned(),
        "Airhand web console listening on {listen} (HTTP and HTTPS on the same port). Open http://localhost:{port} on this computer, or https://<this computer's address>:{port} from other computers",
        port = listen.port(),
    );

    http::serve(listener, router, tls.acceptor, shutdown_signal()).await;
    info!("shutting down…");
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
