//! 被控端的直连（见 [`super`] 的模块说明）。
//!
//! * [`DirectHub`]：被控端和服务器的一条连接上一个。在连中转的 endpoint 上接受控制端连过来的直连，按控制端证书的指纹
//!   交给那个会话（有会话在发候选地址时才接受：被控端不是一台随便能连的服务器）；
//! * [`run`]：每个会话一个任务。会话开始之后发一轮候选地址（[`HostMsg::Direct`]，带着本端证书的指纹），等控制端回答
//!   （带着它的指纹）、打洞、等它连过来（它可能从好几个地址都连上了：用它开了流的那一条，见 [`chosen`]）；
//!   通了之后每 [`PROBE_EVERY`] 在两条路上各探测一次，按 [`Selector`] 换路；断了过一会儿再来一轮。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use airhand_proto::session::{DirectOffer, HostMsg};
use airhand_proto::{SessionLane, datagram};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream};
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::select::{PROBE_EVERY, Selector, Switch};
use super::tls::{self, Cert};
use super::{Mirror, Punch, Route, Via, candidates, probe_payload, probe_sent_at, usable};
use crate::clock::now_us;
use crate::host::{Acks, Outbox, Request};
use crate::lanes::{Feed, accept_direct, open_lane};
use crate::secure::LaneKeys;
use crate::tasks::OwnedTask;

/// 同时最多这么多条还没交给会话的直连（握手中、等控制端的回答到）
const MAX_ARRIVING: usize = 8;
/// 发了候选地址之后最多等控制端的回答这么久
const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// 收到控制端的回答之后最多等它连过来这么久；连过来的连接最多等它的回答这么久（回答和连接谁先到都可能）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// 打洞包每隔这么久发一轮，发这么多轮（之后控制端还在重发 QUIC 的握手包，NAT 上的映射也还在）
const PUNCH_EVERY: Duration = Duration::from_millis(100);
const PUNCHES: usize = 30;
/// 直连断了之后过这么久再来一轮
const RETRY_AFTER_LOSS: Duration = Duration::from_secs(2);
/// 打不通（例如对称 NAT）：再试几次，间隔越来越长，之后这个会话不再试
const RETRY_AFTER_FAILURE: [Duration; 2] = [Duration::from_secs(10), Duration::from_secs(30)];

#[derive(Default)]
struct HubState {
    /// 正在发候选地址的会话有几个：只有这时才接受直连
    offering: usize,
    /// 控制端的证书指纹 → 等它连过来的会话
    expecting: HashMap<[u8; 32], mpsc::Sender<Connection>>,
}

/// 被控端和服务器的一条连接上的直连（见模块说明）
pub struct DirectHub {
    endpoint: Endpoint,
    punch: Punch,
    relay: Connection,
    cert: Cert,
    state: Mutex<HubState>,
    /// 有会话开始等控制端了
    expected: Notify,
}

/// 交给每个会话的直连（[`crate::host::Incoming::direct`]）
#[derive(Clone)]
pub struct HostLink {
    pub hub: Arc<DirectHub>,
    /// 服务器看到的本机地址（`ServerMsg::Incoming::observed`）
    pub observed: SocketAddr,
}

/// 一个会话在发候选地址（[`DirectHub::offering`]），丢掉时不再算
struct Offering(Arc<DirectHub>);

impl Drop for Offering {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().offering -= 1;
    }
}

/// 一个会话在等证书指纹是这个的控制端连过来（[`DirectHub::expect`]），丢掉时不再等
struct Expecting(Arc<DirectHub>, [u8; 32]);

impl Drop for Expecting {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().expecting.remove(&self.1);
    }
}

impl DirectHub {
    /// 在被控端和服务器的连接上开始接受直连（`endpoint`、`punch`、`relay` 是 [`crate::net::Signal`] 里的）。
    /// 返回的任务丢掉时停止
    pub fn start(
        endpoint: &Endpoint,
        punch: &Punch,
        relay: &Connection,
    ) -> Result<(Arc<Self>, OwnedTask<()>)> {
        let cert = Cert::generate()?;
        endpoint.set_server_config(Some(tls::server_config(&cert)?));
        let hub = Arc::new(Self {
            endpoint: endpoint.clone(),
            punch: punch.clone(),
            relay: relay.clone(),
            cert,
            state: Mutex::default(),
            expected: Notify::new(),
        });
        let task = OwnedTask(tokio::spawn(hub.clone().accept()));
        Ok((hub, task))
    }

    fn offering(self: &Arc<Self>) -> Offering {
        self.state.lock().unwrap().offering += 1;
        Offering(self.clone())
    }

