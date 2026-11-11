//! 一个会话的两条路（两端共用，见 `airhand_proto` 的"直连"）：中转一直在，直连通了才有。
//!
//! * 控制通道的每一帧（[`Paths::mirror`]）、Ping / Pong / 画面确认（[`DatagramSender::send`]）两条路都发：
//!   中转上是完整的一份，直连上的那份先到就用它（接收端按计数合并、去重，见 `crate::merge`）；
//! * 画面、声音只走一条路（[`Paths::media`]），被控端的直连任务按两条路的时延决定（[`Paths::set_media`]）；
//!   往直连上写画面失败了，写任务告诉直连任务（[`Paths::report_broken`] / [`Paths::broken`]）；
//! * 探测和回声只发在要量的那条路上（[`DatagramSender::send_on`]）。
//!
//! 直连任务在直连通了、断了的时候改它（[`Paths::attach`] / [`Paths::detach`]），写任务每一帧、每个数据报看一眼。

use std::future::Future;
use std::sync::{Arc, Mutex};

use airhand_proto::datagram;
use airhand_proto::framing;
use airhand_proto::minicbor::Encode;
use bytes::Bytes;
use quinn::{Connection, SendStream};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::lanes::Via;
use crate::secure::Sealer;

/// 直连上的控制通道最多排着这么多帧没写出去，再多就不写了（中转上还有一份）
const MIRROR_QUEUE: usize = 64;

/// 一个会话的两条路（见模块说明）。复制出来的是同一个
#[derive(Clone)]
pub struct Paths(Arc<Shared>);

struct Shared {
    relay: Connection,
    state: Mutex<State>,
    /// 往现在这条直连上写画面失败了几次（[`Paths::report_broken`]）
    broken: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    direct: Option<Direct>,
    /// 画面、声音走直连
    media_direct: bool,
}

struct Direct {
    conn: Connection,
    /// 本端在直连上开的控制通道（的写任务）；写任务不在了就是 `None`，直连上不再发控制帧
    control: Option<mpsc::Sender<Bytes>>,
}

impl Paths {
    /// 只有中转（直连通了由直连任务接上，[`Paths::attach`]）
    pub fn new(relay: Connection) -> Self {
        Self(Arc::new(Shared {
            relay,
            state: Mutex::default(),
            broken: watch::Sender::new(0),
        }))
    }

    pub fn relay(&self) -> &Connection {
        &self.0.relay
    }

    /// 通着的直连
    pub fn direct(&self) -> Option<Connection> {
        let st = self.0.state.lock().unwrap();
        st.direct.as_ref().map(|d| d.conn.clone())
    }

    /// 画面、声音现在走的直连；`None`：走中转
    pub fn media(&self) -> Option<Connection> {
        let st = self.0.state.lock().unwrap();
        let direct = st.direct.as_ref().filter(|_| st.media_direct);
        direct.map(|d| d.conn.clone())
    }

    /// 直连通了：从现在起控制通道的每一帧也写进 `control`（本端在直连上开的控制通道），控制数据报也从 `conn` 发；
    /// 画面、声音还在中转上（[`Paths::set_media`]）。返回往直连上写控制帧的任务（直连断了、[`Paths::detach`] 之后结束）
    pub(crate) fn attach(
        &self,
        conn: Connection,
        mut control: SendStream,
    ) -> impl Future<Output = ()> + Send + 'static {
        let (tx, mut rx) = mpsc::channel::<Bytes>(MIRROR_QUEUE);
        *self.0.state.lock().unwrap() = State {
            direct: Some(Direct {
                conn,
                control: Some(tx),
            }),
            media_direct: false,
        };
        async move {
            while let Some(b) = rx.recv().await {
                if control.write_chunk(b).await.is_err() {
                    return;
                }
            }
            let _ = control.finish();
        }
    }

    /// 直连断了：只剩中转，画面、声音也回到中转
    pub(crate) fn detach(&self) {
        *self.0.state.lock().unwrap() = State::default();
    }

    /// 画面、声音换到 `via` 上（直连没通时只能是中转）
    pub(crate) fn set_media(&self, via: Via) {
        let mut st = self.0.state.lock().unwrap();
        st.media_direct = via == Via::Direct && st.direct.is_some();
    }

    /// 控制通道的写任务每加密一帧就调用一次（`sealed`：带长度前缀的密文）：直连通着的话也从直连发一份。
    /// 直连上排不下就不发了（中转上还有一份）
    pub(crate) fn mirror(&self, sealed: &Bytes) {
        let mut st = self.0.state.lock().unwrap();
        let Some(d) = &mut st.direct else { return };
        if let Some(c) = &d.control
            && let Err(mpsc::error::TrySendError::Closed(_)) = c.try_send(sealed.clone())
        {
            d.control = None;
        }
    }

    /// 往直连 `conn` 上写画面失败了（画面的写任务已经放弃了它上面的流）：告诉直连任务（[`Paths::broken`]）。
    /// 不是现在这条直连的（之前那条的）不算
    pub(crate) fn report_broken(&self, conn: &Connection) {
        let current = self
            .0
            .state
            .lock()
            .unwrap()
            .direct
            .as_ref()
            .is_some_and(|d| d.conn.stable_id() == conn.stable_id());
        if current {
            self.0.broken.send_modify(|n| *n += 1);
        }
    }

    /// [`Paths::report_broken`] 报告的失败，从现在起（之前报告的不算）
    pub(crate) fn broken(&self) -> watch::Receiver<u64> {
        self.0.broken.subscribe()
    }
}

/// 发控制数据报（`airhand_proto::datagram::CONTROL`）：加密之后从两条路或者指定的一条路发出去。丢了就丢了
#[derive(Clone)]
pub struct DatagramSender(Arc<Datagrams>);

struct Datagrams {
    session_id: u64,
    paths: Paths,
    sealer: Mutex<Sealer>,
}

impl DatagramSender {
    /// `sealer`：`Secured::datagrams` 的发送端
    pub fn new(session_id: u64, paths: Paths, sealer: Sealer) -> Self {
        Self(Arc::new(Datagrams {
            session_id,
            paths,
            sealer: Mutex::new(sealer),
        }))
    }

    /// 两条路都发**同一份**（同一个计数，对方的重放窗口挡掉后到的那份）：总是走更快的那条。Ping、Pong、画面确认用它
    pub fn send<T: Encode<()>>(&self, msg: &T) {
        let Some(d) = self.seal(msg) else { return };
        if let Some(direct) = self.0.paths.direct() {
            let _ = direct.send_datagram(d.clone());
        }
        if let Err(e) = self.0.paths.relay().send_datagram(d) {
            debug!("dropped a control datagram: {e}");
        }
    }

    /// 只发在 `via` 这条路上（探测、回声：量的就是这条路）。直连没通时什么也不发
    pub fn send_on<T: Encode<()>>(&self, via: Via, msg: &T) {
        let conn = match via {
            Via::Relay => Some(self.0.paths.relay().clone()),
            Via::Direct => self.0.paths.direct(),
        };
        if let (Some(conn), Some(d)) = (conn, self.seal(msg)) {
            let _ = conn.send_datagram(d);
        }
    }

    fn seal<T: Encode<()>>(&self, msg: &T) -> Option<Bytes> {
        let d = &self.0;
        let sealed = d.sealer.lock().unwrap().seal_datagram(
            d.session_id,
            datagram::CONTROL,
            &framing::to_vec(msg),
        );
        sealed
            .inspect_err(|e| debug!("can't seal a control datagram: {e:#}"))
            .ok()
            .map(Bytes::from)
    }
}
