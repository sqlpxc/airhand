//! Fixed control codes: authoritative in-memory index plus a dedicated SQLite
//! writer. Broker locks never cover disk I/O. Writes are coalesced per code (the
//! namespace has 900,000 entries), retried on failure, and drained on shutdown.
//! A disk error never makes an existing binding appear unowned.
//!
//! 持久化屏障按**写入序号**等待（[`Bindings::barrier`]）：只等到调用之前排进来的那些写入落盘，
//! 之后别人的写入不算。之前的做法是等到"没有待写的数据"，持续有注册时可能一直等不到。
//! 等待是异步的（tokio 的 watch），不占阻塞线程。
//!
//! 数据库里还存着服务器自己的密钥（[`Bindings::server_key`]，用来给发出去的控制码密钥签名，见 broker）：
//! 和绑定表放在一起，服务器重启之后还认得以前发的密钥。
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{info, warn};

pub const BINDING_TTL: Duration = Duration::from_secs(180 * 24 * 3600);
const SCHEMA_VERSION: i64 = 2;

#[derive(Clone)]
struct Binding {
    hash: Vec<u8>,
    /// 换过密钥、但还没确认被控端已经保存了新密钥时，旧密钥的哈希（见 [`Owner::Previous`]）
    prev: Option<Vec<u8>>,
    last_seen: i64,
}

/// 被控端出示的密钥和绑定的关系
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// 没有绑定
    Free,
    /// 是现在的密钥
    Current,
    /// 是换密钥之前的那个：被控端没来得及保存新密钥（例如换密钥时断线、崩溃了），仍然是它
    Previous,
    /// 绑定给了别人
    Other,
}

#[derive(Default)]
struct Writes {
    dirty: HashMap<u32, Option<Binding>>,
    /// 排进来的最后一次写入的序号
    queued: u64,
    writing: bool,
    stop: bool,
}

struct Store {
    state: Mutex<Writes>,
    cv: Condvar,
    /// 已经落盘的写入序号
    committed: watch::Sender<u64>,
}

pub struct Bindings {
    entries: HashMap<u32, Binding>,
    store: Option<Arc<Store>>,
    key: [u8; 32],
}

/// 等到某一时刻之前排进来的写入都落盘（[`Bindings::barrier`]）
pub struct Barrier {
    target: u64,
    committed: Option<watch::Receiver<u64>>,
}

