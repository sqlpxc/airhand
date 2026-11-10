//! 剪贴板文本双向同步（被控端与控制端共用）。
//!
//! * 检测到变化后才读取内容：Windows 用 `GetClipboardSequenceNumber`，Linux 用 XFIXES 的
//!   选择所有者变化通知，macOS 用 `NSPasteboard.changeCount`（都不需要读取内容，几乎零开销）；
//! * 记录"双方已一致"的文本哈希，抑制回声（A 写入 B → B 检测到变化 → 又发回 A）。

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use airhand_proto::session::MAX_CLIPBOARD_TEXT;

/// 轮询间隔（被控端与控制端相同）
pub const POLL: Duration = airhand_link::controller::CLIPBOARD_POLL;

pub fn sequence() -> u32 {
    #[cfg(windows)]
    unsafe {
        windows::Win32::System::DataExchange::GetClipboardSequenceNumber()
    }
    #[cfg(target_os = "linux")]
    {
        x11_watch::sequence()
    }
    #[cfg(target_os = "macos")]
    {
        objc2::rc::autoreleasepool(|_| {
            objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as u32
        })
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    0
}

/// 在剪贴板对象上执行操作。
///
/// X11 的剪贴板内容由"所有者"进程提供，arboard 的最后一个实例释放后内容就没了，
/// 所以 Linux 上整个进程共用一个一直存活的实例；Windows 上每次新建即可。
fn with_clipboard<T>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Result<T, arboard::Error> {
    #[cfg(target_os = "linux")]
    {
        static SHARED: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);
        let mut g = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if g.is_none() {
            *g = Some(arboard::Clipboard::new()?);
        }
        f(g.as_mut().expect("clipboard"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        f(&mut arboard::Clipboard::new()?)
    }
}

pub fn get_text() -> Result<String, arboard::Error> {
    with_clipboard(|c| c.get_text())
}

pub fn set_text(text: &str) -> Result<(), arboard::Error> {
    with_clipboard(|c| c.set_text(text))
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    // 0 保留为"未同步"
    h.finish() | 1
}

fn read_text() -> Result<Option<String>, arboard::Error> {
    for attempt in 0..3 {
        match get_text() {
            Ok(t) => return Ok(Some(t)),
            Err(arboard::Error::ContentNotAvailable) => return Ok(None),
            Err(e) if attempt == 2 => return Err(e),
            // 其它程序正占用剪贴板，稍后重试；失败不能被缓存成这个序号没有文本。
            Err(_) => std::thread::sleep(Duration::from_millis(40)),
        }
    }
    unreachable!()
}

fn write_text(t: &str) -> bool {
    for _ in 0..3 {
        if set_text(t).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    false
}

/// 整个进程只有一个写入任务。正在写一项时，其余更新只保留最新的一项；
/// 不让不同会话或连续更新创建无界的阻塞任务，也不让旧写入晚于新写入完成。
#[derive(Default)]
struct ClipboardWriter(Mutex<Writes>);

#[derive(Default)]
struct Writes {
    running: bool,
    pending: Option<Update>,
}

struct Update {
    text: String,
    synced: Weak<AtomicU64>,
}

impl ClipboardWriter {
    fn busy(&self) -> bool {
        self.0.lock().unwrap().running
    }

    /// true 表示调用方需要启动唯一的写任务。
    fn push(&self, text: String, synced: &Arc<AtomicU64>) -> bool {
        let mut state = self.0.lock().unwrap();
        state.pending = Some(Update {
            text,
            synced: Arc::downgrade(synced),
        });
        !std::mem::replace(&mut state.running, true)
    }

    fn drain(&self, mut write: impl FnMut(&str) -> bool) {
        loop {
            let update = {
                let mut state = self.0.lock().unwrap();
                match state.pending.take() {
                    Some(update) => update,
                    None => {
                        state.running = false;
                        return;
                    }
                }
            };
            // 会话已经结束的、尚未开始的写入不再执行。
            let Some(synced) = update.synced.upgrade() else {
                continue;
            };
            if write(&update.text) {
                synced.store(hash(&update.text), Ordering::Release);
            } else {
                tracing::debug!("writing the clipboard failed");
            }
        }
    }
}

fn writer() -> &'static Arc<ClipboardWriter> {
    static WRITER: OnceLock<Arc<ClipboardWriter>> = OnceLock::new();
    WRITER.get_or_init(Arc::default)
}

/// 读取也只允许一个后台任务。所有会话共享按剪贴板序号缓存的结果；
/// poll 只取已经完成的结果，慢的系统调用不能暂停会话或堆积阻塞任务。
#[derive(Default)]
struct ClipboardReader(Mutex<Reads>);

type ReadResult = Result<Option<Arc<str>>, ()>;

#[derive(Default)]
struct Reads {
    latest: Option<(u32, Option<Arc<str>>)>,
    pending: Option<(u32, tokio::sync::oneshot::Receiver<ReadResult>)>,
}

impl ClipboardReader {
    fn poll(
        &self,
        seq: u32,
        read: impl FnOnce() -> Result<Option<String>, arboard::Error> + Send + 'static,
    ) -> Option<Option<Arc<str>>> {
        let mut state = self.0.lock().unwrap();
        if let Some((at, rx)) = &mut state.pending {
            match rx.try_recv() {
                Ok(Ok(text)) => {
                    let at = *at;
                    state.latest = Some((at, text));
                    state.pending = None;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Ok(Err(())) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    state.pending = None
                }
            }
        }
        if let Some((at, text)) = &state.latest
            && *at == seq
        {
            return Some(text.clone());
        }
        if state.pending.is_none() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            state.pending = Some((seq, rx));
            tokio::task::spawn_blocking(move || {
                let text = read()
                    .map(|text| {
                        text.filter(|t| !t.is_empty() && t.len() <= MAX_CLIPBOARD_TEXT)
                            .map(Arc::<str>::from)
                    })
                    .map_err(|_| ());
                let _ = tx.send(text);
            });
        }
        None
    }
}

fn reader() -> &'static ClipboardReader {
    static READER: OnceLock<ClipboardReader> = OnceLock::new();
    READER.get_or_init(ClipboardReader::default)
}

