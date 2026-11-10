//! 控制码登记：被控端注册（分配或者取回控制码）、换密钥、放弃控制码；控制端按控制码发起会话、等被控端上线。
//!
//! 被控端掉线后控制码保留一段时间（[`RESERVE_TTL`]），固定的控制码（[`crate::codes`]）一直保留。
//! 控制端发起的会话登记到 [`super::relay`]，两端的流到了之后由它配对、转发。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

use airhand_link::wire::read_msg;
use airhand_proto::signal::{
    ClientMsg, ConnectError, Peer, Reclaim, ResumeToken, ServerMsg, SessionGrant,
};
use airhand_proto::{CODE_MAX, CODE_MIN, MAX_SIGNAL_MSG};
use anyhow::Result;
use quinn::{RecvStream, VarInt};
use rand::Rng;
use tracing::{debug, info, warn};

use super::limits::{FAIL_LIMIT, SlotGuard, limiter_key, registration_key};
use super::relay::Pending;
use super::tokens::{Secrets, ct_eq};
use super::{Broker, SignalTx, close};
use crate::codes::{Barrier, Owner};

/// 被控端掉线后，控制码保留的时长（期间只有原设备能取回）。固定的控制码（`keep`）一直保留，见 [`crate::codes`]。
pub(super) const RESERVE_TTL: Duration = Duration::from_secs(10 * 60);

/// 每条控制端连接最多同时等这么多个被控端上线，再多就忘掉最早的
pub(super) const MAX_WAITING: usize = 32;

pub(super) struct HostEntry {
    conn_id: u64,
    /// 登记完了、告诉过它控制码（[`Broker::announce`]）：之前的连接请求和控制码不存在时答得一样
    pub(super) ready: bool,
    secrets: Secrets,
    /// 被控端发过 [`ClientMsg::Release`]：断开后不再保留这个控制码
    released: bool,
    name: String,
    os: String,
    tx: SignalTx,
    conn: quinn::Connection,
    sessions: Arc<AtomicUsize>,
}

pub(super) struct Reserved {
    secrets: Secrets,
    until: Instant,
}

/// 等被控端上线的控制端（带着本服务器为这个控制码发的凭证，回答过它 [`ConnectError::Offline`]）。
/// 被控端用这个控制码上线时发 [`ServerMsg::HostOnline`]，然后忘掉。凭证的后一半对不上（被控端换过密钥）的也通知：
/// 它重连时会收到 [`ConnectError::Revoked`]，而不是一直等下去
pub(super) struct Waiter {
    conn_id: u64,
    tx: SignalTx,
}

/// 发起连接请求的控制端（一条连接）
struct Ctrl<'a> {
    conn_id: u64,
    /// 限速按这个来源统计（[`limiter_key`]）
    ip: IpAddr,
    peer: SocketAddr,
    name: &'a str,
    tx: &'a SignalTx,
}

/// 连接请求被拒绝的原因（发给控制端的是 [`Denied::error`]）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Denied {
    /// 控制码不存在，或者被控端还没准备好（正在注册、正在断开）：算一次猜错，晚一点回答。
    /// 这两种情况必须答得一模一样，否则能靠回答的快慢看出控制码存在
    Unknown,
    /// 回答带凭证的请求：被控端不在线（凭证的前一半是本服务器为这个控制码签的，回答实情不泄露什么）
    Offline,
    /// 回答带凭证的请求：凭证作废了，或者不是本服务器为这个控制码发的
    Revoked,
    /// 猜错次数过多
    Limited,
    /// 这个来源短时间内向这台被控端发起了太多会话（[`super::Config::connects_per_minute`]）
    Flooding,
    /// 对方会话数已满
    Busy,
}

impl Denied {
    fn error(self) -> ConnectError {
        match self {
            Denied::Unknown => ConnectError::NotFound,
            Denied::Offline => ConnectError::Offline,
            Denied::Revoked => ConnectError::Revoked,
            Denied::Limited | Denied::Flooding => ConnectError::RateLimited,
            Denied::Busy => ConnectError::Busy,
        }
    }
}

/// 注册被拒绝的原因（关闭连接时告诉被控端）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refused {
    /// 这个来源短时间内申请了太多新控制码
    Limited,
    /// 控制码用完了
    Exhausted,
}

