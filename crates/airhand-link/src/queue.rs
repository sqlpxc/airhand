//! 消息通道的发送队列，两端共用（被控端的 [`crate::host::Outbox`]、控制端的命令队列）：
//!
//! * 只关心最新值的状态放在**槽位**里：每个槽位只留最新的一个，写任务先发它们（按槽位的先后）。网络慢时不会越积越多，
//!   Pong、确认也不会排在一串别的消息后面、把测到的往返时延拉长；
//! * 其它消息按顺序**排队**，条数、字节数有上限，满了由发的一方决定怎么办（丢掉这一条、丢掉排着的输入、结束会话……）；
//! * 一条排队的消息可以挡住一个槽位（[`Item::before`]）：它带着这个槽位的状态，槽位里之后的值要等它发出去才轮到
//!   （例如被控端的 Hello 带着显示器列表，之后的显示器列表不能越过它，不然新的会被旧的盖掉）。
//!
//! 收的一端是会话的写任务（只有一个）。它不在了，再发都失败（[`Mail::closed`]）；发的一端都不在了，排着的发完就结束
//! （[`Mail::finished`]）。

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use tokio::sync::Notify;

/// 排队的消息最多这么多条、这么多字节（槽位不算）
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limit {
    pub messages: usize,
    pub bytes: usize,
}

/// 排着队的一条消息（带长度前缀的明文帧，写任务加密）。`meta` 是发的一方自己要记的东西（例如输入排进来的时刻）
pub(crate) struct Item<M> {
    pub bytes: Bytes,
    /// 这条消息带着这个槽位的状态：槽位要等它发出去之后才轮到（见模块说明）
    pub before: Option<usize>,
    pub meta: M,
}

pub(crate) struct Mail<M> {
    slots: Vec<Option<Bytes>>,
    /// 每个槽位前面还排着几条它不能越过的消息
    held: Vec<usize>,
    items: VecDeque<Item<M>>,
    bytes: usize,
    /// 收的一端不在了，或者队列失败了：再发都失败，排着的也不发了
    pub closed: bool,
    /// 发的一端都不在了：排着的发完就结束
    pub finished: bool,
}

impl<M> Mail<M> {
    fn new(slots: usize) -> Self {
        Self {
            slots: vec![None; slots],
            held: vec![0; slots],
            items: VecDeque::new(),
            bytes: 0,
            closed: false,
            finished: false,
        }
    }

    /// 槽位里换成最新的值
    pub fn set(&mut self, slot: usize, bytes: Bytes) {
        self.slots[slot] = Some(bytes);
    }

    /// 再排一条 `extra` 字节的消息就超出了上限。一条也没排着的时候总放得进（单条消息的上限由协议管）
    pub fn full(&self, limit: Limit, extra: usize) -> bool {
        self.items.len() >= limit.messages
            || (self.bytes + extra > limit.bytes && !self.items.is_empty())
    }

    pub fn push_back(&mut self, item: Item<M>) {
        if let Some(i) = item.before {
            // 这条消息带着更新的值：槽位里的作废，之后的要排在它后面
            self.slots[i] = None;
            self.held[i] += 1;
        }
        self.bytes += item.bytes.len();
        self.items.push_back(item);
    }

    /// 排到最前面（不挡槽位）
    pub fn push_front(&mut self, bytes: Bytes, meta: M) {
        self.bytes += bytes.len();
        self.items.push_front(Item {
            bytes,
            before: None,
            meta,
        });
    }

    pub fn back(&self) -> Option<&Item<M>> {
        self.items.back()
    }

