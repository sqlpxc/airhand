//! 被控端发给控制端的消息队列（有界），和控制端的命令队列（`crate::controller::queue`）是同一个设计：
//!
//! * **走哪条通道由消息决定**（[`HostMsg::lane`]）：发的一方只管 [`Outbox::send`]，不用自己挑控制流还是大块通道。
//!   大的消息（剪贴板、光标形状）在大块通道上排队，挡不住控制消息；
//! * **只关心最新值的状态不排队**，各占一个槽位、只留最新的一个，写任务先发它们，再按顺序发排着队的：
//!   Pong、显示器列表和新的 CursorUpdate（控制通道），旧端的光标可见性在大块通道。网络慢时不会越积越多、占满队列，
//!   Pong 也不会排在一串提示后面、把控制端测到的往返时延拉长；
//! * 其它消息按顺序排队，有上限。满了怎么办由发的一方决定（[`SendError::Full`]）：控制消息排满了说明对方早就不收了，
//!   会话结束；剪贴板丢掉这一次；光标形状下一轮再发那时最新的。
//!
//! 槽位里的状态不能越过还排着队、带着同一个状态的消息，不然新的会被旧的盖掉：
//!
//! * 显示器列表不越过 [`HostMsg::Hello`]（它带着开始时的显示器列表）；
//! * 旧端的光标可见性不越过带着形状的 [`HostMsg::Cursor`]（它也带着可见性）。新端的图像没有可见性，
//!   [`HostMsg::CursorUpdate`] 使用完整的带代际引用，不需要等大块通道。
//!
//! 反过来，这样的消息排进队列时，槽位里更早的那个值就作废了（它带着的更新）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use airhand_proto::SessionLane;
use airhand_proto::framing;
use airhand_proto::handshake::TAG_LEN;
use airhand_proto::session::{CursorState, HostMsg};
use bytes::Bytes;
use tokio::sync::Notify;

use crate::transport::Frames;

/// 控制通道最多排这么多条。控制消息都很小，排满了说明对方早就不收了
const CONTROL_MESSAGES: usize = 32;
/// 大块通道最多排这么多条、这么多字节（一个剪贴板最多 1 MiB，一个光标形状几十 KB）。
/// 单条帧仍需满足所属通道的协议上限。
const BULK_MESSAGES: usize = 32;
const BULK_BYTES: usize = 4 * 1024 * 1024;

/// 队列里的两条通道
const CONTROL: usize = 0;
const BULK: usize = 1;
/// 每条通道最多这么多个槽位
const SLOTS: usize = 4;

/// 只留最新值的槽位：在哪条通道上、第几个（写任务按这个先后发）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    lane: usize,
    index: usize,
}

const PONG: Slot = Slot {
    lane: CONTROL,
    index: 0,
};
const DISPLAYS: Slot = Slot {
    lane: CONTROL,
    index: 1,
};
const CURSOR_UPDATE: Slot = Slot {
    lane: CONTROL,
    index: 2,
};
const CURSOR_VISIBLE: Slot = Slot {
    lane: BULK,
    index: 0,
};

/// 一条消息在队列里怎么放
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// 只留最新的一个
    Latest(Slot),
    /// 按顺序排队。`before`：这个槽位里的状态它也带着，槽位要等它发出去之后才轮到（见模块说明）
    Queued { lane: usize, before: Option<Slot> },
}

impl Place {
    fn of(msg: &HostMsg) -> Self {
        let queued = |before| Place::Queued {
            lane: lane_index(msg.lane()),
            before,
        };
        match msg {
            HostMsg::Pong { .. } => Place::Latest(PONG),
            HostMsg::Displays { .. } => Place::Latest(DISPLAYS),
            HostMsg::Hello(_) => queued(Some(DISPLAYS)),
            HostMsg::CursorUpdate { .. } => Place::Latest(CURSOR_UPDATE),
            HostMsg::VideoConfig(_) => Place::Latest(Slot {
                lane: CONTROL,
                index: 3,
            }),
            HostMsg::CursorImage { .. } => queued(None),
            HostMsg::Cursor(CursorState { shape: None, .. }) => Place::Latest(CURSOR_VISIBLE),
            HostMsg::Cursor(CursorState { shape: Some(_), .. }) => queued(Some(CURSOR_VISIBLE)),
            HostMsg::AwaitingApproval
            | HostMsg::Notice(_)
            | HostMsg::Encoder(_)
            | HostMsg::Clipboard { .. }
            | HostMsg::Bye(_) => queued(None),
        }
    }