    fn expect(self: &Arc<Self>, controller: [u8; 32]) -> (mpsc::Receiver<Connection>, Expecting) {
        let (tx, rx) = mpsc::channel(1);
        self.state.lock().unwrap().expecting.insert(controller, tx);
        self.expected.notify_waiters();
        (rx, Expecting(self.clone(), controller))
    }

    async fn accept(self: Arc<Self>) {
        let arriving = Arc::new(Semaphore::new(MAX_ARRIVING));
        while let Some(incoming) = self.endpoint.accept().await {
            let wanted = self.state.lock().unwrap().offering > 0;
            let permit = arriving.clone().try_acquire_owned();
            let (true, Ok(permit)) = (wanted, permit) else {
                incoming.refuse();
                continue;
            };
            let hub = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let from = incoming.remote_address();
                match tokio::time::timeout(CONNECT_TIMEOUT, hub.arrive(incoming)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => debug!(%from, "refused a direct connection: {e:#}"),
                    Err(_) => debug!(%from, "a direct connection isn't expected by any session"),
                }
            });
        }
    }

    /// 控制端连过来：握手，按它证书的指纹交给在等它的会话（会话可能还没收到它的回答：等一会儿）
    async fn arrive(&self, incoming: quinn::Incoming) -> Result<()> {
        let conn = incoming.accept()?.await?;
        let fp = tls::peer_fingerprint(&conn).context("no client certificate")?;
        loop {
            let expected = self.expected.notified();
            let to = self.state.lock().unwrap().expecting.get(&fp).cloned();
            if let Some(to) = to {
                return to.send(conn).await.ok().context("the session ended");
            }
            expected.await;
        }
    }
}

/// 会话的直连任务要的东西（[`crate::host::HostStreams`] 给）
pub(crate) struct Session {
    pub link: HostLink,
    pub session_id: u64,
    pub probe: LaneKeys,
    /// 中转、直连上来的探测数据报（会话的 [`crate::lanes::Inbox`]）
    pub probes: mpsc::Receiver<(Via, Bytes)>,
    /// 直连上来的流、数据报交给会话
    pub feed: Feed,
    /// 发 [`HostMsg::Direct`]
    pub out: Outbox,
    /// 控制端的回答（[`airhand_proto::session::ControllerMsg::Direct`]，读任务转过来的）
    pub answers: mpsc::Receiver<DirectOffer>,
    pub mirror: Mirror,
    pub route: watch::Sender<Route>,
    /// 往哪条直连上写画面失败了（画面的写任务报告）：只认现在这条的，别的（之前那条的、换路之前的）不算
    pub broken: watch::Receiver<Option<Connection>>,
    /// 画面的确认交给谁（平台的，会话开始时才有）：换路时告诉它
    pub acks: Arc<OnceLock<Arc<dyn Acks>>>,
    /// 直连断了要一个关键帧：交给会话
    pub keyframe: mpsc::WeakSender<Request>,
    /// 会话开始了（发了 Hello）才开始：之前不把本机的地址告诉控制端
    pub started: Arc<Notify>,
}

/// 会话的直连任务（见模块说明）。会话结束时被丢掉
pub(crate) async fn run(mut s: Session) {
    s.started.notified().await;
    let mut round = 0;
    let mut failures = 0;
    loop {
        round += 1;
        let hub = s.link.hub.clone();
        let ours = candidates(&hub.endpoint, &hub.relay, s.link.observed);
        debug!(
            session_id = s.session_id,
            round,
            ?ours,
            "offering a direct path"
        );
        // 先开始接受再发：控制端一收到就会连过来
        let offering = hub.offering();
        let offer = DirectOffer {
            round,
            addrs: ours,
            cert: hub.cert.fingerprint,
        };
        if s.out.send(&HostMsg::Direct(offer)).is_err() {
            return;
        }
        let conn = establish(&mut s, round).await;
        drop(offering);
        let used = match conn {
            Some((conn, first)) => {
                info!(
                    session_id = s.session_id,
                    peer = %conn.remote_address(),
                    "direct path established"
                );
                serve(&mut s, conn, first).await
            }
            None => false,
        };
        if used {
            failures = 0;
            tokio::time::sleep(RETRY_AFTER_LOSS).await;
            continue;
        }
        // 没连上，或者连上了却用不了：都算没打通，间隔越来越长（不能每轮都当成"通过又断了"，那样会一直重来）
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

/// 等控制端回答这一轮、打洞、等它连过来。返回它用的那条连接和它在上面开的第一条流（见 [`chosen`]）
async fn establish(s: &mut Session, round: u32) -> Option<(Connection, RecvStream)> {
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
    let (mut arrived, _expecting) = hub.expect(answer.cert);
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
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        tokio::select! {
            () = punch => None,
            c = chosen(&mut arrived) => c,
        }
    })
    .await
    .ok()
    .flatten()
}

