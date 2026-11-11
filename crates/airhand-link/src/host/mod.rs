//! 被控端：与服务器的连接（注册、取回控制码、断开所有控制端、更换控制码），以及每个会话。
//!
//! 桌面版（`client/src-tauri/src/host/`）和 Android（`android/native/src/host/`）共用这里，协议层的事只有这一份；
//! 采集、编码、注入输入、请本机用户同意这些各平台不同的事情由它们实现 [`Platform`]：
//!
//! * [`run`]：一直连着服务器（掉线自动重连，取回原来的控制码），每来一个控制端就调用 `session` 开始一个会话；
//!   发生了什么通过 [`HostEvents`] 告诉外面（托盘、Android 的界面）；
//! * [`serve`]（[`session`]）：一个会话从端到端握手到结束：同意流程（同时在等本机用户回答的请求有上限，
//!   [`ApprovalSlots`]）、失联检测、踢人时发 Bye、画质设置的校验，控制端的请求换成 [`Request`] 交给 [`Platform`]；
//! * [`HostStreams`]：会话的链路。先回答端到端握手（[`crate::secure::respond`]，控制端的第一条消息在
//!   [`Incoming::init`] 里）：让不让这个控制端进来由平台给的 [`Gate`] 决定（配对过的、输对配对码的、本机用户同意的……），
//!   发出决定就开始，控制端确认了才显示它、才请本机用户同意（重放的握手消息走不到这一步）。
//!   过时的输入在读任务里就换成了"释放所有按键"（见 [`InputClock`]）；
//!   Ping（控制数据报）在读任务里回 Pong，画面确认在读任务里交给平台的 [`Acks`]，不等会话的主循环；
//! * [`Outbox`]（[`queue`]）：发给控制端的消息。按消息挑通道（大的走大块通道），只关心最新值的状态
//!   （显示器列表、光标可见性）只留最新的一个、排在前面；
//! * [`CursorCache`]：控制端缓存了哪些光标形状；
//! * 每条服务器连接一个 [`Router`]（控制端的通道、数据报按会话 ID 交给会话）和一个 [`DirectHub`]
//!   （接受控制端连过来的直连），每个会话一个直连任务（在 [`HostStreams`] 里，见 [`crate::direct`]）。

mod clock;
mod cursor;
pub mod queue;
pub mod session;
mod streams;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::MAX_SIGNAL_MSG;
use airhand_proto::Role;
use airhand_proto::session::ByeReason;
use airhand_proto::signal::{ClientMsg, Peer, Reclaim, ServerMsg};
use anyhow::{Result, anyhow, bail};
use quinn::Connection;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::ServerState;
pub use crate::direct::{DirectHub, HostLink};
pub use crate::lanes::Router;
use crate::net::{self, LinkConfig, Reconnect, Report};
use crate::tasks::OwnedTask;
use crate::wire::{read_msg, write_msg};

pub use crate::secure::{Access, Admission, Gate};
pub use clock::InputClock;
pub use cursor::CursorCache;
pub use queue::{Encoded, Outbox, SendError};
pub use session::{
    APPROVAL_TIMEOUT, Acks, ApprovalSlots, MAX_PENDING_APPROVALS, PEER_TIMEOUT, Platform, Request,
    Senders, Start, serve,
};
pub use streams::{Admitted, ControllerReceiver, HostStreams, Writer};

/// 本机用户的命令（托盘菜单、Android 的界面）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCmd {
    /// 断开所有正在控制本机的控制端
    KickAll,
    /// 断开所有控制端并更换控制码（旧控制码立即失效）
    NewCode,
}

impl HostCmd {
    /// 告诉被断开的控制端的原因
    pub fn reason(self) -> ByeReason {
        match self {
            HostCmd::KickAll => ByeReason::Kicked,
            HostCmd::NewCode => ByeReason::CodeChanged,
        }
    }
}

/// 被控端服务发生的事（桌面版写进托盘的状态，Android 变成给 Kotlin 的事件）
pub trait HostEvents: Send + Sync + 'static {
    /// 与服务器的连接状态（注册成功之后才算 `Connected`）
    fn server(&self, state: ServerState);
    /// 注册成功（每次连上服务器都会），或者服务器换了密钥（本机用户断开所有控制端之后）。
    /// `changed`：和带去的不一样（第一次注册、原来的控制码取不回来了、换了密钥），要保存下来，下次启动时带上
    fn registered(&self, code: Reclaim, changed: bool) -> impl Future<Output = ()> + Send;
    /// 放弃了控制码（更换控制码）：保存的控制码也要忘掉，之后重新注册
    fn released(&self) -> impl Future<Output = ()> + Send;
    /// 会话结束了
    fn ended(&self, session_id: u64);
    /// 与服务器的连接断了（所有会话随之结束）
    fn offline(&self);
    /// 本机用户断开了所有控制端（包括更换控制码），会话正在结束
    fn kicked(&self, _cmd: HostCmd) {}
}

