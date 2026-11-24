//! macOS 屏幕采集：ScreenCaptureKit。
//!
//! * 系统只在画面有变化时交付新画面（静止时是 `Idle` 状态的空样本），静止的桌面不产生帧；
//! * 系统在 GPU 上缩放到编码尺寸，输出 NV12（sRGB、BT.709 有限范围）；`CVPixelBuffer`（IOSurface）
//!   原样交给 VideoToolbox（[`GpuFrame`]），CPU 不碰像素；
//! * 通常由控制端绘制光标（通过 `NSCursor` 读取形状）；需要带光标的画面时由系统画进去；
//! * 显示器分辨率或排列变化、流被系统停止时报错，由采集线程重建采集器；
//! * 睡着的显示器采集不到画面（流打不开，或者只给几个空样本）：锁屏之后显示器常常跟着熄了。打开时显示器睡着
//!   （或者列不出来）就叫醒它（[`power::wake_display`]）、报错，采集线程过一会儿再来打开；采集期间显示器睡着了
//!   也报错重来。亮着的锁屏界面照常采集。合着笔记本的盖子时不叫（只会在盖子后面亮 10 秒，画面每 10 秒断一次）：
//!   补屏的虚拟显示器顶上，采集它（见 `virtual_display`）；
//! * 锁屏、解锁时重建采集器：一直开着的流在这前后可能停在上一个画面上，等不来新的画面；
//! * 防窥模式盖在本机显示器上的黑窗口不采集（`SCContentFilter` 去掉它们，见 `virtual_display` 的 `curtain`）：
//!   控制端看到的是下面的画面。黑窗口变了（盖上、撤掉、显示器变了）就重建采集器。

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use airhand_proto::session::CursorShape;
use anyhow::{Context, Result, anyhow, bail};
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_foundation::CFRetained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::kCGDisplayStreamYCbCrMatrix_ITU_R_709_2;
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDisplayIsAsleep, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_screen_capture_kit::{SCFrameStatus, SCStreamConfiguration, SCStreamOutputType};

use super::{CaptureUpdate, Capturer, Frame, FramePool};
use crate::host::macos::{self, Display};
use crate::host::sck::{self, Sample};
use crate::host::{power, virtual_display};

/// 定期检查显示器分辨率 / 排列的间隔
const LAYOUT_CHECK: Duration = Duration::from_secs(2);
/// 定期检查显示器睡着没有、屏幕锁着没有的间隔
const STATE_CHECK: Duration = Duration::from_millis(500);
/// 读取光标形状的间隔
const CURSOR_POLL: Duration = Duration::from_millis(50);
/// 编码线程会拿着几帧（上一帧要留着补画质、重发关键帧，几个会话各拿各的），
/// 拿着的画面系统不能复用：多留几个空位（最多 8）
const QUEUE_DEPTH: isize = 7;

/// GPU 上的一帧：系统缩放好的 NV12（BT.709 有限范围，IOSurface），尺寸就是编码尺寸，原样交给 VideoToolbox。
///
/// 画面属于系统的缓冲池，拿着它系统就不能复用（见 [`QUEUE_DEPTH`]）：所以这种帧不放进帧缓冲池。
#[derive(Clone)]
pub struct GpuFrame {
    source: u64,
    buffer: CFRetained<CVPixelBuffer>,
}

// CVPixelBuffer 的引用计数是线程安全的；系统交出来之后不再写它，这里只读
unsafe impl Send for GpuFrame {}
unsafe impl Sync for GpuFrame {}

impl GpuFrame {
    /// 采集的编号（见 [`super::next_source`]）
    pub fn source(&self) -> u64 {
        self.source
    }

    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.buffer
    }

    pub fn size(&self) -> (u32, u32) {
        (
            CVPixelBufferGetWidth(&self.buffer) as u32,
            CVPixelBufferGetHeight(&self.buffer) as u32,
        )
    }

    /// 转成 BGRA 读进内存（软件编码时用）
    pub(super) fn read_pixels(&self, out: &mut [u8]) -> Result<()> {
        let pb = &*self.buffer;
        let ro = CVPixelBufferLockFlags::ReadOnly;
        if unsafe { CVPixelBufferLockBaseAddress(pb, ro) } != 0 {
            bail!("can't lock the frame");
        }
        let plane = |i: usize| {
            let p = CVPixelBufferGetBaseAddressOfPlane(pb, i) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
            let rows = CVPixelBufferGetHeightOfPlane(pb, i);
            (!p.is_null()).then(|| {
                (
                    unsafe { std::slice::from_raw_parts(p, stride * rows) },
                    stride,
                )
            })
        };
        let r = match (plane(0), plane(1)) {
            (Some(y), Some(uv)) => super::nv12_to_bgra(self.size(), y, uv, &mut Vec::new(), out),
            _ => Err(anyhow!("the frame has no pixel data")),
        };
        unsafe { CVPixelBufferUnlockBaseAddress(pb, ro) };
        r
    }
}

