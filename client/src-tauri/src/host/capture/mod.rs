//! 屏幕采集：每个显示器一个采集线程，多个会话共享（引用计数，最后一个订阅者离开时停止）。
//!
//! # 画面放在哪里
//!
//! * **GPU 上**（[`GpuFrame`]）：Windows 的桌面复制把画面复制进自己的纹理，macOS 的 ScreenCaptureKit 直接按编码尺寸
//!   输出 NV12。硬件编码器直接用（见 `encoder` 的"GPU 零拷贝"），画面不离开显卡；
//! * **内存里**：X11 的 BGRA，以及 GPU 上的画面第一次要用像素时读回来的（CPU 缩放、软件编码，
//!   见 [`Frame::pixels`]）。读回的结果留在这一帧里，几个会话共用。
//!
//! # 带光标的画面
//!
//! 采集到的画面里没有光标（控制端用本地光标显示它，见 `airhand_proto::session` 的"鼠标"）。控制端在游戏模式时要
//! 被控端把光标画进画面里（`QualitySettings::draw_cursor`），所以一路采集出两种画面（[`FrameSlot`] 里各存最新的一帧）：
//!
//! * 原始的：一直有；
//! * 带光标的：有订阅者要（[`subscribe`] 的 `draw`）才有，光标单独移动、换了形状也出新的一帧（只是把光标重新画在
//!   最新的原始画面上）。光标隐藏着、不在这块显示器上时和原始的是同一帧。
//!
//! Windows、X11 自己画（[`overlay`]）。macOS 由系统画（ScreenCaptureKit 的 `showsCursor`），一路采集只能有一种：
//! 要不要光标也算进"同一种画面"里，各开一路。

#[cfg(windows)]
mod dxgi;
// macOS 由系统画光标，用不到；Linux 只用彩色的（黑白、带掩码的是 Windows 的格式）
#[cfg_attr(not(windows), allow(dead_code))]
mod overlay;
#[cfg(target_os = "macos")]
mod sck;
#[cfg(target_os = "linux")]
mod x11;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use airhand_proto::session::{CursorShape, CursorVisibility, DisplayInfo, Notice};
use anyhow::{Result, anyhow};
use tracing::{info, warn};

use super::pace::Pacer;

#[cfg(windows)]
pub use dxgi::{D3dSource, GpuFrame, ignore_gpu_preference};
#[cfg(all(windows, test))]
pub(crate) use dxgi::{Rotation, create_device};
#[cfg(target_os = "macos")]
pub use sck::GpuFrame;

/// 这个平台的画面不放在 GPU 上
#[cfg(not(any(windows, target_os = "macos")))]
#[derive(Clone)]
pub enum GpuFrame {}

#[cfg(not(any(windows, target_os = "macos")))]
impl GpuFrame {
    pub fn source(&self) -> u64 {
        match *self {}
    }

    fn read_pixels(&self, _: &mut [u8]) -> Result<()> {
        match *self {}
    }
}

/// 每打开一次采集换一个编号（[`GpuFrame::source`]）：GPU 上的画面只能交给同一个设备上建的编码器
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn next_source() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 一帧画面，宽高是用户看到的方向（见 [`GpuFrame`] 和模块说明）。
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub captured: Instant,
    /// GPU 上的画面；`None` 时画面在 `pixels` 里
    pub gpu: Option<GpuFrame>,
    /// 紧密排列的 BGRA（stride = width * 4）
    pixels: OnceLock<Vec<u8>>,
    /// 读回用的缓冲（帧缓冲池复用上一次的），读回期间一直锁着：同一帧只读回一次
    spare: Mutex<Vec<u8>>,
}

impl Frame {
    /// 内存里的画面（`data`：紧密排列的 BGRA）
    pub fn from_pixels(width: u32, height: u32, data: Vec<u8>, captured: Instant) -> Self {
        assert_eq!(data.len(), width as usize * height as usize * 4);
        Self {
            width,
            height,
            captured,
            gpu: None,
            pixels: OnceLock::from(data),
            spare: Mutex::default(),
        }
    }

