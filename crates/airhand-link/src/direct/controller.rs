//! 控制端的直连（见 [`super`] 的模块说明）：被控端发来一轮候选地址和它的证书指纹（`HostMsg::Direct`），回答本端的
//! 地址和本端证书的指纹，从连中转的 endpoint 往它的每个地址发起 QUIC 连接（先连上的胜出，TLS 只认被控端给的指纹）；
//! 通了之后控制通道也从直连发一份，直连上被控端发来的流、数据报交给会话，探测数据报原样（重新加密之后）弹回去。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::session::DirectOffer;
use airhand_proto::{SessionLane, datagram};
use anyhow::Result;
use bytes::Bytes;
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::tls::{Cert, SERVER_NAME, client_config};
use super::{Mirror, Via, candidates, usable};
use crate::lanes::{Feed, accept_direct, open_lane};
use crate::secure::LaneKeys;

/// 最多花这么久连被控端的直连（被控端那边也在等这么久）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 控制端会话的直连任务要的东西
pub(crate) struct Session {
    pub endpoint: Endpoint,
    pub relay: Connection,
    pub session_id: u64,
    /// 服务器看到的本机地址（`SessionGrant::observed`）
    pub observed: SocketAddr,
    pub probe: LaneKeys,
    /// 中转、直连上来的探测数据报（会话的 [`crate::lanes::Inbox`]）
    pub probes: mpsc::Receiver<(Via, Bytes)>,
    /// 直连上来的流、数据报交给会话
    pub feed: Feed,
    /// 被控端发来的候选地址（会话的读任务转过来的）
    pub offers: mpsc::Receiver<DirectOffer>,
    /// 回答被控端（`ControllerMsg::Direct`，排进命令队列）
    pub answer: Box<dyn Fn(DirectOffer) + Send + Sync>,
    pub mirror: Mirror,
}

/// 会话的直连任务（见模块说明）。会话结束时被丢掉
pub(crate) async fn run(s: Session) {
    let Session {
        endpoint,
        relay,
        session_id,
        observed,
        probe,
        mut probes,
        feed,
        mut offers,
        answer,
        mirror,
    } = s;
    let cert = match Cert::generate() {
        Ok(c) => c,
        Err(e) => {
            warn!(session_id, "direct paths are unavailable: {e:#}");
            return std::future::pending().await;
        }
    };
    let current: Arc<Mutex<Option<Connection>>> = Arc::default();
    // 弹回探测：中转上来的从中转弹回去，直连上来的从直连
    let echo = {
        let current = current.clone();
        let relay = relay.clone();
        let LaneKeys {
            mut sealer,
            mut opener,
        } = probe;
        async move {
            while let Some((via, d)) = probes.recv().await {
                let Some(mut body) = d.get(datagram::HEADER..).map(<[u8]>::to_vec) else {
                    continue;
                };
                if opener.open(&mut body).is_err() {
                    continue;
                }
                let Ok(back) = sealer.seal_datagram(session_id, datagram::PROBE, &body) else {
                    continue;
                };
                let conn = match via {
                    Via::Relay => Some(relay.clone()),
                    Via::Direct => current.lock().unwrap().clone(),
                };
                if let Some(conn) = conn {
                    let _ = conn.send_datagram(Bytes::from(back));
                }
            }
        }
    };
    let direct = async move {
        let mut next: Option<DirectOffer> = None;
        loop {
            let offer = match next.take() {
                Some(o) => o,
                None => match offers.recv().await {
                    Some(o) => o,
                    None => return,
                },
            };
            let ours = candidates(&endpoint, &relay, observed);
            debug!(session_id, round = offer.round, ?ours, theirs = ?offer.addrs, "the host offers a direct path");
            answer(DirectOffer {
                round: offer.round,
                addrs: ours,
                cert: cert.fingerprint,
            });
            let addrs = usable(&offer.addrs, &endpoint);
            let conn = tokio::select! {
                c = connect(&endpoint, &cert, offer.cert, &addrs) => c,
                // 被控端已经开始下一轮了
                o = offers.recv() => {
                    next = o;
                    if next.is_none() { return; }
                    continue;
                }
            };
            let conn = match conn {
                Ok(c) => c,
                Err(e) => {
                    debug!(session_id, "no direct path this round: {e:#}");
                    continue;
                }
            };
            info!(session_id, peer = %conn.remote_address(), "direct path established");
            let mut tasks = JoinSet::new();
            tasks.spawn(accept_direct(conn.clone(), session_id, feed.clone()));
            match open_lane(&conn, session_id, SessionLane::Control).await {
                Ok(send) => {
                    tasks.spawn(mirror.start(send));
                }
                Err(e) => debug!(session_id, "can't open the direct lanes: {e:#}"),
            }
            *current.lock().unwrap() = Some(conn.clone());
            tokio::select! {
                e = conn.closed() => info!(session_id, "direct path closed: {e}"),
                o = offers.recv() => next = o,
            }
            *current.lock().unwrap() = None;
            mirror.stop();
            drop(tasks);
            conn.close(0u32.into(), b"closed");
            if next.is_none() && offers.is_closed() {
                return;
            }
        }
    };
    tokio::join!(echo, direct);
}

/// 往被控端的每个地址发起直连，先连上的胜出（其余的随之取消）。TLS 握手完成时两边就确认了对方
/// （见 [`super::tls`]）
async fn connect(
    endpoint: &Endpoint,
    cert: &Cert,
    host: [u8; 32],
    addrs: &[SocketAddr],
) -> Result<Connection> {
    let config = client_config(cert, host)?;
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut attempts = JoinSet::new();
        for &addr in addrs {
            attempts.spawn(endpoint.connect_with(config.clone(), addr, SERVER_NAME)?);
        }
        let mut last = anyhow::anyhow!("the host has no usable address");
        while let Some(done) = attempts.join_next().await {
            match done {
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => last = e.into(),
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting the direct path"))?
}
