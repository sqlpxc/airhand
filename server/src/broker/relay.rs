//! 会话中转。会话由 [`super::registry`] 登记（[`Broker::add_session`]），从那时起：
//!
//! * 两端开过来的通道（单向流，开头是会话头）转给另一端：服务器向另一端开一条单向流、写同样的会话头，
//!   然后以 `Bytes` 块为单位零拷贝透传（[`pipe`]）。不用配对，也不管通道是什么：每端每条通道只能开一次，最多
//!   [`airhand_proto::MAX_LANES`] 条；
//! * 帧流（会话头带 `SessionHeader::FRAME`，信令 v10，见 `airhand_proto` 的 crate 说明）：同一条通道可以开很多条，
//!   每端同时开着的不超过 [`MAX_FRAME_STREAMS`]，转发的办法一样。一端放弃了一条帧流（reset），转给另一端的那条也 reset；
//!   另一端叫停了（stop），这一端的也叫停。控制通道不能是帧流；
//! * 会话的数据报按开头的会话 ID 在两端之间原样转发（[`Broker::relay_datagrams`]）。
//!
//! 任何一端的控制通道（0 号）结束，整个会话结束：其它通道随之断开、不再转发数据报、归还会话槽位。
//! 两端的控制通道一直没都到的会话由 [`Broker::janitor`] 清掉。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use airhand_proto::{MAX_FRAME_STREAMS, Role, SessionHeader, SessionLane, datagram};
use anyhow::{Context, Result, anyhow, bail};
use quinn::{RecvStream, SendStream, VarInt};
use rand::Rng;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, info};

use super::limits::SlotGuard;
use super::{Broker, HANDSHAKE_TIMEOUT};

/// 两端的控制通道最多等这么久
const PENDING_TTL: Duration = Duration::from_secs(20);

/// 一个会话：从服务器批准开始，到任何一端的控制通道结束为止
pub(super) struct Session {
    /// 两端的连接（编号和连接）
    host: (u64, quinn::Connection),
    ctrl: (u64, quinn::Connection),
    created: Instant,
    /// 每一端开过哪些通道（第 i 位是 i 号通道），`[被控端, 控制端]`
    opened: [u8; 2],
    /// 每一端同时开着几条帧流（见模块说明），`[被控端, 控制端]`
    frames: [u16; 2],
    /// 会话结束时告诉所有转发任务
    ended: watch::Sender<bool>,
    /// 被控端的一个会话槽位，会话结束时归还
    _slot: SlotGuard,
    label: String,
}

const CONTROL: u8 = 1 << SessionLane::Control.number();

impl Session {
    pub(super) fn new(
        host: (u64, quinn::Connection),
        ctrl: (u64, quinn::Connection),
        slot: SlotGuard,
        label: String,
    ) -> Self {
        Self {
            host,
            ctrl,
            created: Instant::now(),
            opened: [0; 2],
            frames: [0; 2],
            ended: watch::Sender::new(false),
            _slot: slot,
            label,
        }
    }

    /// 两端的控制通道都到了
    fn started(&self) -> bool {
        self.opened.iter().all(|o| o & CONTROL != 0)
    }

    /// 连接 `conn_id` 发来的东西转给谁；不是这个会话的两端时为 `None`
    fn peer_of(&self, conn_id: u64) -> Option<&quinn::Connection> {
        if conn_id == self.host.0 {
            Some(&self.ctrl.1)
        } else if conn_id == self.ctrl.0 {
            Some(&self.host.1)
        } else {
            None
        }
    }
}

fn reject(mut recv: RecvStream) {
    let _ = recv.stop(VarInt::from_u32(1));
}

/// 把 `rx` 转进 `tx`，直到 `rx` 结束（之后结束 `tx`）、出错或者会话结束（之后两头都断开）
async fn pipe(mut rx: RecvStream, mut tx: SendStream, mut ended: watch::Receiver<bool>) {
    let copy = async {
        while let Some(chunk) = rx.read_chunk(256 * 1024, true).await? {
            tx.write_chunk(chunk.bytes).await?;
        }
        anyhow::Ok(())
    };
    let r = tokio::select! {
        r = copy => r,
        _ = ended.wait_for(|e| *e) => Err(anyhow!("the session ended")),
    };
    match r {
        Ok(()) => {
            let _ = tx.finish();
        }
        Err(_) => {
            let _ = tx.reset(VarInt::from_u32(0));
            let _ = rx.stop(VarInt::from_u32(0));
        }
    }
}

impl Broker {
    /// 登记一个新会话，从现在起转发它的通道和数据报。返回随机挑的会话 ID（挑和登记在同一把锁里，不会撞上别的会话）
    pub(super) fn add_session(&self, s: Session) -> u64 {
        let mut sessions = self.sessions.lock();
        let mut rng = rand::rng();
        let id = loop {
            let id: u64 = rng.random();
            if id != 0 && !sessions.contains_key(&id) {
                break id;
            }
        };
        sessions.insert(id, s);
        id
    }

    /// 结束一个会话：转发任务随之断开各自的通道，不再转发数据报，归还会话槽位
    pub(super) fn end_session(&self, id: u64) {
        let s = self.sessions.lock().remove(&id);
        if let Some(s) = s {
            s.ended.send_replace(true);
            info!(session_id = id, "{}: session ended", s.label);
        }
    }