impl Barrier {
    /// 返回是否在 `timeout` 之内落盘了（没有数据库时总是 `true`）
    pub async fn wait(self, timeout: Duration) -> bool {
        let Some(mut rx) = self.committed else {
            return true;
        };
        let target = self.target;
        matches!(
            tokio::time::timeout(timeout, rx.wait_for(|c| *c >= target)).await,
            Ok(Ok(_))
        )
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn secret_hash(secret: &[u8; 16]) -> Vec<u8> {
    let mut h = Sha256::new();
    h.update(b"airhand code binding v1\0");
    h.update(secret);
    h.finalize().to_vec()
}

fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!("database schema {version} is newer than this server");
    }
    db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    if version < 1 {
        db.execute_batch("BEGIN;
            CREATE TABLE fixed_codes (code INTEGER PRIMARY KEY, secret_hash BLOB NOT NULL, last_seen INTEGER NOT NULL);
            CREATE INDEX fixed_codes_last_seen ON fixed_codes (last_seen);
            PRAGMA user_version = 1; COMMIT;")?;
    }
    if version < 2 {
        // 2：换密钥时记下旧密钥的哈希；服务器自己的密钥
        db.execute_batch(
            "BEGIN;
            ALTER TABLE fixed_codes ADD COLUMN prev_hash BLOB;
            CREATE TABLE meta (key TEXT PRIMARY KEY, value BLOB NOT NULL);
            PRAGMA user_version = 2; COMMIT;",
        )?;
    }
    Ok(db)
}

fn load(db: &Connection) -> Result<HashMap<u32, Binding>> {
    let mut stmt = db.prepare("SELECT code, secret_hash, prev_hash, last_seen FROM fixed_codes")?;
    Ok(stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                Binding {
                    hash: r.get(1)?,
                    prev: r.get(2)?,
                    last_seen: r.get(3)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// 服务器自己的密钥：第一次启动时生成，之后一直用同一个
fn load_key(db: &Connection) -> Result<[u8; 32]> {
    let saved: Option<Vec<u8>> = db
        .query_row("SELECT value FROM meta WHERE key = 'server_key'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(v) = saved {
        return <[u8; 32]>::try_from(v.as_slice()).context("the saved server key is corrupt");
    }
    let key: [u8; 32] = rand::random();
    db.execute(
        "INSERT INTO meta (key, value) VALUES ('server_key', ?1)",
        [key.as_slice()],
    )?;
    Ok(key)
}

fn writer(mut db: Connection, store: Arc<Store>) {
    loop {
        let (batch, upto) = {
            let mut s = store.state.lock().unwrap();
            while s.dirty.is_empty() && !s.stop {
                s = store.cv.wait(s).unwrap();
            }
            if s.dirty.is_empty() && s.stop {
                break;
            }
            s.writing = true;
            (std::mem::take(&mut s.dirty), s.queued)
        };
        let result = (|| -> rusqlite::Result<()> {
            let tx = db.transaction()?;
            for (code, binding) in &batch {
                match binding {
                    Some(b) => {
                        tx.execute(
                        "INSERT INTO fixed_codes (code, secret_hash, prev_hash, last_seen) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT (code) DO UPDATE SET secret_hash = excluded.secret_hash,
                           prev_hash = excluded.prev_hash, last_seen = excluded.last_seen",
                        params![code, &b.hash, &b.prev, b.last_seen])?;
                    }
                    None => {
                        tx.execute("DELETE FROM fixed_codes WHERE code = ?1", [code])?;
                    }
                }
            }
            tx.commit()
        })();
        let failed = result.is_err();
        let mut s = store.state.lock().unwrap();
        s.writing = false;
        match result {
            // 这一批包含了 upto 之前排进来的全部写入（同一个控制码只留最新的值）
            Ok(()) => {
                store.committed.send_modify(|c| *c = (*c).max(upto));
            }
            Err(e) => {
                warn!(
                    "fixed control codes: persistence failed, retaining bindings and retrying: {e}"
                );
                if s.stop {
                    store.cv.notify_all();
                    break;
                }
                for (code, b) in batch {
                    s.dirty.entry(code).or_insert(b);
                }
            }
        }
        store.cv.notify_all();
        drop(s);
        if failed {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

impl Bindings {
    /// `None` 显式选择内存模式；配置了数据库时，任何初始化错误都必须阻止启动。
    /// 否则空绑定表会把已有控制码重新分配，新的服务器密钥也会使旧凭证失效。
    pub fn open(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self {
                entries: HashMap::new(),
                store: None,
                key: rand::random(),
            });
        };
        let (entries, key, db) = open(path)
            .and_then(|db| Ok((load(&db)?, load_key(&db)?, db)))
            .with_context(|| {
                format!("can't load control code ownership from {}", path.display())
            })?;
        info!(
            "{} fixed control code(s) in {}",
            entries.len(),
            path.display()
        );
        let store = Arc::new(Store {
            state: Mutex::default(),
            cv: Condvar::new(),
            committed: watch::Sender::new(0),
        });
        let worker = store.clone();
        std::thread::Builder::new()
            .name("airhand-codes".into())
            .spawn(move || writer(db, worker))
            .context("can't start the code persistence worker")?;
        Ok(Self {
            entries,
            store: Some(store),
            key,
        })
    }

    /// 服务器自己的密钥（随数据库保存；没有数据库时每次启动随机生成）
    pub fn server_key(&self) -> [u8; 32] {
        self.key
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, code: u32) -> bool {
        self.entries.contains_key(&code)
    }

    pub fn check(&self, code: u32, secret: &[u8; 16]) -> Owner {
        let Some(b) = self.entries.get(&code) else {
            return Owner::Free;
        };
        let h = secret_hash(secret);
        if b.hash == h {
            Owner::Current
        } else if b.prev.as_ref() == Some(&h) {
            Owner::Previous
        } else {
            Owner::Other
        }
    }

    fn persist(&self, code: u32) {
        if let Some(store) = &self.store {
            let mut s = store.state.lock().unwrap();
            s.queued += 1;
            s.dirty.insert(code, self.entries.get(&code).cloned());
            store.cv.notify_one();
        }
    }

    /// 把 `code` 绑定给持有 `secret` 的被控端；`prev`：换了密钥、被控端可能还没保存时的旧密钥。
    /// 返回绑定有没有实质的变化（新绑定、换了密钥）：有的话调用方要等它落盘（[`Bindings::barrier`]）
    /// 再告诉被控端，否则服务器崩溃之后被控端拿着新密钥也取不回控制码
    pub fn bind(&mut self, code: u32, secret: &[u8; 16], prev: Option<&[u8; 16]>) -> bool {
        let hash = secret_hash(secret);
        let changed = self.entries.get(&code).is_none_or(|b| b.hash != hash);
        self.entries.insert(
            code,
            Binding {
                hash,
                prev: prev.map(secret_hash),
                last_seen: now(),
            },
        );
        self.persist(code);
        changed
    }

    pub fn touch(&mut self, code: u32) {
        if let Some(b) = self.entries.get_mut(&code) {
            b.last_seen = now();
            self.persist(code);
        }
    }

    pub fn remove(&mut self, code: u32) -> bool {
        let removed = self.entries.remove(&code).is_some();
        if removed {
            self.persist(code);
        }
        removed
    }

    pub fn expire(&mut self, online: impl Fn(u32) -> bool) {
        let limit = now() - BINDING_TTL.as_secs() as i64;
        let old: Vec<_> = self
            .entries
            .iter()
            .filter(|(code, b)| b.last_seen < limit && !online(**code))
            .map(|(code, _)| *code)
            .collect();
        for code in old {
            self.remove(code);
        }
    }

    /// 到现在为止排进来的写入的屏障：在持有 broker 锁时取，**放开锁之后**再等（[`Barrier::wait`]）
    pub fn barrier(&self) -> Barrier {
        match &self.store {
            Some(store) => Barrier {
                target: store.state.lock().unwrap().queued,
                committed: Some(store.committed.subscribe()),
            },
            None => Barrier {
                target: 0,
                committed: None,
            },
        }
    }
}

impl Drop for Bindings {
    fn drop(&mut self) {
        if let Some(store) = &self.store {
            store.state.lock().unwrap().stop = true;
            store.cv.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn temp() -> std::path::PathBuf {
        std::env::temp_dir()
            .join(format!("airhand-codes-test-{:016x}", rand::random::<u64>()))
            .join("airhand.db")
    }

    #[tokio::test]
    async fn roundtrip_and_expiry() {
        let path = temp();
        let mut b = Bindings::open(Some(&path)).unwrap();
        let key = b.server_key();
        assert!(b.bind(123456, &[1; 16], None));
        assert!(b.bind(654321, &[2; 16], None));
        // 同一个密钥再绑一次（被控端取回控制码）不算变化
        assert!(!b.bind(654321, &[2; 16], None));
        b.entries.get_mut(&654321).unwrap().last_seen = 1;
        b.persist(654321);
        assert!(b.barrier().wait(Duration::from_secs(5)).await);
        drop(b);
        let mut b = Bindings::open(Some(&path)).unwrap();
        assert_eq!(b.server_key(), key, "the server key survives a restart");
        assert_eq!(b.len(), 2);
        assert_eq!(b.check(123456, &[1; 16]), Owner::Current);
        assert_eq!(b.check(123456, &[2; 16]), Owner::Other);
        assert_eq!(b.check(111111, &[2; 16]), Owner::Free);
        b.expire(|_| false);
        assert!(!b.contains(654321));
        b.remove(123456);
        b.bind(123456, &[3; 16], None); // latest write wins, including remove/rebind
        assert!(b.barrier().wait(Duration::from_secs(5)).await);
        let again = Bindings::open(Some(&path)).unwrap();
        assert_eq!(again.check(123456, &[3; 16]), Owner::Current);
        assert_eq!(again.len(), 1);
    }

    #[tokio::test]
    async fn rekey_keeps_the_previous_secret_until_confirmed() {
        let path = temp();
        let mut b = Bindings::open(Some(&path)).unwrap();
        b.bind(123456, &[1; 16], None);
        assert!(b.bind(123456, &[2; 16], Some(&[1; 16])));
        assert!(b.barrier().wait(Duration::from_secs(5)).await);
        let mut again = Bindings::open(Some(&path)).unwrap();
        assert_eq!(again.check(123456, &[2; 16]), Owner::Current);
        assert_eq!(again.check(123456, &[1; 16]), Owner::Previous);
        assert_eq!(again.check(123456, &[3; 16]), Owner::Other);
        // 被控端拿着新密钥回来了：旧的不再认
        again.bind(123456, &[2; 16], None);
        assert_eq!(again.check(123456, &[1; 16]), Owner::Other);
    }

    /// 别人一直在写，屏障也只等调用之前的那些
    #[test]
    fn barrier_does_not_wait_for_later_writes() {
        let path = temp();
        let mut b = Bindings::open(Some(&path)).unwrap();
        b.bind(100000, &[1; 16], None);
        let barrier = b.barrier();
        let writes = std::thread::scope(|s| {
            let busy = s.spawn(|| {
                let start = Instant::now();
                let mut n = 0u32;
                while start.elapsed() < Duration::from_millis(300) {
                    b.bind(200000 + n % 1000, &[n as u8; 16], None);
                    n += 1;
                }
                n
            });
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let start = Instant::now();
            assert!(rt.block_on(barrier.wait(Duration::from_secs(5))));
            assert!(start.elapsed() < Duration::from_secs(2));
            busy.join().unwrap()
        });
        assert!(writes > 0);
    }

    #[tokio::test]
    async fn locked_database_does_not_block_broker_or_lose_ownership() {
        let path = temp();
        let mut b = Bindings::open(Some(&path)).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let start = Instant::now();
        b.bind(123456, &[1; 16], None);
        assert_eq!(b.check(123456, &[2; 16]), Owner::Other);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(!b.barrier().wait(Duration::from_millis(20)).await);
        db.execute_batch("ROLLBACK").unwrap();
        assert!(b.barrier().wait(Duration::from_secs(6)).await);
    }

    #[test]
    fn newer_schema_is_left_alone() {
        let path = temp();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99")
            .unwrap();
        assert!(Bindings::open(Some(&path)).is_err());
        let v: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 99);
    }

    #[test]
    fn startup_failure_preserves_saved_ownership_and_server_key() {
        let path = temp();
        let db = open(&path).unwrap();
        let key = load_key(&db).unwrap();
        db.execute(
            "INSERT INTO fixed_codes (code, secret_hash, last_seen) VALUES (?1, ?2, ?3)",
            params![123456, secret_hash(&[7; 16]), now()],
        )
        .unwrap();
        db.execute_batch("PRAGMA user_version = 99").unwrap();
        assert!(Bindings::open(Some(&path)).is_err());
        db.execute_batch("PRAGMA user_version = 2").unwrap();
        let saved = Bindings::open(Some(&path)).unwrap();
        assert_eq!(saved.check(123456, &[7; 16]), Owner::Current);
        assert_eq!(saved.server_key(), key);

        let invalid = path.with_file_name("corrupt.db");
        std::fs::write(&invalid, b"not a SQLite database").unwrap();
        assert!(Bindings::open(Some(&invalid)).is_err());
        assert!(Bindings::open(Some(path.parent().unwrap())).is_err());
        assert_eq!(Bindings::open(None).unwrap().len(), 0);
    }

    /// 版本 1 的数据库（之前的服务器建的）升级到 2，原来的绑定都还在
    #[test]
    fn upgrades_a_version_1_database() {
        let path = temp();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("BEGIN;
            CREATE TABLE fixed_codes (code INTEGER PRIMARY KEY, secret_hash BLOB NOT NULL, last_seen INTEGER NOT NULL);
            CREATE INDEX fixed_codes_last_seen ON fixed_codes (last_seen);
            PRAGMA user_version = 1; COMMIT;").unwrap();
        db.execute(
            "INSERT INTO fixed_codes (code, secret_hash, last_seen) VALUES (?1, ?2, ?3)",
            params![123456, secret_hash(&[7; 16]), now()],
        )
        .unwrap();
        drop(db);
        let b = Bindings::open(Some(&path)).unwrap();
        assert_eq!(b.check(123456, &[7; 16]), Owner::Current);
        let v: i64 = Connection::open(&path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
    }
}