    fn lane(self) -> usize {
        match self {
            Place::Latest(slot) => slot.lane,
            Place::Queued { lane, .. } => lane,
        }
    }
}

fn lane_index(lane: SessionLane) -> usize {
    match lane {
        SessionLane::Bulk => BULK,
        // 画面、声音不是消息，不走这里
        SessionLane::Control | SessionLane::Video | SessionLane::Audio => CONTROL,
    }
}

/// 编码好的一条消息（带长度前缀，明文：写任务加密）。同一条消息发给几个会话时只编码一次（[`Outbox::encode`]）
#[derive(Debug, Clone)]
pub struct Encoded {
    bytes: Bytes,
    place: Place,
}

impl Encoded {
    /// 走哪条通道
    pub fn lane(&self) -> SessionLane {
        match self.place.lane() {
            BULK => SessionLane::Bulk,
            _ => SessionLane::Control,
        }
    }

    /// 帧：`u32 长度 | CBOR`
    pub fn bytes(&self) -> &Bytes {
        &self.bytes
    }
}

/// 为什么没排进去
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SendError {
    /// 这条通道排满了（网络跟不上，或者对方不收了）
    #[error("the session's send queue is full")]
    Full,
    /// 会话的写任务已经结束了
    #[error("the session's send queue is closed")]
    Closed,
    #[error("message exceeds its lane's frame budget")]
    TooLarge,
}

struct Item {
    bytes: Bytes,
    before: Option<usize>,
}

#[derive(Default)]
struct State {
    slots: [Option<Bytes>; SLOTS],
    /// 每个槽位前面还排着几条它不能越过的消息
    held: [usize; SLOTS],
    items: VecDeque<Item>,
    bytes: usize,
    /// 写任务不在了：再发都是 [`SendError::Closed`]
    closed: bool,
    /// 发送端都不在了：排着的发完就结束
    finished: bool,
}

