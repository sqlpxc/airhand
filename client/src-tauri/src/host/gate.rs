//! 被控端的端到端身份和授权（协议见 `airhand_proto::handshake`）：本机的身份密钥、配对码、配对过的控制端。
//!
//! * 都保存在 config.toml 里（`[host]`、`[[paired]]`，见 [`crate::config`]），身份密钥和配对码加密保存；
//! * 配对的规则（没配对过的都要配对码、连错几次暂停、换码之后旧握手作废）和 Android 共用，在
//!   [`airhand_link::host::pairing`]；这里只管保存到哪里、托盘里显示什么；
//! * 更换控制码时配对码也换一个、配对过的控制端全部忘掉（见 [`DesktopGate::reset`]）。

use std::sync::Arc;

use airhand_link::host::{Admitted, Gate, Pairing, PairingState, PairingStore};
use airhand_link::secure::{Identity, PairingAttempt, hex};
use airhand_proto::handshake::PublicKey;
use tracing::{info, warn};

use crate::config;
use crate::status::StatusHub;

pub struct DesktopGate {
    identity: Identity,
    pairing: Pairing,
    hub: StatusHub,
}

/// 配对状态写进 config.toml，托盘里显示配对码、暂停了没有、配对过几个
struct ConfigStore(StatusHub);

impl PairingStore for ConfigStore {
    fn changed(&self, state: &PairingState) {
        if let Err(e) = config::save_pairing_state(state) {
            warn!("can't save the pairing state: {e:#}");
        }
        publish(&self.0, state);
    }
}

fn publish(hub: &StatusHub, state: &PairingState) {
    let pairing = Some(state.code.clone());
    let (locked, paired) = (state.locked, state.paired.len());
    hub.send_if_modified(|s| {
        let changed =
            s.host.pairing != pairing || s.host.pairing_locked != locked || s.host.paired != paired;
        s.host.pairing = pairing;
        s.host.pairing_locked = locked;
        s.host.paired = paired;
        changed
    });
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
        let pairing = Pairing::new(Some(saved.pairing), ConfigStore(hub.clone()));
        let state = pairing.state();
        info!(key = %hex(identity.public()), paired = state.paired.len(), "host identity loaded");
        publish(&hub, &state);
        Arc::new(Self {
            identity,
            pairing,
            hub,
        })
    }

    /// 本机用户换一个配对码（托盘菜单）：暂停的配对也随之恢复。配对过的控制端不受影响
    pub fn new_pairing_code(&self) {
        self.pairing.new_code();
    }

    /// 忘掉所有配对过的控制端（托盘菜单）：它们下次要重新输入配对码
    pub fn forget_paired(&self) {
        self.pairing.forget();
    }

    /// 更换控制码：配对码也换一个，配对过的控制端全部忘掉
    pub fn reset(&self) {
        self.pairing.reset();
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

    fn begin_pairing(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        self.pairing.begin()
    }

    fn is_known(&self, key: &PublicKey) -> bool {
        self.pairing.is_known(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_link::host::pairing::MAX_FAILURES;

    /// 名额被占满时，真的握手也被拒绝，控制端不会出现在托盘的列表里；名额放掉之后暂停配对，托盘里看得到
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
        assert_eq!(hub.borrow().host.pairing.as_deref(), Some(code.as_str()));
        let me = Identity::generate();
        let intro = controller_intro("unverified", "test", Default::default(), false, Vec::new());
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
        drop(reserved);
        assert!(hub.borrow().host.pairing_locked);
        gate.new_pairing_code();
        assert!(!hub.borrow().host.pairing_locked);
        assert_ne!(hub.borrow().host.pairing.as_deref(), Some(code.as_str()));
    }

    /// 托盘里的配对过几个跟着变；更换控制码时全部忘掉
    #[test]
    fn the_tray_follows_the_pairing_state() {
        let hub = crate::status::new_hub();
        let gate = DesktopGate::load(hub.clone());
        let key = *Identity::generate().public();
        assert!(gate.begin_pairing().unwrap().complete(key, "PC"));
        assert!(gate.is_known(&key));
        assert_eq!(hub.borrow().host.paired, 1);
        gate.reset();
        assert!(!gate.is_known(&key));
        assert_eq!(hub.borrow().host.paired, 0);
    }
}
