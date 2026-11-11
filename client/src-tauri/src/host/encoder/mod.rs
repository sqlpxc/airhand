//! 视频编码。
//!
//! * **硬件编码**（优先）：Windows Media Foundation 硬件 MFT（NVENC / Quick Sync / AMF），
//!   支持 H.264 / H.265 / AV1；macOS VideoToolbox，支持 H.264 / H.265。几乎不占用 CPU；
//! * **软件编码**（兜底）：OpenH264 屏幕内容模式。
//!
//! 被控端只在画面变化时才编码，静止画面完全不占带宽。
//!
//! # 画面怎么到编码器
//!
//! * **GPU 零拷贝**（硬件编码、画面在 GPU 上时）：画面不离开显卡。Windows 用 `ID3D11VideoProcessor` 缩放并转成 NV12，
//!   `MFCreateDXGISurfaceBuffer` 直接交给硬件 MFT（见 mf.rs）；macOS 的 ScreenCaptureKit 采集时就按编码尺寸输出 NV12，
//!   `CVPixelBuffer` 原样交给 VideoToolbox（见 vt.rs、`capture/sck.rs`）。省掉了读回、大块拷贝、CPU 缩放和色彩转换
//!   （4K、Retina 屏上这些加起来每帧十几毫秒）；
//! * **CPU**：BGRA →（SIMD 缩放，分条多线程）→（SIMD 色彩转换）YUV 4:2:0。软件编码、Linux、显卡不支持时走这条路；
//!   画质设置选了清楚优先（[`VideoPriority::Clarity`]）、而且需要缩小时也走这条路：GPU 的缩放滤镜由驱动决定，
//!   缩小很多时文字不一定有 Catmull-Rom 清楚。

#[cfg(windows)]
mod mf;
mod software;
#[cfg(target_os = "macos")]
mod vt;

use std::sync::OnceLock;

use airhand_proto::session::{Codec, EncoderInfo, QualitySettings, VideoPriority};
use anyhow::{Context, Result, anyhow, bail};
use fast_image_resize as fir;
use tracing::info;
#[cfg(any(windows, target_os = "macos"))]
use tracing::warn;

use super::capture::{Frame, GpuFrame};

/// 硬件编码器普遍支持的最大分辨率（同时也是 OpenH264 Level 5.2 的上限）
const MAX_LONG: u32 = 3840;
const MAX_SHORT: u32 = 2160;

/// 缩放时每条至少这么多行：太小的画面分条不划算
const MIN_STRIPE_ROWS: u32 = 128;

/// 缩放分成几条同时做（编码线程自己做其中一条）：一半的核，最多 4 条。
///
/// 缩放在每一帧的关键路径上（采集 → 缩放 → 色彩转换 → 编码 → 发送），省下的时间直接就是每帧的延迟。
/// 实测 Apple M1、30 fps、2880×1800 → 1728×1080：单线程约 18.5 ms，分 4 条约 9 ms；
/// 代价是 CPU 总时间多出约 80%（多出来的核频率低一些，有的落在能效核上）。只在画面变化、需要缩放时才有
fn resize_threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map_or(1, |n| n.get() / 2)
            .clamp(1, 4)
    })
}