    fn empty(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            captured: Instant::now(),
            gpu: None,
            pixels: OnceLock::new(),
            spare: Mutex::default(),
        }
    }

    fn len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// 同一幅画面、但没有读回过（基准测试用：每帧都是新画面时的读回开销）
    #[doc(hidden)]
    pub fn uncached(&self) -> Self {
        let mut f = Self::empty(self.width, self.height);
        f.captured = self.captured;
        f.gpu = self.gpu.clone();
        if f.gpu.is_none()
            && let Some(p) = self.pixels.get()
        {
            let _ = f.pixels.set(p.clone());
        }
        f
    }

    /// 紧密排列的 BGRA。GPU 上的画面第一次调用时读回来（Windows 要等显卡复制完，4K 约几毫秒），之后直接用
    pub fn pixels(&self) -> Result<&[u8]> {
        if let Some(p) = self.pixels.get() {
            return Ok(p);
        }
        let gpu = self
            .gpu
            .as_ref()
            .ok_or_else(|| anyhow!("the frame has no pixels"))?;
        let mut spare = self.spare.lock().unwrap();
        // 别的会话可能刚读回过
        if let Some(p) = self.pixels.get() {
            return Ok(p);
        }
        let mut buf = std::mem::take(&mut *spare);
        buf.resize(self.len(), 0);
        if let Err(e) = gpu.read_pixels(&mut buf) {
            *spare = buf;
            return Err(e.context("reading the frame back from the GPU"));
        }
        Ok(self.pixels.get_or_init(|| buf))
    }

    /// 填写内存里的画面（采集器刚从帧缓冲池取出这一帧时用）
    pub fn pixels_mut(&mut self) -> &mut [u8] {
        let len = self.len();
        if self.pixels.get().is_none() {
            let buf = std::mem::take(self.spare.get_mut().unwrap());
            let _ = self.pixels.set(buf);
        }
        let p = self.pixels.get_mut().expect("just set");
        p.resize(len, 0);
        p
    }
}

/// NV12（BT.709 有限范围）→ 紧密排列的 BGRA（`out`）。`y`、`uv` 是两个平面，每行分别 `y_stride`、`uv_stride` 字节；
/// `scratch` 暂存每行有填充的 UV 平面（yuvutils 0.8 的 NV12 转换只接受紧密排列的 UV，见 `encoder/vt.rs`）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn nv12_to_bgra(
    (w, h): (u32, u32),
    (y, y_stride): (&[u8], usize),
    (uv, uv_stride): (&[u8], usize),
    scratch: &mut Vec<u8>,
    out: &mut [u8],
) -> Result<()> {
    use yuvutils_rs::{YuvBiPlanarImage, YuvConversionMode, YuvRange, YuvStandardMatrix};
    let (uv_row, uv_h) = ((w as usize).div_ceil(2) * 2, (h as usize).div_ceil(2));
    if w == 0
        || h == 0
        || y_stride < w as usize
        || y.len() < y_stride * (h as usize - 1) + w as usize
        || uv_stride < uv_row
        || uv.len() < uv_stride * (uv_h - 1) + uv_row
    {
        anyhow::bail!("unexpected NV12 layout");
    }
    let uv = if uv_stride == uv_row {
        &uv[..uv_row * uv_h]
    } else {
        scratch.clear();
        for row in uv.chunks(uv_stride).take(uv_h) {
            scratch.extend_from_slice(&row[..uv_row]);
        }
        scratch.as_slice()
    };
    let img = YuvBiPlanarImage {
        y_plane: y,
        y_stride: y_stride as u32,
        uv_plane: uv,
        uv_stride: uv_row as u32,
        width: w,
        height: h,
    };
    yuvutils_rs::yuv_nv12_to_bgra(
        &img,
        out,
        w * 4,
        YuvRange::Limited,
        YuvStandardMatrix::Bt709,
        YuvConversionMode::Balanced,
    )
    .map_err(|e| anyhow!("color conversion failed: {e:?}"))
}

