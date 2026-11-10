//! 屏幕采集：每个显示器一个采集线程，多个会话共享（引用计数，最后一个订阅者离开时停止）。

#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "macos")]
mod sck;
#[cfg(target_os = "linux")]
mod x11;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use airhand_proto::session::{CursorShape, DisplayInfo, Notice};
use tracing::{info, warn};

use super::pace::Pacer;

/// 一帧 BGRA 图像（紧密排列，stride = width * 4）。
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    pub captured: Instant,
}

/// 帧缓冲池：避免每帧分配/清零数 MB 内存。
#[derive(Default)]
pub struct FramePool {
    frames: Vec<Arc<Frame>>,
}

impl FramePool {
    const MAX: usize = 6;

    /// 取出一个当前没有被任何人引用的缓冲（返回的 Arc 是唯一引用，可 `get_mut`）。
    pub fn checkout(&mut self, width: u32, height: u32) -> Arc<Frame> {
        let len = width as usize * height as usize * 4;
        if let Some(i) = self
            .frames
            .iter()
            .position(|f| Arc::strong_count(f) == 1 && Arc::weak_count(f) == 0)
        {
            let mut f = self.frames.swap_remove(i);
            let m = Arc::get_mut(&mut f).expect("unique");
            m.width = width;
            m.height = height;
            m.data.resize(len, 0);
            m.captured = Instant::now();
            return f;
        }
        Arc::new(Frame {
            width,
            height,
            data: vec![0; len],
            captured: Instant::now(),
        })
    }

    pub fn keep(&mut self, f: &Arc<Frame>) {
        if self.frames.len() < Self::MAX {
            self.frames.push(f.clone());
        }
    }
}

/// 采集器一次产出的更新。
#[derive(Default)]
pub struct CaptureUpdate {
    pub frame: Option<Arc<Frame>>,
    /// 光标可见性变化
    pub cursor_visible: Option<bool>,
    /// 光标形状变化
    pub cursor_shape: Option<CursorShape>,
}

/// 光标的最新状态（带版本号，编码线程据此判断是否需要通知控制端）。
#[derive(Clone, Default)]
pub struct CursorSnapshot {
    /// 任意变化都会递增
    pub seq: u64,
    /// 形状变化时递增
    pub shape_seq: u64,
    pub visible: bool,
    pub shape: Option<Arc<CursorShape>>,
}

/// 最新帧槽位：采集线程写入，编码线程按需等待"比某序号更新"的帧。
pub struct FrameSlot {
    state: Mutex<SlotState>,
    cv: Condvar,
}

struct SlotState {
    seq: u64,
    frame: Option<Arc<Frame>>,
    error: Option<String>,
    cursor: CursorSnapshot,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                seq: 0,
                frame: None,
                error: None,
                cursor: CursorSnapshot::default(),
            }),
            cv: Condvar::new(),
        }
    }

    fn publish(&self, u: CaptureUpdate) {
        let mut st = self.state.lock().unwrap();
        if let Some(f) = u.frame {
            st.seq += 1;
            st.frame = Some(f);
            st.error = None;
        }
        let mut cursor_changed = false;
        if let Some(v) = u.cursor_visible
            && v != st.cursor.visible
        {
            st.cursor.visible = v;
            cursor_changed = true;
        }
        if let Some(shape) = u.cursor_shape
            && st.cursor.shape.as_deref() != Some(&shape)
        {
            st.cursor.shape = Some(Arc::new(shape));
            st.cursor.shape_seq += 1;
            cursor_changed = true;
        }
        if cursor_changed {
            st.cursor.seq += 1;
        }
        self.cv.notify_all();
    }

    /// 当前光标状态。
    pub fn cursor(&self) -> CursorSnapshot {
        self.state.lock().unwrap().cursor.clone()
    }

    /// 等待新帧或光标变化，返回 (帧序号, 光标序号)。超时也会返回。
    pub fn wait_change(
        &self,
        frame_after: u64,
        cursor_after: u64,
        timeout: Duration,
    ) -> (u64, u64) {
        let st = self.state.lock().unwrap();
        let (st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| {
                s.seq <= frame_after && s.cursor.seq <= cursor_after
            })
            .unwrap();
        (st.seq, st.cursor.seq)
    }

    fn set_error(&self, e: Option<String>) {
        self.state.lock().unwrap().error = e;
    }

    /// 最近一次打开采集失败的原因（见 [`wire_detail`]），成功打开后清空。
    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// 非阻塞地取最新帧（如果比 `after` 新）。
    pub fn try_newer(&self, after: u64) -> Option<(u64, Arc<Frame>)> {
        let st = self.state.lock().unwrap();
        if st.seq > after {
            st.frame.clone().map(|f| (st.seq, f))
        } else {
            None
        }
    }
}

