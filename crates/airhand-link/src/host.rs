//! 被控端：与服务器的连接（注册、取回控制码、断开所有控制端、更换控制码），以及每个会话的三条流。
//!
//! 桌面版（`client/src-tauri/src/host/`）和 Android（`android/native/src/host/`）共用这里，
//! 采集、编码、注入输入、请本机用户同意这些各平台不同的事情由它们自己做：
//!
//! * [`run`]：一直连着服务器（掉线自动重连，取回原来的控制码），每来一个控制端就调用 `session` 开始一个会话；
//!   发生了什么通过 [`HostEvents`] 告诉外面（托盘、Android 的界面）；
//! * [`HostSession`]：会话的三条流。先在控制流上做端到端握手（[`crate::secure::respond`]）：让不让这个控制端进来
//!   由平台给的 [`Gate`] 决定（配对过的、输对配对码的、本机用户同意的……）。之后控制消息、画面包、声音包分别排进
//!   三个有界队列，各自的写任务把它们加密后写进流（声音包优先发成数据报）；控制端的消息由读任务解密后交给
//!   [`HostSession::incoming`]，过时的输入在这里换成"释放所有按键"（见 [`InputClock`]）；
//! * [`CursorCache`]：控制端缓存了哪些光标形状。

use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airhand_proto::framing::{self, Decoded, read_frame, read_msg, write_msg};
use airhand_proto::handshake::{PublicKey, TAG_LEN};
use airhand_proto::session::{
    ByeReason, CURSOR_SLOTS, ControllerMsg, CursorShape, CursorSlot, HostMsg, INPUT_MAX_AGE,
    InputEvent,
};
use airhand_proto::signal::{ClientMsg, Peer, Reclaim, ServerMsg};
use airhand_proto::{Features, MAX_CONTROL_MSG, MAX_SIGNAL_MSG, Role, SessionLane};
use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use quinn::Connection;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::ServerState;
use crate::net::{self, LinkConfig, Reconnect, Report};
use crate::secure::{self, ResponderParams};
use crate::tasks::OwnedTask;
use crate::transport::{SessionStreams, open_session, write_audio, write_lane};

pub use crate::secure::{Access, Admission, Gate};

/// 控制端每秒都会发 Ping，这么久没有任何消息说明对方已失联
pub const PEER_TIMEOUT: Duration = Duration::from_secs(30);
/// 控制流的发送队列。控制消息都很小，排满了说明对方早就不收了
const CONTROL_QUEUE: usize = 32;

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
    /// 有控制端要连进来，会话随之开始
    fn incoming(&self, session_id: u64, peer: &Peer);
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
    pub session_id: u64,
    /// 本机现在的控制码（配对时要用）
    pub code: u32,
    /// 服务器看到的控制端（名称是控制端自己报的，IP 是服务器看到的）
    pub peer: Peer,
    /// 本机用户断开所有控制端时收到原因：会话应发 Bye 然后结束
    pub kick: watch::Receiver<ByeReason>,
}

