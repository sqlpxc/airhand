//! 会话中转：两端的会话流按 `(会话 ID, 通道)` 配对后，以 `Bytes` 块为单位零拷贝透传；会话的数据报按开头的
//! 会话 ID 在两端之间原样转发（[`Route`]）。
//!
//! 会话由 [`super::registry`] 登记（[`Broker::add_session`]），从那时起等两端的流。控制通道（0 号）两端都到了，
//! 会话就开始（[`Broker::run_session`]）；控制通道结束了，整个会话结束。一直没开始的由 [`Broker::janitor`] 清掉。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use airhand_proto::{Role, SessionHeader, datagram};
use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream, VarInt};
use rand::Rng;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{debug, info};

use super::limits::SlotGuard;
use super::{Broker, HANDSHAKE_TIMEOUT};

/// 会话配对等待时长。
const PENDING_TTL: Duration = Duration::from_secs(20);

/// 一个会话的两端（转发数据报用，见 [`Broker::relay_datagrams`]）
pub(super) struct Route {
    host: (u64, quinn::Connection),
    ctrl: (u64, quinn::Connection),
}

impl Route {
    /// 连接 `conn_id` 发来的数据报转给谁；不是这个会话的两端时为 `None`
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

type Stream = (SendStream, RecvStream);

/// 两端都到了的一条通道：`(通道编号, 被控端的流, 控制端的流)`
type LanePair = (u8, Stream, Stream);

/// 一个会话：从服务器批准开始，到控制通道结束为止
pub(super) struct Pending {
    host_conn: u64,
    ctrl_conn: u64,
    /// 两端的连接：会话建立后按它们转发数据报
    host_link: quinn::Connection,
    ctrl_link: quinn::Connection,
    created: Instant,
    /// 只有一端到了的通道，在等另一端
    half: HashMap<u8, (Role, Stream)>,
    /// 已经配好的通道（第 i 位是 i 号通道）：同一条通道不能再开
    paired: u8,
    /// 两端都到了、在等控制通道的通道（会话还没开始）
    ready: Vec<LanePair>,
    /// 会话开始了（控制通道两端都到了）：之后配好的通道交给它（[`Broker::run_session`]）
    live: Option<mpsc::UnboundedSender<LanePair>>,
    /// 会话开始时交给 [`Broker::run_session`]
    slot: Option<SlotGuard>,
    label: String,
}

impl Pending {
    /// `host` / `ctrl`：两端的连接（编号和连接）；`slot`：被控端的一个会话槽位，会话结束时归还
    pub(super) fn new(
        host: (u64, quinn::Connection),
        ctrl: (u64, quinn::Connection),
        slot: SlotGuard,
        label: String,
    ) -> Self {
        Self {
            host_conn: host.0,
            ctrl_conn: ctrl.0,
            host_link: host.1,
            ctrl_link: ctrl.1,
            created: Instant::now(),
            half: HashMap::new(),
            paired: 0,
            ready: Vec::new(),
            live: None,
            slot: Some(slot),
            label,
        }
    }
}

/// 会话结束时不再转发它的数据报
struct RouteGuard(Arc<Broker>, u64);

impl Drop for RouteGuard {
    fn drop(&mut self) {
        self.0.routes.lock().remove(&self.1);
    }
}

/// 双向透传，任一方向结束即拆除整个会话。返回两个方向各自转发的字节数。
async fn relay(a: (SendStream, RecvStream), b: (SendStream, RecvStream)) -> (u64, u64) {
    let (mut a_tx, mut a_rx) = a;
    let (mut b_tx, mut b_rx) = b;
    let ab = Arc::new(AtomicU64::new(0));
    let ba = Arc::new(AtomicU64::new(0));
    tokio::select! {
        _ = pipe(&mut a_rx, &mut b_tx, &ab) => {},
        _ = pipe(&mut b_rx, &mut a_tx, &ba) => {},
    }
    for tx in [&mut a_tx, &mut b_tx] {
        let _ = tx.finish();
    }
    // 给对端一点时间读完已发送数据，然后彻底关闭
    tokio::time::sleep(Duration::from_millis(200)).await;
    for rx in [&mut a_rx, &mut b_rx] {
        let _ = rx.stop(VarInt::from_u32(0));
    }
    (ab.load(Ordering::Relaxed), ba.load(Ordering::Relaxed))
}

async fn pipe(rx: &mut RecvStream, tx: &mut SendStream, counter: &AtomicU64) -> Result<()> {
    while let Some(chunk) = rx.read_chunk(256 * 1024, true).await? {
        counter.fetch_add(chunk.bytes.len() as u64, Ordering::Relaxed);
        tx.write_chunk(chunk.bytes).await?;
    }
    Ok(())
}

impl Broker {
    /// 登记一个新会话，开始等两端的流。返回随机挑的会话 ID（挑和登记在同一把锁里，不会撞上别的会话）
    pub(super) fn add_session(&self, p: Pending) -> u64 {
        let mut pending = self.pending.lock();
        let mut rng = rand::rng();
        let id = loop {
            let id: u64 = rng.random();
            if id != 0 && !pending.contains_key(&id) {
                break id;
            }
        };
        pending.insert(id, p);
        id
    }