trait Capturer {
    /// 获取下一次更新；超时（画面无变化）时返回空更新。出错时调用方会重建采集器。
    fn next(&mut self, timeout: Duration, pool: &mut FramePool) -> anyhow::Result<CaptureUpdate>;

    /// 订阅者期望的最高帧率变化。由系统按帧率推送画面的采集器（macOS）据此调整，其余忽略。
    fn set_fps(&mut self, _fps: u8) {}
}

struct CaptureShared {
    index: u8,
    slot: FrameSlot,
    stop: AtomicBool,
    /// 订阅者 -> 期望帧率
    subs: Mutex<HashMap<u64, u8>>,
}

impl CaptureShared {
    fn max_fps(&self) -> u8 {
        self.subs
            .lock()
            .unwrap()
            .values()
            .copied()
            .max()
            .unwrap_or(30)
            .max(1)
    }
}

/// 对某个显示器采集的订阅，Drop 时自动退订。
pub struct Subscription {
    shared: Arc<CaptureShared>,
    id: u64,
}

impl Subscription {
    pub fn slot(&self) -> &FrameSlot {
        &self.shared.slot
    }

    pub fn set_fps(&self, fps: u8) {
        self.shared.subs.lock().unwrap().insert(self.id, fps);
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // 先拿 hub 锁，保证与 subscribe 的"复用判断"互斥
        let mut hub = HUB.lock().unwrap();
        let mut subs = self.shared.subs.lock().unwrap();
        subs.remove(&self.id);
        if subs.is_empty() {
            self.shared.stop.store(true, Ordering::Release);
            hub.remove(&self.shared.index);
        }
    }
}

static HUB: LazyLock<Mutex<HashMap<u8, Weak<CaptureShared>>>> = LazyLock::new(Default::default);
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// 订阅显示器画面。若该显示器已在采集则直接复用。
pub fn subscribe(index: u8, fps: u8) -> Subscription {
    let mut hub = HUB.lock().unwrap();
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    if let Some(shared) = hub.get(&index).and_then(Weak::upgrade)
        && !shared.stop.load(Ordering::Acquire)
    {
        shared.subs.lock().unwrap().insert(id, fps);
        return Subscription { shared, id };
    }
    let shared = Arc::new(CaptureShared {
        index,
        slot: FrameSlot::new(),
        stop: AtomicBool::new(false),
        subs: Mutex::new(HashMap::from([(id, fps)])),
    });
    hub.insert(index, Arc::downgrade(&shared));
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name(format!("airhand-capture-{index}"))
        .spawn(move || capture_loop(thread_shared))
        .expect("spawn capture thread");
    Subscription { shared, id }
}

/// 连续打开失败这么多次（每次间隔 250 ms）才算采集失败
const REPORT_AFTER: u32 = 4;

