//! 证书加载 / 自动生成。
//!
//! 首次启动时如果证书不存在，会生成一张自签名证书写到证书路径（默认是服务端程序旁边的 `data/` 文件夹，
//! 不存在就创建；Docker 镜像里设成了 `/data`，即 `docker-compose.yml` 旁边的 `data/` 目录），并在日志中打印其 SHA-256 指纹。
//! 把指纹填入客户端 `client/src-tauri/src/config.rs`（或构建时的 `AIRHAND_SERVER_FP` 环境变量，
//! 桌面版和 Android 都是；已经装好的客户端是 `config.toml` 里的 `server_fp`，Android 在应用的设置里改），
//! 以及网页控制台的 `AIRHAND_SERVER_FP` 环境变量，即可启用证书固定，防止中间人攻击。

use std::path::Path;

use anyhow::{Context, Result};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tracing::info;

pub struct LoadedCert {
    pub chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
    pub fingerprint: String,
}

pub fn load_or_generate(cert_path: &Path, key_path: &Path, names: &[String]) -> Result<LoadedCert> {
    if !cert_path.exists() || !key_path.exists() {
        info!(
            "no certificate found, generating a self-signed one: {}",
            cert_path.display()
        );
        let ck = rcgen::generate_simple_self_signed(names.to_vec())
            .context("can't generate the certificate")?;
        if let Some(dir) = cert_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        if let Some(dir) = key_path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        std::fs::write(cert_path, ck.cert.pem()).context("can't write the certificate")?;
        std::fs::write(key_path, ck.signing_key.serialize_pem())
            .context("can't write the private key")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .with_context(|| format!("can't read the certificate: {}", cert_path.display()))?
        .collect::<Result<_, _>>()
        .context("can't parse the certificate")?;
    anyhow::ensure!(
        !chain.is_empty(),
        "the certificate file is empty: {}",
        cert_path.display()
    );
    let key = PrivateKeyDer::from_pem_file(key_path)
        .with_context(|| format!("can't read the private key: {}", key_path.display()))?;

    let fingerprint = fingerprint(&chain[0]);
    Ok(LoadedCert {
        chain,
        key,
        fingerprint,
    })
}

pub fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}
