//! 被控端的直连（见 [`super`] 的模块说明）。
//!
//! * [`DirectHub`]：被控端和服务器的一条连接上一个。在连中转的 endpoint 上接受控制端连过来的直连，按控制端说的会话 ID
//!   交给那个会话（没有会话在等直连时一概拒绝）；中转转来的探测数据报也按会话 ID 交给会话；
//! * [`run`]：每个会话一个任务。会话开始之后发一轮候选地址（[`HostMsg::Direct`]），等控制端回答、打洞、等它连过来；
//!   通了之后每 [`PROBE_EVERY`] 在两条路上各探测一次，按 [`Selector`] 换路；断了过一会儿再来一轮。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use airhand_proto::datagram;
use airhand_proto::session::{ControllerMsg, DirectOffer, HostMsg};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, Endpoint, SendStream};
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::select::{PROBE_EVERY, Selector, Switch};
use super::{
    Attach, HELLO_LEN, Mirrors, Punch, Route, Via, accept_lanes, candidates, probe_payload,
    probe_sent_at, prove_host, usable,
};
use crate::controller::now_us;
use crate::host::{Acks, Outbox};
#[cfg(doc)]
use crate::net::Signal;
use crate::secure::{DirectKey, LaneKeys};
use crate::tasks::OwnedTask;

/// 同时最多这么多条还没说明来意的直连（握手、等控制端的证明）
const MAX_ARRIVING: usize = 8;
/// 连过来之后这么久还没说明是哪个会话的就断开
const ARRIVE_TIMEOUT: Duration = Duration::from_secs(5);
/// 发了候选地址之后最多等控制端的回答这么久
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// 收到控制端的地址之后最多等它连过来这么久
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 打洞包每隔这么久发一轮，发这么多轮（之后控制端还在重发 QUIC 的握手包，NAT 上的映射也还在）
const PUNCH_EVERY: Duration = Duration::from_millis(100);
const PUNCHES: usize = 30;
/// 直连断了之后过这么久再来一轮
const RETRY_AFTER_LOSS: Duration = Duration::from_secs(2);
/// 打不通（例如对称 NAT）：再试几次，间隔越来越长，之后这个会话不再试
const RETRY_AFTER_FAILURE: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

/// 控制端连过来、说明了是哪个会话的直连（证明还没检查）
pub(crate) struct Arriving {
    conn: Connection,
    send: SendStream,
    proof: [u8; 32],
}

struct Slot {
    /// 中转、直连上来的探测数据报
    datagrams: mpsc::Sender<(Via, Bytes)>,
    /// 连过来的直连
    conns: mpsc::Sender<Arriving>,
    /// 正在等控制端连过来（发了候选地址、还没连上）：只有这时才接受直连
    expecting: bool,
}

/// 被控端和服务器的一条连接上的直连（见模块说明）
pub struct DirectHub {
    endpoint: Endpoint,
    punch: Punch,
    relay: Connection,
    sessions: Mutex<HashMap<u64, Slot>>,
}

/// [`DirectHub`] 的后台任务（接受直连、分发中转来的数据报），丢掉时停止
pub struct HubTasks {
    _accept: OwnedTask<()>,
    _datagrams: OwnedTask<()>,
}

/// 交给每个会话的直连（[`crate::host::Incoming::direct`]）
#[derive(Clone)]
pub struct HostLink {
    pub hub: Arc<DirectHub>,
    /// 服务器看到的本机地址（`ServerMsg::Incoming::observed`）
    pub observed: Option<SocketAddr>,
}

impl DirectHub {
    /// 在被控端和服务器的连接上开始接受直连（`endpoint`、`punch`、`relay` 是 [`Signal`] 里的）
    pub fn start(endpoint: &Endpoint, punch: &Punch, relay: &Connection) -> (Arc<Self>, HubTasks) {
        let hub = Arc::new(Self {
            endpoint: endpoint.clone(),
            punch: punch.clone(),
            relay: relay.clone(),
            sessions: Mutex::default(),
        });
        let tasks = HubTasks {
            _accept: OwnedTask(tokio::spawn(hub.clone().accept())),
            _datagrams: OwnedTask(tokio::spawn(hub.clone().relay_datagrams())),
        };
        (hub, tasks)
    }

