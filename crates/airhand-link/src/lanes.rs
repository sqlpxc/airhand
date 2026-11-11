//! 会话的通道和数据报从哪里来、往哪里去（两端共用，中转和直连共用）。
//!
//! 每端为自己要发的每条通道开一条**单向流**（[`open_lane`]），开头是会话头（[`SessionHeader`]）。中转上，服务器按会话 ID
//! 向另一端开一条单向流、写同样的头、转发；直连上，流本来就是另一端开的。所以收的一端不管哪条路，做的都是
//! "接受单向流、读会话头、交给那个会话的那条通道"：中转连接上由 [`Router`] 做（一条连接上可能有好几个会话），
//! 直连上由 [`accept_direct`] 做（一条直连只属于一个会话）。数据报也一样按会话 ID 和种类分发。
//!
//! 一个会话在收的一端是一个 [`Inbox`]：每条通道一个信箱（中转、直连上来的流都放进来，交给 [`crate::merge::Merge`]
//! 合并），声音数据报、探测数据报各一个信箱。往里放东西的是 [`Feed`]（两条路各拿一个）。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::{SessionHeader, SessionLane, datagram};
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, RecvStream, SendStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::debug;

use crate::tasks::OwnedTask;

/// 东西从哪条路来（画面、声音走哪条路）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Via {
    Relay = 0,
    Direct = 1,
}

/// 一条通道的流：中转上的一条，直连上的（每次直连通了一条）
pub type LaneStreams = mpsc::UnboundedReceiver<(RecvStream, Via)>;

/// 每个会话最多排着这么多个还没交出去的数据报（声音、探测各自的）。再多就丢：本来就可能丢
const DATAGRAM_QUEUE: usize = 16;
/// 读会话头最多等这么久
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// 会话还没登记就先到了的流（中转转过来的时候，本端还没处理完服务器的回答）：最多留这么多条、这么久
const EARLY_STREAMS: usize = 16;
const EARLY_TTL: Duration = Duration::from_secs(10);

/// 往一个会话放东西（见模块说明）
#[derive(Clone)]
pub struct Feed {
    lanes: [mpsc::UnboundedSender<(RecvStream, Via)>; SessionLane::ALL.len()],
    audio: mpsc::Sender<Bytes>,
    probes: mpsc::Sender<(Via, Bytes)>,
}

/// 一个会话收的一端（见模块说明）
pub struct Inbox {
    pub control: LaneStreams,
    pub video: LaneStreams,
    pub bulk: LaneStreams,
    /// 声音数据报（整个数据报，还是密文）
    pub audio: mpsc::Receiver<Bytes>,
    /// 探测数据报（同上）和它从哪条路来
    pub probes: mpsc::Receiver<(Via, Bytes)>,
    /// 直连上来的流、数据报也放进来
    pub feed: Feed,
}

impl Inbox {
    pub fn new() -> Self {
        let (control_tx, control) = mpsc::unbounded_channel();
        let (video_tx, video) = mpsc::unbounded_channel();
        let (bulk_tx, bulk) = mpsc::unbounded_channel();
        let (audio_tx, audio) = mpsc::channel(DATAGRAM_QUEUE);
        let (probes_tx, probes) = mpsc::channel(DATAGRAM_QUEUE);
        Self {
            control,
            video,
            bulk,
            audio,
            probes,
            feed: Feed {
                lanes: [control_tx, video_tx, bulk_tx],
                audio: audio_tx,
                probes: probes_tx,
            },
        }
    }
}

impl Default for Inbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Feed {
    /// 一条通道的流到了。会话不认识的通道（对方版本新增的）、不再收的通道：叫对方别发了
    pub fn stream(&self, lane: u8, recv: RecvStream, via: Via) {
        let Some(lane) = SessionLane::from_u8(lane) else {
            debug!(lane, "ignoring a lane this version doesn't know");
            return stop(recv);
        };
        if let Err(mpsc::error::SendError((recv, _))) = self.lanes[lane as usize].send((recv, via))
        {
            stop(recv);
        }
    }

    /// 一个数据报到了。会话跟不上就丢掉：数据报本来就可能丢
    pub fn datagram(&self, d: Bytes, via: Via) {
        match datagram::kind(&d) {
            Some(datagram::AUDIO) => {
                let _ = self.audio.try_send(d);
            }
            Some(datagram::PROBE) => {
                let _ = self.probes.try_send((via, d));
            }
            _ => {}
        }
    }
}

fn stop(mut recv: RecvStream) {
    let _ = recv.stop(0u32.into());
}

/// 开一条本端发的通道：一条单向流，开头写好会话头
pub async fn open_lane(
    conn: &Connection,
    session_id: u64,
    lane: SessionLane,
) -> Result<SendStream> {
    let mut send = tokio::time::timeout(HEADER_TIMEOUT, conn.open_uni())
        .await
        .context("timed out opening a session lane")??;
    send.set_priority(lane.priority() as i32)?;
    send.write_all(&SessionHeader::new(session_id, lane).encode())
        .await?;
    Ok(send)
}