impl Refused {
    fn reason(self) -> &'static [u8] {
        match self {
            Refused::Limited => {
                b"too many new control codes from this network, try again in a few minutes"
            }
            Refused::Exhausted => b"the server has no free control codes left",
        }
    }
}

/// 连接 `conn_id` 不再等 `code` 上线了
fn forget_waiter(all: &mut HashMap<u32, Vec<Waiter>>, code: u32, conn_id: u64) {
    if let Some(list) = all.get_mut(&code) {
        list.retain(|w| w.conn_id != conn_id);
        if list.is_empty() {
            all.remove(&code);
        }
    }
}

/// 被控端的一条连接（注册、换密钥时用）
pub(super) struct HostConn<'a> {
    pub(super) conn: &'a quinn::Connection,
    pub(super) conn_id: u64,
    pub(super) peer: SocketAddr,
    pub(super) name: &'a str,
    pub(super) os: &'a str,
    pub(super) tx: &'a SignalTx,
}

impl Broker {
    pub(super) async fn host_loop(
        self: &Arc<Self>,
        host: &HostConn<'_>,
        mut recv: RecvStream,
        buf: &mut Vec<u8>,
        code: &mut Option<u32>,
    ) -> Result<()> {
        let peer = host.peer;
        let conn_id = host.conn_id;
        while let Some(msg) = read_msg::<_, ClientMsg>(&mut recv, MAX_SIGNAL_MSG, buf).await? {
            match (msg, *code) {
                (
                    ClientMsg::Register {
                        reclaim,
                        keep,
                        rekey,
                    },
                    None,
                ) => {
                    let (got, barrier) = match self.register(host, reclaim, keep, rekey) {
                        Ok(r) => r,
                        Err(refused) => {
                            info!(%peer, conn_id, ?refused, "registration refused");
                            host.conn
                                .close(VarInt::from_u32(close::REGISTER_REFUSED), refused.reason());
                            return Ok(());
                        }
                    };
                    *code = Some(got);
                    // 绑定有实质变化（新绑定、换了密钥）时，落盘之后才告诉被控端（在所有 broker 锁之外等）
                    if let Some(barrier) = barrier {
                        self.persisted(barrier).await;
                    }
                    self.announce(conn_id, got);
                    info!(%peer, conn_id, code = got, keep, rekey, reclaimed = reclaim.is_some_and(|r| r.code == got), "host registered");
                }
                (ClientMsg::Release, Some(c)) => {
                    if self.release(conn_id, c) {
                        info!(%peer, conn_id, code = c, "host released its control code");
                    }
                }
                (ClientMsg::Rekey { secret }, Some(c)) => {
                    let Some((secret, barrier)) = self.rekey(conn_id, c, secret)? else {
                        continue;
                    };
                    if let Some(barrier) = barrier {
                        self.persisted(barrier).await;
                    }
                    let _ = host.tx.send(ServerMsg::Rekeyed { secret });
                    info!(%peer, conn_id, code = c, "host changed its secret, earlier reconnect tokens are void");
                }
                (other, _) => warn!(%peer, "invalid message from host: {other:?}"),
            }
        }
        Ok(())
    }

