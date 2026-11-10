//! 注入线程：会话把输入交给它就接着处理别的，不等注入完。
//!
//! 注入是同步的，有的还很慢：X11 上输入键盘布局里没有的文字要临时绑定键码，每一段都要等 10–20 ms（见 `x11.rs`），
//! 一长段文字要好几秒。在会话的任务里直接注入的话，这期间会话什么也处理不了：Pong 发不出去，画面的确认没人处理
//! （在途帧窗口停住，画面卡住），本机用户断开控制端也要等它输完。读任务还会被堵住，后面的输入读得晚了，
//! 被 `InputClock` 当成网络上耽搁的过时输入丢掉——超长的文字输到一半变成了"释放所有按键"。
//!
//! 所以每个会话一个线程，[`Injector`] 在这个线程上创建、使用、释放：
//!
//! * 输入按交来的顺序排队。排着的有上限（[`MAX_QUEUED`]）：排不下的整批丢掉，并在那个位置松开所有按键
//!   （丢掉的里面可能有松开按键），排着的都注入完了才接着收——不会丢一批收一批、文字中间一块一块地缺；
//! * 会话结束（[`InjectorThread`] 被丢掉）时排着的不再注入，松开所有按键后线程结束。很长的一段文字分成小段交给
//!   注入器（[`TEXT_PIECE`]），每段之间看一眼会话是不是结束了：本机用户断开控制端之后，最多再输入一小段。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use airhand_proto::session::{DisplayInfo, InputEvent};
use tracing::warn;

use super::Injector;

/// 排着还没注入完的输入最多占这么多字节（见 [`cost`]）。平时线程跟得上，排着的不到一批；
/// 只有文字输得慢的时候才会排起来
const MAX_QUEUED: usize = 4 << 20;

/// 一次交给注入器的文字最多这么多个字符。每个事件、每段文字之间都检查取消，
/// 包括前端已经分好的小段：不能把一整批小段重新合成一次不可取消的调用。
const TEXT_PIECE: usize = 512;

/// 注入器：各平台的 [`Injector`]（测试里是假的）
pub(super) trait Inject {
    fn handle(&mut self, events: &[InputEvent], display: &DisplayInfo);
    fn release_all(&mut self);
}

impl Inject for Injector {
    fn handle(&mut self, events: &[InputEvent], display: &DisplayInfo) {
        Injector::handle(self, events, display);
    }

    fn release_all(&mut self) {
        Injector::release_all(self);
    }
}

enum Job {
    Input {
        events: Vec<InputEvent>,
        display: DisplayInfo,
        /// 这一批的 [`cost`]
        cost: usize,
    },
    ReleaseAll,
}

/// 会话和注入线程都看得到的
struct Shared {
    /// 排着的（包括正在注入的那一批）占了多少字节
    queued: AtomicUsize,
    /// 会话结束了：排着的不再注入
    closed: AtomicBool,
}

/// 一个会话的注入线程（见模块说明）。丢掉时松开所有按键
pub struct InjectorThread {
    /// 线程起不来、或者意外结束了时为 `None`：之后的输入不再执行
    tx: Option<mpsc::Sender<Job>>,
    shared: Arc<Shared>,
    /// 排满了，正在丢输入（已经排了一个"松开所有按键"）
    dropping: bool,
}

impl InjectorThread {
    pub fn new() -> Self {
        Self::spawn(Injector::new)
    }

    /// `make` 在注入线程上调用：注入器不用能在线程之间传
    fn spawn<I: Inject>(make: impl FnOnce() -> I + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            queued: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        });
        let sh = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("airhand-inject".into())
            .spawn(move || run(make(), rx, &sh));
        if let Err(e) = &spawned {
            warn!("can't start the input thread, this controller's input will be ignored: {e}");
        }
        Self {
            tx: spawned.is_ok().then_some(tx),
            shared,
            dropping: false,
        }
    }

    /// 把一批输入排给注入线程，不等它注入。`display` 是现在的画面对应的显示器（坐标换算用）
    pub fn handle(&mut self, events: Vec<InputEvent>, display: &DisplayInfo) {
        if self.tx.is_none() {
            return;
        }
        let cost = cost(&events);
        let queued = self.shared.queued.load(Ordering::Acquire);
        // 排满过：排着的都注入完了才接着收
        let room = !self.dropping || queued == 0;
        if !room || queued.saturating_add(cost) > MAX_QUEUED {
            if !self.dropping {
                warn!(
                    queued,
                    "input arrives faster than it can be injected, dropping it until the backlog is gone"
                );
                self.dropping = true;
                // 丢掉的里面可能有松开按键：排在前面的都注入完之后，松开所有按键
                self.send(Job::ReleaseAll);
            }
            return;
        }
        self.dropping = false;
        self.shared.queued.fetch_add(cost, Ordering::AcqRel);
        self.send(Job::Input {
            events,
            display: display.clone(),
            cost,
        });
    }

    /// 排在前面的输入都注入完之后，松开所有按着的键
    pub fn release_all(&mut self) {
        self.send(Job::ReleaseAll);
    }

    /// 马上取消尚未注入的事件，并唤醒空闲线程。停止编码器之前就应调用。
    pub fn cancel(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.tx = None;
    }

    fn send(&mut self, job: Job) {
        if let Some(tx) = &self.tx
            && tx.send(job).is_err()
        {
            warn!("the input thread is gone, this controller's input will be ignored");
            self.tx = None;
        }
    }
}