/// 帧缓冲池：避免每帧分配/清零数 MB 内存，GPU 上的纹理也一样复用。
#[derive(Default)]
pub struct FramePool {
    frames: Vec<Arc<Frame>>,
}

impl FramePool {
    const MAX: usize = 6;

    /// 取出一个当前没有被任何人引用的帧（返回的 Arc 是唯一引用，可 `get_mut`），里面没有画面。
    /// 能接着用的 GPU 资源（`reuse` 返回 true 的）留着，优先挑这样的；其余的释放掉。
    pub fn checkout_with(
        &mut self,
        width: u32,
        height: u32,
        reuse: impl Fn(&GpuFrame) -> bool,
    ) -> Arc<Frame> {
        let free = |f: &Arc<Frame>| Arc::strong_count(f) == 1 && Arc::weak_count(f) == 0;
        let reusable = |f: &Arc<Frame>| f.gpu.as_ref().is_some_and(&reuse);
        let found = self
            .frames
            .iter()
            .position(|f| free(f) && reusable(f))
            .or_else(|| self.frames.iter().position(free));
        let Some(i) = found else {
            return Arc::new(Frame::empty(width, height));
        };
        let mut f = self.frames.swap_remove(i);
        let m = Arc::get_mut(&mut f).expect("unique");
        m.width = width;
        m.height = height;
        m.captured = Instant::now();
        if m.gpu.as_ref().is_some_and(|g| !reuse(g)) {
            m.gpu = None;
        }
        if let Some(p) = m.pixels.take() {
            *m.spare.get_mut().unwrap() = p;
        }
        f
    }