/// 单个会话的同步状态。
pub struct ClipSync {
    last_seq: u32,
    synced: Arc<AtomicU64>,
}

impl ClipSync {
    pub fn new() -> Self {
        // 从"当前状态"开始，只同步之后的变化
        Self {
            last_seq: sequence(),
            synced: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 本机剪贴板有新文本（且不是对端刚写入的）时返回它。
    pub fn poll(&mut self) -> Option<String> {
        // 写入过程中不读取中间状态，否则 A、B 连续更新时会把短暂的 A 当成本机变化回传。
        if writer().busy() {
            return None;
        }
        let seq = sequence();
        if seq == self.last_seq {
            return None;
        }
        let text = reader().poll(seq, read_text)?;
        if writer().busy() || sequence() != seq {
            return None;
        }
        self.last_seq = seq;
        let text = text?;
        let h = hash(&text);
        if self.synced.swap(h, Ordering::AcqRel) == h {
            return None;
        }
        Some(text.to_string())
    }

    /// 串行写入对端的最新文本。只有写入成功才更新同步哈希；写入期间 poll 不回传中间状态。
    pub fn apply(&self, text: String) {
        if text.len() > MAX_CLIPBOARD_TEXT {
            return;
        }
        let writer = writer();
        if !writer.busy() && self.synced.load(Ordering::Acquire) == hash(&text) {
            return;
        }
        if writer.push(text, &self.synced) {
            let writer = writer.clone();
            tokio::task::spawn_blocking(move || writer.drain(write_text));
        }
    }
}

/// 控制端会话用它同步本机剪贴板（见 `airhand_link::controller::Frontend::Desktop`）
impl airhand_link::controller::ClipboardSync for ClipSync {
    fn poll(&mut self) -> Option<String> {
        ClipSync::poll(self)
    }

    fn apply(&self, text: String) {
        ClipSync::apply(self, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[tokio::test]
    async fn stalled_clipboard_read_is_shared_and_never_blocks_polling() {
        let reader = ClipboardReader::default();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = mpsc::channel();
        assert!(
            reader
                .poll(1, move || {
                    let _ = started.send(());
                    wait.recv().unwrap();
                    Ok(Some("old".into()))
                })
                .is_none()
        );
        ready.await.unwrap();
        for _ in 0..100 {
            assert!(
                reader
                    .poll(2, || panic!("must not start another blocked read"))
                    .is_none()
            );
        }
        // 同一线程上的会话仍可处理心跳；序号变化后只发布新读数。
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        let text = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(text) = reader.poll(2, || Ok(Some("new".into()))) {
                    break text;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(text.as_deref(), Some("new"));
        assert_eq!(
            reader
                .poll(2, || panic!("reuse the same sequence"))
                .unwrap()
                .as_deref(),
            Some("new")
        );
    }

    #[test]
    fn clipboard_writes_are_serial_and_keep_only_the_latest_pending_value() {
        let writer = Arc::new(ClipboardWriter::default());
        let synced = Arc::new(AtomicU64::new(0));
        assert!(writer.push("A".into(), &synced));
        let (started, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let w = writer.clone();
        let task = std::thread::spawn(move || {
            let mut written = Vec::new();
            w.drain(|text| {
                if text == "A" {
                    started.send(()).unwrap();
                    wait.recv().unwrap();
                }
                written.push(text.to_string());
                true
            });
            written
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(writer.busy());
        assert!(!writer.push("B".into(), &synced));
        assert!(!writer.push("C".into(), &synced));
        release.send(()).unwrap();
        assert_eq!(task.join().unwrap(), ["A", "C"]);
        assert_eq!(synced.load(Ordering::Acquire), hash("C"));
        assert!(!writer.busy());
        assert!(writer.push("failed".into(), &synced));
        writer.drain(|_| false);
        assert_eq!(synced.load(Ordering::Acquire), hash("C"));
        assert!(writer.push("cancelled".into(), &synced));
        drop(synced);
        writer.drain(|_| panic!("a closed session must not write queued text"));
    }
}

/// Linux：监听 CLIPBOARD 选择的所有者变化（XFIXES），每次变化计数加一，作用相当于 Windows 的剪贴板序号。
#[cfg(target_os = "linux")]
mod x11_watch {
    use std::sync::Once;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
    use x11rb::protocol::xproto::ConnectionExt as _;

    static SEQ: AtomicU32 = AtomicU32::new(1);
    static START: Once = Once::new();

    pub fn sequence() -> u32 {
        START.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("airhand-clipboard".into())
                .spawn(|| {
                    let mut failures = 0u32;
                    loop {
                        if let Err(e) = watch() {
                            failures += 1;
                            if failures == 1 || failures.is_multiple_of(60) {
                                tracing::warn!("can't watch the clipboard: {e:#}");
                            }
                        }
                        // 连接断开：可能错过了变化，让下一次轮询重新读取
                        SEQ.fetch_add(1, Ordering::AcqRel);
                        std::thread::sleep(Duration::from_secs(5));
                    }
                });
        });
        SEQ.load(Ordering::Acquire)
    }

    fn watch() -> anyhow::Result<()> {
        let x = crate::host::x11::X11::connect()?;
        x.conn.xfixes_query_version(5, 0)?.reply()?;
        let atom = x.conn.intern_atom(false, b"CLIPBOARD")?.reply()?.atom;
        let mask = SelectionEventMask::SET_SELECTION_OWNER
            | SelectionEventMask::SELECTION_WINDOW_DESTROY
            | SelectionEventMask::SELECTION_CLIENT_CLOSE;
        x.conn.xfixes_select_selection_input(x.root(), atom, mask)?;
        x.conn.flush()?;
        loop {
            if let Event::XfixesSelectionNotify(_) = x.conn.wait_for_event()? {
                SEQ.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}