impl Drop for InjectorThread {
    fn drop(&mut self) {
        // 排着的不再注入。发送端随之丢掉，线程醒来后松开所有按键、结束（不在这里等它：这里是会话的任务）
        self.shared.closed.store(true, Ordering::Release);
    }
}

/// 一批输入排着的时候大约占多少字节
fn cost(events: &[InputEvent]) -> usize {
    events
        .iter()
        .map(|e| {
            size_of::<InputEvent>()
                + match e {
                    InputEvent::Text { text } => text.len(),
                    InputEvent::Key { code, .. } => code.len(),
                    _ => 0,
                }
        })
        .sum()
}

/// 注入线程：按顺序注入，直到会话结束
fn run<I: Inject>(mut injector: I, jobs: mpsc::Receiver<Job>, shared: &Shared) {
    while let Ok(job) = jobs.recv() {
        if shared.closed.load(Ordering::Acquire) {
            break;
        }
        match job {
            Job::Input {
                events,
                display,
                cost,
            } => {
                inject(&mut injector, &events, &display, shared);
                shared.queued.fetch_sub(cost, Ordering::AcqRel);
            }
            Job::ReleaseAll => injector.release_all(),
        }
    }
    injector.release_all();
}

/// 注入一批输入。太长的文字（见 [`TEXT_PIECE`]）分成小段，会话结束了就不再往下输
fn inject<I: Inject>(
    injector: &mut I,
    events: &[InputEvent],
    display: &DisplayInfo,
    shared: &Shared,
) {
    for event in events {
        if shared.closed.load(Ordering::Acquire) {
            return;
        }
        let InputEvent::Text { text } = event else {
            injector.handle(std::slice::from_ref(event), display);
            continue;
        };
        let mut rest = text.as_str();
        while !rest.is_empty() {
            if shared.closed.load(Ordering::Acquire) {
                return;
            }
            let end = rest
                .char_indices()
                .nth(TEXT_PIECE)
                .map_or(rest.len(), |(at, _)| at);
            let (piece, tail) = rest.split_at(end);
            if piece.len() == text.len() {
                injector.handle(std::slice::from_ref(event), display);
            } else {
                injector.handle(&[InputEvent::Text { text: piece.into() }], display);
            }
            rest = tail;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[derive(Debug, PartialEq)]
    enum Call {
        Input(Vec<InputEvent>),
        ReleaseAll,
    }

    /// 假的注入器：每注入一批先记下来，再等一个许可才算注入完（像 X11 上输入文字那样慢）
    struct Slow {
        calls: mpsc::Sender<Call>,
        permits: mpsc::Receiver<()>,
    }

    impl Inject for Slow {
        fn handle(&mut self, events: &[InputEvent], _: &DisplayInfo) {
            let _ = self.calls.send(Call::Input(events.to_vec()));
            let _ = self.permits.recv();
        }

        fn release_all(&mut self) {
            let _ = self.calls.send(Call::ReleaseAll);
        }
    }

    /// 注入器那边发生了什么，什么时候让它注入完一批
    struct Probe {
        calls: mpsc::Receiver<Call>,
        permits: mpsc::Sender<()>,
    }

    impl Probe {
        /// 注入器接下来做的事
        fn next(&self) -> Call {
            self.calls
                .recv_timeout(Duration::from_secs(5))
                .expect("the injector wasn't called")
        }

        /// 注入器还卡在上一批上（或者没事可做）
        fn idle(&self) -> bool {
            matches!(self.calls.try_recv(), Err(mpsc::TryRecvError::Empty))
        }

        /// 让注入器注入完 `n` 批
        fn permit(&self, n: usize) {
            for _ in 0..n {
                self.permits.send(()).unwrap();
            }
        }

        /// 注入线程结束了（注入器被丢掉了）
        fn ended(&self) -> bool {
            matches!(
                self.calls.recv_timeout(Duration::from_secs(5)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            )
        }
    }

    fn start() -> (InjectorThread, Probe) {
        let (calls_tx, calls) = mpsc::channel();
        let (permits, permits_rx) = mpsc::channel();
        let thread = InjectorThread::spawn(move || Slow {
            calls: calls_tx,
            permits: permits_rx,
        });
        (thread, Probe { calls, permits })
    }

    fn display() -> DisplayInfo {
        DisplayInfo {
            index: 0,
            name: "test".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        }
    }

    fn text(s: &str) -> InputEvent {
        InputEvent::Text { text: s.into() }
    }

    fn key(code: &str, down: bool) -> InputEvent {
        InputEvent::Key {
            code: code.into(),
            down,
        }
    }

    /// 会话把输入交出去就返回，注入器还卡在前一批上也一样；注入的顺序就是交来的顺序
    #[test]
    fn handing_input_over_does_not_wait_for_the_injector() {
        let (mut thread, probe) = start();
        let d = display();
        thread.handle(vec![text("slow")], &d);
        assert_eq!(probe.next(), Call::Input(vec![text("slow")]));
        thread.handle(vec![key("KeyA", true)], &d);
        thread.release_all();
        thread.handle(vec![key("KeyA", false)], &d);
        assert!(probe.idle());
        probe.permit(3);
        assert_eq!(probe.next(), Call::Input(vec![key("KeyA", true)]));
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert_eq!(probe.next(), Call::Input(vec![key("KeyA", false)]));
    }

    /// 会话结束：排着的不再注入，松开所有按键，线程结束
    #[test]
    fn ending_the_session_drops_queued_input_and_releases_the_keys() {
        let (mut thread, probe) = start();
        let d = display();
        thread.handle(vec![key("KeyA", true)], &d);
        assert_eq!(probe.next(), Call::Input(vec![key("KeyA", true)]));
        thread.handle(vec![text("never typed")], &d);
        thread.handle(vec![text("neither is this")], &d);
        drop(thread);
        probe.permit(1);
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert!(probe.ended());
    }

    /// 排不下的输入整批丢掉，在那个位置松开所有按键；排着的都注入完了才接着收
    #[test]
    fn a_backlog_that_is_too_long_is_cut_off_and_the_keys_are_released() {
        let (mut thread, probe) = start();
        let d = display();
        thread.handle(vec![key("ShiftLeft", true)], &d);
        assert_eq!(probe.next(), Call::Input(vec![key("ShiftLeft", true)]));
        thread.handle(vec![text(&"x".repeat(MAX_QUEUED))], &d);
        // 前面的还没注入完：排得下的也不收
        thread.handle(vec![key("KeyB", true)], &d);
        probe.permit(1);
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert!(probe.idle());
        thread.handle(vec![key("KeyC", true)], &d);
        assert_eq!(probe.next(), Call::Input(vec![key("KeyC", true)]));
    }

    /// 正常前端一次发送 64 个 512 字符的事件：断开后不能继续执行这一批的剩余事件。
    #[test]
    fn frontend_text_chunks_remain_cancellable_inside_a_batch() {
        let (mut thread, probe) = start();
        let piece = text(&"界".repeat(TEXT_PIECE));
        thread.handle(vec![piece.clone(); 64], &display());
        assert_eq!(probe.next(), Call::Input(vec![piece]));
        drop(thread);
        probe.permit(1);
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert!(probe.ended());
    }

    /// 太长的文字分成小段输：一个字不少，和前后的事件的先后不变；会话结束了就不再往下输
    #[test]
    fn long_text_is_typed_in_pieces_and_stops_when_the_session_ends() {
        let long = "界".repeat(TEXT_PIECE * 2 + 7);
        let batch = vec![
            key("ShiftLeft", true),
            text(&long),
            text("short"),
            key("ShiftLeft", false),
        ];
        let (mut thread, probe) = start();
        probe.permit(6);
        thread.handle(batch.clone(), &display());
        assert_eq!(probe.next(), Call::Input(vec![key("ShiftLeft", true)]));
        let mut typed = String::new();
        for chars in [TEXT_PIECE, TEXT_PIECE, 7] {
            let Call::Input(events) = probe.next() else {
                panic!("expected a piece of the text");
            };
            let [InputEvent::Text { text }] = &events[..] else {
                panic!("expected a piece of the text, got {events:?}");
            };
            assert_eq!(text.chars().count(), chars);
            typed += text;
        }
        assert_eq!(typed, long);
        assert_eq!(probe.next(), Call::Input(vec![text("short")]));
        assert_eq!(probe.next(), Call::Input(vec![key("ShiftLeft", false)]));
        drop(thread);
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert!(probe.ended());

        // 输第一段的时候会话结束了
        let (mut thread, probe) = start();
        thread.handle(batch, &display());
        probe.permit(1);
        assert_eq!(probe.next(), Call::Input(vec![key("ShiftLeft", true)]));
        assert!(matches!(probe.next(), Call::Input(_)));
        drop(thread);
        probe.permit(1);
        assert_eq!(probe.next(), Call::ReleaseAll);
        assert!(probe.ended());
    }
}
