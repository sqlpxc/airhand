//! 控制端的直连（见 [`super`] 的模块说明）：被控端发来一轮候选地址（`HostMsg::Direct`），回答本端的地址，
//! 从连中转的 endpoint 往它的每个地址发起 QUIC 连接（先连上的胜出），证明自己、确认对面；
//! 通了之后控制、大块通道也从直连发一份，被控端从直连发来的通道交给接收端，探测数据报原样（重新加密之后）弹回去。

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::datagram;
use airhand_proto::session::DirectOffer;
use anyhow::Result;
use bytes::Bytes;
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info};

use super::tls::{SERVER_NAME, client_config};
use super::{Attach, Mirrors, Via, accept_lanes, candidates, prove_controller, usable};
use crate::secure::{DirectKey, LaneKeys};

/// 最多花这么久连被控端的直连（被控端那边也在等这么久）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 控制端会话的直连任务要的东西
pub(crate) struct Session {
    pub endpoint: Endpoint,
    pub relay: Connection,
    pub session_id: u64,
    /// 服务器看到的本机地址（`SessionGrant::observed`）
    pub observed: Option<SocketAddr>,
    pub key: DirectKey,
    pub probe: LaneKeys,
    /// 被控端发来的候选地址（会话的读任务转过来的）
    pub offers: mpsc::Receiver<DirectOffer>,
    /// 回答被控端（`ControllerMsg::Direct`，排进命令队列）
    pub answer: Box<dyn Fn(DirectOffer) + Send + Sync>,
    /// 直连上被控端开的控制、大块、画面通道交给接收端
    pub attach: Attach,
    pub mirrors: Mirrors,
    /// 直连上来的声音数据报交给接收端（和中转上来的一起）
    pub audio: mpsc::Sender<Bytes>,
    /// 探测数据报：中转上来的（控制端服务按会话 ID 分过来的）和直连上来的
    pub probes: mpsc::Receiver<(Via, Bytes)>,
    pub probe_tx: mpsc::Sender<(Via, Bytes)>,
}

/// 会话的直连任务（见模块说明）。会话结束时被丢掉
pub(crate) async fn run(s: Session) {
    let Session {
        endpoint,
        relay,
        session_id,
        observed,
        key,
        probe,
        mut offers,
        answer,
        attach,
        mirrors,
        audio,
        mut probes,
        probe_tx,
    } = s;
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
        let Ok(config) = client_config() else {
            return;
        };
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
            });
            let addrs = usable(&offer.addrs, &endpoint);
            let conn = tokio::select! {
                c = connect(&endpoint, &config, &addrs, session_id, &key) => c,
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
            tasks.spawn(accept_lanes(conn.clone(), attach.clone()));
            {
                let conn = conn.clone();
                let audio = audio.clone();
                let probes = probe_tx.clone();
                tasks.spawn(async move {
                    while let Ok(d) = conn.read_datagram().await {
                        match datagram::kind(&d) {
                            Some(datagram::AUDIO) => {
                                let _ = audio.try_send(d);
                            }
                            Some(datagram::PROBE) => {
                                let _ = probes.try_send((Via::Direct, d));
                            }
                            _ => {}
                        }
                    }
                });
            }
            match mirrors.start(&conn).await {
                Ok(writers) => {
                    for w in writers {
                        tasks.spawn(w);
                    }
                }
                Err(e) => debug!(session_id, "can't open the direct lanes: {e:#}"),
            }
            *current.lock().unwrap() = Some(conn.clone());
            tokio::select! {
                e = conn.closed() => info!(session_id, "direct path closed: {e}"),
                o = offers.recv() => next = o,
            }
            *current.lock().unwrap() = None;
            mirrors.stop();
            drop(tasks);
            conn.close(0u32.into(), b"closed");
            if next.is_none() && offers.is_closed() {
                return;
            }
        }
    };
    tokio::join!(echo, direct);
}

/// 往被控端的每个地址发起直连，先连上的胜出（其余的随之取消，被控端那边也就等不到它们的证明），
/// 然后证明自己、确认对面
async fn connect(
    endpoint: &Endpoint,
    config: &quinn::ClientConfig,
    addrs: &[SocketAddr],
    session_id: u64,
    key: &DirectKey,
) -> Result<Connection> {
    tokio::time::timeout(CONNECT_TIMEOUT, async {
        let mut attempts = JoinSet::new();
        for &addr in addrs {
            let connecting = endpoint.connect_with(config.clone(), addr, SERVER_NAME)?;
            attempts.spawn(connecting);
        }
        let mut last = anyhow::anyhow!("the host has no usable address");
        while let Some(done) = attempts.join_next().await {
            match done {
                Ok(Ok(conn)) => {
                    drop(attempts);
                    prove_controller(&conn, session_id, key).await?;
                    return Ok(conn);
                }
                Ok(Err(e)) => last = e.into(),
                Err(e) => last = e.into(),
            }
        }
        Err(last)
    })
    .await
    .map_err(|_| anyhow::anyhow!("timed out connecting the direct path"))?
}