/// 被控端服务：一直连着服务器，直到 future 被丢掉（丢掉时依次释放会话、QUIC 连接）。
///
/// * `keep`：让服务器一直为本机保留控制码（被控端会保存控制码，见 `airhand_proto::signal` 的"固定控制码"）；
/// * `reclaim`：上次保存的控制码，第一次连接就取回它；
/// * `session`：开始一个会话（[`HostSession::open`]）。
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
    loop {
        reconnect.begin();
        let result = connect_once(&link, keep, &mut id, &*events, &session, &mut cmds).await;
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
) -> Result<()>
where
    E: HostEvents,
    S: Fn(Incoming) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    let mut sig = net::connect_signal(link, Role::Host).await?;
    let reclaim = &mut id.reclaim;
    write_msg(
        &mut sig.send,
        &ClientMsg::Register {
            reclaim: *reclaim,
            keep,
            rekey: id.rekey && reclaim.is_some(),
        },
    )
    .await?;

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

    let mut sessions: JoinSet<u64> = JoinSet::new();
    // 每次发送都会让所有会话向控制端发送 Bye（带上这个原因）并结束
    let (kick_tx, _) = watch::channel(ByeReason::Kicked);
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ServerMsg::Incoming { session_id, peer }) => {
                    info!(session_id, name = %peer.name, ip = %peer.ip, "controller wants to connect");
                    events.incoming(session_id, &peer);
                    let run = session(Incoming {
                        conn: sig.conn.clone(),
                        session_id,
                        code,
                        peer,
                        kick: kick_tx.subscribe(),
                    });
                    sessions.spawn(async move {
                        run.await;
                        session_id
                    });
                }
                // 服务器换了取回控制码的密钥（断开所有控制端之后，见 HostCmd::KickAll）：保存下来，下次取回用它
                Some(ServerMsg::Rekeyed { secret }) => {
                    let got = Reclaim { code, secret };
                    id.rekey = false;
                    *reclaim = Some(got);
                    events.registered(got, true).await;
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
                    id.rekey = true;
                    write_msg(&mut sig.send, &ClientMsg::Rekey).await?;
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

// ───────────────────────────── 会话的三条流 ─────────────────────────────

/// 控制流的发送端（可以复制，例如交给编码线程）。消息编码好之后排进有界队列，排满了算对方失联
#[derive(Clone)]
pub struct Control(mpsc::Sender<Bytes>);

impl Control {
    /// 在自己管理的队列上发控制消息（[`HostSession::open`] 里就是这样建的；测试也用）
    pub fn new(queue: mpsc::Sender<Bytes>) -> Self {
        Self(queue)
    }

    /// 编码一条控制消息（带长度前缀，明文：写任务加密）。同一条消息发给多个会话时只编码一次
    pub fn encode(msg: &HostMsg) -> Bytes {
        Bytes::from(framing::encode_with_spare(msg, TAG_LEN))
    }

    pub fn send(&self, msg: &HostMsg) -> Result<()> {
        self.send_encoded(Self::encode(msg))
    }

    /// 发送 [`Control::encode`] 编好的消息
    pub fn send_encoded(&self, bytes: Bytes) -> Result<()> {
        self.0
            .try_send(bytes)
            .map_err(|_| anyhow!("the session's control queue is closed or full"))
    }

    /// 底层的队列：编码线程满了要等着，不能直接报错（见 `client/src-tauri/src/host/pipeline.rs`）
    pub fn queue(&self) -> &mpsc::Sender<Bytes> {
        &self.0
    }
}

/// 被控端一个会话的三条流。
///
/// 结束时调用 [`HostSession::close`]：在那之前要先停掉还拿着 `video` / `audio` 发送端的东西
/// （编码管线、声音订阅），写任务才能把队列里剩下的（例如 Bye）发完、正常结束流。
pub struct HostSession {
    pub ctl: Control,
    /// 画面包（[`airhand_proto::media::frame`] 编好的）
    pub video: mpsc::Sender<Bytes>,
    /// 声音包（同上）
    pub audio: mpsc::Sender<Bytes>,
    /// 控制端发来的消息
    pub incoming: mpsc::Receiver<ControllerMsg>,
    /// 写任务（三条流一起）：意外结束就是连接出了问题
    pub writer: Writer,
    reader: OwnedTask<Result<()>>,
}

/// 判断控制端的输入晚到了多久（见 `airhand_proto::session` 的"过时的输入"）。
///
/// 两边的时钟不同步，只能比"比平时晚了多少"：`收到的时刻 − t` 是两边时钟之差加上这一路的耽搁，
/// 记下它的最小值当作基准（时钟之差 + 最快的那一次），超出基准的部分就是这条消息额外耽搁了多久——
/// 在控制端的队列里、在 QUIC 的发送缓冲里（断网时往里写不会阻塞）、在服务器上。Ping 和输入都带着 `t`，都算样本。
///
/// * 基准每秒最多上调 1 ms（[`InputClock::DRIFT`]），跟得上两边时钟的漂移（通常不到 0.01%），
///   断网再久，恢复后涌来的旧消息也远远超出基准；
/// * 连续 [`InputClock::RESYNC`] 收到的全是晚到的消息，说明这一路本身变慢了（例如换了网络），而不是积压：
///   以现在为准重新取基准，不能一直丢下去。
pub struct InputClock {
    epoch: Instant,
    /// 基准（微秒）和取它的时刻
    base: Option<(i64, Instant)>,
    /// 从什么时候起收到的全是晚到的消息
    late_since: Option<Instant>,
}

impl Default for InputClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            base: None,
            late_since: None,
        }
    }
}

impl InputClock {
    /// 基准每过这么多微秒最多上调 1 微秒
    const DRIFT: i64 = 1000;
    const RESYNC: Duration = Duration::from_secs(5);

