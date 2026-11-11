//! 控制端发给被控端的命令队列（有界），建在 [`crate::queue`] 上（和被控端的 `Outbox` 一样）。
//!
//! * 只关心最新值的画质设置不排队：它占一个槽位，只留最新的一个，写任务**先发它**，再按顺序发排着队的
//!   输入和其它命令。Ping、画面确认不在这里：它们是控制数据报（见 `super::session::Feedback`），不排在输入后面；
//! * 连续的绝对移动只留最后一个，连续的相对移动加起来；移动只和紧挨着的移动合并，
//!   不会越过点击、按键（松开按键不会被合并掉）；
//! * 排着的输入太旧了（网络卡了一阵子，[`INPUT_MAX_AGE`]），或者队列满了：丢掉排着的输入、
//!   补发一个"释放所有按键"（丢掉的里面可能有松开按键），会话照常继续——不在网络恢复时把一串过时的远程操作补发出去，
//!   也不因为网络卡了一下就断开重连。真正失联由会话的超时处理。
//!   丢掉输入之后还是放不下（例如排满了不能丢的命令）才算会话失败（[`Sender::failed`]）。
//!
//! 这里只管得了还在队列里的输入：断网时往 QUIC 里写不会阻塞，写出去的输入进了 QUIC 的发送缓冲，
//! 网络恢复后照样发出去。所以输入还带着排进队列的时刻（[`ControllerMsg::Input`] 的 `t`），
//! 被控端丢掉晚到太久的（见 `airhand_proto::session` 的"过时的输入"）。
use crate::Problem;
use crate::clock::now_us;
use crate::queue::{Item, Limit, Mail, Mailbox};
use crate::transport::Frames;
use airhand_proto::{
    framing,
    session::{ControllerMsg, INPUT_MAX_AGE, InputEvent},
};
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tracing::warn;

const LIMIT: Limit = Limit {
    messages: 128,
    bytes: 2 * 1024 * 1024,
};

/// 排队的东西是什么
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// 其它输入（点击、按键、文字……）
    Input,
    /// 只有绝对移动：只留最后一个
    Move,
    /// 只有相对移动：加起来
    Delta,
    /// 不是输入的命令（切换显示器、请求关键帧、剪贴板……）：不会过时，不丢
    Command,
}

impl Kind {
    fn is_input(self) -> bool {
        self != Kind::Command
    }
}

/// 排着的一条命令的来历（合并、丢掉过时的输入时用）
struct Meta {
    kind: Kind,
    /// 相对移动的总量（合并时用）
    delta: (i32, i32),
    at: Instant,
    /// 输入里的 `t`（合并相对移动时用最早的那一个）
    t: u64,
}

/// 只留最新值的槽位：画质设置
const SETTINGS: usize = 0;
const SLOTS: usize = 1;

struct Shared {
    mail: Mailbox<Meta>,
    /// 排满了不能丢的命令：会话失败
    failed: AtomicBool,
}

#[derive(Clone)]
pub(super) struct Sender(Arc<Shared>);
pub(super) struct Receiver(Arc<Shared>);

pub(super) fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        mail: Mailbox::new(SLOTS),
        failed: AtomicBool::new(false),
    });
    (Sender(shared.clone()), Receiver(shared))
}

/// 编码（明文，带长度前缀）；写任务加密时在后面追加认证标签，这里先留好位置
fn encode(msg: &ControllerMsg) -> Result<Bytes, Problem> {
    let bytes = framing::encode_with_spare(msg, airhand_proto::handshake::SEAL_OVERHEAD);
    if msg.lane() != airhand_proto::SessionLane::Control
        || bytes.len() - framing::PREFIX + airhand_proto::handshake::SEAL_OVERHEAD
            > airhand_proto::MAX_CONTROL_MSG
    {
        return Err(Problem::failed(
            "controller message exceeds its lane budget",
        ));
    }
    Ok(Bytes::from(bytes))
}