impl State {
    fn push(&mut self, msg: Encoded, limit: &Limit) -> Result<(), SendError> {
        if self.closed {
            return Err(SendError::Closed);
        }
        match msg.place {
            Place::Latest(slot) => self.slots[slot.index] = Some(msg.bytes),
            Place::Queued { before, .. } => {
                let len = msg.bytes.len();
                if self.items.len() >= limit.messages
                    || (self.bytes + len > limit.bytes && !self.items.is_empty())
                {
                    return Err(SendError::Full);
                }
                let before = before.map(|slot| slot.index);
                if let Some(i) = before {
                    // 这条消息带着更新的值：槽位里的作废，之后的要排在它后面
                    self.slots[i] = None;
                    self.held[i] += 1;
                }
                self.bytes += len;
                self.items.push_back(Item {
                    bytes: msg.bytes,
                    before,
                });
            }
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<Bytes> {
        for i in 0..SLOTS {
            if self.held[i] == 0
                && let Some(b) = self.slots[i].take()
            {
                return Some(b);
            }
        }
        let item = self.items.pop_front()?;
        self.bytes -= item.bytes.len();
        if let Some(i) = item.before {
            self.held[i] -= 1;
        }
        Some(item.bytes)
    }
}

struct Limit {
    messages: usize,
    bytes: usize,
}

struct Lane {
    state: Mutex<State>,
    ready: Notify,
    limit: Limit,
}

impl Lane {
    fn new(messages: usize, bytes: usize) -> Self {
        Self {
            state: Mutex::default(),
            ready: Notify::new(),
            limit: Limit { messages, bytes },
        }
    }
}

struct Shared {
    lanes: [Lane; 2],
}

/// 最后一个 [`Outbox`] 丢掉时告诉写任务：排着的发完就结束
struct Alive(Arc<Shared>);

impl Drop for Alive {
    fn drop(&mut self) {
        for lane in &self.0.lanes {
            lane.state.lock().unwrap().finished = true;
            lane.ready.notify_one();
        }
    }
}

/// 会话的消息发送端（可以复制，例如交给编码线程）。见模块说明
#[derive(Clone)]
pub struct Outbox {
    shared: Arc<Shared>,
    _alive: Arc<Alive>,
}

/// 一条通道的接收端，交给写任务（[`crate::transport::write_lane`]）。丢掉之后发送端收到 [`SendError::Closed`]
pub struct LaneReceiver {
    shared: Arc<Shared>,
    lane: usize,
}

/// 一个会话的消息队列：发送端，控制通道、大块通道各自的接收端
pub fn channel() -> (Outbox, LaneReceiver, LaneReceiver) {
    let shared = Arc::new(Shared {
        lanes: [
            Lane::new(CONTROL_MESSAGES, usize::MAX),
            Lane::new(BULK_MESSAGES, BULK_BYTES),
        ],
    });
    let receiver = |lane| LaneReceiver {
        shared: shared.clone(),
        lane,
    };
    (
        Outbox {
            shared: shared.clone(),
            _alive: Arc::new(Alive(shared.clone())),
        },
        receiver(CONTROL),
        receiver(BULK),
    )
}

impl Outbox {
    /// 编码一条消息。同一条消息发给多个会话时只编码一次，每个会话 [`Outbox::send_encoded`]
    pub fn encode(msg: &HostMsg) -> Encoded {
        Encoded {
            // 写任务加密时在后面追加认证标签，这里先留好位置
            bytes: Bytes::from(framing::encode_with_spare(msg, TAG_LEN)),
            place: Place::of(msg),
        }
    }

    pub fn send(&self, msg: &HostMsg) -> Result<(), SendError> {
        self.send_encoded(Self::encode(msg))
    }

    /// 发送 [`Outbox::encode`] 编好的消息。只留最新值的状态（见模块说明）不会排满
    pub fn send_encoded(&self, msg: Encoded) -> Result<(), SendError> {
        if msg.bytes.len() - framing::PREFIX + TAG_LEN > msg.lane().max_frame_size() {
            return Err(SendError::TooLarge);
        }
        let lane = &self.shared.lanes[msg.place.lane()];
        lane.state.lock().unwrap().push(msg, &lane.limit)?;
        lane.ready.notify_one();
        Ok(())
    }
}

impl LaneReceiver {
    /// 下一帧要发的；发送端都不在了、排着的也发完了时为 `None`。可以放在 `select!` 里（取消不丢消息）
    pub async fn recv(&mut self) -> Option<Bytes> {
        let lane = &self.shared.lanes[self.lane];
        loop {
            let notified = lane.ready.notified();
            {
                let mut s = lane.state.lock().unwrap();
                if let Some(b) = s.pop() {
                    return Some(b);
                }
                if s.finished {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// 现在就能发的下一帧（没有就是 `None`）
    pub fn try_recv(&mut self) -> Option<Bytes> {
        self.shared.lanes[self.lane].state.lock().unwrap().pop()
    }
}

impl Frames for LaneReceiver {
    async fn next(&mut self) -> Option<Bytes> {
        self.recv().await
    }
}

impl Drop for LaneReceiver {
    fn drop(&mut self) {
        let mut s = self.shared.lanes[self.lane].state.lock().unwrap();
        s.closed = true;
        s.items.clear();
        s.slots = Default::default();
        s.held = Default::default();
        s.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::{
        ByeReason, CursorShape, CursorSlot, DisplayInfo, HostHello, Notice, Platform,
    };
    use std::time::Duration;

    fn decode(b: &Bytes) -> HostMsg {
        framing::decode_known(&b[framing::PREFIX..]).unwrap()
    }

    /// 现在排着的，按写任务发的先后
    fn drain(rx: &mut LaneReceiver) -> Vec<HostMsg> {
        std::iter::from_fn(|| rx.try_recv())
            .map(|b| decode(&b))
            .collect()
    }

    fn display(name: &str) -> DisplayInfo {
        DisplayInfo {
            index: 0,
            name: name.into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        }
    }

    fn displays(name: &str) -> HostMsg {
        HostMsg::Displays {
            displays: vec![display(name)],
            current: 0,
        }
    }

    fn hello(name: &str) -> HostMsg {
        HostMsg::Hello(HostHello {
            hostname: "PC".into(),
            os: "test".into(),
            version: "0".into(),
            displays: vec![display(name)],
            current_display: 0,
            codecs: vec![],
            platform: Platform::Linux,
            touch: false,
            actions: vec![],
        })
    }

    fn notice() -> HostMsg {
        HostMsg::Notice(Some(Notice::NoDisplays))
    }

    fn cursor(visible: bool, slot: Option<u8>) -> HostMsg {
        HostMsg::Cursor(CursorState {
            visible,
            shape: slot.map(|slot| CursorSlot {
                slot,
                image: Some(CursorShape {
                    width: 1,
                    height: 1,
                    hot_x: 0,
                    hot_y: 0,
                    rgba: vec![slot; 4],
                }),
            }),
        })
    }

    fn clipboard(len: usize) -> HostMsg {
        HostMsg::Clipboard {
            text: "x".repeat(len),
        }
    }

    /// 显示器列表里第一个显示器的名字（Hello 和 Displays 都带着）
    fn shown(m: &HostMsg) -> &str {
        match m {
            HostMsg::Hello(h) => &h.displays[0].name,
            HostMsg::Displays { displays, .. } => &displays[0].name,
            other => panic!("{other:?}"),
        }
    }

    /// 每条消息排进它自己的通道：槽位说的通道和协议说的是同一条
    #[test]
    fn messages_go_to_their_lane() {
        for (msg, lane) in [
            (HostMsg::Pong { t: 1 }, SessionLane::Control),
            (displays("a"), SessionLane::Control),
            (hello("a"), SessionLane::Control),
            (notice(), SessionLane::Control),
            (HostMsg::AwaitingApproval, SessionLane::Control),
            (HostMsg::Bye(ByeReason::Kicked), SessionLane::Control),
            (cursor(true, None), SessionLane::Bulk),
            (cursor(true, Some(1)), SessionLane::Bulk),
            (clipboard(3), SessionLane::Bulk),
        ] {
            assert_eq!(msg.lane(), lane, "{msg:?}");
            assert_eq!(Outbox::encode(&msg).lane(), lane, "{msg:?}");
            let (tx, mut control, mut bulk) = channel();
            tx.send(&msg).unwrap();
            let (here, other) = match lane {
                SessionLane::Bulk => (&mut bulk, &mut control),
                _ => (&mut control, &mut bulk),
            };
            assert_eq!(drain(here).len(), 1, "{msg:?}");
            assert!(drain(other).is_empty(), "{msg:?}");
        }
    }

    /// 写任务先发 Pong、显示器列表（各只留最新的），再按顺序发排着的
    #[test]
    fn states_go_first_and_keep_only_the_latest() {
        let (tx, mut control, _bulk) = channel();
        tx.send(&notice()).unwrap();
        tx.send(&HostMsg::Pong { t: 1 }).unwrap();
        tx.send(&displays("old")).unwrap();
        tx.send(&HostMsg::Bye(ByeReason::Kicked)).unwrap();
        tx.send(&HostMsg::Pong { t: 2 }).unwrap();
        tx.send(&displays("new")).unwrap();
        let out = drain(&mut control);
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(matches!(out[0], HostMsg::Pong { t: 2 }));
        assert_eq!(shown(&out[1]), "new");
        assert!(matches!(out[2], HostMsg::Notice(_)));
        assert!(matches!(out[3], HostMsg::Bye(ByeReason::Kicked)));
        // 槽位不占队列的位置：排满了它们照样放得进去
        for _ in 0..CONTROL_MESSAGES {
            tx.send(&notice()).unwrap();
        }
        assert_eq!(tx.send(&notice()), Err(SendError::Full));
        for t in 0..1000 {
            tx.send(&HostMsg::Pong { t }).unwrap();
            tx.send(&displays("x")).unwrap();
        }
        assert_eq!(drain(&mut control).len(), CONTROL_MESSAGES + 2);
    }

    /// 显示器列表不越过还排着的 Hello：不然控制端先收到新的列表，再被 Hello 里旧的盖掉
    #[test]
    fn displays_never_overtake_hello() {
        let (tx, mut control, _bulk) = channel();
        tx.send(&notice()).unwrap();
        tx.send(&hello("at start")).unwrap();
        tx.send(&displays("changed")).unwrap();
        tx.send(&HostMsg::Pong { t: 7 }).unwrap();
        let out = drain(&mut control);
        // Pong 不受影响，照样最先
        assert!(matches!(out[0], HostMsg::Pong { t: 7 }));
        assert!(matches!(out[1], HostMsg::Notice(_)));
        assert_eq!(shown(&out[2]), "at start");
        assert_eq!(shown(&out[3]), "changed");
        assert_eq!(out.len(), 4);
        // Hello 发出去之后，显示器列表又能排到前面
        tx.send(&notice()).unwrap();
        tx.send(&displays("again")).unwrap();
        let out = drain(&mut control);
        assert_eq!(shown(&out[0]), "again");
        // Hello 带着更新的列表：槽位里更早的作废
        tx.send(&displays("stale")).unwrap();
        tx.send(&hello("fresh")).unwrap();
        let out = drain(&mut control);
        assert_eq!(out.len(), 1);
        assert_eq!(shown(&out[0]), "fresh");
    }

    #[test]
    fn fast_cursor_state_bypasses_a_full_bulk_lane() {
        let (tx, mut control, _bulk) = channel();
        while tx.send(&clipboard(1 << 20)).is_ok() {}
        let reference = airhand_proto::session::CursorRef {
            slot: 2,
            generation: 7,
        };
        tx.send(&HostMsg::CursorUpdate {
            visible: true,
            shape: Some(reference),
        })
        .unwrap();
        tx.send(&HostMsg::CursorUpdate {
            visible: false,
            shape: Some(reference),
        })
        .unwrap();
        let messages = drain(&mut control);
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(messages[0], HostMsg::CursorUpdate { visible: false, shape: Some(r) } if r == reference)
        );
    }

    /// 光标的可见性只留最新的，排在剪贴板前面，但不越过带着形状的光标消息
    #[test]
    fn cursor_visibility_follows_the_shapes() {
        let visible = |m: &HostMsg| match m {
            HostMsg::Cursor(c) => (c.visible, c.shape.as_ref().map(|s| s.slot)),
            other => panic!("{other:?}"),
        };
        let (tx, _control, mut bulk) = channel();
        tx.send(&clipboard(10)).unwrap();
        for i in 0..100 {
            tx.send(&cursor(i % 2 == 0, None)).unwrap();
        }
        let out = drain(&mut bulk);
        assert_eq!(out.len(), 2);
        assert_eq!(visible(&out[0]), (false, None));
        assert!(matches!(out[1], HostMsg::Clipboard { .. }));

        // 形状（带着"可见"）排在剪贴板后面，之后光标隐藏了：隐藏要在形状之后到，不然又被形状改回可见
        tx.send(&clipboard(10)).unwrap();
        tx.send(&cursor(true, Some(3))).unwrap();
        tx.send(&cursor(true, Some(4))).unwrap();
        tx.send(&cursor(false, None)).unwrap();
        let out = drain(&mut bulk);
        assert!(matches!(out[0], HostMsg::Clipboard { .. }));
        assert_eq!(visible(&out[1]), (true, Some(3)));
        assert_eq!(visible(&out[2]), (true, Some(4)));
        assert_eq!(visible(&out[3]), (false, None));
        assert_eq!(out.len(), 4);

        // 形状带着更新的可见性：槽位里更早的作废
        tx.send(&cursor(false, None)).unwrap();
        tx.send(&cursor(true, Some(5))).unwrap();
        let out = drain(&mut bulk);
        assert_eq!(out.len(), 1);
        assert_eq!(visible(&out[0]), (true, Some(5)));
    }

    /// 大块通道按字节数限量：控制通道不受影响，单条消息也不能突破协议上限。
    #[test]
    fn bulk_is_bounded_by_bytes_and_does_not_block_control() {
        let (tx, mut control, mut bulk) = channel();
        let mut queued = 0;
        while tx.send(&clipboard(1 << 20)).is_ok() {
            queued += 1;
            assert!(queued < BULK_MESSAGES, "the byte limit must kick in first");
        }
        assert!((2..=4).contains(&queued), "{queued}");
        assert_eq!(tx.send(&clipboard(1 << 20)), Err(SendError::Full));
        // 小的（光标形状）字节数还没到上限，照样放得进；可见性是槽位，也放得进；控制消息在另一条通道
        tx.send(&cursor(true, Some(1))).unwrap();
        tx.send(&cursor(false, None)).unwrap();
        tx.send(&notice()).unwrap();
        assert_eq!(drain(&mut control).len(), 1);
        assert_eq!(drain(&mut bulk).len(), queued + 2);
        // 条数也有上限
        for _ in 0..BULK_MESSAGES {
            let _ = tx.send(&cursor(true, Some(1)));
        }
        assert_eq!(tx.send(&cursor(true, Some(1))), Err(SendError::Full));
        drain(&mut bulk);
        let huge = Outbox::encode(&clipboard(BULK_BYTES + 1));
        assert_eq!(tx.send_encoded(huge), Err(SendError::TooLarge));
        assert!(bulk.try_recv().is_none());
        let mut h = hello("large");
        if let HostMsg::Hello(ref mut h) = h {
            h.hostname = "x".repeat(airhand_proto::MAX_CONTROL_MSG);
        }
        assert_eq!(tx.send(&h), Err(SendError::TooLarge));
    }

    /// 发送端都丢掉之后，写任务把排着的发完再结束（Bye 就是这样发出去的）；写任务不在了，发送端马上知道
    #[tokio::test]
    async fn closing() {
        let (tx, mut control, mut bulk) = channel();
        let tx2 = tx.clone();
        tx.send(&HostMsg::Pong { t: 1 }).unwrap();
        tx.send(&HostMsg::Bye(ByeReason::Kicked)).unwrap();
        drop(tx);
        // 还有一个发送端：不结束
        assert!(matches!(
            decode(&control.recv().await.unwrap()),
            HostMsg::Pong { t: 1 }
        ));
        assert!(matches!(
            decode(&control.recv().await.unwrap()),
            HostMsg::Bye(_)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), control.recv())
                .await
                .is_err()
        );
        tx2.send(&notice()).unwrap();
        drop(tx2);
        assert!(matches!(
            decode(&control.recv().await.unwrap()),
            HostMsg::Notice(_)
        ));
        assert!(control.recv().await.is_none());
        assert!(bulk.recv().await.is_none());

        let (tx, control, mut bulk) = channel();
        tx.send(&notice()).unwrap();
        drop(control);
        assert_eq!(tx.send(&notice()), Err(SendError::Closed));
        assert_eq!(tx.send(&HostMsg::Pong { t: 1 }), Err(SendError::Closed));
        // 另一条通道的写任务还在
        tx.send(&clipboard(1)).unwrap();
        assert!(bulk.try_recv().is_some());
    }

    /// 等着的写任务被新消息叫醒
    #[tokio::test]
    async fn the_writer_is_woken() {
        let (tx, mut control, _bulk) = channel();
        let writer = tokio::spawn(async move { control.recv().await.map(|b| decode(&b)) });
        tokio::time::sleep(Duration::from_millis(10)).await;
        tx.send(&HostMsg::Pong { t: 5 }).unwrap();
        assert!(matches!(
            writer.await.unwrap(),
            Some(HostMsg::Pong { t: 5 })
        ));
    }
}