    /// 记下一条带着控制端时刻 `t` 的消息（Ping、输入），`now` 是收到它的时刻。返回它是不是过时了（只对输入有意义）
    pub fn stale(&mut self, t: u64, now: Instant) -> bool {
        let offset = now.saturating_duration_since(self.epoch).as_micros() as i64 - t as i64;
        let base = match self.base {
            Some((b, at)) => {
                (b + now.saturating_duration_since(at).as_micros() as i64 / Self::DRIFT).min(offset)
            }
            None => offset,
        };
        let late = Duration::from_micros((offset - base) as u64) > INPUT_MAX_AGE;
        let since = match (late, self.late_since) {
            (false, _) => None,
            (true, since) => Some(since.unwrap_or(now)),
        };
        if since.is_some_and(|s| now.saturating_duration_since(s) > Self::RESYNC) {
            warn!(
                "input has been arriving late for {:?}, the network path got slower: resynchronizing",
                Self::RESYNC
            );
            self.base = Some((offset, now));
            self.late_since = None;
            return false;
        }
        self.base = Some((base, now));
        self.late_since = since;
        late
    }
}

/// 发给一个控制端的光标形状缓存：控制端按槽位存形状（[`CURSOR_SLOTS`] 个），这里记着每个槽位里是什么
/// （见 [`CursorSlot`]）。只在一个会话里有效，会话里换显示器、重建编码管线时接着用。
///
/// 槽位满了换掉最久没用到的那个。用形状内容的 64 位哈希认形状，几十个形状里撞上的概率可以忽略
pub struct CursorCache {
    /// 每个槽位里的形状（哈希）和最近一次用到的时刻
    slots: Vec<Option<(u64, u64)>>,
    clock: u64,
}

impl Default for CursorCache {
    fn default() -> Self {
        Self {
            slots: vec![None; CURSOR_SLOTS as usize],
            clock: 0,
        }
    }
}

impl CursorCache {
    /// 光标换成了 `shape`：控制端已经有它的话只给槽位，否则挑一个槽位、带上图像
    pub fn place(&mut self, shape: &CursorShape) -> CursorSlot {
        let mut h = DefaultHasher::new();
        shape.hash(&mut h);
        let hash = h.finish();
        self.clock += 1;
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|(h, _)| h == hash))
        {
            self.slots[i] = Some((hash, self.clock));
            return CursorSlot {
                slot: i as u8,
                image: None,
            };
        }
        let i = self
            .slots
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                (0..self.slots.len())
                    .min_by_key(|&i| self.slots[i].map_or(0, |(_, used)| used))
                    .unwrap_or(0)
            });
        self.slots[i] = Some((hash, self.clock));
        CursorSlot {
            slot: i as u8,
            image: Some(shape.clone()),
        }
    }

    /// 带着图像的 `slot` 没能发出去（例如编码管线正在停下）：控制端没有它，下次还得带图像
    pub fn forget(&mut self, slot: &CursorSlot) {
        if slot.image.is_some()
            && let Some(s) = self.slots.get_mut(slot.slot as usize)
        {
            *s = None;
        }
    }
}

/// 三条流的写任务
pub struct Writer(OwnedTask<Result<((), (), ())>>);

impl Writer {
    /// 等到写任务意外结束（可以放在 `select!` 里）
    pub async fn failed(&mut self) -> anyhow::Error {
        match (&mut self.0.0).await {
            Ok(Err(e)) => e.context("session writer failed"),
            r => anyhow!("session writer ended: {r:?}"),
        }
    }
}

/// 端到端握手之后，被控端知道的这个控制端（见 [`crate::secure::respond`]）
#[derive(Debug, Clone)]
pub struct Admitted {
    /// 控制端的公钥（它的身份）
    pub key: PublicKey,
    /// 控制端自己报的名称（空字符串表示不知道）和系统：在加密的握手里，服务器改不了
    pub name: String,
    pub os: String,
    /// 安全码：控制端显示的是同一个，两边的人可以对一下（见 `airhand_proto::handshake`）
    pub safety: String,
    /// 为什么让它进来；[`Admission::NeedsApproval`] 时会话要先请本机用户同意
    pub admission: Admission,
    /// 选定的会话版本、双方都支持的会话能力位
    pub version: u16,
    pub features: Features,
}