/// 一批输入，`t` 是现在（界面的输入都从这里进来）
pub(super) fn input(events: Vec<InputEvent>) -> ControllerMsg {
    ControllerMsg::Input {
        t: now_us(),
        events,
    }
}

fn release_msg(t: u64) -> ControllerMsg {
    ControllerMsg::Input {
        t,
        events: vec![InputEvent::ReleaseAll],
    }
}

fn delta_msg(t: u64, (dx, dy): (i32, i32)) -> ControllerMsg {
    ControllerMsg::Input {
        t,
        events: vec![InputEvent::MouseDelta { dx, dy }],
    }
}

/// 丢掉排着的输入，补一个"释放所有按键"排在最前面。返回丢掉了几条（没有可丢的就什么也不做）
fn drop_input(m: &mut Mail<Meta>) -> usize {
    let dropped = m.retain(|i| !i.meta.kind.is_input());
    if dropped > 0 {
        // 现在的时刻：被控端不会把它当成过时的输入
        let t = now_us();
        let release = encode(&release_msg(t)).expect("ReleaseAll always encodes");
        m.push_front(
            release,
            Meta {
                kind: Kind::Input,
                delta: (0, 0),
                at: Instant::now(),
                t,
            },
        );
    }
    dropped
}

// CBOR 的固定开销按上限估计，给消息外层和 AEAD 标签留空间。
const INPUT_BUDGET: usize = airhand_proto::MAX_CONTROL_MSG - 128;
fn input_cost(event: &InputEvent) -> usize {
    64 + match event {
        InputEvent::Text { text } => text.len(),
        InputEvent::Key { code, .. } => code.len(),
        _ => 0,
    }
}

impl Sender {
    pub fn send(&self, msg: ControllerMsg) -> Result<(), Problem> {
        let ControllerMsg::Input { t, mut events } = msg else {
            return self.send_one(msg);
        };
        InputEvent::coalesce(&mut events);
        if events.is_empty() {
            return Ok(());
        }
        if events.iter().map(input_cost).sum::<usize>() <= INPUT_BUDGET {
            return self.send_one(ControllerMsg::Input { t, events });
        }
        let mut batch = Vec::new();
        let mut size = 0;
        let mut push = |event: InputEvent| -> Result<(), Problem> {
            let cost = input_cost(&event);
            if cost > INPUT_BUDGET {
                return Err(Problem::failed(
                    "input event exceeds the control lane budget",
                ));
            }
            if size + cost > INPUT_BUDGET {
                self.send_one(ControllerMsg::Input {
                    t,
                    events: std::mem::take(&mut batch),
                })?;
                size = 0;
            }
            size += cost;
            batch.push(event);
            Ok(())
        };
        for event in events {
            match event {
                InputEvent::Text { text } if text.len() + 64 > INPUT_BUDGET => {
                    let mut rest = text.as_str();
                    while !rest.is_empty() {
                        let mut end = rest.len().min(INPUT_BUDGET - 64);
                        while !rest.is_char_boundary(end) {
                            end -= 1;
                        }
                        push(InputEvent::Text {
                            text: rest[..end].to_owned(),
                        })?;
                        rest = &rest[end..];
                    }
                }
                event => push(event)?,
            }
        }
        if !batch.is_empty() {
            self.send_one(ControllerMsg::Input { t, events: batch })?;
        }
        Ok(())
    }

    /// 队列失败了（排满了不能丢的命令）
    pub fn failed(&self) -> bool {
        self.0.failed.load(Ordering::Relaxed)
    }