/// 把 `src` 缩放进 `dst`（紧密排列的 `w`×`h` BGRA）：按输出的行分成几条，每条用自己的缩放器同时缩放。
///
/// 每条只裁出对应的源区域，卷积仍然会用到区域外的像素，所以结果与整幅缩放逐像素相同
///（见测试 `striped_resize_matches_whole`）。
fn resize_striped(
    src: &fir::images::ImageRef,
    (src_w, src_h): (u32, u32),
    dst: &mut [u8],
    (w, h): (u32, u32),
    resizers: &mut [fir::Resizer],
) -> Result<()> {
    let n = (resizers.len() as u32).min(h / MIN_STRIPE_ROWS).max(1);
    let rows = h.div_ceil(n);
    let scale = src_h as f64 / h as f64;
    // 缩小用 Catmull-Rom 卷积：文字边缘比双线性清晰得多。Retina 屏（例如 2880×1800）缩到 1080p
    // 是 0.6 这样的非整数倍，macOS 的细字体用双线性缩完明显发虚（单线程时 Catmull-Rom 比双线性慢约 60%）
    let job = |r: &mut fir::Resizer, y0: u32, band: &mut [u8]| -> Result<()> {
        let rows = (band.len() / (w as usize * 4)) as u32;
        let mut img = fir::images::Image::from_slice_u8(w, rows, band, fir::PixelType::U8x4)
            .map_err(|e| anyhow!("invalid target image: {e}"))?;
        let opts = fir::ResizeOptions::new()
            .resize_alg(fir::ResizeAlg::Convolution(fir::FilterType::CatmullRom))
            .use_alpha(false)
            .crop(0.0, y0 as f64 * scale, src_w as f64, rows as f64 * scale);
        r.resize(src, &mut img, &opts).context("scaling failed")
    };
    let job = &job;
    let mut bands = dst
        .chunks_mut((rows * w * 4) as usize)
        .zip(resizers.iter_mut())
        .enumerate();
    let Some((_, (first, first_resizer))) = bands.next() else {
        return Ok(());
    };
    std::thread::scope(|s| {
        let others: Vec<_> = bands
            .map(|(i, (band, r))| {
                s.spawn(move || {
                    super::capture::raise_priority();
                    job(r, i as u32 * rows, band)
                })
            })
            .collect();
        let mine = job(first_resizer, 0, first);
        for t in others {
            t.join()
                .map_err(|_| anyhow!("a scaling thread panicked"))??;
        }
        mine
    })
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub key: bool,
}

pub trait VideoEncoder {
    fn info(&self) -> &EncoderInfo;
    /// 运行时调整码率（不重建编码器、不产生关键帧）。
    fn set_bitrate(&mut self, kbps: u32);
    /// 编码一帧 BGRA（尺寸已经等于输出尺寸，stride = width * 4）。
    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>>;
    /// 这个编码器直接编码 GPU 上的画面（[`Self::encode_gpu`]）。是的话只会收到同一路采集的画面
    fn takes_gpu(&self) -> bool {
        false
    }
    /// 编码 GPU 上的一帧：尺寸是采集的尺寸，编码器自己在 GPU 上缩放、转换颜色
    fn encode_gpu(&mut self, _frame: &GpuFrame, _force_key: bool) -> Result<Option<EncodedFrame>> {
        bail!("this encoder only takes frames in memory")
    }
}

/// 采集的 `src_w`×`src_h` 在这个短边上限下要不要缩放（只是奇数尺寸多出来的 1 像素不算，裁掉就行）
pub fn needs_scaling(src_w: u32, src_h: u32, max_short: u16) -> bool {
    let (w, h) = target_size(src_w, src_h, max_short);
    !crop_only((src_w, src_h), (w, h))
}

fn crop_only((src_w, src_h): (u32, u32), (w, h): (u32, u32)) -> bool {
    src_w >= w && src_h >= h && src_w - w <= 1 && src_h - h <= 1
}

/// 根据短边上限计算输出分辨率（偶数、不超过编码器上限）。`max_short = 0` 表示不限制。
pub fn target_size(src_w: u32, src_h: u32, max_short: u16) -> (u32, u32) {
    let (mut w, mut h) = (src_w as f64, src_h as f64);
    let short = w.min(h);
    if max_short > 0 && short > max_short as f64 {
        let f = max_short as f64 / short;
        w *= f;
        h *= f;
    }
    let (max_w, max_h) = if w >= h {
        (MAX_LONG, MAX_SHORT)
    } else {
        (MAX_SHORT, MAX_LONG)
    };
    let f = (max_w as f64 / w).min(max_h as f64 / h).min(1.0);
    w *= f;
    h *= f;
    let even = |v: f64| ((v.round() as u32) & !1).max(64);
    (even(w), even(h))
}