fn capture_loop(shared: Arc<CaptureShared>) {
    info!(screen = shared.index, "capture started");
    raise_priority();
    // 采集线程整体使用物理像素坐标
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetThreadDpiAwarenessContext,
        };
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let mut pool = FramePool::default();
    let mut cap: Option<Box<dyn Capturer>> = None;
    let mut failures = 0u32;
    let mut fps = shared.max_fps();
    let mut pacer = Pacer::new(fps);
    while !shared.stop.load(Ordering::Acquire) {
        let c = match cap.as_mut() {
            Some(c) => c,
            None => match open(shared.index, shared.max_fps()) {
                Ok(c) => {
                    failures = 0;
                    fps = shared.max_fps();
                    pacer.set_fps(fps);
                    cap.insert(c)
                }
                Err(e) => {
                    failures += 1;
                    if failures == 1 || failures.is_multiple_of(20) {
                        warn!(screen = shared.index, "can't open capture: {e:#}");
                    }
                    // 缺少系统权限不算采集失败：会话已经用 Notice::MissingPermissions 告诉控制端了。
                    // 切换桌面（UAC 确认框、锁屏）时可能要重试几次才打得开，持续 1 秒以上才告诉控制端
                    let permission = super::missing_permission(&e);
                    if permission || failures >= REPORT_AFTER {
                        shared
                            .slot
                            .set_error((!permission).then(|| wire_detail(&e)));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            },
        };
        // 按订阅者中最高的帧率节流（按画面的时间戳，见 pace）；DXGI 会在等待期间累积画面更新
        let max_fps = shared.max_fps();
        if max_fps != fps {
            fps = max_fps;
            c.set_fps(fps);
            pacer.set_fps(fps);
        }
        let wait = pacer.wait(Instant::now());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        match c.next(Duration::from_millis(100), &mut pool) {
            Ok(u) => {
                if let Some(f) = &u.frame {
                    pacer.done(f.captured);
                }
                if u.frame.is_some() || u.cursor_visible.is_some() || u.cursor_shape.is_some() {
                    shared.slot.publish(u);
                }
            }
            Err(e) => {
                // 常见于：分辨率变化、UAC/锁屏导致的桌面切换、显卡驱动重置
                info!(
                    screen = shared.index,
                    "capture interrupted, reopening: {e:#}"
                );
                cap = None;
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    info!(screen = shared.index, "capture stopped");
}

/// 错误链的文字说明，发给控制端（见 [`Notice::CaptureFailed`]、`Notice::AudioFailed`）。
///
/// 操作系统给出的错误说明是按本机语言写的（Windows 的 FormatMessage，Linux 设置 locale 之后的 strerror
/// 和 libpulse 的错误说明），这里换成与语言无关的错误码，免得控制端看到被控端电脑的语言。日志里照常记录完整说明。
pub(crate) fn wire_detail(e: &anyhow::Error) -> String {
    let part = |c: &(dyn std::error::Error + 'static)| -> String {
        #[cfg(windows)]
        if let Some(w) = c.downcast_ref::<windows::core::Error>() {
            return w.code().to_string();
        }
        #[cfg(target_os = "linux")]
        if let Some(p) = c.downcast_ref::<crate::host::audio::pulse::PulseError>() {
            return format!("PulseAudio error {}", p.code());
        }
        match c.downcast_ref::<std::io::Error>() {
            Some(io) => match io.raw_os_error() {
                Some(code) => format!("{:?} (os error {code})", io.kind()),
                None => io.to_string(),
            },
            None => c.to_string(),
        }
    };
    e.chain().map(part).collect::<Vec<_>>().join(": ")
}

#[cfg(windows)]
fn open(index: u8, _fps: u8) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(dxgi::DxgiCapturer::open(index)?))
}

#[cfg(target_os = "linux")]
fn open(index: u8, _fps: u8) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(x11::X11Capturer::open(index)?))
}

#[cfg(target_os = "macos")]
fn open(index: u8, fps: u8) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(sck::SckCapturer::open(index, fps)?))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open(_index: u8, _fps: u8) -> anyhow::Result<Box<dyn Capturer>> {
    anyhow::bail!("screen capture isn't supported on this platform yet")
}

/// 列出所有显示器。
pub fn displays() -> Vec<DisplayInfo> {
    #[cfg(windows)]
    {
        super::dpi_aware(|| dxgi::displays().unwrap_or_default())
    }
    #[cfg(target_os = "linux")]
    {
        super::x11::displays().unwrap_or_else(|e| {
            warn!("can't list displays: {e:#}");
            Vec::new()
        })
    }
    #[cfg(target_os = "macos")]
    {
        super::macos::display_infos()
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// 被控端运行环境需要让控制端知道的情况（例如 Linux 的 Wayland 桌面只能采集 X11 程序）。
pub fn environment_notice() -> Option<Notice> {
    #[cfg(target_os = "linux")]
    {
        super::x11::environment_notice()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

pub(crate) fn raise_priority() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
    #[cfg(target_os = "macos")]
    super::macos::raise_thread_qos();
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    /// 发给控制端的说明只有错误码，没有操作系统按本机语言给出的文字
    #[test]
    fn wire_detail_has_no_os_text() {
        let io = std::io::Error::from_raw_os_error(5);
        let kind = io.kind();
        let e = Err::<(), _>(io).context("DuplicateOutput").unwrap_err();
        assert_eq!(
            super::wire_detail(&e),
            format!("DuplicateOutput: {kind:?} (os error 5)")
        );

        #[cfg(windows)]
        {
            use windows::core::HRESULT;
            let w = windows::core::Error::from_hresult(HRESULT(0x887A0026_u32 as i32));
            let e = Err::<(), _>(w).context("AcquireNextFrame").unwrap_err();
            assert_eq!(super::wire_detail(&e), "AcquireNextFrame: 0x887A0026");
        }
    }
}
