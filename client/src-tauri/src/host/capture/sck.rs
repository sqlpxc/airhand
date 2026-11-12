//! macOS 屏幕采集：ScreenCaptureKit。
//!
//! * 系统只在画面有变化时交付新画面（静止时是 `Idle` 状态的空样本），静止的桌面不产生帧；
//! * 两种画面（见 [`Output`]）：
//!   * 缩放好的（`Output::Scaled`，GPU 零拷贝）：系统采集时就在 GPU 上缩放成编码尺寸、转换成 NV12（BT.709 有限范围），
//!     `CVPixelBuffer`（IOSurface）原样交给 VideoToolbox（[`GpuFrame`]），CPU 不碰像素；
//!   * 原始分辨率（`Output::Native`）：物理像素（Retina 屏是"点"的两倍）的 BGRA 复制进内存，在 CPU 上缩放（清楚优先时）；
//! * 都转换到 sRGB；
//! * 画面里不含光标：光标形状另外读取（`NSCursor`），与 Windows / Linux 一样由控制端本地绘制；
//! * 显示器分辨率或排列变化、流被系统停止时报错，由采集线程重建采集器。

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
    CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo, CGImageByteOrderInfo,
    kCGColorSpaceSRGB,
};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVPixelFormatType_32BGRA, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_screen_capture_kit::{SCFrameStatus, SCStreamConfiguration, SCStreamOutputType};

use super::{CaptureUpdate, Capturer, Frame, FramePool, Output};
use crate::host::macos::{self, Display};
use crate::host::sck::{self, Sample};

/// 定期检查显示器分辨率 / 排列的间隔
const LAYOUT_CHECK: Duration = Duration::from_secs(2);
/// 读取光标形状的间隔
const CURSOR_POLL: Duration = Duration::from_millis(50);
/// 系统内部最多排队的画面数
const QUEUE_DEPTH: isize = 4;
/// 缩放好的画面直接交给编码器，编码线程会拿着几帧（上一帧要留着补画质、重发关键帧，几个会话各拿各的），
/// 拿着的画面系统不能复用：多留几个空位（最多 8）
const GPU_QUEUE_DEPTH: isize = 7;

/// GPU 上的一帧：系统缩放好的 NV12（BT.709 有限范围，IOSurface），尺寸就是编码尺寸，原样交给 VideoToolbox。
///
/// 画面属于系统的缓冲池，拿着它系统就不能复用（见 [`GPU_QUEUE_DEPTH`]）：所以这种帧不放进帧缓冲池。
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
    /// 系统直接缩放成这个尺寸的 NV12（`Output::Scaled`）；`None`：原始分辨率的 BGRA
    scaled: Option<(u32, u32)>,
    /// 见 [`super::next_source`]
    source: u64,
    config: Retained<SCStreamConfiguration>,
    stream: sck::Stream,
    inbox: Arc<Inbox>,
    fps: u8,
    first: bool,
    cursor: CursorReader,
    layout_checked: Instant,
}

fn frame_interval(fps: u8) -> CMTime {
    unsafe { CMTime::new(1, fps.max(1) as i32) }
}

impl SckCapturer {
    pub fn open(index: u8, fps: u8, output: Output) -> Result<Self> {
        macos::check_screen_permission()?;
        let display = macos::display(index)?;
        let target = sck::shareable_display(display.id)?;
        let scaled = match output {
            Output::Scaled(max_short) => Some(crate::host::encoder::target_size(
                display.width,
                display.height,
                max_short,
            )),
            Output::Native => None,
        };
        let config = unsafe {
            let c = SCStreamConfiguration::new();
            match scaled {
                Some((w, h)) => {
                    c.setWidth(w as usize);
                    c.setHeight(h as usize);
                    c.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
                    // 和 VideoToolbox 的设置、内存那条路的色彩转换一样：BT.709
                    c.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
                    c.setQueueDepth(GPU_QUEUE_DEPTH);
                    // 编码尺寸取整到偶数，宽高比和显示器差不到一个像素：铺满，不留黑边（macOS 14 起才能设置）
                    if objc2::available!(macos = 14.0) {
                        c.setPreservesAspectRatio(false);
                    }
                }
                None => {
                    c.setWidth(display.width as usize);
                    c.setHeight(display.height as usize);
                    c.setPixelFormat(kCVPixelFormatType_32BGRA);
                    c.setQueueDepth(QUEUE_DEPTH);
                }
            }
            c.setColorSpaceName(kCGColorSpaceSRGB);
            c.setShowsCursor(false);
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
            ?scaled,
            "ScreenCaptureKit capture opened"
        );
        Ok(Self {
            index,
            display,
            scaled,
            source: super::next_source(),
            config,
            stream,
            inbox,
            fps,
            first: true,
            cursor: CursorReader::default(),
            layout_checked: Instant::now(),
        })
    }