    /// 分配（或者还回原来的）控制码并登记被控端（还不发布，见 [`Broker::announce`]）。
    /// `rekey`：还回原来的控制码时换一个密钥（被控端之前的 [`ClientMsg::Rekey`] 没等到回答，见 `ClientMsg::Register`）。
    /// 返回控制码，以及绑定有实质变化时要等它落盘的屏障
    fn register(
        &self,
        host: &HostConn<'_>,
        reclaim: Option<Reclaim>,
        keep: bool,
        rekey: bool,
    ) -> Result<(u32, Option<Barrier>), Refused> {
        let mut hosts = self.hosts.lock();
        let mut reserved = self.reserved.lock();
        let mut bindings = self.bindings.lock();
        let now = Instant::now();

        let mut chosen: Option<(u32, Secrets)> = None;
        if let Some(r) = reclaim.filter(|r| (CODE_MIN..=CODE_MAX).contains(&r.code)) {
            let owner = if let Some(old) = hosts.get(&r.code) {
                old.secrets.owner(&r.secret)
            } else if let Some(res) = reserved.get(&r.code).filter(|res| res.until > now) {
                res.secrets.owner(&r.secret)
            } else {
                match bindings.check(r.code, &r.secret) {
                    // 空闲的控制码（服务器重启过，或者保留期过了、还没给别人）：只还给出示了本服务器为它签发的密钥的
                    Owner::Free if self.issued_here(r.code, &r.secret) => Owner::Current,
                    Owner::Free => Owner::Other,
                    owner => owner,
                }
            };
            let secrets = match owner {
                Owner::Current if !rekey => Some(Secrets::new(r.secret)),
                // 被控端没来得及保存上次换的密钥，或者请求换的密钥没等到回答（服务器可能没收到、或者没记下来就重启了）：
                // 签发一个新的给它，用它手里的密钥算出的重连凭证随之作废；它手里的密钥继续认到它保存了新的为止
                Owner::Current | Owner::Previous => Some(Secrets {
                    cur: self.issue_secret(r.code),
                    prev: Some(r.secret),
                }),
                Owner::Free | Owner::Other => None,
            };
            if let Some(secrets) = secrets {
                // 旧连接尚未超时（例如网络切换后立即重连）：顶替旧连接
                if let Some(old) = hosts.remove(&r.code) {
                    old.conn
                        .close(VarInt::from_u32(close::REPLACED), b"replaced");
                }
                reserved.remove(&r.code);
                chosen = Some((r.code, secrets));
            }
        }

        let (code, secrets) = match chosen {
            Some(chosen) => chosen,
            None => {
                let source = registration_key(host.peer.ip());
                if self.new_codes.count(source) >= self.cfg.new_codes_per_ip {
                    return Err(Refused::Limited);
                }
                let free = |c: &u32| {
                    !hosts.contains_key(c) && !reserved.contains_key(c) && !bindings.contains(*c)
                };
                let mut rng = rand::rng();
                // 随机挑几次（平常第一次就是空闲的），挑不到再从随机位置开始逐个找
                let code = (0..64)
                    .map(|_| rng.random_range(CODE_MIN..=CODE_MAX))
                    .find(free)
                    .or_else(|| {
                        let count = CODE_MAX - CODE_MIN + 1;
                        let start = rng.random_range(0..count);
                        (0..count)
                            .map(|i| CODE_MIN + (start + i) % count)
                            .find(free)
                    })
                    .ok_or(Refused::Exhausted)?;
                self.new_codes.hit(source);
                (code, Secrets::new(self.issue_secret(code)))
            }
        };
        let barrier = if keep {
            bindings
                .bind(code, &secrets.cur, secrets.prev.as_ref())
                .then(|| bindings.barrier())
        } else {
            // 这个控制码是它的（取回的或者新分配的），它不再保存控制码了：解除绑定
            bindings.remove(code);
            None
        };
        drop(bindings);

        hosts.insert(
            code,
            HostEntry {
                conn_id: host.conn_id,
                ready: false,
                secrets,
                released: false,
                name: host.name.to_string(),
                os: host.os.to_string(),
                tx: host.tx.clone(),
                conn: host.conn.clone(),
                sessions: Arc::new(AtomicUsize::new(0)),
            },
        );
        Ok((code, barrier))
    }

    fn announce(&self, conn_id: u64, code: u32) {
        let mut hosts = self.hosts.lock();
        let Some(h) = hosts.get_mut(&code).filter(|h| h.conn_id == conn_id) else {
            return;
        };
        h.ready = true;
        let secret = h.secrets.cur;
        let tx = &h.tx;
        // 在通知控制端之前（并且还持有锁）发出：被控端收到的第一条消息必须是 Registered，
        // 控制端收到通知后马上发起的会话（Incoming）只能排在它后面
        let _ = tx.send(ServerMsg::Registered { code, secret });

        // 叫醒在等这个控制码的控制端（它们的凭证是本服务器为这个控制码发的）。凭证作废了的（被控端换过密钥、
        // 控制码换了主人）重连时会收到 Revoked，而不是一直等下去
        if let Some(list) = self.waiters.lock().remove(&code) {
            for w in &list {
                debug!(
                    code,
                    ctrl = w.conn_id,
                    "telling a waiting controller that the host is back"
                );
                let _ = w.tx.send(ServerMsg::HostOnline { code });
            }
        }
    }

    /// 保留期过了的控制码不再保留（[`Broker::janitor`]）
    pub(super) fn expire_reserved(&self, now: Instant) {
        self.reserved.lock().retain(|_, r| r.until > now);
    }