    /// 撤销还没开始的会话（被控端正在断开，告诉不了它）
    pub(super) fn remove_session(&self, id: u64) {
        let p = self.pending.lock().remove(&id);
        drop(p);
    }

    /// 控制通道一直没配上的会话（开始了的会话由 [`Broker::run_session`] 自己清理）：关掉已经到了的流
    pub(super) fn expire_pending(&self, now: Instant) {
        let expired: Vec<Pending> = {
            let mut p = self.pending.lock();
            let ids: Vec<u64> = p
                .iter()
                .filter(|(_, v)| v.live.is_none() && now - v.created > PENDING_TTL)
                .map(|(k, _)| *k)
                .collect();
            ids.into_iter().filter_map(|id| p.remove(&id)).collect()
        };
        for p in expired {
            debug!("session pairing timed out: {}", p.label);
            let halves = p.half.into_values().map(|(_, s)| s);
            let pairs = p.ready.into_iter().flat_map(|(_, a, b)| [a, b]);
            for (mut s, mut r) in halves.chain(pairs) {
                let _ = s.reset(VarInt::from_u32(1));
                let _ = r.stop(VarInt::from_u32(1));
            }
        }
    }

    /// 转发连接 `conn_id` 发来的数据报：开头的会话 ID 是这条连接所在的会话，就原样转给会话的另一端，否则丢掉。
    /// 数据报本来就可能丢：另一端的发送缓冲满了、太大了，都直接丢掉
    pub(super) async fn relay_datagrams(self: Arc<Self>, conn: quinn::Connection, conn_id: u64) {
        while let Ok(d) = conn.read_datagram().await {
            let Some(session_id) = datagram::session(&d) else {
                continue;
            };
            let to = self
                .routes
                .lock()
                .get(&session_id)
                .and_then(|r| r.peer_of(conn_id))
                .cloned();
            if let Some(to) = to {
                let _ = to.send_datagram(d);
            }
        }
    }