/// 采集流交给采集线程的东西：最新的一帧画面（旧的直接丢弃）
#[derive(Default)]
struct Inbox {
    st: Mutex<InboxState>,
    cv: Condvar,
}

#[derive(Default)]
struct InboxState {
    sample: Option<Sample>,
    stopped: Option<String>,
}

pub struct SckCapturer {
    index: u8,
    display: Display,
    /// 见 [`super::next_source`]
    source: u64,
    config: Retained<SCStreamConfiguration>,
    stream: sck::Stream,
    inbox: Arc<Inbox>,
    fps: u8,
    first: bool,
    cursor: CursorReader,
    layout_checked: Instant,
    /// 打开时屏幕锁着没有（见 [`macos::screen_locked`]）：变了就重建采集器
    locked: Option<bool>,
    state_checked: Instant,
    /// 系统把光标画进了采集到的画面里（要带光标的画面的订阅者单独一路，见 `super` 的"带光标的画面"）
    shows_cursor: bool,
    /// 打开时防窥的黑窗口是哪一版（见 [`virtual_display::cover_generation`]）：变了就重建采集器
    covers: u64,
}

fn frame_interval(fps: u8) -> CMTime {
    unsafe { CMTime::new(1, fps.max(1) as i32) }
}

impl SckCapturer {
    /// `shows_cursor`：让系统把光标画进画面里（给要带光标的画面的订阅者）
    pub fn open(index: u8, fps: u8, max_short: u16, shows_cursor: bool) -> Result<Self> {
        macos::check_screen_permission()?;
        // 显示器睡着了（锁屏之后常见）采集不到：叫醒它，过一会儿再来（见模块说明）。睡着的显示器可能根本列不出来
        let display = match macos::display(index) {
            Ok(d) if !CGDisplayIsAsleep(d.id) => d,
            Ok(_) => {
                power::wake_display();
                bail!("the display is asleep");
            }
            Err(e) => {
                power::wake_display();
                return Err(e);
            }
        };
        let locked = macos::screen_locked();
        // 先记下是哪一版再取窗口：取完之后又变了的话，下一帧就会发现
        let covers = virtual_display::cover_generation();
        let target = sck::shareable_display(display.id, &virtual_display::cover_windows())?;
        let (w, h) = crate::host::encoder::target_size(display.width, display.height, max_short);
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            c.setWidth(w as usize);
            c.setHeight(h as usize);
            c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            c.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
            c.setQueueDepth(QUEUE_DEPTH);
            // 编码尺寸取整到偶数，宽高比和显示器差不到一个像素：铺满，不留黑边（macOS 14 起才能设置）
            if objc2::available!(macos = 14.0) {
                c.setPreservesAspectRatio(false);
            }
            c.setColorSpaceName(kCGColorSpaceSRGB);
            // 平时画面里不要光标（控制端用本地光标显示它）；控制端的游戏模式要系统画上去，光标一动也会出新的一帧
            c.setShowsCursor(shows_cursor);
            c.setMinimumFrameInterval(frame_interval(fps));
            c
        };
        let inbox = Arc::new(Inbox::default());
        let (rx, stop) = (inbox.clone(), inbox.clone());
        let stream = sck::Stream::start(
            &target,
            &config,
            &[SCStreamOutputType::Screen],
            "app.airhand.capture",
            move |sb, kind| {
                if kind != SCStreamOutputType::Screen
                    || sck::frame_status(sb) != Some(SCFrameStatus::Complete.0)
                {
                    return;
                }
                let mut st = rx.st.lock().unwrap();
                st.sample = Some(Sample::retain(sb));
                rx.cv.notify_all();
            },
            move |e| {
                let mut st = stop.st.lock().unwrap();
                st.stopped = Some(e);
                stop.cv.notify_all();
            },
        )?;
        // 注意：tracing 的宏里 `display` 是它自己的函数名，变量换个名字
        let d = &display;
        tracing::info!(
            screen = index,
            id = d.id,
            w = d.width,
            h = d.height,
            scale = d.scale(),
            output_w = w,
            output_h = h,
            ?locked,
            "ScreenCaptureKit capture opened"
        );
        Ok(Self {
            index,
            display,
            source: super::next_source(),
            config,
            stream,
            inbox,
            fps,
            first: true,
            cursor: CursorReader::default(),
            layout_checked: Instant::now(),
            locked,
            state_checked: Instant::now(),
            shows_cursor,
            covers,
        })
    }

    fn frame(&self, sb: &CMSampleBuffer) -> Result<Arc<Frame>> {
        let pb = unsafe { sb.image_buffer() }.ok_or_else(|| anyhow!("the frame has no image"))?;
        if CVPixelBufferGetPixelFormatType(&pb) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
            bail!("unexpected pixel format from ScreenCaptureKit");
        }
        let gpu = GpuFrame {
            source: self.source,
            buffer: pb,
        };
        let (w, h) = gpu.size();
        let mut frame = Frame::empty(w, h);
        // 采集时间用画面的呈现时间（主机时钟），声音包用同一个时钟，音画同步靠它对齐
        if let Some(t) = macos::instant_from_host_time(unsafe { sb.presentation_time_stamp() }) {
            frame.captured = t;
        }
        frame.gpu = Some(gpu);
        Ok(Arc::new(frame))
    }
}