    fn send_one(&self, msg: ControllerMsg) -> Result<(), Problem> {
        let slot = matches!(msg, ControllerMsg::Settings(_)).then_some(SETTINGS);
        let (kind, delta, t) = match &msg {
            ControllerMsg::Input { t, events } => match events[..] {
                [InputEvent::MouseMove { .. }] => (Kind::Move, (0, 0), *t),
                [InputEvent::MouseDelta { dx, dy }] => (Kind::Delta, (dx, dy), *t),
                _ => (Kind::Input, (0, 0), *t),
            },
            _ => (Kind::Command, (0, 0), 0),
        };
        let mut bytes = encode(&msg)?;
        let mail = &self.0.mail;
        let mut m = mail.lock();
        if m.closed {
            return Err(Problem::SessionEnded);
        }
        if let Some(slot) = slot {
            m.set(slot, bytes);
            drop(m);
            mail.wake();
            return Ok(());
        }
        let mut meta = Meta {
            kind,
            delta,
            at: Instant::now(),
            t,
        };
        // 移动只和紧挨着的移动合并（队尾只会是输入或命令，确认这类状态不在队列里）
        if matches!(kind, Kind::Move | Kind::Delta) && m.back().is_some_and(|i| i.meta.kind == kind)
        {
            let old = m.pop_back().expect("checked above").meta;
            if kind == Kind::Delta {
                meta.delta = (
                    old.delta.0.saturating_add(delta.0),
                    old.delta.1.saturating_add(delta.1),
                );
                // 合并之后的移动从最早的那一部分算起，过时了一起丢（这里和被控端都是）
                meta.at = old.at;
                meta.t = old.t;
                bytes = encode(&delta_msg(meta.t, meta.delta))?;
            }
        }
        if m.full(LIMIT, bytes.len()) {
            // 网络卡住了：排着的输入已经没有意义，丢掉它们再放这一条
            let dropped = drop_input(&mut m);
            warn!(
                dropped,
                "controller command queue full, dropped the queued input"
            );
            if m.full(LIMIT, bytes.len()) {
                self.0.failed.store(true, Ordering::Relaxed);
                m.close();
                drop(m);
                mail.wake();
                return Err(Problem::failed("controller command queue overflow"));
            }
        }
        m.push_back(Item {
            bytes,
            before: None,
            meta,
        });
        drop(m);
        mail.wake();
        Ok(())
    }
}

impl Receiver {
    /// 写任务要发的下一帧；队列失败了、会话结束了时为 `None`
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.0
            .mail
            .recv(|m| {
                // 最早的输入已经过时：排着的输入全都丢掉（后面的也是在网络卡住时攒下的）
                if m.items()
                    .find(|i| i.meta.kind.is_input())
                    .is_some_and(|i| i.meta.at.elapsed() > INPUT_MAX_AGE)
                {
                    let dropped = drop_input(m);
                    warn!(
                        dropped,
                        "dropped stale input queued for more than {INPUT_MAX_AGE:?}"
                    );
                }
            })
            .await
    }
}