    /// 取出一个内存里的帧（画面用 [`Frame::pixels_mut`] 填写）
    pub fn checkout(&mut self, width: u32, height: u32) -> Arc<Frame> {
        self.checkout_with(width, height, |_| false)
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
    /// 新的原始画面
    pub frame: Option<Arc<Frame>>,
    /// 新的带光标的画面（见模块说明）：只在要的时候有
    pub drawn: Option<Arc<Frame>>,
    /// 光标可见性（变没变都可以给，没变不算变化）
    pub cursor_visibility: Option<CursorVisibility>,
    /// 光标在不在这块显示器上（见 [`CursorSnapshot::here`]；不给就是没变）
    pub cursor_here: Option<bool>,
    /// 光标形状变化
    pub cursor_shape: Option<CursorShape>,
}

/// 光标的最新状态（带版本号，编码线程据此判断是否需要通知控制端）。
#[derive(Clone)]
pub struct CursorSnapshot {
    /// 任意变化都会递增
    pub seq: u64,
    /// 形状变化时递增
    pub shape_seq: u64,
    pub visibility: CursorVisibility,
    /// 光标在这块显示器上（或者看不出来在哪里：隐藏了、这个平台只有一个全局的光标状态）。在别的显示器上时
    /// 这块显示器报的可见性、形状不算数：同时看好几个显示器时只听光标所在的那一块（见 `super::cursor`）
    pub here: bool,
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
    /// 带光标的画面（见模块说明）：各自的序号
    drawn_seq: u64,
    drawn: Option<Arc<Frame>>,
    error: Option<String>,
    cursor: CursorSnapshot,
}

impl FrameSlot {
    fn new() -> Self {
        Self {
            state: Mutex::new(SlotState {
                seq: 0,
                frame: None,
                drawn_seq: 0,
                drawn: None,
                error: None,
                cursor: CursorSnapshot {
                    seq: 0,
                    shape_seq: 0,
                    visibility: CursorVisibility::Hidden,
                    here: true,
                    shape: None,
                },
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
        if let Some(f) = u.drawn {
            st.drawn_seq += 1;
            st.drawn = Some(f);
            st.error = None;
        }
        let mut cursor_changed = false;
        if let Some(v) = u.cursor_visibility
            && v != st.cursor.visibility
        {
            st.cursor.visibility = v;
            cursor_changed = true;
        }
        if let Some(here) = u.cursor_here
            && here != st.cursor.here
        {
            st.cursor.here = here;
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

    /// 等待新帧或光标变化，返回 (帧序号, 光标序号)。超时也会返回。`drawn`：看带光标的那一路画面（见模块说明），
    /// 帧序号也是那一路的
    pub fn wait_change(
        &self,
        frame_after: u64,
        cursor_after: u64,
        timeout: Duration,
        drawn: bool,
    ) -> (u64, u64) {
        let st = self.state.lock().unwrap();
        let (st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| {
                s.seq_of(drawn) <= frame_after && s.cursor.seq <= cursor_after
            })
            .unwrap();
        (st.seq_of(drawn), st.cursor.seq)
    }

    /// 没有订阅者要带光标的画面了：最后一帧不留着，之后再要时不会先拿到过时的画面
    fn clear_drawn(&self) {
        self.state.lock().unwrap().drawn = None;
    }

    fn set_error(&self, e: Option<String>) {
        self.state.lock().unwrap().error = e;
    }

    /// 最近一次打开采集失败的原因（见 [`wire_detail`]），成功打开后清空。
    pub fn error(&self) -> Option<String> {
        self.state.lock().unwrap().error.clone()
    }

    /// 非阻塞地取最新帧（如果比 `after` 新）。`drawn`：带光标的那一路（见 [`FrameSlot::wait_change`]）
    pub fn try_newer(&self, after: u64, drawn: bool) -> Option<(u64, Arc<Frame>)> {
        let st = self.state.lock().unwrap();
        let seq = st.seq_of(drawn);
        if seq > after {
            let f = if drawn { &st.drawn } else { &st.frame };
            f.clone().map(|f| (seq, f))
        } else {
            None
        }
    }
}

impl SlotState {
    /// 这一路最新的一帧的序号。带光标的那一路没人要了被清掉（见 [`FrameSlot::clear_drawn`]）之后是 0：
    /// 再要的时候等新的一帧，不会以为有新帧却拿不到
    fn seq_of(&self, drawn: bool) -> u64 {
        match drawn {
            true if self.drawn.is_some() => self.drawn_seq,
            true => 0,
            false => self.seq,
        }
    }
}

trait Capturer {
    /// 获取下一次更新；超时（画面无变化）时返回空更新。出错时调用方会重建采集器。
    ///
    /// `draw`：有订阅者要带光标的画面（见模块说明）。要的时候除了原始画面，还要在光标移动、换形状、原始画面变了的时候
    /// 给出新的带光标的画面；刚开始要（之前不要）时马上用最新的原始画面给一帧，不等画面变化。
    /// 不要了以后再要，也要马上给一帧
    fn next(
        &mut self,
        timeout: Duration,
        pool: &mut FramePool,
        draw: bool,
    ) -> anyhow::Result<CaptureUpdate>;

    /// 订阅者期望的最高帧率变化。由系统按帧率推送画面的采集器（macOS）据此调整，其余忽略。
    fn set_fps(&mut self, _fps: u8) {}
}

/// 哪个显示器、采集时的短边上限、系统画不画光标。后两项只影响 macOS，其余平台同一显示器共用一路采集
type Key = (u8, u16, bool);

/// 一个订阅者要什么
#[derive(Clone, Copy)]
struct Want {
    fps: u8,
    /// 要带光标的画面
    draw: bool,
}

struct CaptureShared {
    key: Key,
    slot: FrameSlot,
    stop: AtomicBool,
    subs: Mutex<HashMap<u64, Want>>,
}

impl CaptureShared {
    fn max_fps(&self) -> u8 {
        self.subs
            .lock()
            .unwrap()
            .values()
            .map(|w| w.fps)
            .max()
            .unwrap_or(30)
            .max(1)
    }

    /// 有订阅者要带光标的画面
    fn wants_drawn(&self) -> bool {
        self.subs.lock().unwrap().values().any(|w| w.draw)
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
        if let Some(w) = self.shared.subs.lock().unwrap().get_mut(&self.id) {
            w.fps = fps;
        }
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
            hub.remove(&self.shared.key);
        }
    }
}

type Hub = HashMap<Key, Weak<CaptureShared>>;
static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// 订阅显示器画面。若该显示器已在按同样的方式采集则直接复用。
/// `max_short`：macOS 让系统按这个短边上限直接交付 NV12；其余平台由编码器缩放。
/// `draw`：要带光标的画面（见模块说明，读的时候给 [`FrameSlot::wait_change`] 和 [`FrameSlot::try_newer`] 也带上它）
pub fn subscribe(index: u8, fps: u8, max_short: u16, draw: bool) -> Subscription {
    let key = if cfg!(target_os = "macos") {
        (index, max_short, draw)
    } else {
        (index, 0, false)
    };
    let want = Want { fps, draw };
    let mut hub = HUB.lock().unwrap();
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    if let Some(shared) = hub.get(&key).and_then(Weak::upgrade)
        && !shared.stop.load(Ordering::Acquire)
    {
        shared.subs.lock().unwrap().insert(id, want);
        return Subscription { shared, id };
    }
    let shared = Arc::new(CaptureShared {
        key,
        slot: FrameSlot::new(),
        stop: AtomicBool::new(false),
        subs: Mutex::new(HashMap::from([(id, want)])),
    });
    hub.insert(key, Arc::downgrade(&shared));
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
    let (index, max_short, system_cursor) = shared.key;
    info!(screen = index, max_short, system_cursor, "capture started");
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
            None => match open(index, shared.max_fps(), max_short, system_cursor) {
                Ok(c) => {
                    failures = 0;
                    fps = shared.max_fps();
                    pacer.set_fps(fps);
                    cap.insert(c)
                }
                Err(e) => {
                    failures += 1;
                    if failures == 1 || failures.is_multiple_of(20) {
                        warn!(screen = index, "can't open capture: {e:#}");
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
        let draw = shared.wants_drawn();
        if !draw {
            shared.slot.clear_drawn();
        }
        match c.next(Duration::from_millis(100), &mut pool, draw) {
            Ok(u) => {
                // 只有光标动了（带光标的画面）也算一帧：鼠标一动就是每秒几百次更新，也要按帧率来
                if let Some(f) = u.frame.as_ref().or(u.drawn.as_ref()) {
                    pacer.done(f.captured);
                }
                if u.frame.is_some()
                    || u.drawn.is_some()
                    || u.cursor_visibility.is_some()
                    || u.cursor_here.is_some()
                    || u.cursor_shape.is_some()
                {
                    shared.slot.publish(u);
                }
            }
            Err(e) => {
                // 常见于：分辨率变化、UAC/锁屏导致的桌面切换、显卡驱动重置
                info!(screen = index, "capture interrupted, reopening: {e:#}");
                cap = None;
                // 池里的纹理属于旧的设备，重新打开之后用不上了（会话还拿着的帧不受影响）
                pool = FramePool::default();
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    info!(screen = index, "capture stopped");
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

/// `cursor`：让系统把光标画进采集到的画面里（macOS，见模块说明）；别的平台总是 false
#[cfg(windows)]
fn open(index: u8, _fps: u8, _max_short: u16, _cursor: bool) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(dxgi::DxgiCapturer::open(index)?))
}

#[cfg(target_os = "linux")]
fn open(index: u8, _fps: u8, _max_short: u16, _cursor: bool) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(x11::X11Capturer::open(index)?))
}

#[cfg(target_os = "macos")]
fn open(index: u8, fps: u8, max_short: u16, cursor: bool) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(sck::SckCapturer::open(
        index, fps, max_short, cursor,
    )?))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open(_index: u8, _fps: u8, _max_short: u16, _cursor: bool) -> anyhow::Result<Box<dyn Capturer>> {
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

    /// NV12 → BGRA：每行有填充（CoreVideo 按 64 字节对齐）时和紧密排列的结果一样；BT.709 有限范围的红、蓝
    #[test]
    fn nv12_with_padded_rows() {
        let (w, h) = (6u32, 4u32);
        // 左边 4 列红（Y=63 U=102 V=240），右边 2 列蓝（Y=32 U=240 V=118）
        let y_row = [63, 63, 63, 63, 32, 32];
        let uv_row = [102, 240, 102, 240, 240, 118];
        let tight_y = y_row.repeat(h as usize);
        let tight_uv = uv_row.repeat(h as usize / 2);
        let pad = |row: &[u8], stride: usize, n: usize| -> Vec<u8> {
            (0..n)
                .flat_map(|_| {
                    let mut r = row.to_vec();
                    r.resize(stride, 0xEE);
                    r
                })
                .collect()
        };
        let mut scratch = Vec::new();
        let mut want = vec![0u8; (w * h * 4) as usize];
        super::nv12_to_bgra(
            (w, h),
            (&tight_y, 6),
            (&tight_uv, 6),
            &mut scratch,
            &mut want,
        )
        .unwrap();
        let mut got = vec![0u8; want.len()];
        let (py, puv) = (
            pad(&y_row, 64, h as usize),
            pad(&uv_row, 64, h as usize / 2),
        );
        super::nv12_to_bgra((w, h), (&py, 64), (&puv, 64), &mut scratch, &mut got).unwrap();
        assert_eq!(got, want);
        let px = |x: usize| &want[x * 4..x * 4 + 4];
        let near = |p: &[u8], bgr: [u8; 3]| p[..3].iter().zip(bgr).all(|(a, b)| a.abs_diff(b) <= 3);
        assert!(near(px(0), [0, 0, 255]), "red: {:?}", px(0));
        assert!(near(px(5), [255, 0, 0]), "blue: {:?}", px(5));
        // 平面太小：报错，不越界
        assert!(
            super::nv12_to_bgra((w, h), (&py[..100], 64), (&puv, 64), &mut scratch, &mut got)
                .is_err()
        );
    }

    /// 真的采集第 0 个显示器（不会动鼠标）：要带光标的订阅者和不要的共用一路采集，从带光标的那一路拿到画面；
    /// 没人要了之后那一路清空，不会把过时的画面留给之后再要的。
    /// `cargo test --lib drawn_channel -- --ignored --nocapture`
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn drawn_channel() {
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let raw = super::subscribe(0, 30, 1080, false);
        let drawn = super::subscribe(0, 30, 720, true);
        assert!(Arc::ptr_eq(&raw.shared, &drawn.shared), "one capture");
        let (seq, _) = drawn
            .slot()
            .wait_change(0, u64::MAX, Duration::from_secs(3), true);
        assert!(seq > 0, "a frame with the cursor");
        let (_, d) = drawn.slot().try_newer(0, true).unwrap();
        let (_, r) = raw.slot().try_newer(0, false).unwrap();
        println!(
            "{}x{}, the cursor is drawn on it: {}",
            d.width,
            d.height,
            !Arc::ptr_eq(&d, &r)
        );
        drop(drawn);
        let started = Instant::now();
        while raw.slot().try_newer(0, true).is_some() {
            assert!(started.elapsed() < Duration::from_secs(2), "not cleared");
            std::thread::sleep(Duration::from_millis(20));
        }
        // 不要带光标的画面了：等它的时候不会马上返回（序号是 0）
        let (seq, _) = raw
            .slot()
            .wait_change(0, u64::MAX, Duration::from_millis(50), true);
        assert_eq!(seq, 0);
    }

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