/// 控制端连过来的直连里它用的那一条，和它在上面开的第一条流。
///
/// 它往本端的每个地址都发起连接，好几个地址通到这里时（同一台电脑的不同地址、局域网的 IPv4 和 IPv6……）会连上好几条。
/// 它用它那边先连上的那条，马上在上面开控制通道，其余的关掉；这边要是也拿先连上的那条，两边的"先"不一定是同一条：
/// 这边用着它已经关掉的那条，它用着这边不要的那条，直连通了却用不上。所以这边等的是第一条开了流的连接。
/// 其余的连接随之丢掉（关掉）。`arrived` 没有连接要来了时返回 `None`
async fn chosen(arrived: &mut mpsc::Receiver<Connection>) -> Option<(Connection, RecvStream)> {
    let mut opened = JoinSet::new();
    loop {
        tokio::select! {
            Some(conn) = arrived.recv() => {
                opened.spawn(async move {
                    let first = conn.accept_uni().await;
                    (conn, first)
                });
            }
            // 只匹配 `Some(_)`：没被选中、被关掉的连接不能让这个分支在这一轮 select 里停用（见 lanes 的 Router::run）
            Some(r) = opened.join_next() => {
                if let Ok((conn, Ok(first))) = r {
                    return Some((conn, first));
                }
            }
            else => return None,
        }
    }
}

/// 直连通着的时候：两条路上探测，按 [`Selector`] 换路，直到直连断了。`first`：控制端在它上面开的第一条流。
/// 本端的通道开不了（直连没用上）时返回 false
async fn serve(s: &mut Session, conn: Connection, first: RecvStream) -> bool {
    let mut tasks = JoinSet::new();
    tasks.spawn(accept_direct(
        conn.clone(),
        s.session_id,
        s.feed.clone(),
        Some(first),
    ));
    match open_lane(&conn, s.session_id, SessionLane::Control).await {
        Ok(send) => {
            tasks.spawn(s.mirror.start(send, conn.clone()));
        }
        Err(e) => {
            warn!(
                session_id = s.session_id,
                "can't open the direct lanes: {e:#}"
            );
            conn.close(0u32.into(), b"lanes");
            return false;
        }
    }
    let mut sel = Selector::default();
    // 之前报告的都是别的直连的
    s.broken.mark_unchanged();
    let mut tick = tokio::time::interval(PROBE_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let switch = tokio::select! {
            _ = tick.tick() => {
                probe(s, &conn);
                sel.decide(Instant::now())
            }
            d = s.probes.recv() => {
                let Some((via, d)) = d else { break };
                if let Some(rtt) = echo(s, via, &d) {
                    sel.sample(via, rtt, Instant::now());
                }
                None
            }
            r = s.broken.changed() => {
                // 写任务不在了：会话在结束
                if r.is_err() {
                    break;
                }
                let this = s.broken.borrow_and_update().as_ref().is_some_and(|c| c.stable_id() == conn.stable_id());
                this.then(|| sel.broken(Instant::now()))
            }
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
    s.mirror.stop();
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
    true
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
        let _ = tx.try_send(Request::Keyframe(None));
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
    let mut body = d.get(datagram::HEADER..)?.to_vec();
    s.probe.opener.open(&mut body).ok()?;
    let sent = probe_sent_at(&body, via)?;
    Some(now_us().saturating_sub(sent) as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::quic_pair;

    /// 控制端从两个地址都连上了：先到这边的那条它没选（还没来得及关），它在后到的那条上开了控制通道。
    /// 这边要用后那条，而不是先连上的那条（那样两边用的不是同一条，直连通了却用不上）
    #[tokio::test]
    async fn the_host_uses_the_connection_the_controller_chose() {
        let (_ignored, first) = quic_pair().await;
        let (picked, second) = quic_pair().await;
        let want = second.stable_id();
        let (tx, mut arrived) = mpsc::channel(1);
        tokio::spawn(async move {
            tx.send(first).await.unwrap();
            tx.send(second).await.unwrap();
        });
        let controller = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut lane = picked.open_uni().await.unwrap();
            lane.write_all(b"lane").await.unwrap();
            lane
        };
        let (got, _lane) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), chosen(&mut arrived)),
            controller
        );
        let (conn, mut stream) = got.unwrap().unwrap();
        assert_eq!(conn.stable_id(), want);
        let mut b = [0u8; 4];
        stream.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"lane");
    }

    /// 一条连接也没来、也不会再来了：不等
    #[tokio::test]
    async fn no_connection_no_choice() {
        let (tx, mut arrived) = mpsc::channel::<Connection>(1);
        drop(tx);
        assert!(chosen(&mut arrived).await.is_none());
    }
}
