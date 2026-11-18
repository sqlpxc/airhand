//! 会话中转。会话由 [`super::registry`] 登记（[`Broker::add_session`]），从那时起：
//!
//! * 每条流的头只携带会话 ID 和通道编号。中转按通道设置优先级，成批转发已有的 `Bytes` 块（[`pipe`]），不解析负载；
//! * 控制和大块通道每端各一条常驻流；视频每帧一条流，每端同时开着的不超过 [`MAX_FRAME_STREAMS`]；
//! * 文件每次取用一条流，每端同时开着的不超过 [`MAX_FILE_STREAMS`]。当一条流超过服务器配置的
//!   `files::wire_size(file_limit)` 就两头都 reset（`files::reset::RELAY_LIMIT`）。服务器看不到内容，只能按单条流兜底：
//!   一次粘贴的总量由给的一方遵守；
//! * 一端 reset / 叫停了一条流，转给另一端的那条也用同一个错误码 reset / 叫停（文件流靠它说明原因）；
//! * 会话的数据报按开头的会话 ID 在两端之间原样转发（[`Broker::relay_datagrams`]）。
//!
//! 任何一端的控制通道（0 号）结束，整个会话结束：其它通道随之断开、不再转发数据报、归还会话槽位。
//! 两端的控制通道一直没都到的会话由 [`Broker::janitor`] 清掉。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use airhand_proto::files::{self, reset};
use airhand_proto::{
    MAX_FILE_STREAMS, MAX_FRAME_STREAMS, Role, SessionHeader, SessionLane, datagram,
};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use quinn::{ReadError, RecvStream, SendStream, VarInt, WriteError};
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
    /// 每一端同时开着几条文件流
    files: [u16; 2],
    /// 这个会话的文件流一共转发了多少字节（会话结束时记进日志）
    file_bytes: Arc<AtomicU64>,
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
            files: [0; 2],
            file_bytes: Arc::default(),
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

fn reject(recv: RecvStream) {
    reject_with(recv, 1);
}

fn reject_with(mut recv: RecvStream, code: u32) {
    let _ = recv.stop(VarInt::from_u32(code));
}

/// 一条文件流最多转发多少字节、转发了的记在哪里（见模块说明）
pub(super) struct Budget {
    /// 还能转发多少（`None`：不限）
    left: Option<u64>,
    /// 这个会话的、整个服务器的
    counted: [Arc<AtomicU64>; 2],
}

/// [`pipe`] 为什么停下来：另一端也用同一个错误码断开
enum Stop {
    /// 发的一端 reset 了（带着它的错误码）
    Reset(VarInt),
    /// 收的一端叫停了
    Stopped(VarInt),
    /// 超过了文件流的上限
    OverLimit,
    /// 连接断了、会话结束了……
    Other,
}