    pub(super) async fn accept_sessions(
        self: Arc<Self>,
        conn: quinn::Connection,
        conn_id: u64,
        role: Role,
    ) {
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                stream = conn.accept_bi() => {
                    let Ok((send, recv)) = stream else { break; };
                    let me = self.clone();
                    tasks.spawn(async move {
                        if let Err(e) = me.attach_stream(conn_id, role, send, recv).await {
                            debug!(conn_id, "session stream rejected: {e:#}");
                        }
                    });
                }
                Some(_) = tasks.join_next() => {}
            }
        }
    }

    async fn attach_stream(
        self: Arc<Self>,
        conn_id: u64,
        role: Role,
        send: SendStream,
        mut recv: RecvStream,
    ) -> Result<()> {
        let mut hdr = [0u8; SessionHeader::LEN];
        timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut hdr))
            .await
            .context("timed out reading the session header")??;
        let reject = |mut send: SendStream, mut recv: RecvStream| {
            let _ = send.reset(VarInt::from_u32(1));
            let _ = recv.stop(VarInt::from_u32(1));
        };
        let Some(h) = SessionHeader::decode(&hdr).filter(|h| h.role == role) else {
            reject(send, recv);
            bail!("invalid session header");
        };

        let session_id = h.session_id;
        let mut pending = self.pending.lock();
        let Some(p) = pending.get_mut(&session_id) else {
            drop(pending);
            reject(send, recv);
            bail!("no such session: {session_id}");
        };
        let expected = match role {
            Role::Host => p.host_conn,
            Role::Controller => p.ctrl_conn,
        };
        if expected != conn_id {
            drop(pending);
            reject(send, recv);
            bail!("the session doesn't belong to this connection");
        }
        // 这一端往这条通道写数据时用它自己在会话头里给的优先级
        send.set_priority(i32::from(h.priority))?;
        let mine = (send, recv);
        if p.paired & (1 << h.lane) != 0 {
            drop(pending);
            let (send, recv) = mine;
            reject(send, recv);
            bail!("session lane {} is already paired", h.lane);
        }
        let pair = match p.half.remove(&h.lane) {
            None => {
                p.half.insert(h.lane, (role, mine));
                return Ok(());
            }
            Some((other, theirs)) if other != role => match role {
                Role::Host => (h.lane, mine, theirs),
                Role::Controller => (h.lane, theirs, mine),
            },
            // 同一端把同一条通道开了两次
            Some(first) => {
                p.half.insert(h.lane, first);
                drop(pending);
                let (send, recv) = mine;
                reject(send, recv);
                bail!("duplicate session lane {}", h.lane);
            }
        };
        p.paired |= 1 << h.lane;
        if let Some(live) = &p.live {
            // 会话已经开始了：交给它转发（它已经结束的话，流随之关闭）
            let _ = live.send(pair);
            return Ok(());
        }
        if pair.0 != airhand_proto::SessionLane::Control as u8 {
            p.ready.push(pair);
            return Ok(());
        }
        // 控制通道两端都到了：会话开始
        let (tx, rx) = mpsc::unbounded_channel();
        p.live = Some(tx);
        let mut lanes = std::mem::take(&mut p.ready);
        lanes.push(pair);
        let slot = p.slot.take();
        let label = p.label.clone();
        let route = Route {
            host: (p.host_conn, p.host_link.clone()),
            ctrl: (p.ctrl_conn, p.ctrl_link.clone()),
        };
        drop(pending);
        info!(session_id, "{label}: session established");
        tokio::spawn(self.run_session(session_id, label, route, lanes, rx, slot));
        Ok(())
    }

    /// 一个开始了的会话：转发它的每条通道（`lanes` 是已经配好的，之后配好的从 `more` 来）和数据报，
    /// 直到控制通道结束。结束时停掉其它通道、不再转发数据报、归还会话槽位
    async fn run_session(
        self: Arc<Self>,
        session_id: u64,
        label: String,
        route: Route,
        lanes: Vec<LanePair>,
        mut more: mpsc::UnboundedReceiver<LanePair>,
        slot: Option<SlotGuard>,
    ) {
        self.routes.lock().insert(session_id, route);
        let route = RouteGuard(self.clone(), session_id);
        let mut relays = tokio::task::JoinSet::new();
        let mut control = None;
        /// 开始转发一条通道，记下控制通道是哪个任务
        fn spawn(
            relays: &mut tokio::task::JoinSet<(u64, u64)>,
            control: &mut Option<tokio::task::Id>,
            (lane, host, ctrl): LanePair,
        ) {
            let id = relays.spawn(relay(host, ctrl)).id();
            if lane == airhand_proto::SessionLane::Control as u8 {
                *control = Some(id);
            }
        }
        for pair in lanes {
            spawn(&mut relays, &mut control, pair);
        }
        loop {
            tokio::select! {
                pair = more.recv() => match pair {
                    Some(pair) => spawn(&mut relays, &mut control, pair),
                    None => break,
                },
                done = relays.join_next_with_id() => match done {
                    // 控制通道结束了：整个会话结束
                    Some(Ok((id, _))) if Some(id) == control => break,
                    Some(Err(e)) if Some(e.id()) == control => break,
                    Some(_) => {}
                    None => break,
                },
            }
        }
        // 不再接受这个会话的通道；还在等另一端的通道随之关闭
        let p = self.pending.lock().remove(&session_id);
        if let Some(p) = p {
            for (_, (mut s, mut r)) in p.half.into_values() {
                let _ = s.reset(VarInt::from_u32(0));
                let _ = r.stop(VarInt::from_u32(0));
            }
        }
        drop(relays);
        drop(route);
        info!(session_id, "{label}: session ended");
        drop(slot);
    }
}
