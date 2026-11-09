//! 没有设置 `AIRHAND_SERVER_FP` 时，第一次连上中转服务器时记下它的证书指纹（见 `airhand_link::net` 的模块说明），
//! 存在一个文件里（`AIRHAND_SERVER_TRUST`，Docker 镜像里是 `/data/server-trust.txt`）：重启之后也只认这个指纹，
//! 服务器的证书变了就拒绝连接。服务器确实换了证书时删掉这个文件（或者设置 `AIRHAND_SERVER_FP`）。
//!
//! 文件只有一行：`服务器地址 指纹`。换了服务器地址就重新记。

use std::path::PathBuf;

use airhand_link::net::{MemoryTrust, TrustStore, format_fingerprint, parse_fingerprint};
use tracing::{info, warn};

pub struct FileTrust {
    path: PathBuf,
    mem: MemoryTrust,
}

impl FileTrust {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            mem: MemoryTrust::default(),
        }
    }

    fn read(&self, server: &str) -> Option<[u8; 32]> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let (saved, fp) = text.trim().rsplit_once(char::is_whitespace)?;
        if saved.trim() != server {
            return None;
        }
        parse_fingerprint(fp).ok()?
    }
}

impl TrustStore for FileTrust {
    fn load(&self, server: &str) -> Option<[u8; 32]> {
        let fp = self.mem.load(server).or_else(|| self.read(server))?;
        self.mem.save(server, fp);
        Some(fp)
    }

    fn save(&self, server: &str, fp: [u8; 32]) {
        self.mem.save(server, fp);
        let line = format!("{server} {}\n", format_fingerprint(&fp));
        let written = self
            .path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&self.path, line));
        match written {
            Ok(()) => info!(
                "remembered the relay server's certificate fingerprint in {}",
                self.path.display()
            ),
            Err(e) => warn!(
                "can't save the relay server's certificate fingerprint to {} ({e}); it's only remembered until restart",
                self.path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembers_per_server_across_restarts() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("airhand-web-trust-{}-{nanos}", std::process::id()))
            .join("server-trust.txt");
        let a = FileTrust::new(path.clone());
        assert_eq!(a.load("relay:4433"), None);
        a.save("relay:4433", [7; 32]);
        // 重新启动：从文件里读
        let b = FileTrust::new(path.clone());
        assert_eq!(b.load("relay:4433"), Some([7; 32]));
        assert_eq!(b.load("other:4433"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