/// 把 `rx` 转进 `tx`，直到 `rx` 结束（之后结束 `tx`）、出错或者会话结束（之后两头都断开）。
/// 一头 reset / 叫停了，另一头用同一个错误码；有 `budget` 时超过了就两头都用 [`reset::RELAY_LIMIT`]
async fn pipe(
    mut rx: RecvStream,
    mut tx: SendStream,
    mut ended: watch::Receiver<bool>,
    mut budget: Option<Budget>,
) {
    // 文件接收方可能在发送方等额度时取消，不能等下一次写入才叫停源流。
    let stopped = budget.is_some().then(|| tx.stopped());
    let copy = async {
        let mut chunks = [const { Bytes::new() }; 16];
        loop {
            // 只批量处理已经可读的块，不等凑满。两端 QUIC 每批各拿一次连接锁。
            let n = match rx.read_chunks(&mut chunks).await {
                Ok(Some(n)) => n,
                Ok(None) => return Ok(()),
                Err(ReadError::Reset(code)) => return Err(Stop::Reset(code)),
                Err(_) => return Err(Stop::Other),
            };
            let batch = &mut chunks[..n];
            if let Some(b) = &mut budget {
                let n = batch.iter().map(|chunk| chunk.len() as u64).sum();
                if let Some(left) = &mut b.left {
                    *left = left.checked_sub(n).ok_or(Stop::OverLimit)?;
                }
                for c in &b.counted {
                    c.fetch_add(n, Ordering::Relaxed);
                }
            }
            match tx.write_all_chunks(batch).await {
                Ok(()) => {}
                Err(WriteError::Stopped(code)) => return Err(Stop::Stopped(code)),
                Err(_) => return Err(Stop::Other),
            }
        }
    };
    let r = tokio::select! {
        r = copy => r,
        _ = ended.wait_for(|e| *e) => Err(Stop::Other),
        r = async {
            match stopped {
                Some(stopped) => stopped.await,
                None => std::future::pending().await,
            }
        } => Err(match r {
            Ok(Some(code)) => Stop::Stopped(code),
            _ => Stop::Other,
        }),
    };
    let zero = VarInt::from_u32(0);
    let limit = VarInt::from_u32(reset::RELAY_LIMIT);
    let (reset_with, stop_with) = match r {
        Ok(()) => {
            let _ = tx.finish();
            return;
        }
        Err(Stop::Reset(code)) => (code, zero),
        Err(Stop::Stopped(code)) => (zero, code),
        Err(Stop::OverLimit) => (limit, limit),
        Err(Stop::Other) => (zero, zero),
    };
    let _ = tx.reset(reset_with);
    let _ = rx.stop(stop_with);
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
            let file_bytes = s.file_bytes.load(Ordering::Relaxed);
            if file_bytes > 0 {
                info!(
                    session_id = id,
                    file_bytes, "{}: session ended (relayed files)", s.label
                );
            } else {
                info!(session_id = id, "{}: session ended", s.label);
            }
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

    /// 一条帧流转发完了（或者没转成）：会话 `session_id` 第 `side` 端开着的帧流（`file`：文件流）少了一条
    fn frame_done(&self, session_id: u64, side: usize, file: bool) {
        if let Some(s) = self.sessions.lock().get_mut(&session_id) {
            let n = if file {
                &mut s.files[side]
            } else {
                &mut s.frames[side]
            };
            *n = n.saturating_sub(1);
        }
    }

    /// 会话 `session_id` 的文件流一共转发了多少字节（测试用）
    #[cfg(test)]
    pub(super) fn session_file_bytes(&self, session_id: u64) -> u64 {
        self.sessions
            .lock()
            .get(&session_id)
            .map_or(0, |s| s.file_bytes.load(Ordering::Relaxed))
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
        let lane = h.lane;
        let file = lane == SessionLane::File;
        // 文件流的上限（会话头之后的字节，见模块说明）：上限是 0 的服务器不转文件
        let file_budget = file.then(|| self.cfg.file_limit.map(files::wire_size));
        if file && self.cfg.file_limit == Some(0) {
            reject_with(recv, reset::RELAY_LIMIT);
            bail!("this server doesn't relay files");
        }
        let found = {
            let mut sessions = self.sessions.lock();
            sessions.get_mut(&session_id).and_then(|s| {
                let side = match role {
                    Role::Host if s.host.0 == conn_id => 0,
                    Role::Controller if s.ctrl.0 == conn_id => 1,
                    _ => return None,
                };
                if file {
                    // 文件流：另外数，上限小得多
                    if s.files[side] >= MAX_FILE_STREAMS {
                        return Some(Err("too many file streams are open"));
                    }
                    s.files[side] += 1;
                } else if matches!(lane, SessionLane::Video(_)) {
                    if s.frames[side] >= MAX_FRAME_STREAMS {
                        return Some(Err("too many frame streams are open"));
                    }
                    s.frames[side] += 1;
                } else {
                    let bit = 1 << lane.number();
                    if s.opened[side] & bit != 0 {
                        return Some(Err("the lane was opened twice"));
                    }
                    s.opened[side] |= bit;
                }
                let peer = s.peer_of(conn_id).expect("one of the two ends").clone();
                let budget = file_budget.map(|left| Budget {
                    left,
                    counted: [s.file_bytes.clone(), self.file_bytes.clone()],
                });
                Some(Ok((peer, s.ended.subscribe(), side, budget)))
            })
        };
        let (peer, ended, side, budget) = match found {
            Some(Ok(found)) => found,
            Some(Err(why)) => {
                reject(recv);
                bail!("session lane {lane:?} rejected: {why}");
            }
            None => {
                reject(recv);
                bail!("no such session on this connection: {session_id}");
            }
        };
        let open = async {
            let mut send = peer.open_uni().await?;
            send.set_priority(lane.priority())?;
            send.write_all(&h.encode()).await?;
            anyhow::Ok(send)
        };
        let piped = match timeout(HANDSHAKE_TIMEOUT, open).await {
            Ok(Ok(send)) => {
                pipe(recv, send, ended, budget).await;
                Ok(())
            }
            r => {
                reject(recv);
                Err(anyhow!("can't open the lane to the other end: {r:?}"))
            }
        };
        match lane {
            SessionLane::Video(_) | SessionLane::File => self.frame_done(session_id, side, file),
            SessionLane::Control => self.end_session(session_id),
            SessionLane::Bulk => {}
        }
        piped
    }
}
