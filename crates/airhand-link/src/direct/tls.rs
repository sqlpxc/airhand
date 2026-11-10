//! 直连的 QUIC 配置。
//!
//! 直连上的 TLS 只管把这一条 QUIC 连接加密：证书是被控端在本次运行里随便生成的一张（自签名），控制端不看它是谁。
//! 对面是谁由会话密钥证明（[`super::prove`]，绑定这条连接的 TLS 导出密钥，中间人转发不了）；
//! 直连上跑的帧本来就是端到端加密的，TLS 这一层被冒充了也看不到、改不了内容。

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use airhand_proto::DIRECT_ALPN;
use anyhow::{Context, Result, anyhow};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, ServerConfig, TransportConfig, VarInt};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};

/// 控制端连直连时用的服务器名（和证书里的一样，不过控制端本来也不看）
pub(crate) const SERVER_NAME: &str = "airhand";

/// 直连的传输参数：和连中转的差不多（BBR、大窗口、2 秒内发现掉线），另外
///
/// * 打洞时第一个包常常被对方的 NAT 丢掉（对方还没往这边发过包）：起始 RTT 按 100 ms 算，重发得快一点；
/// * 每条通道是一条单向流（见 `airhand_proto` 的"直连"），双向流只有控制端证明身份用的那一条
pub(crate) fn transport() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(VarInt::from_u32(2));
    t.max_concurrent_uni_streams(VarInt::from_u32(16));
    t.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.initial_rtt(Duration::from_millis(100));
    t.stream_receive_window(VarInt::from_u32(16 * 1024 * 1024));
    t.receive_window(VarInt::from_u32(64 * 1024 * 1024));
    t.send_window(64 * 1024 * 1024);
    t.datagram_send_buffer_size(64 * 1024);
    t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
    t
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 被控端在本次运行里用的证书（第一次用时生成）
fn certificate() -> Result<&'static (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>)> {
    static CERT: OnceLock<Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>), String>> =
        OnceLock::new();
    CERT.get_or_init(|| {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(|e| e.to_string())?;
        Ok((
            ck.cert.der().clone(),
            PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        ))
    })
    .as_ref()
    .map_err(|e| anyhow!("can't generate the direct path certificate: {e}"))
}

/// 被控端：接受控制端连过来的直连（放在连中转的那个 endpoint 上）
pub(crate) fn server_config() -> Result<ServerConfig> {
    let (cert, key) = certificate()?;
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.clone_key().into())
        .context("bad direct path certificate")?;
    tls.alpn_protocols = vec![DIRECT_ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport()));
    Ok(cfg)
}

/// 控制端：连被控端的直连
pub(crate) fn client_config() -> Result<ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![DIRECT_ALPN.to_vec()];
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport()));
    Ok(cfg)
}

/// 什么证书都接受，但照样校验 TLS 1.3 的握手签名（见模块说明：对面是谁由会话密钥证明）
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