/// 读一条控制端的消息（解密、解码）；对方版本新增的消息跳过。流在消息边界处结束时为 `None`
async fn next_controller_msg(
    recv: &mut quinn::RecvStream,
    opener: &mut secure::FrameOpener,
    buf: &mut Vec<u8>,
) -> Result<Option<ControllerMsg>> {
    loop {
        if !read_frame(recv, MAX_CONTROL_MSG, buf).await? {
            return Ok(None);
        }
        opener.open(buf)?;
        match framing::decode(buf)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown => debug!("skipping a message from a newer controller"),
        }
    }
}

impl HostSession {
    /// 打开会话的三条流，在控制流上做端到端握手（让不让这个控制端进来由 `gate` 决定，拒绝时返回错误）。
    /// `video_queue` / `audio_queue`：画面、声音的发送队列有多长（满了由发送方决定丢弃还是等待）
    pub async fn open(
        inc: &Incoming,
        video_queue: usize,
        audio_queue: usize,
        gate: &dyn Gate,
    ) -> Result<(Self, Admitted)> {
        let session_id = inc.session_id;
        let SessionStreams {
            control: (mut send, mut recv),
            video: (video_send, _),
            audio: (audio_send, _),
        } = open_session(&inc.conn, session_id, Role::Host).await?;
        let r = secure::respond(
            &mut recv,
            &mut send,
            ResponderParams {
                session_id,
                code: inc.code,
                gate,
            },
        )
        .await?;
        let admitted = Admitted {
            key: r.secured.peer,
            name: r.intro.name.chars().take(64).collect(),
            os: r.intro.os.chars().take(64).collect(),
            safety: r.secured.safety.clone(),
            admission: r.admission,
            version: r.version,
            features: r.features,
        };
        info!(
            session_id,
            admission = ?admitted.admission,
            controller = %secure::hex(&admitted.key),
            safety = %admitted.safety,
            "end-to-end session established"
        );
        let (video_sealer, audio_sealer, datagrams) = (
            r.secured.sealer(SessionLane::Video),
            r.secured.sealer(SessionLane::Audio),
            r.secured.datagram_sealer(),
        );
        let mut opener = r.secured.opener(SessionLane::Control);
        let control_sealer = r.sealer;
        let (ctl_tx, ctl_rx) = mpsc::channel(CONTROL_QUEUE);
        let (video_tx, video_rx) = mpsc::channel(video_queue);
        let (audio_tx, audio_rx) = mpsc::channel(audio_queue);
        let conn = inc.conn.clone();
        let writer = OwnedTask(tokio::spawn(async move {
            tokio::try_join!(
                write_lane(send, ctl_rx, control_sealer),
                write_lane(video_send, video_rx, video_sealer),
                write_audio(
                    conn,
                    session_id,
                    audio_send,
                    audio_rx,
                    audio_sealer,
                    datagrams
                )
            )
        }));
        // 读任务：读一帧不是取消安全的，单独放在一个任务里，通过通道交给会话
        let (in_tx, incoming) = mpsc::channel(64);
        let reader = OwnedTask(tokio::spawn(async move {
            let mut buf = Vec::with_capacity(1024);
            let mut clock = InputClock::default();
            // 这一串过时的输入已经换成了一个"释放所有按键"
            let mut released = false;
            while let Some(m) = next_controller_msg(&mut recv, &mut opener, &mut buf).await? {
                let m = match m {
                    ControllerMsg::Ping { t } => {
                        clock.stale(t, Instant::now());
                        m
                    }
                    ControllerMsg::Input { t, events } => {
                        if !clock.stale(t, Instant::now()) {
                            released = false;
                            ControllerMsg::Input { t, events }
                        } else if released {
                            continue;
                        } else {
                            // 不在网络恢复后重放过时的操作；丢掉的里面可能有松开按键，所以松开所有按键
                            warn!(session_id, "dropping input that arrived too late");
                            released = true;
                            ControllerMsg::Input {
                                t,
                                events: vec![InputEvent::ReleaseAll],
                            }
                        }
                    }
                    m => m,
                };
                if in_tx.send(m).await.is_err() {
                    break;
                }
            }
            Ok(())
        }));
        Ok((
            Self {
                ctl: Control(ctl_tx),
                video: video_tx,
                audio: audio_tx,
                incoming,
                writer: Writer(writer),
                reader,
            },
            admitted,
        ))
    }