    fn register(self: &Arc<Self>, session_id: u64) -> Registration {
        let (dtx, datagrams) = mpsc::channel(32);
        let (ctx, conns) = mpsc::channel(4);
        self.sessions.lock().unwrap().insert(
            session_id,
            Slot {
                datagrams: dtx.clone(),
                conns: ctx,
                expecting: false,
            },
        );
        Registration {
            hub: self.clone(),
            session_id,
            datagrams,
            datagram_tx: dtx,
            conns,
        }
    }

    async fn accept(self: Arc<Self>) {
        let arriving = Arc::new(Semaphore::new(MAX_ARRIVING));
        while let Some(incoming) = self.endpoint.accept().await {
            // 没有会话在等直连：谁都不接受（被控端不是一台随便能连的服务器）
            let wanted = self.sessions.lock().unwrap().values().any(|s| s.expecting);
            let permit = arriving.clone().try_acquire_owned();
            let (true, Ok(permit)) = (wanted, permit) else {
                incoming.refuse();
                continue;
            };
            let hub = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let from = incoming.remote_address();
                match tokio::time::timeout(ARRIVE_TIMEOUT, hub.arrive(incoming)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => debug!(%from, "refused a direct connection: {e:#}"),
                    Err(_) => debug!(%from, "a direct connection didn't say what it wants"),
                }
            });
        }
    }

    /// 控制端连过来：等它说明是哪个会话的，交给那个会话检查证明
    async fn arrive(&self, incoming: quinn::Incoming) -> Result<()> {
        let conn = incoming.accept()?.await?;
        let (send, mut recv) = conn.accept_bi().await?;
        let mut hello = [0u8; HELLO_LEN];
        recv.read_exact(&mut hello).await?;
        let session_id = u64::from_le_bytes(hello[..8].try_into().unwrap());
        let to = self
            .sessions
            .lock()
            .unwrap()
            .get(&session_id)
            .filter(|s| s.expecting)
            .map(|s| s.conns.clone())
            .context("no session is expecting this direct connection")?;
        to.send(Arriving {
            conn,
            send,
            proof: hello[8..].try_into().unwrap(),
        })
        .await
        .ok()
        .context("the session ended")
    }

    /// 中转转来的数据报（探测的回声）按会话 ID 交给会话。会话跟不上就丢掉：本来就可能丢
    async fn relay_datagrams(self: Arc<Self>) {
        while let Ok(d) = self.relay.read_datagram().await {
            let Some(session_id) = datagram::session(&d) else {
                continue;
            };
            let to = self
                .sessions
                .lock()
                .unwrap()
                .get(&session_id)
                .map(|s| s.datagrams.clone());
            if let Some(to) = to {
                let _ = to.try_send((Via::Relay, d));
            }
        }
    }
}

/// 一个会话在 [`DirectHub`] 上的登记，丢掉时注销
struct Registration {
    hub: Arc<DirectHub>,
    session_id: u64,
    datagrams: mpsc::Receiver<(Via, Bytes)>,
    /// 直连上来的数据报也放进 `datagrams`
    datagram_tx: mpsc::Sender<(Via, Bytes)>,
    conns: mpsc::Receiver<Arriving>,
}

