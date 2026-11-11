//! 屏幕采集：每个显示器一个采集线程，多个会话共享（引用计数，最后一个订阅者离开时停止）。
//!
//! # 画面放在哪里
//!
//! * **GPU 上**（[`GpuFrame`]）：Windows 的桌面复制把画面复制进自己的纹理，macOS 的 ScreenCaptureKit 直接按编码尺寸
//!   输出 NV12。硬件编码器直接用（见 `encoder` 的"GPU 零拷贝"），画面不离开显卡；
//! * **内存里**：X11、macOS 的原始分辨率 BGRA，以及 GPU 上的画面第一次要用像素时读回来的（CPU 缩放、软件编码，
//!   见 [`Frame::pixels`]）。读回的结果留在这一帧里，几个会话共用。

#[cfg(windows)]
mod dxgi;
#[cfg(target_os = "macos")]
mod sck;
#[cfg(target_os = "linux")]
mod x11;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use airhand_proto::session::{CursorShape, DisplayInfo, Notice, QualitySettings};
use anyhow::{Result, anyhow};
use tracing::{info, warn};

use super::pace::Pacer;

#[cfg(windows)]
pub use dxgi::{D3dSource, GpuFrame};
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

/// 订阅者要什么样的画面。同一个显示器、同一种画面的订阅者共用一路采集
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Output {
    /// 原始分辨率（Windows 上在 GPU 上，编码器自己在 GPU 上缩放）
    Native,
    /// 系统采集时就按这个短边上限缩放好（见 `encoder::target_size`），直接输出编码器要的 NV12，在 GPU 上
    ///（macOS 的 ScreenCaptureKit，见 sck.rs）。别的平台没有这种，当作 `Native`
    Scaled(u16),
}

/// 按画质设置该订阅哪种画面。
///
/// macOS：低延迟优先时让系统直接缩放成编码尺寸的 NV12；清楚优先、而且需要缩小时要原始分辨率的 BGRA，
/// 在 CPU 上缩放（见 `encoder` 的说明）。不用缩放时两种一样，都用系统输出的 NV12。
pub fn output_for(index: u8, s: &QualitySettings) -> Output {
    #[cfg(target_os = "macos")]
    {
        use airhand_proto::session::VideoPriority;
        if s.priority.or_latency() == VideoPriority::Clarity
            && let Ok(d) = super::macos::display(index)
            && super::encoder::needs_scaling(d.width, d.height, s.max_short)
        {
            return Output::Native;
        }
        Output::Scaled(s.max_short)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (index, s);
        Output::Native
    }
}

struct CaptureShared {
    key: (u8, Output),
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
            hub.remove(&self.shared.key);
        }
    }
}

type Hub = HashMap<(u8, Output), Weak<CaptureShared>>;
static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);
static NEXT_SUB: AtomicU64 = AtomicU64::new(1);

/// 订阅显示器画面。若该显示器已在按同样的方式采集则直接复用。
pub fn subscribe(index: u8, fps: u8, output: Output) -> Subscription {
    #[cfg(not(target_os = "macos"))]
    let output = {
        let _ = output;
        Output::Native
    };
    let key = (index, output);
    let mut hub = HUB.lock().unwrap();
    let id = NEXT_SUB.fetch_add(1, Ordering::Relaxed);
    if let Some(shared) = hub.get(&key).and_then(Weak::upgrade)
        && !shared.stop.load(Ordering::Acquire)
    {
        shared.subs.lock().unwrap().insert(id, fps);
        return Subscription { shared, id };
    }
    let shared = Arc::new(CaptureShared {
        key,
        slot: FrameSlot::new(),
        stop: AtomicBool::new(false),
        subs: Mutex::new(HashMap::from([(id, fps)])),
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
    let (index, output) = shared.key;
    info!(screen = index, ?output, "capture started");
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
            None => match open(index, shared.max_fps(), output) {
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

#[cfg(windows)]
fn open(index: u8, _fps: u8, _output: Output) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(dxgi::DxgiCapturer::open(index)?))
}

#[cfg(target_os = "linux")]
fn open(index: u8, _fps: u8, _output: Output) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(x11::X11Capturer::open(index)?))
}

#[cfg(target_os = "macos")]
fn open(index: u8, fps: u8, output: Output) -> anyhow::Result<Box<dyn Capturer>> {
    Ok(Box::new(sck::SckCapturer::open(index, fps, output)?))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open(_index: u8, _fps: u8, _output: Output) -> anyhow::Result<Box<dyn Capturer>> {
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