/// 对方在这条通道上开的第一条流（中转上的那条）。握手用：那时还没有直连
pub async fn first_stream(lane: &mut LaneStreams, wait: Duration) -> Result<RecvStream> {
    let (recv, _) = tokio::time::timeout(wait, lane.recv())
        .await
        .context("the peer didn't open its lane")?
        .context("the session ended")?;
    Ok(recv)
}

/// 读一条流开头的会话头；读不出来（超时、不是会话头）就叫对方别发了
async fn header(mut recv: RecvStream) -> Option<(SessionHeader, RecvStream)> {
    let mut b = [0u8; SessionHeader::LEN];
    let read = tokio::time::timeout(HEADER_TIMEOUT, recv.read_exact(&mut b)).await;
    match read {
        Ok(Ok(())) => match SessionHeader::decode(&b) {
            Some(h) => Some((h, recv)),
            None => {
                stop(recv);
                None
            }
        },
        _ => {
            stop(recv);
            None
        }
    }
}

/// 一条中转连接上的会话：按会话 ID 把对方的流、数据报交给会话（见模块说明）
pub struct Router {
    state: Mutex<RouterState>,
}

#[derive(Default)]
struct RouterState {
    sessions: HashMap<u64, Feed>,
    early: VecDeque<(Instant, SessionHeader, RecvStream)>,
}

/// 会话在 [`Router`] 上的登记，丢掉时注销（之后来的流、数据报都不要了）
pub struct Registration {
    router: Arc<Router>,
    session_id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.router
            .state
            .lock()
            .unwrap()
            .sessions
            .remove(&self.session_id);
    }
}

impl Router {
    /// 开始分发 `conn` 上对方的流和数据报。返回的任务丢掉时停止
    pub fn start(conn: &Connection) -> (Arc<Self>, OwnedTask<()>) {
        let router = Arc::new(Self {
            state: Mutex::default(),
        });
        let task = OwnedTask(tokio::spawn(router.clone().run(conn.clone())));
        (router, task)
    }

    /// 登记一个会话：之后（以及之前先到了的）这个会话的流、数据报都交给返回的 [`Inbox`]
    pub fn register(self: &Arc<Self>, session_id: u64) -> (Inbox, Registration) {
        let inbox = Inbox::new();
        let mut st = self.state.lock().unwrap();
        st.sessions.insert(session_id, inbox.feed.clone());
        let early = std::mem::take(&mut st.early);
        for (at, h, recv) in early {
            if h.session_id == session_id {
                inbox.feed.stream(h.lane, recv, Via::Relay);
            } else {
                st.early.push_back((at, h, recv));
            }
        }
        drop(st);
        let reg = Registration {
            router: self.clone(),
            session_id,
        };
        (inbox, reg)
    }

    fn stream(&self, h: SessionHeader, recv: RecvStream) {
        let mut st = self.state.lock().unwrap();
        if let Some(feed) = st.sessions.get(&h.session_id) {
            return feed.stream(h.lane, recv, Via::Relay);
        }
        let now = Instant::now();
        while st
            .early
            .front()
            .is_some_and(|(at, ..)| now.duration_since(*at) > EARLY_TTL)
            || st.early.len() >= EARLY_STREAMS
        {
            if let Some((_, _, old)) = st.early.pop_front() {
                stop(old);
            }
        }
        st.early.push_back((now, h, recv));
    }

    fn datagram(&self, d: Bytes) {
        let Some(session_id) = datagram::session(&d) else {
            return;
        };
        let feed = self
            .state
            .lock()
            .unwrap()
            .sessions
            .get(&session_id)
            .cloned();
        if let Some(feed) = feed {
            feed.datagram(d, Via::Relay);
        }
    }

    async fn run(self: Arc<Self>, conn: Connection) {
        let mut headers = JoinSet::new();
        loop {
            tokio::select! {
                s = conn.accept_uni() => match s {
                    Ok(recv) => {
                        headers.spawn(header(recv));
                    }
                    Err(_) => break,
                },
                d = conn.read_datagram() => match d {
                    Ok(d) => self.datagram(d),
                    Err(_) => break,
                },
                Some(Ok(Some((h, recv)))) = headers.join_next() => self.stream(h, recv),
            }
        }
    }
}

/// 一条直连上对方的流和数据报都交给会话 `session_id`（直连只属于一个会话）。直连断了就结束
pub async fn accept_direct(conn: Connection, session_id: u64, feed: Feed) {
    let mut headers = JoinSet::new();
    loop {
        tokio::select! {
            s = conn.accept_uni() => match s {
                Ok(recv) => {
                    headers.spawn(header(recv));
                }
                Err(_) => break,
            },
            d = conn.read_datagram() => match d {
                Ok(d) if datagram::session(&d) == Some(session_id) => feed.datagram(d, Via::Direct),
                Ok(_) => {}
                Err(_) => break,
            },
            Some(Ok(Some((h, recv)))) = headers.join_next() => {
                if h.session_id == session_id {
                    feed.stream(h.lane, recv, Via::Direct);
                } else {
                    stop(recv);
                }
            }
        }
    }
}