impl Registration {
    /// 开始 / 不再等控制端连过来
    fn expect(&self, on: bool) {
        if let Some(s) = self.hub.sessions.lock().unwrap().get_mut(&self.session_id) {
            s.expecting = on;
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.sessions.lock().unwrap().remove(&self.session_id);
    }
}

/// 会话的直连任务要的东西（[`crate::host::HostStreams`] 给）
pub(crate) struct Session {
    pub link: HostLink,
    pub session_id: u64,
    pub key: DirectKey,
    pub probe: LaneKeys,
    /// 发 [`HostMsg::Direct`]
    pub out: Outbox,
    /// 控制端的回答（[`ControllerMsg::Direct`]，读任务转过来的）
    pub answers: mpsc::Receiver<DirectOffer>,
    /// 直连上控制端开的控制、大块通道交给读任务
    pub attach: Attach,
    pub mirrors: Mirrors,
    pub route: watch::Sender<Route>,
    /// 往直连上写画面失败了（画面的写任务通知）
    pub broken: Arc<Notify>,
    /// 画面的确认交给谁（平台的，会话开始时才有）：换路时告诉它
    pub acks: Arc<OnceLock<Arc<dyn Acks>>>,
    /// 直连断了要一个关键帧：当成控制端发来的请求交给会话
    pub keyframe: mpsc::WeakSender<ControllerMsg>,
    /// 会话开始了（发了 Hello）才开始：之前不把本机的地址告诉控制端
    pub started: Arc<Notify>,
}

/// 会话的直连任务（见模块说明）。会话结束时被丢掉
pub(crate) async fn run(mut s: Session) {
    s.started.notified().await;
    let mut reg = s.link.hub.register(s.session_id);
    let mut round = 0;
    let mut failures = 0;
    loop {
        round += 1;
        let hub = &s.link.hub;
        let ours = candidates(&hub.endpoint, &hub.relay, s.link.observed);
        if ours.is_empty() {
            return;
        }
        debug!(
            session_id = s.session_id,
            round,
            ?ours,
            "offering a direct path"
        );
        // 上一轮剩下的（控制端已经不用了）不要
        while reg.conns.try_recv().is_ok() {}
        // 先开始等再发：控制端一收到就会连过来
        reg.expect(true);
        if s.out
            .send(&HostMsg::Direct(DirectOffer { round, addrs: ours }))
            .is_err()
        {
            return;
        }
        let conn = establish(&mut s, &mut reg, round).await;
        reg.expect(false);
        match conn {
            Some(conn) => {
                failures = 0;
                info!(
                    session_id = s.session_id,
                    peer = %conn.remote_address(),
                    "direct path established"
                );
                serve(&mut s, &mut reg, conn).await;
                tokio::time::sleep(RETRY_AFTER_LOSS).await;
            }
            None => {
                let Some(wait) = RETRY_AFTER_FAILURE.get(failures) else {
                    info!(
                        session_id = s.session_id,
                        "no direct path (symmetric NAT or a firewall?), staying on the relay"
                    );
                    return;
                };
                failures += 1;
                debug!(
                    session_id = s.session_id,
                    round, "no direct path this round"
                );
                tokio::time::sleep(*wait).await;
            }
        }
    }
}

/// 等控制端回答这一轮、打洞、等它连过来并证明自己
async fn establish(s: &mut Session, reg: &mut Registration, round: u32) -> Option<Connection> {
    let answer = tokio::time::timeout(ANSWER_TIMEOUT, async {
        loop {
            let a = s.answers.recv().await?;
            if a.round == round {
                return Some(a);
            }
        }
    })
    .await
    .ok()
    .flatten()?;
    let hub = s.link.hub.clone();
    let addrs = usable(&answer.addrs, &hub.endpoint);
    debug!(session_id = s.session_id, ?addrs, "punching");
    // 往控制端的地址发打洞包（打开本端 NAT 上的映射，控制端的握手包才进得来），同时等它连过来
    let punch = async {
        for _ in 0..PUNCHES {
            for a in &addrs {
                hub.punch.send(*a);
            }
            tokio::time::sleep(PUNCH_EVERY).await;
        }
        std::future::pending::<()>().await;
    };
    let arrive = async {
        loop {
            let mut a = reg.conns.recv().await?;
            match prove_host(&a.conn, s.session_id, &s.key, &mut a.send, &a.proof).await {
                Ok(()) => return Some(a.conn),
                Err(e) => warn!(
                    session_id = s.session_id,
                    "rejected a direct connection: {e:#}"
                ),
            }
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        tokio::select! {
            () = punch => None,
            c = arrive => c,
        }
    })
    .await
    .ok()
    .flatten()
}

/// 直连通着的时候：两条路上探测，按 [`Selector`] 换路，直到直连断了
async fn serve(s: &mut Session, reg: &mut Registration, conn: Connection) {
    let mut tasks = JoinSet::new();
    tasks.spawn(accept_lanes(conn.clone(), s.attach.clone()));
    match s.mirrors.start(&conn).await {
        Ok(writers) => {
            for w in writers {
                tasks.spawn(w);
            }
        }
        Err(e) => {
            warn!(
                session_id = s.session_id,
                "can't open the direct lanes: {e:#}"
            );
            conn.close(0u32.into(), b"lanes");
            return;
        }
    }
    {
        let conn = conn.clone();
        let to = reg.datagram_tx.clone();
        tasks.spawn(async move {
            while let Ok(d) = conn.read_datagram().await {
                let _ = to.try_send((Via::Direct, d));
            }
        });
    }
    let mut sel = Selector::default();
    let mut tick = tokio::time::interval(PROBE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let switch = tokio::select! {
            _ = tick.tick() => {
                probe(s, &conn);
                sel.decide(Instant::now())
            }
            d = reg.datagrams.recv() => {
                let Some((via, d)) = d else { break };
                if let Some(rtt) = echo(s, via, &d) {
                    sel.sample(via, rtt, Instant::now());
                }
                None
            }
            () = s.broken.notified() => sel.broken(Instant::now()),
            e = conn.closed() => {
                info!(session_id = s.session_id, "direct path closed: {e}");
                break;
            }
        };
        if let Some(sw) = switch {
            info!(
                session_id = s.session_id,
                to = ?sw.to,
                lost = sw.lost,
                relay_ms = ?sel.rtt(Via::Relay),
                direct_ms = ?sel.rtt(Via::Direct),
                "video and audio switch paths"
            );
            apply(s, sw, &conn);
        }
    }
    s.mirrors.stop();
    if sel.media() == Via::Direct {
        apply(
            s,
            Switch {
                to: Via::Relay,
                lost: true,
            },
            &conn,
        );
    }
    conn.close(0u32.into(), b"closed");
}

/// 换路：告诉画面、声音的写任务，告诉平台的码率控制（新的路时延不一样；断了的话在途的帧不会确认了），
/// 断了的话要一个关键帧（接收端不再等断在直连上的帧）
fn apply(s: &Session, sw: Switch, conn: &Connection) {
    let media = (sw.to == Via::Direct).then(|| conn.clone());
    s.route.send_replace(Route { media });
    if let Some(acks) = s.acks.get() {
        acks.path_changed(sw.lost);
    }
    if sw.lost
        && let Some(tx) = s.keyframe.upgrade()
    {
        let _ = tx.try_send(ControllerMsg::RequestKeyframe);
    }
}

/// 两条路上各发一个探测数据报
fn probe(s: &mut Session, direct: &Connection) {
    for (via, conn) in [(Via::Relay, &s.link.hub.relay), (Via::Direct, direct)] {
        let payload = probe_payload(via, now_us());
        match s
            .probe
            .sealer
            .seal_datagram(s.session_id, datagram::PROBE, &payload)
        {
            Ok(d) => {
                let _ = conn.send_datagram(Bytes::from(d));
            }
            Err(e) => warn!("can't seal a probe: {e:#}"),
        }
    }
}

/// 弹回来的探测：这条路的往返时延（毫秒）
fn echo(s: &mut Session, via: Via, d: &[u8]) -> Option<f64> {
    if datagram::kind(d)? != datagram::PROBE {
        return None;
    }
    let mut body = d.get(datagram::HEADER..)?.to_vec();
    s.probe.opener.open(&mut body).ok()?;
    let sent = probe_sent_at(&body, via)?;
    Some(now_us().saturating_sub(sent) as f64 / 1000.0)
}