    /// 结束会话：等写任务把排好队的数据（例如 Bye）发完（最多 1 秒），返回会话的结果
    ///（`result` 是会话自己的结果；会话正常结束时，读任务的错误也算进来）
    pub async fn close(self, session_id: u64, result: Result<()>) -> Result<()> {
        let HostSession {
            ctl,
            video,
            audio,
            incoming,
            mut writer,
            mut reader,
        } = self;
        drop((ctl, video, audio, incoming));
        reader.abort();
        if !writer.0.is_finished() {
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut writer.0.0).await;
        }
        let result = match result {
            Ok(()) => match (&mut reader.0).await {
                Ok(Err(e)) => Err(e),
                _ => Ok(()),
            },
            Err(e) => Err(e),
        };
        match &result {
            Ok(()) => info!(session_id, "session ended"),
            Err(e) => warn!(session_id, "session failed: {e:#}"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 控制端的时钟比被控端晚 1000 秒、正常时这一路 30 ms
    fn clock_at(start: Instant) -> InputClock {
        InputClock {
            epoch: start,
            ..InputClock::default()
        }
    }

    const SKEW: u64 = 1_000_000_000;

    #[test]
    fn input_that_waited_out_a_network_outage_is_stale() {
        let start = Instant::now();
        let mut c = clock_at(start);
        // 控制端在 sent 时刻（被控端时钟，毫秒）发出、被控端在 at 时刻收到
        let mut msg =
            |sent: u64, at: u64| c.stale(SKEW + sent * 1000, start + Duration::from_millis(at));
        // 时快时慢的正常网络
        for i in 0..100 {
            assert!(!msg(i * 100, i * 100 + 30 + (i % 7) * 20));
        }
        // 断网 30 秒：这期间的 Ping 和输入都积在 QUIC 的发送缓冲里，恢复后一起到
        let back = 10_000 + 30_000;
        assert!(msg(10_000, back));
        assert!(msg(25_000, back + 1));
        assert!(msg(37_000, back + 2));
        // 恢复前 1 秒才发的：不到 INPUT_MAX_AGE，照样执行
        assert!(!msg(39_000, back + 3));
        // 恢复之后新发的
        assert!(!msg(39_990, back + 40));
        assert!(!msg(40_100, 40_100 + 35));
    }

    #[test]
    fn follows_clock_drift_and_a_slower_path() {
        let start = Instant::now();
        let mut c = clock_at(start);
        // 控制端的时钟每秒慢 0.05 ms（50 ppm），跑一个小时
        for s in 0..3600u64 {
            let sent = SKEW + s * 1_000_000 - s * 50;
            assert!(!c.stale(sent, start + Duration::from_millis(s * 1000 + 30)));
        }
        // 换了一条慢 3 秒的网络：丢一阵子（不超过 RESYNC），然后重新取基准
        let mut dropped = 0;
        for i in 0..100u64 {
            let at = 3_700_000 + i * 100;
            if c.stale(
                SKEW + (at - 3_030) * 1000 - 3600 * 50,
                start + Duration::from_millis(at),
            ) {
                dropped += 1;
            }
        }
        assert!(dropped > 0 && dropped <= 51, "{dropped}");
        assert!(!c.stale(
            SKEW + (3_720_000 - 3_030) * 1000 - 3600 * 50,
            start + Duration::from_millis(3_720_000)
        ));
    }

    #[test]
    fn cursor_cache_sends_each_shape_once() {
        let shape = |n: u8| CursorShape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![n; 4],
        };
        let mut c = CursorCache::default();
        // 动画光标：同几个形状循环
        let first: Vec<CursorSlot> = (0..8).map(|n| c.place(&shape(n))).collect();
        assert!(first.iter().all(|s| s.image.is_some()));
        for round in 0..3 {
            for n in 0..8 {
                let s = c.place(&shape(n));
                assert_eq!(
                    (s.slot, s.image.is_none()),
                    (first[n as usize].slot, true),
                    "round {round}"
                );
            }
        }
        // 槽位满了：换掉最久没用的（现在是 0 号形状）
        for n in 8..CURSOR_SLOTS {
            assert!(c.place(&shape(n)).image.is_some());
        }
        for n in 1..8 {
            c.place(&shape(n));
        }
        let s = c.place(&shape(200));
        assert_eq!(s.slot, first[0].slot);
        assert!(s.image.is_some());
        assert!(c.place(&shape(0)).image.is_some());
        // 没发出去的要忘掉，下次还带图像
        let s = c.place(&shape(201));
        c.forget(&s);
        assert!(c.place(&shape(201)).image.is_some());
    }
}