    /// 两端的控制通道一直没都到的会话
    pub(super) fn expire_sessions(&self, now: Instant) {
        let expired: Vec<u64> = self
            .sessions
            .lock()
            .iter()
            .filter(|(_, s)| !s.started() && now - s.created > PENDING_TTL)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            debug!(session_id = id, "the session never started");
            self.end_session(id);
        }
    }

    /// 转发连接 `conn_id` 发来的数据报：开头的会话 ID 是这条连接所在的会话，就原样转给会话的另一端，否则丢掉。
    /// 数据报本来就可能丢：另一端的发送缓冲满了、太大了，都直接丢掉。
    ///
    /// 声音、画面确认、Ping 每个会话每秒几百个：这条连接自己记着它的会话的另一端（`peers`），
    /// 不用每个数据报都拿一次全局的 `sessions` 锁。会话结束时 `ended` 变了（会话丢掉之后发送端也没了），
    /// 记着的就作废；作废的记录在下一次查全局表时清掉
    pub(super) async fn relay_datagrams(self: Arc<Self>, conn: quinn::Connection, conn_id: u64) {
        let mut peers: HashMap<u64, (quinn::Connection, watch::Receiver<bool>)> = HashMap::new();
        let live = |ended: &watch::Receiver<bool>| matches!(ended.has_changed(), Ok(false));
        while let Ok(d) = conn.read_datagram().await {
            let Some(session_id) = datagram::session(&d) else {
                continue;
            };
            if let Some((to, ended)) = peers.get(&session_id)
                && live(ended)
            {
                let _ = to.send_datagram(d);
                continue;
            }
            peers.retain(|_, (_, ended)| live(ended));
            let found = self.sessions.lock().get(&session_id).and_then(|s| {
                let to = s.peer_of(conn_id)?.clone();
                Some((to, s.ended.subscribe()))
            });
            if let Some((to, ended)) = found {
                let _ = to.send_datagram(d);
                peers.insert(session_id, (to, ended));
            }
        }
    }

    /// 接受连接 `conn_id`（角色是 `role`）开过来的通道，各自转给会话的另一端
    pub(super) async fn accept_lanes(
        self: Arc<Self>,
        conn: quinn::Connection,
        conn_id: u64,
        role: Role,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                stream = conn.accept_uni() => {
                    let Ok(recv) = stream else { break };
                    let me = self.clone();
                    tasks.spawn(async move {
                        if let Err(e) = me.forward(conn_id, role, recv).await {
                            debug!(conn_id, "session lane rejected: {e:#}");
                        }
                    });
                }
                Some(_) = tasks.join_next() => {}
            }
        }
    }

    /// 会话 `session_id` 的被控端现在开着几条帧流（测试用）
    #[cfg(test)]
    pub(super) fn host_frame_streams(&self, session_id: u64) -> u16 {
        self.sessions
            .lock()
            .get(&session_id)
            .map_or(0, |s| s.frames[0])
    }

    /// 一条帧流转发完了（或者没转成）：会话 `session_id` 第 `side` 端开着的帧流少了一条
    fn frame_done(&self, session_id: u64, side: usize) {
        if let Some(s) = self.sessions.lock().get_mut(&session_id) {
            s.frames[side] = s.frames[side].saturating_sub(1);
        }
    }

    /// 一条通道（或者一条帧流）：读会话头，向会话的另一端开一条单向流、写同样的会话头，转发到结束。
    /// 控制通道结束时整个会话结束
    async fn forward(
        self: Arc<Self>,
        conn_id: u64,
        role: Role,
        mut recv: RecvStream,
    ) -> Result<()> {
        let mut hdr = [0u8; SessionHeader::LEN];
        timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut hdr))
            .await
            .context("timed out reading the session header")??;
        let Some(h) = SessionHeader::decode(&hdr) else {
            reject(recv);
            bail!("invalid session header");
        };
        let session_id = h.session_id;
        let frame = h.is_frame();
        if frame && h.lane == SessionLane::Control.number() {
            reject(recv);
            bail!("a frame stream on the control lane");
        }
        let found = {
            let mut sessions = self.sessions.lock();
            sessions.get_mut(&session_id).and_then(|s| {
                let side = match role {
                    Role::Host if s.host.0 == conn_id => 0,
                    Role::Controller if s.ctrl.0 == conn_id => 1,
                    _ => return None,
                };
                if frame {
                    // 帧流：同一条通道可以开很多条，只限制同时开着的条数
                    if s.frames[side] >= MAX_FRAME_STREAMS {
                        return Some(Err("too many frame streams are open"));
                    }
                    s.frames[side] += 1;
                } else {
                    let bit = 1 << h.lane;
                    if s.opened[side] & bit != 0 {
                        return Some(Err("the lane was opened twice"));
                    }
                    s.opened[side] |= bit;
                }
                let peer = s.peer_of(conn_id).expect("one of the two ends").clone();
                Some(Ok((peer, s.ended.subscribe(), side)))
            })
        };
        let (peer, ended, side) = match found {
            Some(Ok(found)) => found,
            Some(Err(why)) => {
                reject(recv);
                bail!("session lane {} rejected: {why}", h.lane);
            }
            None => {
                reject(recv);
                bail!("no such session on this connection: {session_id}");
            }
        };
        let open = async {
            let mut send = peer.open_uni().await?;
            // 往另一端写数据时用这一端在会话头里给的优先级（解码时已经压到 MAX_PRIORITY 以内：
            // 握手之前就转发，不压的话一端能把自己的流排到同一台被控端别的会话的控制通道前面）
            send.set_priority(i32::from(h.priority))?;
            send.write_all(&h.encode()).await?;
            anyhow::Ok(send)
        };
        let piped = match timeout(HANDSHAKE_TIMEOUT, open).await {
            Ok(Ok(send)) => {
                pipe(recv, send, ended).await;
                Ok(())
            }
            r => {
                reject(recv);
                Err(anyhow!("can't open the lane to the other end: {r:?}"))
            }
        };
        if frame {
            self.frame_done(session_id, side);
        } else if h.lane == SessionLane::Control.number() {
            self.end_session(session_id);
        }
        piped
    }
}
