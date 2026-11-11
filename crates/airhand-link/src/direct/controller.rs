//! 控制端的直连（见 [`super`] 的模块说明）：被控端发来一轮候选地址和它的证书指纹（`HostMsg::Direct`），回答本端的
//! 地址和本端证书的指纹，从连中转的 endpoint 往它的每个地址发起 QUIC 连接（先连上的胜出，TLS 只认被控端给的指纹）；
//! 通了之后接到会话的 [`Paths`] 上（控制通道、Ping / 确认也从直连发一份），直连上被控端发来的流、数据报交给会话。
//! 被控端的探测由会话的接收端从同一条路弹回去（[`crate::controller::HostReceiver`]）。
//!
//! 好几个地址可能通到同一个被控端（同一台电脑的不同地址、局域网的 IPv4 和 IPv6……），两边都会连上好几条：
//! 用哪一条由这边决定，选中之后马上在上面开控制通道、其余的关掉，被控端认的是先开了流的那一条（见 [`super::host`]）。

use std::net::SocketAddr;
use std::time::Duration;

use airhand_proto::SessionLane;
use airhand_proto::session::DirectOffer;
use anyhow::Result;
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use super::tls::{Cert, SERVER_NAME, client_config};
use super::{Paths, candidates, usable};
use crate::lanes::{Feed, accept_direct, open_lane};

/// 最多花这么久连被控端的直连（被控端那边也在等这么久）
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 控制端会话的直连任务要的东西
pub(crate) struct Session {
    pub endpoint: Endpoint,
    pub session_id: u64,
    /// 服务器看到的本机地址（`SessionGrant::observed`）
    pub observed: SocketAddr,
    /// 会话的两条路：直连通了接上去、断了拿下来
    pub paths: Paths,
    /// 直连上来的流、数据报交给这个会话
    pub feed: Feed,
    /// 被控端发来的候选地址（会话的读任务转过来的）
    pub offers: mpsc::Receiver<DirectOffer>,
    /// 回答被控端（`ControllerMsg::Direct`，排进命令队列）
    pub answer: Box<dyn Fn(DirectOffer) + Send + Sync>,
}

/// 会话的直连任务（见模块说明）。被控端不再发候选地址（会话结束）时返回
pub(crate) async fn run(s: Session) {
    let Session {
        endpoint,
        session_id,
        observed,
        paths,
        feed,
        mut offers,
        answer,
    } = s;
    let cert = match Cert::generate() {
        Ok(c) => c,
        Err(e) => {
            warn!(session_id, "direct paths are unavailable: {e:#}");
            return;
        }
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
        let ours = candidates(&endpoint, paths.relay(), observed);
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
        // 被控端用的是这边开了控制通道的那一条：开不了就用不上
        let send = match open_lane(&conn, session_id, SessionLane::Control).await {
            Ok(s) => s,
            Err(e) => {
                debug!(session_id, "can't open the direct lanes: {e:#}");
                conn.close(0u32.into(), b"lanes");
                continue;
            }
        };
        info!(session_id, peer = %conn.remote_address(), "direct path established");
        let mut tasks = JoinSet::new();
        tasks.spawn(accept_direct(conn.clone(), session_id, feed.clone(), None));
        tasks.spawn(paths.attach(conn.clone(), send));
        tokio::select! {
            e = conn.closed() => info!(session_id, "direct path closed: {e}"),
            o = offers.recv() => next = o,
        }
        paths.detach();
        drop(tasks);
        conn.close(0u32.into(), b"closed");
        if next.is_none() && offers.is_closed() {
            return;
        }
    }
}

/// 往被控端的每个地址发起直连，先连上的胜出：其余的随之取消，已经连上的随之关掉（它们上面一条流也没开过，
/// 被控端不会选它们）。TLS 握手完成时两边就确认了对方（见 [`super::tls`]）
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
