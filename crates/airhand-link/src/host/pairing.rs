//! 被控端的配对（协议见 `airhand_proto::handshake` 的"配对"）：配对码、配对过的控制端、未成功尝试的预算。
//!
//! 桌面版和 Android 共用这一份，保存在哪里由平台决定（[`PairingStore`]：桌面版写 config.toml，Android 交给 Kotlin）：
//!
//! * 没配对过的控制端都要输入配对码（只凭控制码不行：服务器知道所有控制码）。配对码显示在本机上
//!   （桌面的托盘菜单、手机的"共享本机"页），从不经过服务器；
//! * 每次配对握手开始时先占一个名额：未成功的尝试（输错、中断、超时）加上在途的，最多 [`MAX_FAILURES`] 次。
//!   用完了就暂停配对（`locked`，保存下来，重启之后也一样），本机用户换一个配对码才恢复：中间人（包括中转服务器）
//!   一共只能猜这么几次；
//! * 换配对码、忘掉配对过的控制端之后，之前开始的握手不能再记住控制端（代际）；
//! * 更换控制码时配对码也换一个、配对过的控制端全部忘掉（[`Pairing::reset`]）。

use std::sync::Mutex;

use airhand_proto::handshake::PublicKey;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::secure::{PairingAttempt, hex, unhex32};

/// 未成功和在途的配对尝试合计上限
pub const MAX_FAILURES: u32 = 5;

/// 最多记住这么多个配对过的控制端（再多就忘掉最早的）
pub const MAX_PAIRED: usize = 64;

/// 配对过的控制端的名称最多这么多个字符
const MAX_NAME: usize = 64;

/// 配对过的一个控制端
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paired {
    /// 公钥（64 位小写十六进制）
    pub key: String,
    /// 它配对时报的名称
    pub name: String,
    /// 配对的时间（Unix 毫秒）
    pub since: i64,
}

/// 要保存、要显示的配对状态
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PairingState {
    /// 配对码（[`airhand_proto::PAIRING_DIGITS`] 位数字）
    pub code: String,
    /// 有人多次输错，暂停了配对：本机用户换一个配对码才恢复
    pub locked: bool,
    pub paired: Vec<Paired>,
}

/// 配对状态保存在哪里、显示在哪里（各平台实现）
pub trait PairingStore: Send + Sync + 'static {
    /// 配对状态变了：保存下来，刷新界面。在配对状态的锁里调用，保存的顺序和变化的顺序一致；
    /// 不要在这里回头调用 [`Pairing`]。保存不了只能记日志：这次运行里照样有效
    fn changed(&self, state: &PairingState);
}

pub struct Pairing {
    state: Mutex<Inner>,
    store: Box<dyn PairingStore>,
}

