//! 被控端的端到端身份和授权（协议见 `airhand_proto::handshake`）：本机的身份密钥、配对码、配对过的控制端。
//!
//! * 都保存在 config.toml 里（`[host]`、`[[paired]]`，见 [`crate::config`]），身份密钥和配对码加密保存；
//! * 没配对过的控制端：默认要配对码（config.toml 的 `access`），配对码显示在托盘菜单里，从不经过服务器；
//! * 有人连续输错 [`MAX_FAILURES`] 次配对码就暂停配对（`pairing_locked`，重启之后也一样），
//!   托盘里提示，本机用户换一个配对码才恢复：中间人（包括中转服务器）一共只能猜这么几次；
//! * 更换控制码时配对码也换一个、配对过的控制端全部忘掉（见 [`DesktopGate::reset`]）。

use std::sync::{Arc, Mutex};

use airhand_link::host::{Access, Admitted, Gate};
use airhand_link::secure::{Identity, hex};
use airhand_proto::handshake::PublicKey;
use tracing::{info, warn};

use crate::config::{self, Paired};
use crate::status::StatusHub;

/// 连续输错这么多次配对码就暂停配对
pub const MAX_FAILURES: u32 = 5;

pub struct DesktopGate {
    identity: Identity,
    access: Access,
    state: Mutex<State>,
    hub: StatusHub,
}

struct State {
    pairing: String,
    locked: bool,
    /// 连续输错了几次（配对成功、换配对码时清零；不保存：重启之后从零数，但暂停是保存的）
    failures: u32,
    paired: Vec<Paired>,
}