    pub fn pop_back(&mut self) -> Option<Item<M>> {
        let item = self.items.pop_back()?;
        self.taken(&item);
        Some(item)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item<M>> {
        self.items.iter()
    }

    #[cfg(test)]
    pub fn items_mut(&mut self) -> impl Iterator<Item = &mut Item<M>> {
        self.items.iter_mut()
    }

    /// 只留下 `keep` 的；返回去掉了几条
    pub fn retain(&mut self, mut keep: impl FnMut(&Item<M>) -> bool) -> usize {
        let before = self.items.len();
        let mut removed = Vec::new();
        self.items.retain(|i| {
            let k = keep(i);
            if !k {
                removed.push((i.bytes.len(), i.before));
            }
            k
        });
        for (len, slot) in removed {
            self.bytes -= len;
            if let Some(s) = slot {
                self.held[s] -= 1;
            }
        }
        before - self.items.len()
    }

    /// 写任务要发的下一帧：先是没被挡住的槽位，再是排着的
    pub fn pop(&mut self) -> Option<Bytes> {
        for i in 0..self.slots.len() {
            if self.held[i] == 0
                && let Some(b) = self.slots[i].take()
            {
                return Some(b);
            }
        }
        let item = self.items.pop_front()?;
        self.taken(&item);
        Some(item.bytes)
    }

    fn taken(&mut self, item: &Item<M>) {
        self.bytes -= item.bytes.len();
        if let Some(i) = item.before {
            self.held[i] -= 1;
        }
    }

    /// 关掉：再发都失败，排着的都不要了
    pub fn close(&mut self) {
        self.closed = true;
        self.items.clear();
        self.slots.iter_mut().for_each(|s| *s = None);
        self.held.iter_mut().for_each(|h| *h = 0);
        self.bytes = 0;
    }
}

/// 一条消息通道的队列（见模块说明）
pub(crate) struct Mailbox<M> {
    state: Mutex<Mail<M>>,
    ready: Notify,
}

impl<M> Mailbox<M> {
    pub fn new(slots: usize) -> Self {
        Self {
            state: Mutex::new(Mail::new(slots)),
            ready: Notify::new(),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, Mail<M>> {
        self.state.lock().unwrap()
    }

    /// 改了队列之后叫醒写任务
    pub fn wake(&self) {
        self.ready.notify_one();
    }

    /// 写任务要发的下一帧；关掉了、或者发的一端都不在了并且排着的也发完了时为 `None`。
    /// `prepare` 在每次取之前（拿着锁）调用：例如丢掉排得太久的输入。可以放在 `select!` 里（取消不丢消息）
    pub async fn recv(&self, mut prepare: impl FnMut(&mut Mail<M>)) -> Option<Bytes> {
        loop {
            let notified = self.ready.notified();
            {
                let mut m = self.lock();
                if m.closed {
                    return None;
                }
                prepare(&mut m);
                if let Some(b) = m.pop() {
                    return Some(b);
                }
                if m.finished {
                    return None;
                }
            }
            notified.await;
        }
    }

    /// 现在就能发的下一帧（没有就是 `None`）
    pub fn try_recv(&self) -> Option<Bytes> {
        self.lock().pop()
    }

    /// 发的一端都不在了：排着的发完就结束
    pub fn finish(&self) {
        self.lock().finished = true;
        self.wake();
    }

    /// 收的一端不在了（或者队列失败了）：再发都失败
    pub fn close(&self) {
        self.lock().close();
        self.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(b: &'static [u8], before: Option<usize>) -> Item<()> {
        Item {
            bytes: Bytes::from_static(b),
            before,
            meta: (),
        }
    }

    #[test]
    fn slots_go_first_unless_held() {
        let q = Mailbox::<()>::new(2);
        let mut m = q.lock();
        m.push_back(item(b"a", None));
        m.set(1, Bytes::from_static(b"s1"));
        m.push_back(item(b"hello", Some(0)));
        m.set(0, Bytes::from_static(b"s0"));
        // 0 号槽位被 hello 挡着，1 号不受影响
        assert_eq!(m.pop().unwrap(), "s1");
        assert_eq!(m.pop().unwrap(), "a");
        assert_eq!(m.pop().unwrap(), "hello");
        assert_eq!(m.pop().unwrap(), "s0");
        assert!(m.pop().is_none());
        assert_eq!(m.bytes, 0);
    }

    #[test]
    fn limits_and_retain() {
        let q = Mailbox::<u8>::new(1);
        let mut m = q.lock();
        let limit = Limit {
            messages: 3,
            bytes: 4,
        };
        assert!(
            !m.full(limit, 100),
            "an empty queue takes one oversized message"
        );
        for (i, b) in [&b"ab"[..], b"cd"].into_iter().enumerate() {
            m.push_back(Item {
                bytes: Bytes::from(b),
                before: (i == 1).then_some(0),
                meta: i as u8,
            });
        }
        assert!(m.full(limit, 1));
        assert_eq!(m.retain(|i| i.meta == 0), 1);
        assert_eq!((m.bytes, m.held[0]), (2, 0));
        m.push_front(Bytes::from_static(b"z"), 9);
        assert_eq!(m.items().map(|i| i.meta).collect::<Vec<_>>(), [9, 0]);
        assert_eq!(m.pop_back().unwrap().meta, 0);
        m.close();
        assert!(m.closed && m.pop().is_none());
    }

    #[tokio::test]
    async fn the_writer_drains_then_ends() {
        let q = std::sync::Arc::new(Mailbox::<()>::new(0));
        let w = {
            let q = q.clone();
            tokio::spawn(async move {
                let mut got = Vec::new();
                while let Some(b) = q.recv(|_| {}).await {
                    got.push(b);
                }
                got
            })
        };
        tokio::task::yield_now().await;
        q.lock().push_back(item(b"x", None));
        q.wake();
        q.lock().push_back(item(b"y", None));
        q.finish();
        assert_eq!(w.await.unwrap(), ["x", "y"]);
    }
}