impl Capturer for SckCapturer {
    fn next(
        &mut self,
        timeout: Duration,
        _pool: &mut FramePool,
        _draw: bool,
    ) -> Result<CaptureUpdate> {
        // 防窥的黑窗口变了：没去掉的那几个会把控制端的画面也盖黑
        if virtual_display::cover_generation() != self.covers {
            bail!("the privacy mode's black windows changed");
        }
        if self.layout_checked.elapsed() > LAYOUT_CHECK {
            self.layout_checked = Instant::now();
            if macos::display(self.index).ok().as_ref() != Some(&self.display) {
                bail!("the display layout or resolution changed");
            }
        }
        if self.state_checked.elapsed() > STATE_CHECK {
            self.state_checked = Instant::now();
            if CGDisplayIsAsleep(self.display.id) {
                bail!("the display went to sleep");
            }
            let locked = macos::screen_locked();
            if locked != self.locked {
                bail!(
                    "the screen was {}",
                    if locked == Some(true) {
                        "locked"
                    } else {
                        "unlocked"
                    }
                );
            }
        }
        let mut update = CaptureUpdate::default();
        if let Some(shape) = self.cursor.poll(self.display.scale()) {
            update.cursor_shape = Some(shape);
        }
        if self.first {
            update.cursor_visibility = Some(super::CursorVisibility::Shown);
        }
        let sample = {
            let st = self.inbox.st.lock().unwrap();
            let (mut st, _) = self
                .inbox
                .cv
                .wait_timeout_while(st, timeout.min(CURSOR_POLL * 2), |s| {
                    s.sample.is_none() && s.stopped.is_none()
                })
                .unwrap();
            if let Some(e) = st.stopped.take() {
                bail!("the system stopped the capture ({e})");
            }
            st.sample.take()
        };
        if let Some(s) = sample {
            let frame = self.frame(&s.0).context("reading the frame")?;
            // 系统画了光标：这一路的订阅者都要带光标的画面，就是它
            if self.shows_cursor {
                update.drawn = Some(frame.clone());
            }
            update.frame = Some(frame);
            self.first = false;
        }
        Ok(update)
    }

    fn set_fps(&mut self, fps: u8) {
        if fps != self.fps {
            self.fps = fps;
            unsafe { self.config.setMinimumFrameInterval(frame_interval(fps)) };
            self.stream.update(&self.config);
        }
    }
}

// ───────────────────────────── 光标 ─────────────────────────────

/// 定期读取系统当前的光标形状，变化时才返回。
#[derive(Default)]
struct CursorReader {
    checked: Option<Instant>,
    last: u64,
}

impl CursorReader {
    fn poll(&mut self, scale: f64) -> Option<CursorShape> {
        if self.checked.is_some_and(|t| t.elapsed() < CURSOR_POLL) {
            return None;
        }
        self.checked = Some(Instant::now());
        let shape = autoreleasepool(|_| current_cursor(scale))?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            shape.width,
            shape.height,
            shape.hot_x,
            shape.hot_y,
            &shape.rgba,
        )
            .hash(&mut h);
        let h = h.finish();
        (h != self.last).then(|| {
            self.last = h;
            shape
        })
    }
}

/// 系统当前的光标（按显示器的缩放比例渲染成物理像素大小的 RGBA）。
fn current_cursor(scale: f64) -> Option<CursorShape> {
    // 已标记为过时，但仍然是读取"其他程序设置的光标"的唯一办法（macOS 27 上照常可用）
    #[allow(deprecated)]
    let cursor = objc2_app_kit::NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let hot = cursor.hotSpot();
    let size = image.size();
    let w = (size.width * scale).round() as usize;
    let h = (size.height * scale).round() as usize;
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w as f64, h as f64));
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    let mut rgba = vec![0u8; w * h * 4];
    {
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
        let ctx = unsafe {
            CGBitmapContextCreate(
                rgba.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                Some(&space),
                CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0,
            )
        }?;
        let r = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64, h as f64));
        CGContext::draw_image(Some(&ctx), r, Some(&cg));
    }
    unpremultiply(&mut rgba);
    Some(CursorShape {
        width: w as u16,
        height: h as u16,
        hot_x: ((hot.x * scale).round().max(0.0) as usize).min(w - 1) as u16,
        hot_y: ((hot.y * scale).round().max(0.0) as usize).min(h - 1) as u16,
        rgba,
    })
}