/// 被控端支持的编码格式（H.264 永远可用，其余取决于是否有硬件编码器）。
pub fn available_codecs() -> Vec<Codec> {
    static CACHE: OnceLock<Vec<Codec>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            #[allow(unused_mut)]
            let mut list = vec![Codec::H264];
            #[cfg(any(windows, target_os = "macos"))]
            for c in [Codec::H265, Codec::AV1] {
                if has_hardware(c) {
                    list.push(c);
                }
            }
            info!(?list, "available codecs");
            list
        })
        .clone()
}

#[cfg(windows)]
fn has_hardware(codec: Codec) -> bool {
    mf::has_hardware(codec)
}

#[cfg(target_os = "macos")]
fn has_hardware(codec: Codec) -> bool {
    vt::has_hardware(codec)
}

#[cfg(windows)]
fn hardware_encoder(
    codec: Codec,
    (w, h): (u32, u32),
    fps: u8,
    kbps: u32,
    gpu: Option<&GpuFrame>,
) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(mf::MfEncoder::new(codec, w, h, fps, kbps, gpu)?))
}

#[cfg(target_os = "macos")]
fn hardware_encoder(
    codec: Codec,
    (w, h): (u32, u32),
    fps: u8,
    kbps: u32,
    gpu: Option<&GpuFrame>,
) -> Result<Box<dyn VideoEncoder>> {
    Ok(Box::new(vt::VtEncoder::new(codec, w, h, fps, kbps, gpu)?))
}

/// 缩放 + 编码。持有可复用的缩放缓冲区。
pub struct Encoder {
    inner: Box<dyn VideoEncoder>,
    /// 每条一个（见 [`resize_striped`]），缓冲区逐帧复用
    resizers: Vec<fir::Resizer>,
    scaled: Vec<u8>,
    src_w: u32,
    src_h: u32,
    fps: u8,
    max_short: u16,
    codec: Codec,
    priority: VideoPriority,
    /// 创建时画面来自哪一路 GPU 采集（[`GpuFrame::source`]）。采集重新打开了就重建：
    /// GPU 零拷贝的编码器只能处理创建时那个设备上的画面，内存里的编码器也趁这个机会试试 GPU 零拷贝
    source: Option<u64>,
    /// 直接编码 GPU 上的画面（GPU 零拷贝）
    gpu: bool,
}

