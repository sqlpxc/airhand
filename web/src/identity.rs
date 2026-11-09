//! 网页控制台的身份：端到端加密的静态密钥（见 `airhand_link::secure`）。所有标签页共用一个：
//! 被控端把它当成一台控制端，配对过一次之后，用这个网页控制台的人都不用再输配对码（网页控制台自己的访问密码见 [`crate::auth`]）。
//!
//! 保存在一个文件里（`AIRHAND_WEB_IDENTITY`，默认是 `AIRHAND_WEB_CERT` 所在目录里的 `controller-key.txt`），
//! 一行 64 位十六进制的私钥，只有本账户能读写（600）。没有地方保存时只记在内存里：重启之后，
//! 配对过的电脑会把它当成新的控制端，要重新配对。

use std::path::Path;

use airhand_link::secure::{Identity, hex, unhex32};
use tracing::{info, warn};

pub fn load(path: Option<&Path>) -> Identity {
    let Some(path) = path else {
        warn!(
            "AIRHAND_WEB_IDENTITY is not set: the console's end-to-end identity is only kept until restart (computers it paired with will ask for the pairing code again)"
        );
        return Identity::generate();
    };
    if let Ok(text) = std::fs::read_to_string(path) {
        match unhex32(&text) {
            Some(k) => return Identity::from_private(k),
            None => warn!(
                "{} isn't a valid identity, creating a new one",
                path.display()
            ),
        }
    }
    let id = Identity::generate();
    match save(path, &hex(id.private())) {
        Ok(()) => info!(
            "created the console's end-to-end identity in {}",
            path.display()
        ),
        Err(e) => warn!(
            "can't save the console's identity to {} ({e}); it's only kept until restart",
            path.display()
        ),
    }
    id
}

fn save(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_across_restarts() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("airhand-web-id-{}-{nanos}", std::process::id()));
        let path = dir.join("controller-key.txt");
        let a = load(Some(&path));
        let b = load(Some(&path));
        assert_eq!(a.public(), b.public());
        let _ = std::fs::remove_dir_all(dir);
    }
}
