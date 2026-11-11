//! 被控端的端到端身份和授权（协议见 `airhand_proto::handshake`）：本机的身份密钥、配对码、配对过的控制端。
//!
//! * 都保存在 config.toml 里（`[host]`、`[[paired]]`，见 [`crate::config`]），身份密钥和配对码加密保存；
//! * 没配对过的控制端：默认要配对码（config.toml 的 `access`），配对码显示在托盘菜单里，从不经过服务器；
//! * 连续 [`MAX_FAILURES`] 次未成功的配对尝试就暂停配对（`pairing_locked`，重启之后也一样），
//!   托盘里提示，本机用户换一个配对码才恢复：中间人（包括中转服务器）一共只能猜这么几次；
//! * 更换控制码时配对码也换一个、配对过的控制端全部忘掉（见 [`DesktopGate::reset`]）。

use std::sync::{Arc, Mutex};

use airhand_link::host::{Access, Admitted, Gate};
use airhand_link::secure::{Identity, PairingAttempt, hex};
use airhand_proto::handshake::PublicKey;
use tracing::{info, warn};

use crate::config::{self, Paired};
use crate::status::StatusHub;

/// 未成功和在途的配对尝试合计上限
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
    /// 未成功几次（配对成功、换配对码时清零；不保存：重启之后从零数，但暂停是保存的）
    failures: u32,
    pending: u32,
    generation: u64,
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
                pending: 0,
                generation: 0,
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
        st.pending = 0;
        st.generation += 1;
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
        // 已经拿到旧授权、尚未提交的配对不能在清空之后重新加入。
        st.generation += 1;
        st.pending = 0;
        if let Err(e) = config::save_paired(&st.paired) {
            warn!("can't save the paired controllers: {e:#}");
        }
        drop(st);
        info!("forgot all paired controllers");
        self.publish();
    }

    /// 更换控制码：配对码也换一个，配对过的控制端全部忘掉
    pub fn reset(&self) {
        let mut st = self.state.lock().unwrap();
        st.paired.clear();
        st.pairing = new_code();
        st.locked = false;
        st.failures = 0;
        st.pending = 0;
        st.generation += 1;
        if let Err(e) = config::save_paired(&st.paired) {
            warn!("can't save the paired controllers: {e:#}");
        }
        if let Err(e) = config::save_pairing(&st.pairing, false) {
            warn!("can't save the pairing code: {e:#}");
        }
        drop(st);
        self.publish();
    }

    /// 会话已经通过认证并开始：只显示加密握手中确认过的名称。服务器的 Incoming 不发布到列表。
    pub fn admitted(&self, session_id: u64, ip: std::net::IpAddr, a: &Admitted) {
        self.hub.send_modify(|s| {
            s.host.controllers.push(crate::status::Controller {
                session_id,
                peer: airhand_proto::signal::Peer {
                    name: a.name.clone(),
                    ip,
                },
                safety: Some(a.safety.clone()),
            });
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

    fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        let mut st = self.state.lock().unwrap();
        if st.locked || st.failures + st.pending >= MAX_FAILURES {
            return None;
        }
        st.pending += 1;
        Some(Box::new(Attempt {
            gate: self,
            code: st.pairing.clone(),
            generation: st.generation,
            completed: false,
        }))
    }

    fn is_known(&self, key: &PublicKey) -> bool {
        let k = hex(key);
        self.state.lock().unwrap().paired.iter().any(|p| p.key == k)
    }
}

struct Attempt<'a> {
    gate: &'a DesktopGate,
    code: String,
    generation: u64,
    completed: bool,
}

impl PairingAttempt for Attempt<'_> {
    fn code(&self) -> &str {
        &self.code
    }

    fn complete(mut self: Box<Self>, key: PublicKey, name: &str) -> bool {
        let mut st = self.gate.state.lock().unwrap();
        if st.generation != self.generation || st.locked {
            return false;
        }
        st.pending -= 1;
        self.completed = true;
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
        self.gate.publish();
        true
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut st = self.gate.state.lock().unwrap();
        if st.generation != self.generation {
            return;
        }
        st.pending -= 1;
        st.failures += 1;
        warn!(failures = st.failures, "a pairing attempt did not complete");
        if st.failures >= MAX_FAILURES {
            st.locked = true;
            if let Err(e) = config::save_pairing(&st.pairing, true) {
                warn!("can't save the pairing state: {e:#}");
            }
        }
        drop(st);
        self.gate.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserved_pairing_budget_rejects_a_real_handshake_without_publishing_a_controller() {
        use airhand_link::secure::{
            InitiatorParams, ResponderParams, controller_intro, initiate, respond,
        };
        let hub = crate::status::new_hub();
        let gate = DesktopGate::load(hub.clone());
        let reserved: Vec<_> = (0..MAX_FAILURES)
            .map(|_| gate.begin_pairing().unwrap())
            .collect();
        let code = reserved[0].code().to_string();
        let me = Identity::generate();
        let intro = controller_intro("unverified", "test", Default::default(), false);
        let (a, b) = tokio::io::duplex(65536);
        let (mut ar, mut aw) = tokio::io::split(a);
        let (br, mut bw) = tokio::io::split(b);
        let (initiator, init) = initiate(InitiatorParams {
            code: 123456,
            me: &me,
            host_key: None,
            pairing: Some(&code),
            intro: &intro,
        })
        .unwrap();
        let (controller, host) = tokio::join!(
            initiator.finish(&mut ar, &mut aw),
            respond(
                &init,
                &mut bw,
                async { Ok(br) },
                ResponderParams {
                    code: 123456,
                    gate: &*gate
                }
            ),
        );
        let error = controller
            .err()
            .expect("a sixth concurrent attempt must be refused");
        assert_eq!(
            error.downcast_ref::<airhand_link::Problem>(),
            Some(&airhand_link::Problem::PairingUnavailable)
        );
        assert!(host.is_err());
        assert!(hub.borrow().host.controllers.is_empty());
        assert_eq!(gate.state.lock().unwrap().pending, MAX_FAILURES);
        drop(reserved);
        assert!(hub.borrow().host.pairing_locked);
    }

    #[test]
    fn wrong_codes_pause_pairing_until_a_new_code() {
        let hub = crate::status::new_hub();
        let gate = DesktopGate::load(hub.clone());
        let attempts: Vec<_> = (0..MAX_FAILURES)
            .map(|_| gate.begin_pairing().unwrap())
            .collect();
        assert_eq!(attempts[0].code().len(), 6);
        assert!(
            gate.begin_pairing().is_none(),
            "in-flight attempts already reserve the budget"
        );
        drop(attempts);
        assert!(gate.begin_pairing().is_none());
        assert!(hub.borrow().host.pairing_locked);
        gate.new_pairing_code();
        assert!(!hub.borrow().host.pairing_locked);

        let stale = gate.begin_pairing().unwrap();
        gate.new_pairing_code();
        let key = *Identity::generate().public();
        assert!(!stale.complete(key, "stale"));
        assert!(!gate.is_known(&key));
        assert!(gate.begin_pairing().unwrap().complete(key, "PC"));
        assert!(gate.is_known(&key));
        assert_eq!(hub.borrow().host.paired, 1);
        let stale = gate.begin_pairing().unwrap();
        gate.reset();
        assert!(!stale.complete(key, "stale"));
        assert!(!gate.is_known(&key));
        assert_eq!(gate.state.lock().unwrap().pending, 0);
    }
}
