//! Airhand 中转服务器。
//!
//! 环境变量：
//! * `AIRHAND_LISTEN`        监听地址，默认 `0.0.0.0:4433`（UDP）
//! * `AIRHAND_CERT`          证书路径，默认 `/data/cert.pem`（不存在则自动生成）
//! * `AIRHAND_KEY`           私钥路径，默认 `/data/key.pem`
//! * `AIRHAND_DB`            SQLite 数据库（固定控制码的绑定表，见 [`codes`]），默认和证书放在同一个目录（`/data/airhand.db`）
//! * `AIRHAND_CERT_NAMES`    自动生成证书时的 SAN，逗号分隔，默认 `airhand,localhost`
//! * `AIRHAND_MAX_SESSIONS`  每台被控端最多同时被几个控制端连接，默认 4
//! * `AIRHAND_MAX_CONNS_PER_IP` 同一个来源（IPv4 地址 / IPv6 的 /64）最多同时有几条连接，默认 128
//!   （网页控制台每个标签页一条；公司出口 IP 后面可能有很多台被控端）
//! * `AIRHAND_NEW_CODES_PER_IP` 同一个来源（IPv4 地址 / IPv6 的 /48）10 分钟内最多分配几个新控制码，默认 30
//!   （取回原来的控制码不算）
//! * `AIRHAND_CONNECTS_PER_MINUTE` 同一个来源（IPv4 地址 / IPv6 的 /64）一分钟内最多向同一台被控端发起几个会话，默认 6
//!   （不低于 `AIRHAND_MAX_SESSIONS`；带重连凭证的也算）
//! * `RUST_LOG`              日志级别，默认 `info`

mod broker;
mod codes;
mod tls;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::ALPN;
use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, ServerConfig, TransportConfig, VarInt};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tracing::info;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 针对实时画面优化的传输参数（客户端见 `crates/airhand-link/src/net.rs`）。
///
/// QUIC 的空闲超时取两端中较短的一个：客户端设置的更短（10 秒，2 秒保活），掉线检测以客户端为准；
/// 这里的值兜底那些不发保活的连接。
pub fn transport_config() -> TransportConfig {
    let mut t = TransportConfig::default();
    // 双向流只有信令流；会话的通道是单向流，每个会话每端几条（服务器限制每端最多 MAX_LANES 条）
    t.max_concurrent_bidi_streams(VarInt::from_u32(1));
    t.max_concurrent_uni_streams(VarInt::from_u32(256));
    t.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    // 服务器只转发：收到就转出去，窗口只要盖住一段链路的带宽时延积（100 Mbps × 300 ms ≈ 3.75 MB）。
    // 窗口就是服务器肯替一条连接缓存多少：不读数据的一方（或者故意这样做的攻击者）最多让服务器缓存
    // 接收 16 MiB + 发送 16 MiB，而不是几百 MiB。在途的画面由应用层 ACK 控制，不靠这里的窗口：
    // 每个会话最多 码率 × (RTT + 50 ms)、不超过 4 MiB（airhand_link::flow），画面占不满发送窗口，
    // 同一连接上的 Pong、Bye 还写得进去
    t.stream_receive_window(VarInt::from_u32(4 * 1024 * 1024));
    t.receive_window(VarInt::from_u32(16 * 1024 * 1024));
    t.send_window(16 * 1024 * 1024);
    // 数据报（声音，约 12 KB/s 一路）收到就转出去：收的缓冲不用大；转不出去（那一段网络跟不上）时丢掉最旧的，
    // 不替接收方放几秒钟以前的声音
    t.datagram_receive_buffer_size(Some(256 * 1024));
    t.datagram_send_buffer_size(64 * 1024);
    // BBR：在有随机丢包的网络（Wi-Fi/跨境）上比 Cubic 更能维持吞吐，且排队延迟更低
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("the certificate and private key don't match")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;

    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport_config()));
    Ok(cfg)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let listen: SocketAddr = env_or("AIRHAND_LISTEN", "0.0.0.0:4433")
        .parse()
        .context("invalid AIRHAND_LISTEN")?;
    let cert_path = PathBuf::from(env_or("AIRHAND_CERT", "/data/cert.pem"));
    let key_path = PathBuf::from(env_or("AIRHAND_KEY", "/data/key.pem"));
    let db_path = match std::env::var("AIRHAND_DB") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        _ => cert_path
            .parent()
            .map(|d| d.join("airhand.db"))
            .unwrap_or_else(|| PathBuf::from("airhand.db")),
    };
    let names: Vec<String> = env_or("AIRHAND_CERT_NAMES", "airhand,localhost")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let max_sessions: usize = env_or("AIRHAND_MAX_SESSIONS", "4")
        .parse()
        .unwrap_or(4)
        .max(1);
    let number = |key: &str, default: usize| -> usize {
        env_or(key, &default.to_string())
            .parse()
            .unwrap_or(default)
            .max(1)
    };
    let max_conns_per_ip = number(
        "AIRHAND_MAX_CONNS_PER_IP",
        broker::Config::DEFAULT_MAX_CONNS_PER_IP,
    );
    let new_codes_per_ip = number(
        "AIRHAND_NEW_CODES_PER_IP",
        broker::Config::DEFAULT_NEW_CODES_PER_IP,
    );
    // 不低于每台被控端的会话数：不然同一个出口 IP 后面的人坐不满一台被控端
    let connects_per_minute = number(
        "AIRHAND_CONNECTS_PER_MINUTE",
        broker::Config::DEFAULT_CONNECTS_PER_MINUTE,
    )
    .max(max_sessions);

    let cert = tls::load_or_generate(&cert_path, &key_path, &names)?;
    info!("Certificate SHA-256 fingerprint: {}", cert.fingerprint);
    info!(
        "(to enable certificate pinning, set AIRHAND_SERVER_FP to this fingerprint when building the clients and when starting the web console; installed clients can also set it as server_fp in config.toml, or in Settings on Android)"
    );

    let endpoint = Endpoint::server(server_config(cert.chain, cert.key)?, listen)
        .with_context(|| format!("can't listen on {listen}"))?;
    info!("Airhand server started on udp://{listen}, up to {max_sessions} sessions per host");

    let broker = broker::Broker::new(broker::Config {
        max_sessions_per_host: max_sessions,
        db: Some(db_path),
        max_conns_per_ip,
        new_codes_per_ip,
        connects_per_minute,
    })?;
    tokio::spawn(broker.clone().janitor());
    {
        let broker = broker.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(300));
            tick.tick().await;
            loop {
                tick.tick().await;
                let (hosts, pending, fixed) = broker.stats();
                info!(hosts, pending, fixed, "online");
            }
        });
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                tokio::spawn(broker.clone().handle_incoming(incoming));
            }
            _ = &mut shutdown => {
                info!("shutting down…");
                break;
            }
        }
    }
    endpoint.close(VarInt::from_u32(0), b"server shutdown");
    let _ = tokio::time::timeout(Duration::from_secs(3), endpoint.wait_idle()).await;
    broker.flush_bindings().await;
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