struct Inner {
    saved: PairingState,
    /// 未成功几次（配对成功、换配对码时清零；不保存：重启之后从零数，但暂停是保存的）
    failures: u32,
    /// 在途的尝试
    pending: u32,
    /// 换配对码、忘掉配对过的控制端时加一：之前开始的尝试不能再提交
    generation: u64,
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

/// 保存过的配对状态，去掉不对的：配对码不对就没有（[`Pairing::new`] 换一个新的），公钥不对的控制端去掉
fn sanitize(saved: Option<PairingState>) -> (Option<String>, bool, Vec<Paired>) {
    let Some(s) = saved else {
        return (None, false, Vec::new());
    };
    let code = airhand_proto::parse_pairing(&s.code);
    let mut paired: Vec<Paired> = s
        .paired
        .into_iter()
        .filter_map(|p| {
            let key = p.key.trim().to_ascii_lowercase();
            unhex32(&key)?;
            Some(Paired {
                key,
                name: p.name.chars().take(MAX_NAME).collect(),
                since: p.since,
            })
        })
        .collect();
    let extra = paired.len().saturating_sub(MAX_PAIRED);
    paired.drain(..extra);
    (code, s.locked, paired)
}

impl Pairing {
    /// `saved`：上次保存的。没有、或者配对码不对时生成一个新的配对码（并保存）
    pub fn new(saved: Option<PairingState>, store: impl PairingStore) -> Self {
        let (code, locked, paired) = sanitize(saved);
        let fresh = code.is_none();
        let pairing = Self {
            state: Mutex::new(Inner {
                saved: PairingState {
                    code: code.unwrap_or_else(new_code),
                    // 新的配对码不会是暂停的
                    locked: locked && !fresh,
                    paired,
                },
                failures: 0,
                pending: 0,
                generation: 0,
            }),
            store: Box::new(store),
        };
        if fresh {
            let st = pairing.lock();
            pairing.store.changed(&st.saved);
        }
        pairing
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 现在的配对状态
    pub fn state(&self) -> PairingState {
        self.lock().saved.clone()
    }

    /// 本机用户换一个配对码：暂停的配对也随之恢复，之前开始的配对作废。配对过的控制端不受影响
    pub fn new_code(&self) {
        let mut st = self.lock();
        st.saved.code = new_code();
        st.saved.locked = false;
        st.failures = 0;
        st.pending = 0;
        st.generation += 1;
        self.store.changed(&st.saved);
        info!("new pairing code");
    }

    /// 忘掉所有配对过的控制端：它们下次要重新输入配对码
    pub fn forget(&self) {
        let mut st = self.lock();
        st.saved.paired.clear();
        // 已经拿到旧授权、尚未提交的配对不能在清空之后重新加入
        st.pending = 0;
        st.generation += 1;
        self.store.changed(&st.saved);
        info!("forgot all paired controllers");
    }

    /// 更换控制码：配对码也换一个，配对过的控制端全部忘掉
    pub fn reset(&self) {
        let mut st = self.lock();
        st.saved = PairingState {
            code: new_code(),
            locked: false,
            paired: Vec::new(),
        };
        st.failures = 0;
        st.pending = 0;
        st.generation += 1;
        self.store.changed(&st.saved);
        info!("new pairing code, forgot all paired controllers");
    }

    /// 原子地占一个配对名额；暂停了、预算用完了为 `None`（见 [`crate::secure::Gate::begin_pairing`]）
    pub fn begin(&self) -> Option<Box<dyn PairingAttempt + '_>> {
        let mut st = self.lock();
        if st.saved.locked || st.failures + st.pending >= MAX_FAILURES {
            return None;
        }
        st.pending += 1;
        Some(Box::new(Attempt {
            pairing: self,
            code: st.saved.code.clone(),
            generation: st.generation,
            completed: false,
        }))
    }

    /// 这个控制端配对过
    pub fn is_known(&self, key: &PublicKey) -> bool {
        let k = hex(key);
        self.lock().saved.paired.iter().any(|p| p.key == k)
    }

    #[cfg(test)]
    fn pending(&self) -> u32 {
        self.lock().pending
    }
}

struct Attempt<'a> {
    pairing: &'a Pairing,
    code: String,
    generation: u64,
    completed: bool,
}