impl Frames for Receiver {
    async fn next(&mut self) -> Option<Bytes> {
        self.recv().await
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.mail.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::{MouseButton, QualitySettings};
    use std::time::Duration;

    fn decode(b: &Bytes) -> ControllerMsg {
        framing::decode_known(&b[4..]).unwrap()
    }

    fn queued(tx: &Sender) -> Vec<ControllerMsg> {
        tx.0.mail.lock().items().map(|i| decode(&i.bytes)).collect()
    }

    /// 输入里的事件（不是输入时为空）
    fn events(m: &ControllerMsg) -> &[InputEvent] {
        match m {
            ControllerMsg::Input { events, .. } => events,
            _ => &[],
        }
    }

    fn is_release(m: &ControllerMsg) -> bool {
        events(m) == [InputEvent::ReleaseAll]
    }

    fn settings(fps: u8) -> ControllerMsg {
        ControllerMsg::Settings(QualitySettings {
            fps,
            ..QualitySettings::default()
        })
    }

    const KEYFRAME: ControllerMsg = ControllerMsg::RequestKeyframe { after: 0 };

    #[test]
    fn large_text_batches_keep_order_and_fit_the_control_lane() {
        let (tx, _rx) = channel();
        let text = "😀汉字".repeat(20_000);
        tx.send(ControllerMsg::Input {
            t: 42,
            events: vec![
                InputEvent::Key {
                    code: "ShiftLeft".into(),
                    down: true,
                },
                InputEvent::Text { text: text.clone() },
                InputEvent::ReleaseAll,
            ],
        })
        .unwrap();
        let messages = queued(&tx);
        assert!(messages.len() > 1);
        let mut joined = String::new();
        let mut nontext = Vec::new();
        for m in messages {
            let wire = encode(&m).unwrap();
            assert!(
                wire.len() - 4 + airhand_proto::handshake::SEAL_OVERHEAD
                    <= airhand_proto::MAX_CONTROL_MSG
            );
            let ControllerMsg::Input { t, events } = m else {
                panic!()
            };
            assert_eq!(t, 42);
            for e in events {
                match e {
                    InputEvent::Text { text } => joined.push_str(&text),
                    e => nontext.push(e),
                }
            }
        }
        assert_eq!(joined, text);
        assert_eq!(
            nontext,
            [
                InputEvent::Key {
                    code: "ShiftLeft".into(),
                    down: true
                },
                InputEvent::ReleaseAll
            ]
        );
    }

    #[test]
    fn movement_coalesces_without_crossing_key_release() {
        let (tx, _rx) = channel();
        for _ in 0..1000 {
            tx.send(input(vec![InputEvent::MouseMove { x: 0.5, y: 0.5 }]))
                .unwrap();
        }
        tx.send(input(vec![InputEvent::ReleaseAll])).unwrap();
        tx.send(input(vec![InputEvent::MouseMove { x: 1.0, y: 1.0 }]))
            .unwrap();
        assert_eq!(queued(&tx).len(), 3);
    }

    /// 画质设置夹在鼠标移动中间：移动照样合并，设置只留最新的一个
    #[test]
    fn settings_do_not_break_movement_coalescing() {
        let (tx, _rx) = channel();
        for fps in 0..50 {
            tx.send(input(vec![InputEvent::MouseMove { x: 0.5, y: 0.5 }]))
                .unwrap();
            tx.send(settings(fps)).unwrap();
            tx.send(input(vec![InputEvent::MouseDelta { dx: 1, dy: 0 }]))
                .unwrap();
        }
        // 绝对移动和相对移动交替，各不合并；但设置不在队列里
        assert_eq!(queued(&tx).len(), 100);
        let (tx, _rx) = channel();
        for fps in 0..50 {
            tx.send(input(vec![InputEvent::MouseMove { x: 0.5, y: 0.5 }]))
                .unwrap();
            tx.send(settings(fps)).unwrap();
        }
        assert_eq!(queued(&tx).len(), 1);
    }

    /// 写任务先发画质设置（只留最新的），再按顺序发排着的输入和命令
    #[tokio::test]
    async fn settings_go_first_and_keep_only_the_latest() {
        let (tx, mut rx) = channel();
        let click = InputEvent::MouseButton {
            button: MouseButton::Left,
            down: true,
        };
        tx.send(input(vec![click.clone()])).unwrap();
        tx.send(KEYFRAME).unwrap();
        tx.send(settings(30)).unwrap();
        tx.send(settings(60)).unwrap();
        let mut out = Vec::new();
        for _ in 0..3 {
            out.push(decode(&rx.recv().await.unwrap()));
        }
        assert!(matches!(out[0], ControllerMsg::Settings(s) if s.fps == 60));
        assert_eq!(events(&out[1]), [click]);
        assert!(matches!(out[2], ControllerMsg::RequestKeyframe { .. }));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }

    #[test]
    fn relative_movement_adds_up() {
        let (tx, _rx) = channel();
        for _ in 0..500 {
            tx.send(input(vec![
                InputEvent::MouseDelta { dx: 1, dy: -2 },
                InputEvent::MouseDelta { dx: 1, dy: 0 },
            ]))
            .unwrap();
        }
        let up = InputEvent::MouseButton {
            button: MouseButton::Right,
            down: false,
        };
        tx.send(input(vec![up.clone()])).unwrap();
        tx.send(input(vec![InputEvent::MouseDelta { dx: 5, dy: 5 }]))
            .unwrap();
        let q = queued(&tx);
        assert_eq!(q.len(), 3);
        assert_eq!(
            events(&q[0]),
            [InputEvent::MouseDelta {
                dx: 1000,
                dy: -1000
            }]
        );
        assert_eq!(events(&q[1]), [up]);
        assert_eq!(events(&q[2]), [InputEvent::MouseDelta { dx: 5, dy: 5 }]);
        // 绝对移动和相对移动不互相合并
        tx.send(input(vec![InputEvent::MouseMove { x: 0.1, y: 0.1 }]))
            .unwrap();
        tx.send(input(vec![InputEvent::MouseDelta { dx: 1, dy: 1 }]))
            .unwrap();
        assert_eq!(queued(&tx).len(), 5);
    }

    /// 输入过时了：丢掉排着的输入、补一个"释放所有按键"，其它命令照发，会话继续
    #[tokio::test]
    async fn stale_input_is_dropped_and_keys_released() {
        let (tx, mut rx) = channel();
        let key = |down| {
            input(vec![InputEvent::Key {
                code: "KeyA".into(),
                down,
            }])
        };
        tx.send(key(true)).unwrap();
        tx.send(ControllerMsg::SelectDisplay { index: 1 }).unwrap();
        tx.send(key(false)).unwrap();
        for i in tx.0.mail.lock().items_mut() {
            i.meta.at = Instant::now() - INPUT_MAX_AGE - Duration::from_millis(1);
        }
        assert!(is_release(&decode(&rx.recv().await.unwrap())));
        assert!(matches!(
            decode(&rx.recv().await.unwrap()),
            ControllerMsg::SelectDisplay { index: 1 }
        ));
        // 之后的输入照常发
        tx.send(key(true)).unwrap();
        assert!(matches!(
            decode(&rx.recv().await.unwrap()),
            ControllerMsg::Input { .. }
        ));
        assert!(queued(&tx).is_empty());
    }

    /// 输入带着排进队列的时刻：合并的相对移动用最早的那个（被控端据此判断过没过时），
    /// 丢掉过时输入时补的"释放所有按键"用现在
    #[test]
    fn input_carries_the_time_it_was_queued() {
        let (tx, _rx) = channel();
        let delta = |t, dx| ControllerMsg::Input {
            t,
            events: vec![InputEvent::MouseDelta { dx, dy: 0 }],
        };
        tx.send(delta(100, 1)).unwrap();
        tx.send(delta(200, 2)).unwrap();
        assert!(matches!(
            &queued(&tx)[..],
            [ControllerMsg::Input { t: 100, events }] if events[..] == [InputEvent::MouseDelta { dx: 3, dy: 0 }]
        ));
        let before = now_us();
        assert_eq!(drop_input(&mut tx.0.mail.lock()), 1);
        let q = queued(&tx);
        assert!(is_release(&q[0]));
        assert!(matches!(q[0], ControllerMsg::Input { t, .. } if t >= before));
        assert!(matches!(input(vec![]), ControllerMsg::Input { t, .. } if t >= before));
    }

    #[tokio::test]
    async fn overflow_drops_input_and_only_fails_without_input_to_drop() {
        let (tx, mut rx) = channel();
        for i in 0..LIMIT.messages * 2 {
            tx.send(input(vec![InputEvent::Key {
                code: "KeyA".into(),
                down: i % 2 == 0,
            }]))
            .unwrap();
        }
        let q = queued(&tx);
        assert!(q.len() <= LIMIT.messages);
        assert!(q.iter().any(is_release));
        assert!(is_release(&decode(&rx.recv().await.unwrap())));
        assert!(!tx.failed());

        // 排满了不能丢的命令：会话失败
        let (tx, mut rx) = channel();
        for _ in 0..LIMIT.messages {
            tx.send(KEYFRAME).unwrap();
        }
        assert!(tx.send(KEYFRAME).is_err());
        assert!(tx.failed());
        assert!(rx.recv().await.is_none());
        assert!(tx.send(settings(1)).is_err());
    }
}