/// 有控制端要连进来（交给 `run` 的 `session`）
pub struct Incoming {
    pub conn: Connection,
    /// 这条服务器连接上的会话的通道、数据报从这里来
    pub router: Arc<Router>,
    pub session_id: u64,
    /// 控制端的端到端握手的第一条消息（`ServerMsg::Incoming::init`）
    pub init: Vec<u8>,
    /// 本机现在的控制码（配对时要用）
    pub code: u32,
    /// 服务器看到的控制端（名称是控制端自己报的，IP 是服务器看到的）
    pub peer: Peer,
    /// 本机用户断开所有控制端时收到原因：会话应发 Bye 然后结束
    pub kick: watch::Receiver<ByeReason>,
    /// 在等本机用户回答的连接请求的名额，这个被控端服务的所有会话共用（见 [`MAX_PENDING_APPROVALS`]）
    pub approvals: ApprovalSlots,
    /// 直连（见 [`crate::direct`]）：这条服务器连接上的 [`DirectHub`] 和服务器看到的本机地址；`None`：不建直连
    pub direct: Option<HostLink>,
}

/// 被控端服务：一直连着服务器，直到 future 被丢掉（丢掉时依次释放会话、QUIC 连接）。
///
/// * `keep`：让服务器一直为本机保留控制码（被控端会保存控制码，见 `airhand_proto::signal` 的"固定控制码"）；
/// * `reclaim`：上次保存的控制码，第一次连接就取回它；
/// * `session`：开始一个会话（[`serve`]）。
pub async fn run<E, S, F>(
    link: Arc<LinkConfig>,
    keep: bool,
    reclaim: Option<Reclaim>,
    events: Arc<E>,
    session: S,
    mut cmds: mpsc::UnboundedReceiver<HostCmd>,
) where
    E: HostEvents,
    S: Fn(Incoming) -> F + Send + Sync + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    let report: Report = {
        let e = events.clone();
        Arc::new(move |s| e.server(s))
    };
    let mut reconnect = Reconnect::new(report);
    let mut id = Identity {
        reclaim,
        rekey: false,
    };
    let approvals = ApprovalSlots::default();
    loop {
        reconnect.begin();
        let result = connect_once(
            &link, keep, &mut id, &*events, &session, &mut cmds, &approvals,
        )
        .await;
        events.offline();
        reconnect.end("host", result).await;
    }
}

/// 被控端在服务器上是谁：取回控制码用的控制码和密钥，以及有没有还没换成的密钥
struct Identity {
    reclaim: Option<Reclaim>,
    /// 发了 [`ClientMsg::Rekey`] 还没收到 [`ServerMsg::Rekeyed`]（例如这时断线了）。下次注册时请服务器顺便换密钥
    /// （[`ClientMsg::Register::rekey`]）：服务器可能根本没收到 `Rekey`，或者收到之后还没记下来就重启了，
    /// 手里的旧密钥又成了有效的，被断开的控制端拿着旧凭证就能连回来
    rekey: bool,
}