impl PairingAttempt for Attempt<'_> {
    fn code(&self) -> &str {
        &self.code
    }

    fn complete(mut self: Box<Self>, key: PublicKey, name: &str) -> bool {
        let mut st = self.pairing.lock();
        if st.generation != self.generation || st.saved.locked {
            return false;
        }
        st.pending -= 1;
        self.completed = true;
        let k = hex(&key);
        let paired = &mut st.saved.paired;
        paired.retain(|p| p.key != k);
        paired.push(Paired {
            key: k,
            name: name.chars().take(MAX_NAME).collect(),
            since: now_ms(),
        });
        let extra = paired.len().saturating_sub(MAX_PAIRED);
        paired.drain(..extra);
        st.failures = 0;
        self.pairing.store.changed(&st.saved);
        true
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut st = self.pairing.lock();
        if st.generation != self.generation {
            return;
        }
        st.pending -= 1;
        st.failures += 1;
        warn!(failures = st.failures, "a pairing attempt did not complete");
        if st.failures >= MAX_FAILURES && !st.saved.locked {
            st.saved.locked = true;
            self.pairing.store.changed(&st.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure::Identity;
    use std::sync::Arc;

    /// 记下每次保存的状态
    #[derive(Clone, Default)]
    struct Saved(Arc<Mutex<Vec<PairingState>>>);

    impl PairingStore for Saved {
        fn changed(&self, state: &PairingState) {
            self.0.lock().unwrap().push(state.clone());
        }
    }

    impl Saved {
        fn last(&self) -> PairingState {
            self.0.lock().unwrap().last().cloned().unwrap()
        }
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    fn key() -> PublicKey {
        *Identity::generate().public()
    }

    /// 第一次没有配对码：生成一个并保存；保存过的照样用，不对的去掉
    #[test]
    fn loads_or_creates_the_state() {
        let store = Saved::default();
        let p = Pairing::new(None, store.clone());
        assert_eq!(store.count(), 1);
        assert_eq!(store.last(), p.state());
        assert_eq!(p.state().code.len(), airhand_proto::PAIRING_DIGITS);

        let good = hex(&key());
        let saved = PairingState {
            code: "482 913".into(),
            locked: true,
            paired: vec![
                Paired {
                    key: good.to_ascii_uppercase(),
                    name: "x".repeat(100),
                    since: 1,
                },
                Paired {
                    key: "not a key".into(),
                    name: "bad".into(),
                    since: 2,
                },
            ],
        };
        let store = Saved::default();
        let p = Pairing::new(Some(saved.clone()), store.clone());
        assert_eq!(store.count(), 0, "nothing changed, nothing to save");
        let s = p.state();
        assert_eq!((s.code.as_str(), s.locked), ("482913", true));
        assert_eq!(s.paired.len(), 1);
        assert_eq!(s.paired[0].key, good);
        assert_eq!(s.paired[0].name.chars().count(), MAX_NAME);
        assert!(p.begin().is_none(), "paused pairing stays paused");

        // 配对码坏了：换一个新的（不暂停），配对过的照样记得
        let p = Pairing::new(
            Some(PairingState {
                code: "12".into(),
                ..saved
            }),
            store.clone(),
        );
        assert!(!p.state().locked);
        assert_eq!(store.last(), p.state());
        assert_eq!(p.state().paired.len(), 1);
    }

    #[test]
    fn wrong_codes_pause_pairing_until_a_new_code() {
        let store = Saved::default();
        let p = Pairing::new(None, store.clone());
        let attempts: Vec<_> = (0..MAX_FAILURES).map(|_| p.begin().unwrap()).collect();
        assert_eq!(attempts[0].code(), p.state().code);
        assert!(
            p.begin().is_none(),
            "in-flight attempts already reserve the budget"
        );
        drop(attempts);
        assert!(p.begin().is_none());
        assert!(p.state().locked);
        assert!(store.last().locked, "the pause is saved");
        p.new_code();
        assert!(!p.state().locked);
        assert!(!store.last().locked);

        // 换配对码、忘掉配对过的、更换控制码之后，之前开始的尝试不能再记住控制端
        let stale = p.begin().unwrap();
        p.new_code();
        let k = key();
        assert!(!stale.complete(k, "stale"));
        assert!(!p.is_known(&k));
        assert!(p.begin().unwrap().complete(k, "PC"));
        assert!(p.is_known(&k));
        assert_eq!(store.last().paired[0].name, "PC");
        let stale = p.begin().unwrap();
        p.forget();
        assert!(!stale.complete(k, "stale"));
        assert!(!p.is_known(&k));
        assert!(p.begin().unwrap().complete(k, "PC"));
        let code = p.state().code;
        let stale = p.begin().unwrap();
        p.reset();
        assert!(!stale.complete(k, "stale"));
        assert!(!p.is_known(&k));
        assert_ne!(p.state().code, code);
        assert_eq!(p.pending(), 0);
        assert!(store.last().paired.is_empty());
    }

    /// 再配对一次不会重复记；记太多了忘掉最早的
    #[test]
    fn paired_controllers_are_deduplicated_and_bounded() {
        let p = Pairing::new(None, Saved::default());
        let first = key();
        assert!(p.begin().unwrap().complete(first, "a"));
        assert!(p.begin().unwrap().complete(first, "b"));
        assert_eq!(p.state().paired.len(), 1);
        assert_eq!(p.state().paired[0].name, "b");
        for _ in 0..MAX_PAIRED {
            assert!(p.begin().unwrap().complete(key(), "x"));
        }
        assert_eq!(p.state().paired.len(), MAX_PAIRED);
        assert!(!p.is_known(&first));
    }
}