    /// 很久没有上线的固定控制码解除绑定（[`Broker::janitor`]，每小时一次）。在线的被控端不算
    pub(super) fn expire_bindings(&self) {
        let hosts = self.hosts.lock();
        let mut bindings = self.bindings.lock();
        bindings.expire(|code| hosts.contains_key(&code));
    }

    /// 被控端断开：控制码保留一段时间（固定的控制码一直保留）
    pub(super) fn unregister(&self, conn_id: u64, code: u32) {
        let mut hosts = self.hosts.lock();
        // 被同一台被控端的新连接顶替了：登记已经是新连接的
        if !hosts.get(&code).is_some_and(|h| h.conn_id == conn_id) {
            return;
        }
        let h = hosts.remove(&code).unwrap();
        if h.released {
            return;
        }
        self.reserved.lock().insert(
            code,
            Reserved {
                secrets: h.secrets,
                until: Instant::now() + RESERVE_TTL,
            },
        );
        self.bindings.lock().touch(code);
    }

    /// 被控端放弃控制码：解除绑定，断开后也不保留
    fn release(&self, conn_id: u64, code: u32) -> bool {
        let mut hosts = self.hosts.lock();
        let Some(h) = hosts.get_mut(&code).filter(|h| h.conn_id == conn_id) else {
            return false;
        };
        h.released = true;
        self.bindings.lock().remove(code);
        true
    }

    /// 被控端的用户断开了所有控制端：换一个密钥，之前发出的重连凭证随之作废。
    /// 被控端手里的旧密钥继续认，直到它带着新密钥回来（它可能没来得及保存新的）。
    /// 返回新的密钥，以及固定的控制码要等它落盘的屏障
    /// 被控端明确证明持有新密钥后才淘汰上一把（`known`：它已经保存的密钥）；重复请求不会继续轮换。
    fn rekey(
        &self,
        conn_id: u64,
        code: u32,
        known: [u8; 16],
    ) -> Result<Option<([u8; 16], Option<Barrier>)>> {
        let mut hosts = self.hosts.lock();
        let Some(h) = hosts
            .get_mut(&code)
            .filter(|h| h.conn_id == conn_id && !h.released)
        else {
            return Ok(None);
        };
        let owner = h.secrets.owner(&known);
        anyhow::ensure!(
            matches!(owner, Owner::Current | Owner::Previous),
            "rekey used an unknown host secret"
        );
        if owner == Owner::Current {
            h.secrets = Secrets {
                cur: self.issue_secret(code),
                prev: Some(h.secrets.cur),
            };
        }
        let secrets = h.secrets;
        let mut bindings = self.bindings.lock();
        let barrier = bindings.contains(code).then(|| {
            if owner == Owner::Current {
                bindings.bind(code, &secrets.cur, secrets.prev.as_ref());
            }
            bindings.barrier()
        });
        Ok(Some((secrets.cur, barrier)))
    }

    pub(super) async fn controller_loop(
        self: &Arc<Self>,
        conn_id: u64,
        peer: SocketAddr,
        name: String,
        tx: SignalTx,
        mut recv: RecvStream,
        buf: &mut Vec<u8>,
    ) -> Result<()> {
        let ctrl = Ctrl {
            conn_id,
            ip: limiter_key(peer.ip()),
            peer,
            name: &name,
            tx: &tx,
        };
        // 这条连接在等哪些被控端上线（最早的在前面）
        let mut waiting: Vec<u32> = Vec::new();
        let result = async {
            while let Some(msg) = read_msg::<_, ClientMsg>(&mut recv, MAX_SIGNAL_MSG, buf).await? {
                let ClientMsg::Connect {
                    req_id,
                    code,
                    resume,
                } = msg
                else {
                    warn!(%peer, "invalid message from controller");
                    continue;
                };
                let result = self.connect(&ctrl, code, resume, &mut waiting);
                if let Err(denied) = result {
                    if denied == Denied::Unknown {
                        self.record_failure(ctrl.ip);
                        // 放慢猜码速度
                        tokio::time::sleep(Duration::from_millis(400)).await;
                    }
                    info!(%peer, code, "connect request failed: {denied:?}");
                }
                let result = result.map_err(Denied::error);
                let _ = tx.send(ServerMsg::connect_result(req_id, result));
            }
            anyhow::Ok(())
        }
        .await;
        let mut all = self.waiters.lock();
        for code in waiting {
            forget_waiter(&mut all, code, conn_id);
        }
        result
    }