impl Encoder {
    /// 为 `frame` 这样的画面创建编码器：按"请求的格式硬件 → H.264 硬件 → OpenH264"顺序尝试。
    /// 画面在 GPU 上、`allow_gpu` 而且不是"清楚优先并且要缩小"时，硬件编码器先试 GPU 零拷贝（见模块说明）。
    ///
    /// 格式的优先级（AV1 > HEVC > H.264）由控制端按双方都支持的格式来选（见 [`available_codecs`]），
    /// 这里不自作主张改用别的高效格式：控制端不一定能解码，只有 H.264 是双方都一定支持的。
    pub fn new(
        frame: &Frame,
        s: &QualitySettings,
        kbps: u32,
        allow_hw: bool,
        allow_gpu: bool,
    ) -> Result<Self> {
        let (src_w, src_h) = (frame.width, frame.height);
        let (w, h) = target_size(src_w, src_h, s.max_short);
        let priority = s.priority.or_latency();
        // 清楚优先、又要缩小：在 CPU 上用 Catmull-Rom 缩放
        let gpu = frame.gpu.as_ref().filter(|_| {
            allow_gpu
                && (priority == VideoPriority::Latency || !needs_scaling(src_w, src_h, s.max_short))
        });
        #[allow(unused_mut)]
        let mut inner: Option<Box<dyn VideoEncoder>> = None;

        #[cfg(any(windows, target_os = "macos"))]
        if allow_hw {
            let mut order = vec![s.codec];
            if s.codec != Codec::H264 {
                order.push(Codec::H264);
            }
            for codec in order {
                if codec != Codec::H264 && !available_codecs().contains(&codec) {
                    continue;
                }
                match hardware_encoder(codec, (w, h), s.fps, kbps, gpu) {
                    Ok(e) => {
                        inner = Some(e);
                        break;
                    }
                    Err(e) => warn!(?codec, "hardware encoder unavailable: {e:#}"),
                }
            }
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = (allow_hw, gpu);

        let inner = match inner {
            Some(e) => e,
            None => {
                Box::new(software::OpenH264Encoder::new(w, h, s.fps, kbps).context("OpenH264")?)
            }
        };
        let gpu = gpu.is_some() && inner.takes_gpu();
        let i = inner.info();
        info!(encoder = %i.name, hw = i.hardware, gpu, codec = ?i.codec, src_w, src_h, w, h, kbps, "encoder created");
        Ok(Self {
            inner,
            resizers: (0..resize_threads()).map(|_| fir::Resizer::new()).collect(),
            scaled: Vec::new(),
            src_w,
            src_h,
            fps: s.fps,
            max_short: s.max_short,
            codec: s.codec,
            priority,
            source: frame.gpu.as_ref().map(GpuFrame::source),
            gpu,
        })
    }

    /// 画面的尺寸、来源或需要重建编码器的参数发生变化时返回 false。
    pub fn compatible(&self, frame: &Frame, s: &QualitySettings) -> bool {
        self.src_w == frame.width
            && self.src_h == frame.height
            && self.source == frame.gpu.as_ref().map(GpuFrame::source)
            && self.fps == s.fps
            && self.max_short == s.max_short
            && self.codec == s.codec
            && self.priority == s.priority.or_latency()
    }

    pub fn info(&self) -> &EncoderInfo {
        self.inner.info()
    }

    /// 画面不离开显卡（GPU 零拷贝）
    pub fn uses_gpu(&self) -> bool {
        self.gpu
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.inner.set_bitrate(kbps);
    }

    pub fn encode(&mut self, frame: &Frame, force_key: bool) -> Result<Option<EncodedFrame>> {
        if self.gpu
            && let Some(g) = &frame.gpu
        {
            return self.inner.encode_gpu(g, force_key);
        }
        let (w, h) = (self.inner.info().width, self.inner.info().height);
        let data = frame.pixels()?;
        // 尺寸一致（或仅因奇数尺寸多出 1 像素）时直接使用原图，不缩放
        let crop_only = crop_only((frame.width, frame.height), (w, h));
        let bgra: &[u8] = if crop_only && frame.width == w {
            &data[..(w * h * 4) as usize]
        } else if crop_only {
            // 宽度为奇数：逐行裁掉最后一列
            self.scaled.resize((w * h * 4) as usize, 0);
            let (src_row, dst_row) = (frame.width as usize * 4, w as usize * 4);
            for y in 0..h as usize {
                self.scaled[y * dst_row..(y + 1) * dst_row]
                    .copy_from_slice(&data[y * src_row..y * src_row + dst_row]);
            }
            &self.scaled
        } else {
            let src =
                fir::images::ImageRef::new(frame.width, frame.height, data, fir::PixelType::U8x4)
                    .map_err(|e| anyhow!("invalid source image: {e}"))?;
            self.scaled.resize((w * h * 4) as usize, 0);
            resize_striped(
                &src,
                (frame.width, frame.height),
                &mut self.scaled,
                (w, h),
                &mut self.resizers,
            )?;
            &self.scaled
        };
        self.inner.encode(bgra, force_key)
    }
}

/// 在 Annex-B 码流中查找 NAL 类型（用于判断是否包含参数集 / 关键帧）。
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
pub(crate) fn annexb_nal_types(data: &[u8], hevc: bool) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        while i + 3 < data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                let b = data[i + 3];
                i += 3;
                return Some(if hevc { (b >> 1) & 0x3f } else { b & 0x1f });
            }
            i += 1;
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_needed() {
        assert!(!needs_scaling(1920, 1080, 1080));
        assert!(!needs_scaling(1921, 1081, 0));
        assert!(!needs_scaling(1280, 720, 1080));
        assert!(needs_scaling(3840, 2160, 1080));
        assert!(needs_scaling(2880, 1800, 1080));
        // 不限制也要缩到编码器的上限以内
        assert!(needs_scaling(5120, 2880, 0));
    }

    #[test]
    fn sizes() {
        assert_eq!(target_size(1920, 1080, 0), (1920, 1080));
        assert_eq!(target_size(1921, 1081, 0), (1920, 1080));
        assert_eq!(target_size(5120, 2880, 0), (3840, 2160));
        assert_eq!(target_size(1440, 2560, 0), (1440, 2560));
        assert_eq!(target_size(3840, 2160, 1080), (1920, 1080));
        assert_eq!(target_size(2560, 1600, 1080), (1728, 1080));
        assert_eq!(target_size(1280, 720, 1080), (1280, 720));
        assert_eq!(target_size(1080, 1920, 720), (720, 1280));
    }

    #[test]
    fn nal_scan() {
        let d = [0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 1];
        assert_eq!(
            annexb_nal_types(&d, false).collect::<Vec<_>>(),
            vec![7, 8, 5]
        );
    }

    fn synthetic(w: u32, h: u32) -> Frame {
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                data[i] = (x % 256) as u8;
                data[i + 1] = (y % 256) as u8;
                data[i + 2] = ((x / 8 + y / 8) % 2 * 255) as u8;
                data[i + 3] = 255;
            }
        }
        Frame::from_pixels(w, h, data, std::time::Instant::now())
    }

    fn check(codec: Codec, allow_hw: bool) -> Option<String> {
        let frame = synthetic(1281, 721);
        let s = QualitySettings {
            codec,
            max_short: 0,
            ..Default::default()
        };
        let mut enc = Encoder::new(&frame, &s, 3000, allow_hw, true).unwrap();
        assert!(!enc.uses_gpu());
        if enc.info().codec != codec {
            return None;
        }
        assert_eq!((enc.info().width, enc.info().height), (1280, 720));
        let mut got_key = false;
        for i in 0..10 {
            if let Some(f) = enc.encode(&frame, i == 0).unwrap() {
                got_key |= f.key;
                assert!(!f.data.is_empty());
            }
        }
        enc.set_bitrate(1000);
        let _ = enc.encode(&frame, true).unwrap();
        assert!(got_key, "{codec:?} 没有产生关键帧");
        Some(enc.info().name.clone())
    }

    /// 分条缩放与整幅缩放逐像素相同（条与条之间没有接缝）
    #[test]
    fn striped_resize_matches_whole() {
        let src = synthetic(2880, 1800);
        let img =
            fir::images::ImageRef::new(2880, 1800, src.pixels().unwrap(), fir::PixelType::U8x4)
                .unwrap();
        for (w, h) in [
            (1728u32, 1080u32),
            (1590, 994),
            (1554, 972),
            (576, 360),
            (320, 200),
        ] {
            let mut whole = vec![0u8; (w * h * 4) as usize];
            let mut one = [fir::Resizer::new()];
            resize_striped(&img, (2880, 1800), &mut whole, (w, h), &mut one).unwrap();
            for n in [2, 3, 4] {
                let mut striped = vec![1u8; whole.len()];
                let mut rs: Vec<_> = (0..n).map(|_| fir::Resizer::new()).collect();
                resize_striped(&img, (2880, 1800), &mut striped, (w, h), &mut rs).unwrap();
                assert!(striped == whole, "{w}x{h} 分 {n} 条与整幅缩放不同");
            }
        }
    }

    #[test]
    fn software_h264() {
        assert_eq!(check(Codec::H264, false).as_deref(), Some("OpenH264"));
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn hardware_all() {
        for c in available_codecs() {
            match check(c, true) {
                Some(name) => println!("{c:?}: {name}"),
                None => println!("{c:?}: 回退到其他编码器"),
            }
        }
    }
}