/// 预乘 alpha 的 RGBA → 普通 RGBA
fn unpremultiply(rgba: &mut [u8]) {
    for p in rgba.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiplies() {
        let mut px = [255, 0, 0, 255, 0x80, 0x80, 0x80, 0x80, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [255, 0, 0, 255, 255, 255, 255, 128, 0, 0, 0, 0]);
    }

    /// 读取光标形状不需要任何权限
    #[test]
    fn reads_cursor() {
        let Some(c) = current_cursor(2.0) else {
            println!("没有读到光标（没有图形会话？）");
            return;
        };
        println!(
            "光标 {}x{}，热点 ({}, {})",
            c.width, c.height, c.hot_x, c.hot_y
        );
        assert_eq!(c.rgba.len(), c.width as usize * c.height as usize * 4);
        assert!(c.hot_x < c.width && c.hot_y < c.height);
        assert!(
            c.rgba.as_chunks::<4>().0.iter().any(|p| p[3] == 255),
            "光标是全透明的"
        );
        let mut r = CursorReader::default();
        assert!(r.poll(2.0).is_some());
        r.checked = None;
        assert!(r.poll(2.0).is_none(), "形状没变时不应重复返回");
    }

    /// 需要"屏幕与系统录音"权限。`cargo test --lib sck_capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sck_capture() {
        if !sck::require_permission() {
            return;
        }
        for max_short in [0, 720] {
            let mut cap = SckCapturer::open(0, 30, max_short, false).expect("打开采集");
            let mut pool = FramePool::default();
            let started = Instant::now();
            let frame = loop {
                assert!(started.elapsed() < Duration::from_secs(5), "没有收到画面");
                if let Some(f) = cap
                    .next(Duration::from_millis(100), &mut pool, false)
                    .unwrap()
                    .frame
                {
                    break f;
                }
            };
            let d = &cap.display;
            let size = crate::host::encoder::target_size(d.width, d.height, max_short);
            assert_eq!((frame.width, frame.height), size);
            assert_eq!(frame.gpu.as_ref().expect("画面应当在 GPU 上").size(), size);
            assert!(frame.captured.elapsed() < Duration::from_millis(500));
            // 软件编码需要时仍能按目标尺寸读回 BGRA
            let pixels = frame.pixels().unwrap();
            assert_eq!(pixels.len(), (size.0 * size.1 * 4) as usize);
            assert!(pixels.iter().any(|&b| b != 0));
            cap.set_fps(10);
        }
    }

    /// 本机的主显示器调黑（gamma 表全是 0，防窥模式就是这样做的，见 `virtual_display` 的 `curtain`）时采集到的画面
    /// 不变：gamma 表在帧缓冲后面。需要"屏幕与系统录音"权限；本机屏幕会黑 1 秒左右（测试进程崩溃了系统也会恢复）。
    /// `cargo test --lib gamma_is_not_captured -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gamma_is_not_captured() {
        use objc2_core_graphics::{
            CGDisplayRestoreColorSyncSettings, CGError, CGSetDisplayTransferByTable,
        };
        if !sck::require_permission() {
            return;
        }
        let id = macos::display(0).expect("没有显示器").id;
        // 画面不变时 ScreenCaptureKit 不交新的帧：每次重新打开采集，取第一帧的平均亮度
        let brightness = || {
            let mut cap = SckCapturer::open(0, 30, 360, false).expect("打开采集");
            let mut pool = FramePool::default();
            let started = Instant::now();
            let frame = loop {
                assert!(started.elapsed() < Duration::from_secs(5), "没有收到画面");
                if let Some(f) = cap
                    .next(Duration::from_millis(100), &mut pool, false)
                    .unwrap()
                    .frame
                {
                    break f;
                }
            };
            let px = frame.pixels().unwrap().as_chunks::<4>().0;
            px.iter()
                .map(|p| p[0] as f64 + p[1] as f64 + p[2] as f64)
                .sum::<f64>()
                / px.len() as f64
        };
        let before = brightness();
        let zeros = [0.0f32; 256];
        let r = unsafe {
            CGSetDisplayTransferByTable(id, 256, zeros.as_ptr(), zeros.as_ptr(), zeros.as_ptr())
        };
        std::thread::sleep(Duration::from_millis(300));
        let during = brightness();
        CGDisplayRestoreColorSyncSettings();
        println!("采集到的平均亮度：调黑前 {before:.1}，调黑时 {during:.1}");
        assert_eq!(r, CGError::Success);
        assert!(
            (during - before).abs() <= before * 0.2 + 3.0,
            "调黑之后采集到的画面变了"
        );
    }
}
