//! 直连的 QUIC 配置和证书。
//!
//! 两端各有一张临时生成的自签名证书（[`Cert`]），指纹在加密的控制通道上交换（`DirectOffer::cert`）。
//! TLS 双向认证：控制端只认被控端给的那个指纹（[`client_config`]）；被控端要求控制端出示证书（[`server_config`]），
//! 握手完成后按它的指纹（[`peer_fingerprint`]）找到是哪个会话的、核对是不是那个会话的控制端给的。
//! 两边都照样校验 TLS 1.3 的握手签名：拿得出私钥的才是证书的主人，中间人冒充不了任何一边。

use std::sync::Arc;
use std::time::Duration;

use airhand_proto::DIRECT_ALPN;
use anyhow::{Context, Result, anyhow};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Connection, ServerConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};

/// 控制端连直连时用的服务器名（和证书里的一样，不过谁也不看它：只认指纹）
pub(crate) const SERVER_NAME: &str = "airhand";

/// 直连的传输参数：和连中转的差不多（BBR、大窗口、2 秒内发现掉线），另外打洞时第一个包常常被对方的 NAT 丢掉
/// （对方还没往这边发过包）：起始 RTT 按 100 ms 算，重发得快一点。每条通道是一条单向流，没有双向流
pub(crate) fn transport() -> TransportConfig {
    let mut t = TransportConfig::default();
    t.max_concurrent_bidi_streams(VarInt::from_u32(0));
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

/// 证书的 SHA-256 指纹
pub(crate) fn fingerprint(der: &[u8]) -> [u8; 32] {
    Sha256::digest(der).into()
}

/// 本端直连用的证书（每条服务器连接、每个控制端会话生成一张，不保存）
pub(crate) struct Cert {
    der: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    pub fingerprint: [u8; 32],
}

impl Cert {
    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])
            .map_err(|e| anyhow!("can't generate a direct path certificate: {e}"))?;
        let der = ck.cert.der().clone();
        Ok(Self {
            fingerprint: fingerprint(&der),
            der,
            key: PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        })
    }
}

/// 被控端：接受控制端连过来的直连（放在连中转的那个 endpoint 上）。控制端必须出示证书，是谁握手之后按指纹认
pub(crate) fn server_config(cert: &Cert) -> Result<ServerConfig> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyClient(provider)))
        .with_single_cert(vec![cert.der.clone()], cert.key.clone_key().into())
        .context("bad direct path certificate")?;
    tls.alpn_protocols = vec![DIRECT_ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let mut cfg = ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport()));
    Ok(cfg)
}

/// 控制端：用证书 `cert` 连被控端的直连，只认指纹是 `host` 的被控端
pub(crate) fn client_config(cert: &Cert, host: [u8; 32]) -> Result<ClientConfig> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinned {
            provider,
            fingerprint: host,
        }))
        .with_client_auth_cert(vec![cert.der.clone()], cert.key.clone_key().into())
        .context("bad direct path certificate")?;
    tls.alpn_protocols = vec![DIRECT_ALPN.to_vec()];
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    cfg.transport_config(Arc::new(transport()));
    Ok(cfg)
}

/// 直连另一头的证书指纹（握手完成之后才有）
pub(crate) fn peer_fingerprint(conn: &Connection) -> Option<[u8; 32]> {
    let certs = conn
        .peer_identity()?
        .downcast::<Vec<CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|c| fingerprint(c))
}

fn tls13_signature(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

/// 只认这个指纹的服务器证书（照样校验握手签名）
#[derive(Debug)]
struct Pinned {
    provider: Arc<CryptoProvider>,
    fingerprint: [u8; 32],
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the direct peer isn't this session's host".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 isn't used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls13_signature(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 任何客户端证书都接受（照样校验握手签名），是谁握手之后按指纹认（见模块说明）
#[derive(Debug)]
struct AnyClient(Arc<CryptoProvider>);

impl ClientCertVerifier for AnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 isn't used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls13_signature(&self.0, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