    /// 样本里的画面：缩放好的直接留在 GPU 上，原始分辨率的复制进帧缓冲（紧密排列的 BGRA）。
    fn frame(&self, sb: &CMSampleBuffer, pool: &mut FramePool) -> Result<Arc<Frame>> {
        let pb = unsafe { sb.image_buffer() }.ok_or_else(|| anyhow!("the frame has no image"))?;
        // 采集时间用画面的呈现时间（主机时钟），声音包用同一个时钟，音画同步靠它对齐
        let captured = macos::instant_from_host_time(unsafe { sb.presentation_time_stamp() });
        if self.scaled.is_some() {
            if CVPixelBufferGetPixelFormatType(&pb)
                != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
            {
                bail!("unexpected pixel format from ScreenCaptureKit");
            }
            let gpu = GpuFrame {
                source: self.source,
                buffer: pb,
            };
            let (w, h) = gpu.size();
            let mut f = Frame::empty(w, h);
            if let Some(t) = captured {
                f.captured = t;
            }
            f.gpu = Some(gpu);
            return Ok(Arc::new(f));
        }
        if CVPixelBufferGetPixelFormatType(&pb) != kCVPixelFormatType_32BGRA {
            bail!("unexpected pixel format from ScreenCaptureKit");
        }
        let ro = CVPixelBufferLockFlags::ReadOnly;
        if unsafe { CVPixelBufferLockBaseAddress(&pb, ro) } != 0 {
            bail!("can't lock the frame");
        }
        let (w, h) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
        let pitch = CVPixelBufferGetBytesPerRow(&pb);
        let src = CVPixelBufferGetBaseAddress(&pb) as *const u8;
        let row = w * 4;
        let result = if src.is_null() || pitch < row || w == 0 || h == 0 {
            Err(anyhow!("the frame has no pixel data"))
        } else {
            let mut frame = pool.checkout(w as u32, h as u32);
            let f = Arc::get_mut(&mut frame).expect("unique frame");
            if let Some(t) = captured {
                f.captured = t;
            }
            let dst = f.pixels_mut().as_mut_ptr();
            unsafe {
                if pitch == row {
                    std::ptr::copy_nonoverlapping(src, dst, row * h);
                } else {
                    for y in 0..h {
                        std::ptr::copy_nonoverlapping(src.add(y * pitch), dst.add(y * row), row);
                    }
                }
            }
            pool.keep(&frame);
            Ok(frame)
        };
        unsafe { CVPixelBufferUnlockBaseAddress(&pb, ro) };
        result
    }
}

impl Capturer for SckCapturer {
    fn next(&mut self, timeout: Duration, pool: &mut FramePool) -> Result<CaptureUpdate> {
        if self.layout_checked.elapsed() > LAYOUT_CHECK {
            self.layout_checked = Instant::now();
            if macos::display(self.index).ok().as_ref() != Some(&self.display) {
                bail!("the display layout or resolution changed");
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
            update.frame = Some(self.frame(&s.0, pool).context("reading the frame")?);
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
        let mut cap = SckCapturer::open(0, 30, Output::Native).expect("打开采集");
        let mut pool = FramePool::default();
        let started = Instant::now();
        let frame = loop {
            assert!(started.elapsed() < Duration::from_secs(5), "没有收到画面");
            if let Some(f) = cap
                .next(Duration::from_millis(100), &mut pool)
                .unwrap()
                .frame
            {
                break f;
            }
        };
        assert_eq!(
            (frame.width, frame.height),
            (cap.display.width, cap.display.height)
        );
        let age = frame.captured.elapsed();
        println!(
            "采集 {}x{}，采集时间戳距今 {:?}",
            frame.width, frame.height, age
        );
        assert!(age < Duration::from_millis(500));
        assert!(frame.pixels().unwrap().iter().any(|&b| b != 0), "画面全黑");
        cap.set_fps(10);
    }

    /// 系统直接缩放成编码尺寸的 NV12：画面在 GPU 上，尺寸和 target_size 一样；需要时转成 BGRA 读回来，
    /// 和原始分辨率的画面大致一样（缩放过，比较整幅的平均亮度）。
    /// 需要"屏幕与系统录音"权限。`cargo test --lib sck_scaled -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn sck_scaled() {
        if !sck::require_permission() {
            return;
        }
        let grab = |output: Output| {
            let mut cap = SckCapturer::open(0, 30, output).expect("打开采集");
            let mut pool = FramePool::default();
            let started = Instant::now();
            loop {
                assert!(started.elapsed() < Duration::from_secs(5), "没有收到画面");
                if let Some(f) = cap
                    .next(Duration::from_millis(100), &mut pool)
                    .unwrap()
                    .frame
                {
                    break (f, cap.display.clone());
                }
            }
        };
        let (scaled, d) = grab(Output::Scaled(720));
        let (w, h) = crate::host::encoder::target_size(d.width, d.height, 720);
        assert_eq!((scaled.width, scaled.height), (w, h));
        let g = scaled.gpu.as_ref().expect("缩放好的画面应当在 GPU 上");
        assert_eq!(g.size(), (w, h));
        let (native, _) = grab(Output::Native);
        assert!(native.gpu.is_none());
        let mean = |p: &[u8]| p.iter().map(|&v| v as u64).sum::<u64>() as f64 / p.len() as f64;
        let (a, b) = (
            mean(scaled.pixels().unwrap()),
            mean(native.pixels().unwrap()),
        );
        println!(
            "缩放 {w}x{h}，原始 {}x{}，平均亮度 {a:.1} / {b:.1}",
            native.width, native.height
        );
        assert!((a - b).abs() < 12.0, "缩放后的画面和原始画面差得太多");
    }
}