    /// `resume` 是控制端带来的重连凭证（见 [`Broker::resume_token`]）。
    fn connect(
        &self,
        ctrl: &Ctrl,
        code: u32,
        resume: Option<ResumeToken>,
        waiting: &mut Vec<u32>,
    ) -> Result<SessionGrant, Denied> {
        let hosts = self.hosts.lock();
        let host = match resume {
            Some(token) => {
                // 不是本服务器为这个控制码发的（伪造的、旧版本的、别的服务器的）：回答只取决于凭证本身，
                // 和控制码在不在、有没有人用都无关，所以凭证不能用来猜码
                if !self.vouches(code, &token) {
                    return Err(Denied::Revoked);
                }
                // 是本服务器为这个控制码发的：控制端本来就知道这个控制码，回答实情，不算猜错，也不必限速
                match hosts.get(&code) {
                    Some(h) if h.ready => {
                        if !ct_eq(&self.resume_token(code, &h.secrets.cur), &token) {
                            // 被控端换过密钥（断开过所有控制端）、控制码换了主人
                            return Err(Denied::Revoked);
                        }
                        h
                    }
                    // 不在线，或者正在上线：等它
                    _ => {
                        self.wait(ctrl, code, waiting);
                        return Err(Denied::Offline);
                    }
                }
            }
            None => {
                // 先检查限速再查控制码：被限速时不透露控制码是否存在
                if self.failure_count(ctrl.ip) >= FAIL_LIMIT {
                    return Err(Denied::Limited);
                }
                // 还没准备好的被控端（正在注册、正在断开）和不存在的控制码答得一样
                match hosts.get(&code) {
                    Some(h) if h.ready => h,
                    _ => return Err(Denied::Unknown),
                }
            }
        };
        // 控制码对了（带凭证的也一样）也限速：每批准一个，被控端就要开一个会话、可能还要问一次本机用户，
        // 并且占着一个会话槽位直到配对超时。只数批准了的：被限速的来源本来就连上过这个控制码，这个回答不透露什么
        let source = (ctrl.ip, code);
        if self.connects.count(source) >= self.cfg.connects_per_minute {
            return Err(Denied::Flooding);
        }
        // 预占一个会话槽位（超过上限时随之归还）
        let (slot, before) = SlotGuard::take(&host.sessions);
        if before >= self.cfg.max_sessions_per_host {
            return Err(Denied::Busy);
        }
        let label = format!("{}@{} -> {code}", ctrl.name, ctrl.peer);
        let session_id = self.add_session(Pending::new(
            (host.conn_id, host.conn.clone()),
            (ctrl.conn_id, ctrl.tx.conn.clone()),
            slot,
            label.clone(),
        ));
        let incoming = ServerMsg::Incoming {
            session_id,
            peer: Peer {
                name: ctrl.name.to_string(),
                ip: ctrl.peer.ip(),
            },
        };
        if host.tx.send(incoming).is_err() {
            // 被控端正在断开（槽位随着会话的登记一起归还）
            self.remove_session(session_id);
            return Err(match resume {
                Some(_) => {
                    self.wait(ctrl, code, waiting);
                    Denied::Offline
                }
                None => Denied::Unknown,
            });
        }
        self.connects.hit(source);
        info!(session_id, "{label}: session created");
        Ok(SessionGrant {
            session_id,
            host_name: host.name.clone(),
            host_os: host.os.clone(),
            resume: self.resume_token(code, &host.secrets.cur),
        })
    }

    /// 记下 `ctrl` 在等 `code` 上线（调用方持有 hosts 锁，所以不会错过同时发生的上线）。
    /// `waiting` 是这条连接在等的控制码，最多 [`MAX_WAITING`] 个
    fn wait(&self, ctrl: &Ctrl, code: u32, waiting: &mut Vec<u32>) {
        let mut all = self.waiters.lock();
        let list = all.entry(code).or_default();
        list.retain(|w| w.conn_id != ctrl.conn_id);
        list.push(Waiter {
            conn_id: ctrl.conn_id,
            tx: ctrl.tx.clone(),
        });
        waiting.retain(|c| *c != code);
        waiting.push(code);
        if waiting.len() > MAX_WAITING {
            let oldest = waiting.remove(0);
            forget_waiter(&mut all, oldest, ctrl.conn_id);
        }
    }
}
