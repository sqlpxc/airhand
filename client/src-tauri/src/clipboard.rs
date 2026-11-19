//! 剪贴板双向同步（被控端与控制端共用）：文本，和复制的文件（只发清单，粘贴时才传，见 `airhand_link::files`）。
//!
//! * 检测到变化后才读取内容：Windows 用 `GetClipboardSequenceNumber`，Linux 用 XFIXES 的
//!   选择所有者变化通知，macOS 用 `NSPasteboard.changeCount`（都不需要读取内容，几乎零开销）；
//! * 记录"双方已一致"的内容哈希，抑制回声（A 写入 B → B 检测到变化 → 又发回 A）；
//! * 对方复制的文件由 [`crate::clipfiles`] 放进本机剪贴板；本机剪贴板里是自己放的那些时不算本机复制了文件。

use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use airhand_link::controller::Clip;
use airhand_link::files::RemoteOffer;
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

fn hash_clip(c: &Clip) -> u64 {
    match c {
        Clip::Text(t) => hash(t),
        Clip::Files(paths) => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            "files".hash(&mut h);
            paths.hash(&mut h);
            h.finish() | 1
        }
    }
}

/// 剪贴板里复制的文件（没有时为 `None`）
fn get_files() -> Result<Option<Vec<std::path::PathBuf>>, arboard::Error> {
    match with_clipboard(|c| c.get().file_list()) {
        // 自己放进去的对方的文件（Linux：本程序挂的文件系统里的）不算本机复制的
        Ok(files)
            if !files.is_empty() && !files.iter().all(|f| crate::clipfiles::is_our_path(f)) =>
        {
            Ok(Some(files))
        }
        Ok(_) | Err(arboard::Error::ContentNotAvailable) => Ok(None),
        // 有的平台上剪贴板里不是文件时报的是别的错误：当作没有文件，接着读文本
        Err(arboard::Error::ConversionFailure) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 剪贴板里是什么：复制的文件优先，不然是文本。自己放进去的对方的文件不算（见模块说明）
fn read_clip() -> Result<Option<Clip>, arboard::Error> {
    if crate::clipfiles::is_ours() {
        return Ok(None);
    }
    for attempt in 0..3 {
        let read = get_files().and_then(|files| match files {
            Some(files) => Ok(Some(Clip::Files(files))),
            None => match get_text() {
                Ok(t) => Ok(Some(Clip::Text(t))),
                Err(arboard::Error::ContentNotAvailable) => Ok(None),
                Err(e) => Err(e),
            },
        });
        match read {
            Ok(c) => return Ok(c),
            Err(e) if attempt == 2 => return Err(e),
            // 其它程序正占用剪贴板，稍后重试；失败不能被缓存成这个序号没有内容。
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

type ReadResult = Result<Option<Arc<Clip>>, ()>;

#[derive(Default)]
struct Reads {
    latest: Option<(u32, Option<Arc<Clip>>)>,
    pending: Option<(u32, tokio::sync::oneshot::Receiver<ReadResult>)>,
}

impl ClipboardReader {
    fn poll(
        &self,
        seq: u32,
        read: impl FnOnce() -> Result<Option<Clip>, arboard::Error> + Send + 'static,
    ) -> Option<Option<Arc<Clip>>> {
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
                let clip = read()
                    .map(|clip| {
                        clip.filter(|c| match c {
                            Clip::Text(t) => !t.is_empty() && t.len() <= MAX_CLIPBOARD_TEXT,
                            Clip::Files(f) => !f.is_empty(),
                        })
                        .map(Arc::new)
                    })
                    .map_err(|_| ());
                let _ = tx.send(clip);
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
    /// 最近放进本机剪贴板的对方的文件：会话结束时还在剪贴板里的话拿掉
    applied: Mutex<Option<RemoteOffer>>,
}

impl ClipSync {
    pub fn new() -> Self {
        // 从"当前状态"开始，只同步之后的变化
        Self {
            last_seq: sequence(),
            synced: Arc::new(AtomicU64::new(0)),
            applied: Mutex::new(None),
        }
    }

    /// 本机剪贴板有新内容（文本、复制的文件，且不是对端刚写入的）时返回它。
    pub fn poll(&mut self) -> Option<Clip> {
        // 写入过程中不读取中间状态，否则 A、B 连续更新时会把短暂的 A 当成本机变化回传。
        if writer().busy() {
            return None;
        }
        let seq = sequence();
        if seq == self.last_seq {
            return None;
        }
        let clip = reader().poll(seq, read_clip)?;
        if writer().busy() || sequence() != seq {
            return None;
        }
        self.last_seq = seq;
        let clip = clip?;
        let h = hash_clip(&clip);
        if self.synced.swap(h, Ordering::AcqRel) == h {
            return None;
        }
        Some((*clip).clone())
    }

    /// 对方复制了文件：放进本机剪贴板（粘贴时才传，见 [`crate::clipfiles`]）
    pub fn apply_files(&self, offer: RemoteOffer) {
        // 之后本机再复制同样的东西也要同步过去
        self.synced.store(0, Ordering::Release);
        *self.applied.lock().unwrap() = Some(offer.clone());
        crate::clipfiles::apply(offer);
    }

    /// 串行写入对端的最新文本。只有写入成功才更新同步哈希；写入期间 poll 不回传中间状态。
    pub fn apply(&self, text: String) {
        if text.len() > MAX_CLIPBOARD_TEXT {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Some(offer) = self.applied.lock().unwrap().take() {
            crate::clipfiles::withdraw(&offer);
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
    fn poll(&mut self) -> Option<Clip> {
        ClipSync::poll(self)
    }

    fn apply(&self, text: String) {
        ClipSync::apply(self, text)
    }

    fn apply_files(&self, offer: RemoteOffer) {
        ClipSync::apply_files(self, offer)
    }
}

impl Drop for ClipSync {
    /// 会话结束了：本机剪贴板里还是它的文件的话拿掉（之后粘贴也取不到了）
    fn drop(&mut self) {
        if let Some(offer) = self.applied.lock().unwrap().take() {
            crate::clipfiles::withdraw(&offer);
        }
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
                    Ok(Some(Clip::Text("old".into())))
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
                if let Some(text) = reader.poll(2, || Ok(Some(Clip::Text("new".into())))) {
                    break text;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let new = Clip::Text("new".into());
        assert_eq!(text.as_deref(), Some(&new));
        assert_eq!(
            reader
                .poll(2, || panic!("reuse the same sequence"))
                .unwrap()
                .as_deref(),
            Some(&new)
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