/// 返回 `Ok(())` 表示需要立即重连（更换控制码），`Err` 表示掉线。
async fn connect_once<E, S, F>(
    link: &LinkConfig,
    keep: bool,
    id: &mut Identity,
    events: &E,
    session: &S,
    cmds: &mut mpsc::UnboundedReceiver<HostCmd>,
    approvals: &ApprovalSlots,
) -> Result<()>
where
    E: HostEvents,
    S: Fn(Incoming) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    // 注册紧跟着版本握手发出去，不等服务器回答 Hello（省一个往返）
    let register = ClientMsg::Register {
        reclaim: id.reclaim,
        keep,
        rekey: id.rekey && id.reclaim.is_some(),
    };
    let mut sig = net::connect_signal(link, Role::Host, Some(&register)).await?;
    let reclaim = &mut id.reclaim;

    // 独立的读任务，保证 select 中的接收是取消安全的
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(16);
    let mut recv = sig.recv;
    let _reader = OwnedTask(tokio::spawn(async move {
        let mut buf = Vec::new();
        while let Ok(Some(m)) = read_msg::<_, ServerMsg>(&mut recv, MAX_SIGNAL_MSG, &mut buf).await
        {
            if tx.send(m).await.is_err() {
                break;
            }
        }
    }));

    let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .map_err(|_| anyhow!("registration timed out"))?;
    let Some(first) = first else {
        // 例如这个网络短时间内申请了太多新的控制码：服务器关闭连接时写明了原因
        match tokio::time::timeout(Duration::from_secs(2), sig.conn.closed()).await {
            Ok(quinn::ConnectionError::ApplicationClosed(c)) => bail!(
                "the server refused to register: {}",
                String::from_utf8_lossy(&c.reason)
            ),
            Ok(e) => bail!("the server closed the connection before registering: {e}"),
            Err(_) => bail!("the server closed the control stream before registering"),
        }
    };
    let ServerMsg::Registered { code, secret } = first else {
        bail!("registration failed: {first:?}");
    };
    if reclaim.is_some_and(|r| r.code != code) {
        info!("the previous control code isn't available anymore, got a new one");
    }
    let got = Reclaim { code, secret };
    let changed = *reclaim != Some(got);
    if id.rekey {
        info!(
            code,
            "the server changed this host's secret while registering"
        );
    }
    // 带着 rekey 注册成功：要么换了密钥，要么拿到的是新的控制码，旧凭证都作废了
    id.rekey = false;
    *reclaim = Some(got);
    events.registered(got, changed).await;
    info!(code, "host connected to the server");
    events.server(ServerState::Connected);
    // 控制端的通道和数据报按会话交给会话；控制端连过来的直连（见 crate::direct），建不了就只走中转
    let (router, _router) = Router::start(&sig.conn);
    let direct = DirectHub::start(&sig.endpoint, &sig.punch, &sig.conn)
        .inspect_err(|e| warn!("direct paths are unavailable: {e:#}"))
        .ok();

    let mut sessions: JoinSet<u64> = JoinSet::new();
    // 每次发送都会让所有会话向控制端发送 Bye（带上这个原因）并结束
    let (kick_tx, _) = watch::channel(ByeReason::Kicked);
    // 换钥过程中再次踢人：先保存当前回答，再轮换一次，覆盖两次操作之间发出的凭证。
    let mut rekey_again = false;
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ServerMsg::Incoming { session_id, peer, observed, init }) => {
                    info!(session_id, name = %peer.name, ip = %peer.ip, "controller wants to connect");
                    let run = session(Incoming {
                        conn: sig.conn.clone(),
                        router: router.clone(),
                        session_id,
                        init,
                        code,
                        peer,
                        kick: kick_tx.subscribe(),
                        approvals: approvals.clone(),
                        direct: direct.as_ref().map(|(hub, _)| HostLink {
                            hub: hub.clone(),
                            observed,
                        }),
                    });
                    sessions.spawn(async move {
                        run.await;
                        session_id
                    });
                }
                // 服务器换了取回控制码的密钥（断开所有控制端之后，见 HostCmd::KickAll）：保存下来，下次取回用它
                Some(ServerMsg::Rekeyed { secret }) => {
                    let got = Reclaim { code, secret };
                    *reclaim = Some(got);
                    events.registered(got, true).await;
                    id.rekey = std::mem::take(&mut rekey_again);
                    if id.rekey {
                        write_msg(&mut sig.send, &ClientMsg::Rekey { secret }).await?;
                    }
                    info!(code, "the server changed this host's secret, old reconnect tokens are void");
                }
                Some(other) => warn!("ignoring server message: {other:?}"),
                None => match sig.conn.close_reason() {
                    Some(reason) => bail!(reason),
                    None => bail!("the server closed the control stream"),
                },
            },
            Some(done) = sessions.join_next() => {
                if let Ok(id) = done {
                    events.ended(id);
                }
            }
            Some(cmd) = cmds.recv() => {
                info!(?cmd, "command from the local user");
                if cmd == HostCmd::KickAll {
                    // 先让服务器换密钥（之前发出的重连凭证随之作废），再让会话发 Bye：
                    // 被断开的控制端不能带着凭证马上连回来。更换控制码时旧码直接作废，不用换。
                    // 收到 Rekeyed 之前一直记着：断线了就在下次注册时换（见 Identity::rekey）
                    if id.rekey {
                        rekey_again = true;
                    } else {
                        id.rekey = true;
                        let saved = reclaim.expect("set when registered");
                        write_msg(&mut sig.send, &ClientMsg::Rekey { secret: saved.secret }).await?;
                    }
                }
                events.kicked(cmd);
                kick_tx.send_replace(cmd.reason());
                if cmd == HostCmd::NewCode {
                    // 给会话一点时间把 Bye 发出去，然后放弃旧控制码重新注册
                    let _ = tokio::time::timeout(Duration::from_millis(800), async {
                        while sessions.join_next().await.is_some() {}
                    })
                    .await;
                    // 解除服务器上的绑定，并忘掉保存的控制码（万一重新注册之前就退出了，下次启动也不会再用旧码）
                    let release = async {
                        write_msg(&mut sig.send, &ClientMsg::Release).await?;
                        // 服务器处理完 Release 才会读到控制流结束、断开连接
                        let _ = sig.send.finish();
                        sig.conn.closed().await;
                        anyhow::Ok(())
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(2), release).await;
                    *reclaim = None;
                    events.released().await;
                    sig.conn.close(quinn::VarInt::from_u32(0), b"new code");
                    return Ok(());
                }
            }
            reason = sig.conn.closed() => bail!(reason),
        }
    }
}