/// 新的配对码：[`airhand_proto::PAIRING_DIGITS`] 位随机数字
fn new_code() -> String {
    format!("{:06}", rand::random_range(0..1_000_000u32))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl DesktopGate {
    /// 读出 config.toml 里的身份、配对码、配对过的控制端；没有的生成并保存（写不进去的只在这次运行里有效）
    pub fn load(hub: StatusHub) -> Arc<Self> {
        let saved = config::host_security();
        let identity = match saved.key {
            Some(k) => Identity::from_private(k),
            None => {
                let id = Identity::generate();
                if let Err(e) = config::save_host_key(id.private()) {
                    warn!(
                        "can't save this computer's identity ({e:#}); controllers will see a new one after a restart"
                    );
                }
                info!(key = %hex(id.public()), "created this computer's end-to-end identity");
                id
            }
        };
        let pairing = match saved.pairing {
            Some(p) => p,
            None => {
                let p = new_code();
                if let Err(e) = config::save_pairing(&p, false) {
                    warn!("can't save the pairing code: {e:#}");
                }
                p
            }
        };
        let access = config::settings().access;
        info!(?access, key = %hex(identity.public()), paired = saved.paired.len(), "host identity loaded");
        let gate = Arc::new(Self {
            identity,
            access,
            state: Mutex::new(State {
                pairing,
                locked: saved.pairing_locked,
                failures: 0,
                paired: saved.paired,
            }),
            hub,
        });
        gate.publish();
        gate
    }

    /// 托盘里显示的：配对码、暂停了没有、配对过几个
    fn publish(&self) {
        let st = self.state.lock().unwrap();
        let pairing = (self.access == Access::Pairing).then(|| st.pairing.clone());
        let (locked, paired) = (st.locked, st.paired.len());
        drop(st);
        self.hub.send_if_modified(|s| {
            let changed = s.host.pairing != pairing
                || s.host.pairing_locked != locked
                || s.host.paired != paired;
            s.host.pairing = pairing;
            s.host.pairing_locked = locked;
            s.host.paired = paired;
            changed
        });
    }

    /// 本机用户换一个配对码（托盘菜单）：暂停的配对也随之恢复。配对过的控制端不受影响
    pub fn new_pairing_code(&self) {
        let mut st = self.state.lock().unwrap();
        st.pairing = new_code();
        st.locked = false;
        st.failures = 0;
        if let Err(e) = config::save_pairing(&st.pairing, false) {
            warn!("can't save the pairing code: {e:#}");
        }
        drop(st);
        info!("new pairing code");
        self.publish();
    }

    /// 忘掉所有配对过的控制端（托盘菜单）：它们下次要重新输入配对码
    pub fn forget_paired(&self) {
        let mut st = self.state.lock().unwrap();
        st.paired.clear();
        if let Err(e) = config::save_paired(&st.paired) {
            warn!("can't save the paired controllers: {e:#}");
        }
        drop(st);
        info!("forgot all paired controllers");
        self.publish();
    }

    /// 更换控制码：配对码也换一个，配对过的控制端全部忘掉
    pub fn reset(&self) {
        self.forget_paired();
        self.new_pairing_code();
    }

    /// 会话的端到端握手完成了：托盘里的这一项换成控制端在加密握手里报的名称，加上安全码
    pub fn admitted(&self, session_id: u64, a: &Admitted) {
        let (name, safety) = (a.name.clone(), a.safety.clone());
        self.hub.send_modify(|s| {
            if let Some(c) = s
                .host
                .controllers
                .iter_mut()
                .find(|c| c.session_id == session_id)
            {
                if !name.is_empty() {
                    c.peer.name = name;
                }
                c.safety = Some(safety);
            }
        });
    }
}

impl Gate for DesktopGate {
    fn identity(&self) -> Identity {
        self.identity.clone()
    }

    fn access(&self) -> Access {
        self.access
    }

    fn pairing_code(&self) -> Option<String> {
        let st = self.state.lock().unwrap();
        (!st.locked).then(|| st.pairing.clone())
    }

    fn pairing_failed(&self) {
        let mut st = self.state.lock().unwrap();
        st.failures += 1;
        warn!(
            failures = st.failures,
            "someone entered a wrong pairing code"
        );
        if st.failures >= MAX_FAILURES && !st.locked {
            st.locked = true;
            warn!(
                "too many wrong pairing codes, pairing is paused until the local user picks a new code"
            );
            if let Err(e) = config::save_pairing(&st.pairing, true) {
                warn!("can't save the pairing state: {e:#}");
            }
        }
        drop(st);
        self.publish();
    }

    fn is_known(&self, key: &PublicKey) -> bool {
        let k = hex(key);
        self.state.lock().unwrap().paired.iter().any(|p| p.key == k)
    }

    fn paired(&self, key: PublicKey, name: &str) {
        let mut st = self.state.lock().unwrap();
        let k = hex(&key);
        st.paired.retain(|p| p.key != k);
        st.paired.push(Paired {
            key: k,
            name: name.chars().take(64).collect(),
            since: now_ms(),
        });
        let extra = st.paired.len().saturating_sub(config::MAX_PAIRED);
        st.paired.drain(..extra);
        st.failures = 0;
        if let Err(e) = config::save_paired(&st.paired) {
            warn!("can't save the paired controllers: {e:#}");
        }
        drop(st);
        self.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrong_codes_pause_pairing_until_a_new_code() {
        let hub = crate::status::new_hub();
        let gate = DesktopGate::load(hub.clone());
        let code = gate.pairing_code().unwrap();
        assert_eq!(code.len(), 6);
        assert_eq!(hub.borrow().host.pairing.as_deref(), Some(code.as_str()));
        for _ in 0..MAX_FAILURES - 1 {
            gate.pairing_failed();
        }
        assert!(gate.pairing_code().is_some());
        gate.pairing_failed();
        assert_eq!(gate.pairing_code(), None);
        assert!(hub.borrow().host.pairing_locked);
        gate.new_pairing_code();
        assert!(gate.pairing_code().is_some());
        assert!(!hub.borrow().host.pairing_locked);

        let key = *Identity::generate().public();
        assert!(!gate.is_known(&key));
        gate.paired(key, "PC");
        assert!(gate.is_known(&key));
        assert_eq!(hub.borrow().host.paired, 1);
        gate.reset();
        assert!(!gate.is_known(&key));
        assert_ne!(gate.pairing_code(), None);
    }
}
