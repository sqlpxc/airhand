//! Windows Media Foundation 硬件编码（NVENC / Intel Quick Sync / AMD AMF 都以硬件 MFT 形式提供）。
//!
//! * 低延迟模式、无 B 帧、超长 GOP（只在需要时插入关键帧）；
//! * 硬件 MFT 通常是异步的，这里用事件轮询把它变成"一进一出"的同步调用；
//! * 画面的两种来路：
//!   * **GPU 零拷贝**（[`GpuInput`]）：采集的纹理在同一个 D3D11 设备上用视频处理器（`ID3D11VideoProcessor`）
//!     缩放、旋转、转成 NV12，`MFCreateDXGISurfaceBuffer` 包起来直接交给编码器（`IMFDXGIDeviceManager` 共用设备）。
//!     只用和采集同一块显卡上的编码器（双显卡的笔记本上另一块显卡的编码器拿不到这个设备的纹理）；
//!   * **内存**：BGRA 直接转换进 MF 缓冲区（NV12），编码器自己再传到显卡上。

use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use airhand_proto::session::{Codec, EncoderInfo};
use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};
use yuvutils_rs::{YuvBiPlanarImageMut, YuvConversionMode, YuvRange, YuvStandardMatrix};

use super::{EncodedFrame, LtrControl, LtrReport, VideoEncoder, annexb_nal_types};
use crate::host::capture::{D3dSource, GpuFrame};

fn subtype(c: Codec) -> GUID {
    match c {
        // 不认识的格式不会走到这里：Encoder::new 只用认识的格式
        Codec::H264 | Codec::Unknown(_) => MFVideoFormat_H264,
        Codec::H265 => MFVideoFormat_HEVC,
        Codec::AV1 => MFVideoFormat_AV1,
    }
}

/// mfplat.dll 是延迟加载的：先确认系统里有它（Windows N 版可能没有），否则调用会直接崩溃。
fn mf_available() -> bool {
    use windows::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe {
        LoadLibraryExW(
            windows::core::w!("mfplat.dll"),
            None,
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
        .is_ok()
    })
}

fn ensure_init() -> Result<()> {
    static MF: OnceLock<bool> = OnceLock::new();
    if !mf_available() {
        bail!("Media Foundation (mfplat.dll) is missing");
    }
    unsafe {
        // 每个线程都需要初始化 COM（重复调用是安全的）
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    if *MF.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok() }) {
        Ok(())
    } else {
        bail!("MFStartup failed")
    }
}

fn enum_hardware(codec: Codec) -> Vec<IMFActivate> {
    if ensure_init().is_err() {
        return Vec::new();
    }
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype(codec),
    };
    let mut pp: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    unsafe {
        if MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            None,
            Some(&output),
            &mut pp,
            &mut n,
        )
        .is_err()
            || pp.is_null()
        {
            return Vec::new();
        }
        let list = std::slice::from_raw_parts_mut(pp, n as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(pp as *const _));
        list
    }
}

pub fn has_hardware(codec: Codec) -> bool {
    !enum_hardware(codec).is_empty()
}

fn friendly_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len)
            .is_ok()
            && !p.is_null()
        {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
    }
    "Media Foundation encoder".into()
}

/// 这个编码器的长期参考帧信得过吗。恢复帧在报告里说只参考了长期参考帧、实际上却还参考了被放弃的短期参考帧的话，
/// 控制端解出来是花屏，而且看不出来（只看报告的冒烟测试也看不出来）。所以只用在：
///
/// * H.264：微软的文档只规定了 H.264 编码器怎么做，实测 NVIDIA、Intel 的编码器都照做；
/// * NVIDIA 的 AV1：实测照做。
///
/// HEVC 不用：NVIDIA 的编码器恢复之后还在参考被放弃的帧（解出来和不丢帧时不一样），Intel 的不支持。
/// 别的编码器用 `ltr_recovery_decodes_without_the_lost_frames` 测过之后再加
fn ltr_trusted(codec: Codec, name: &str) -> bool {
    match codec {
        Codec::H264 => true,
        Codec::AV1 => name.contains("NVIDIA"),
        Codec::H265 | Codec::Unknown(_) => false,
    }
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

// ───────────────────────────── GPU 零拷贝 ─────────────────────────────

/// 视频处理器输出的 NV12 纹理轮流用几个：编码器可能还拿着上一帧
const GPU_TARGETS: usize = 3;

/// GPU 零拷贝：采集的纹理（BGRA，没有旋转过）在同一个设备上用视频处理器缩放、旋转、转成 NV12（BT.709 有限范围，
/// 和内存那条路一样），直接交给编码器。
pub(crate) struct GpuInput {
    /// 只处理这一路采集的画面（见 [`GpuFrame::source`]）
    source: u64,
    manager: IMFDXGIDeviceManager,
    video_device: ID3D11VideoDevice,
    video_ctx: ID3D11VideoContext,
    proc_enum: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    targets: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

impl GpuInput {
    /// 处理 `g` 这样的画面（同一路采集、同样的尺寸和旋转方向），输出 `w`×`h`。
    /// 显卡的视频处理器不支持需要的格式、旋转时出错
    pub(crate) fn new(g: &GpuFrame, w: u32, h: u32, fps: u8) -> Result<Self> {
        let d3d: &D3dSource = g.d3d();
        let (in_w, in_h) = g.texture_size();
        let degrees = g.rotation_degrees();
        unsafe {
            let video_device: ID3D11VideoDevice = d3d.device().cast()?;
            let video_ctx: ID3D11VideoContext = d3d.context().cast()?;
            let rate = DXGI_RATIONAL {
                Numerator: fps.max(1) as u32,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: in_w,
                InputHeight: in_h,
                OutputFrameRate: rate,
                OutputWidth: w,
                OutputHeight: h,
                // 缩一帧画面在显卡上不到 1 毫秒，要质量好一点的缩放（驱动支持的话）
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let proc_enum = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .context("CreateVideoProcessorEnumerator")?;
            let input = proc_enum.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM)?;
            if input & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
                bail!("the video processor can't read BGRA");
            }
            let output = proc_enum.CheckVideoProcessorFormat(DXGI_FORMAT_NV12)?;
            if output & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
                bail!("the video processor can't write NV12");
            }
            if degrees != 0 {
                let mut caps = D3D11_VIDEO_PROCESSOR_CAPS::default();
                proc_enum.GetVideoProcessorCaps(&mut caps)?;
                if caps.FeatureCaps & D3D11_VIDEO_PROCESSOR_FEATURE_CAPS_ROTATION.0 as u32 == 0 {
                    bail!("the video processor can't rotate");
                }
            }
            let processor = video_device
                .CreateVideoProcessor(&proc_enum, 0)
                .context("CreateVideoProcessor")?;

            let vp = &processor;
            video_ctx.VideoProcessorSetStreamFrameFormat(
                vp,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            // 不要驱动自作主张的"画质增强"（降噪、锐化、肤色……）
            video_ctx.VideoProcessorSetStreamAutoProcessingMode(vp, 0, false);
            // 奇数尺寸多出来的 1 像素裁掉（和内存那条路一样），不为它缩放
            let (fw, fh) = if degrees % 180 == 90 {
                (in_h, in_w)
            } else {
                (in_w, in_h)
            };
            let (cw, ch) = if super::crop_only((fw, fh), (w, h)) {
                (w, h)
            } else {
                (fw, fh)
            };
            // 源矩形是旋转之前的纹理坐标
            let (cw, ch) = if degrees % 180 == 90 {
                (ch, cw)
            } else {
                (cw, ch)
            };
            let src = RECT {
                left: 0,
                top: 0,
                right: cw as i32,
                bottom: ch as i32,
            };
            let dst = RECT {
                left: 0,
                top: 0,
                right: w as i32,
                bottom: h as i32,
            };
            video_ctx.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
            video_ctx.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
            video_ctx.VideoProcessorSetOutputTargetRect(vp, true, Some(&dst));
            if degrees != 0 {
                let r = match degrees {
                    90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
                    180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
                    _ => D3D11_VIDEO_PROCESSOR_ROTATION_270,
                };
                video_ctx.VideoProcessorSetStreamRotation(vp, 0, true, r);
            }
            // 桌面是全范围的 sRGB；输出 BT.709 有限范围（和内存那条路、控制端的解码器一致）
            match video_ctx.cast::<ID3D11VideoContext1>() {
                Ok(c1) => {
                    c1.VideoProcessorSetStreamColorSpace1(
                        vp,
                        0,
                        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                    );
                    c1.VideoProcessorSetOutputColorSpace1(
                        vp,
                        DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
                    );
                }
                Err(_) => {
                    // 位域：Usage(1) RGB_Range(1) YCbCr_Matrix(1) YCbCr_xvYCC(1) Nominal_Range(2)
                    let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
                    let bt709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                        _bitfield: (1 << 2) | (1 << 4),
                    };
                    video_ctx.VideoProcessorSetStreamColorSpace(vp, 0, &rgb_full);
                    video_ctx.VideoProcessorSetOutputColorSpace(vp, &bt709_limited);
                }
            }

            let mut targets = Vec::with_capacity(GPU_TARGETS);
            for _ in 0..GPU_TARGETS {
                let tex = nv12_texture(d3d.device(), w, h)?;
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                video_device
                    .CreateVideoProcessorOutputView(&tex, &proc_enum, &desc, Some(&mut view))
                    .context("CreateVideoProcessorOutputView")?;
                let view = view.ok_or_else(|| anyhow!("the output view is null"))?;
                targets.push((tex, view));
            }

            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)
                .context("MFCreateDXGIDeviceManager")?;
            let manager = manager.ok_or_else(|| anyhow!("the device manager is null"))?;
            manager
                .ResetDevice(d3d.device(), token)
                .context("IMFDXGIDeviceManager::ResetDevice")?;
            Ok(Self {
                source: g.source(),
                manager,
                video_device,
                video_ctx,
                proc_enum,
                processor,
                targets,
                next: 0,
            })
        }
    }

    /// 把 `g` 缩放、转换进下一个 NV12 纹理（只是排进显卡的命令队列，编码器在同一个设备上按顺序执行）
    pub(crate) fn convert(&mut self, g: &GpuFrame) -> Result<ID3D11Texture2D> {
        if g.source() != self.source {
            bail!("the frame comes from another capture device");
        }
        let (tex, out_view) = &self.targets[self.next];
        self.next = (self.next + 1) % self.targets.len();
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        unsafe {
            // 输入视图很便宜，每帧建一个（帧缓冲池里的纹理会轮流用，不必缓存）
            let mut in_view = None;
            self.video_device
                .CreateVideoProcessorInputView(
                    g.texture(),
                    &self.proc_enum,
                    &desc,
                    Some(&mut in_view),
                )
                .context("CreateVideoProcessorInputView")?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(in_view),
                ..Default::default()
            };
            let r = self.video_ctx.VideoProcessorBlt(
                &self.processor,
                out_view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            r.context("VideoProcessorBlt")?;
        }
        Ok(tex.clone())
    }
}

/// 视频处理器的输出、编码器的输入
fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let mut desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // 视频处理器的输出视图要 RENDER_TARGET；声明给编码器用（有的驱动据此放在编码器读起来快的地方），不认就不要
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe {
        if device.CreateTexture2D(&desc, None, Some(&mut tex)).is_err() {
            desc.BindFlags = D3D11_BIND_RENDER_TARGET.0 as u32;
            device
                .CreateTexture2D(&desc, None, Some(&mut tex))
                .context("CreateTexture2D(NV12)")?;
        }
    }
    tex.ok_or_else(|| anyhow!("the NV12 texture is null"))
}

// ───────────────────────────── 编码器 ─────────────────────────────

/// 长期参考帧要几个槽位（见 `airhand_link::refs`）：一个是控制端确认收到了的，另一个轮流标最近的帧
const LTR_SLOTS: u8 = 2;

pub struct MfEncoder {
    activate: IMFActivate,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    codec_api: Option<ICodecAPI>,
    info: EncoderInfo,
    seq_header: Vec<u8>,
    need_input: u32,
    have_output: u32,
    provides_samples: bool,
    out_size: u32,
    kbps: u32,
    fps: u8,
    epoch: Instant,
    /// GPU 零拷贝（编码器关掉之后才释放，见 Drop）
    gpu: Option<GpuInput>,
    /// 长期参考帧的槽位数：编码器认了 `CODECAPI_AVEncVideoLTRBufferControl`、输出的样本上又带着
    /// `MFSampleExtension_LongTermReferenceFrameInfo`（说明它真的在控制）才不是 0
    ltr_slots: u8,
    /// 下一帧的长期参考帧控制
    next_ltr: LtrControl,
}

impl MfEncoder {
    /// `gpu`：采集的画面在 GPU 上时，先试 GPU 零拷贝（同一块显卡上的编码器），不行再用内存里的画面
    pub fn new(
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
        gpu: Option<&GpuFrame>,
    ) -> Result<Self> {
        ensure_init()?;
        if let Some(g) = gpu {
            match Self::open_any(codec, w, h, fps, kbps, Some(g)) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::warn!(
                        ?codec,
                        "GPU zero-copy unavailable, encoding frames from memory: {e:#}"
                    )
                }
            }
        }
        Self::open_any(codec, w, h, fps, kbps, None)
    }

    fn open_any(
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
        gpu: Option<&GpuFrame>,
    ) -> Result<Self> {
        let mut last_err = anyhow!("no hardware encoder available");
        for activate in enum_hardware(codec) {
            let name = friendly_name(&activate);
            // GPU 零拷贝只能用同一块显卡上的编码器（不知道在哪块显卡上的也试试）
            if let Some(g) = gpu
                && let Ok(luid) = unsafe { activate.GetUINT64(&MFT_ENUM_ADAPTER_LUID) }
                && luid != g.d3d().luid()
            {
                last_err = anyhow!("{name} is on another GPU");
                continue;
            }
            let input = match gpu.map(|g| GpuInput::new(g, w, h, fps)).transpose() {
                Ok(i) => i,
                // 视频处理器不行，换哪个编码器都一样
                Err(e) => return Err(e.context("the GPU can't scale and convert the frames")),
            };
            match unsafe { Self::open(activate.clone(), &name, codec, w, h, fps, kbps, input, gpu) }
            {
                Ok(e) => return Ok(e),
                Err(e) => {
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                    last_err = e.context(name);
                }
            }
        }
        Err(last_err)
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn open(
        activate: IMFActivate,
        name: &str,
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
        gpu: Option<GpuInput>,
        first: Option<&GpuFrame>,
    ) -> Result<Self> {
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("ActivateObject")?;
            let attrs = transform.GetAttributes().context("GetAttributes")?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0;
            if is_async {
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("ASYNC_UNLOCK")?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
            // GPU 零拷贝：编码器和采集共用一个设备（要在设置媒体类型之前）
            if let Some(input) = &gpu {
                if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                    bail!("the encoder can't take Direct3D 11 textures");
                }
                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, input.manager.as_raw() as usize)
                    .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
            }

            let codec_api: Option<ICodecAPI> = transform.cast().ok();
            let bps = kbps.saturating_mul(1000);
            let mut ltr_slots = 0;
            if let Some(api) = &codec_api {
                let set = |k: &GUID, v: VARIANT| api.SetValue(k, &v).is_ok();
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
                if !set(
                    &CODECAPI_AVEncCommonRateControlMode,
                    VARIANT::from(eAVEncCommonRateControlMode_LowDelayVBR.0 as u32),
                ) {
                    set(
                        &CODECAPI_AVEncCommonRateControlMode,
                        VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                    );
                }
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bps));
                set(
                    &CODECAPI_AVEncCommonMaxBitRate,
                    VARIANT::from(bps.saturating_mul(3) / 2),
                );
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                // 超长 GOP：画面静止时不编码，关键帧只在控制端请求或分辨率变化时产生
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps as u32 * 600));
                // 偏向速度（0=速度 100=质量）
                set(&CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(60u32));
                // 长期参考帧（丢了帧时从控制端确认收到的那一帧接着编，不用编关键帧，见 airhand_link::refs）：
                // 静态属性，要在设置媒体类型之前。低 16 位是槽位数，高 16 位是信任模式（1：一直可用，直到用
                // CODECAPI_AVEncVideoUseLTRFrame 说明只用哪些）。编码器真的支持与否看输出（见下面的冒烟测试）。
                // 只用在信得过的地方（见 ltr_trusted）
                ltr_slots = if ltr_trusted(codec, name)
                    && set(
                        &CODECAPI_AVEncVideoLTRBufferControl,
                        VARIANT::from(LTR_SLOTS as u32 | 1 << 16),
                    ) {
                    LTR_SLOTS
                } else {
                    0
                };
                // H.264 High 档次用 CABAC（比 CAVLC 省约一成码率）：大多数硬件编码器默认就开，但不保证，显式打开。
                // 不支持这个属性的编码器忽略即可
                if codec == Codec::H264 {
                    set(&CODECAPI_AVEncH264CABACEnable, VARIANT::from(true));
                }
            }

            // 编码器必须先设置输出类型
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &subtype(codec))?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bps)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps as u32, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            match codec {
                Codec::H264 => {
                    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?
                }
                Codec::H265 => {
                    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?
                }
                Codec::AV1 | Codec::Unknown(_) => {}
            }
            transform
                .SetOutputType(0, &out, 0)
                .context("SetOutputType")?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps as u32, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, w)?;
            transform
                .SetInputType(0, &inp, 0)
                .context("SetInputType(NV12)")?;

            let seq_header = Self::sequence_header(&transform);
            let osi = transform.GetOutputStreamInfo(0)?;
            let provides_samples = osi.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32
                != 0;

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let events = if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };
            let mut enc = Self {
                activate,
                transform,
                events,
                codec_api,
                info: EncoderInfo {
                    codec,
                    name: name.to_string(),
                    hardware: true,
                    width: w,
                    height: h,
                    // 第几路画面由编码管线填（见 pipeline）
                    stream: 0,
                },
                seq_header,
                need_input: 0,
                have_output: 0,
                provides_samples,
                out_size: osi.cbSize.max(w * h * 3 / 2),
                kbps,
                fps,
                epoch: Instant::now(),
                gpu,
                ltr_slots,
                next_ltr: LtrControl::default(),
            };
            // 冒烟测试：编码一帧（GPU 零拷贝时就用第一帧画面，整条路都走一遍），确保这个编码器在当前参数下真的能工作
            let gpu_first = first.filter(|_| enc.gpu.is_some());
            let black = vec![
                0u8;
                if gpu_first.is_some() {
                    0
                } else {
                    (w * h * 4) as usize
                }
            ];
            let test = |enc: &mut Self, key: bool| match gpu_first {
                Some(g) => enc.encode_gpu(g, key),
                None => enc.encode(&black, key),
            };
            test(&mut enc, true).context("test encode failed")?;
            // 长期参考帧：标一帧、再只参考它编一帧，编码器报告的要和要求的一样才用（有的编码器认了属性却不照做，
            // 例如 Intel 的 AV1 编码器每一帧都说自己标进了 0 号槽位）。之后的第一帧总是关键帧，这几帧不会发出去
            if enc.ltr_slots > 0 {
                let slot = enc.ltr_slots - 1;
                let probe = |enc: &mut Self, ctl| {
                    enc.set_ltr(ctl);
                    test(enc, false).map(|f| f.and_then(|f| f.ltr))
                };
                let marked = probe(
                    &mut enc,
                    LtrControl {
                        mark: Some(slot),
                        recover: None,
                    },
                );
                let used = probe(
                    &mut enc,
                    LtrControl {
                        mark: None,
                        recover: Some(slot),
                    },
                );
                let ok = matches!(marked, Ok(Some(r)) if r.marked == Some(slot))
                    && matches!(used, Ok(Some(r)) if r.used == 1 << slot);
                if !ok {
                    tracing::info!(
                        encoder = name,
                        ?marked,
                        ?used,
                        "the encoder doesn't follow long-term reference frame controls, not using them"
                    );
                    enc.ltr_slots = 0;
                }
            }
            Ok(enc)
        }
    }

    unsafe fn sequence_header(t: &IMFTransform) -> Vec<u8> {
        unsafe {
            let Ok(ty) = t.GetOutputCurrentType(0) else {
                return Vec::new();
            };
            let Ok(n) = ty.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else {
                return Vec::new();
            };
            let mut buf = vec![0u8; n as usize];
            if ty
                .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None)
                .is_ok()
            {
                buf
            } else {
                Vec::new()
            }
        }
    }

    fn pump(&mut self, deadline: Instant) -> Result<()> {
        let ev = self.events.as_ref().expect("async");
        match unsafe { ev.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(e) => {
                let t = unsafe { e.GetType()? } as i32;
                if t == METransformNeedInput.0 {
                    self.need_input += 1;
                } else if t == METransformHaveOutput.0 {
                    self.have_output += 1;
                } else if let Ok(status) = unsafe { e.GetStatus() }
                    && status.is_err()
                {
                    bail!("the hardware encoder reported an error: {status:?}");
                }
                Ok(())
            }
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                if Instant::now() > deadline {
                    bail!("the hardware encoder didn't respond in time");
                }
                std::thread::sleep(Duration::from_micros(250));
                Ok(())
            }
            Err(e) => Err(anyhow!("GetEvent: {e}")),
        }
    }

    /// 带上时间戳的输入样本
    fn sample(&self, buf: &IMFMediaBuffer) -> Result<IMFSample> {
        unsafe {
            let sample = MFCreateSample()?;
            sample.AddBuffer(buf)?;
            let dur = 10_000_000i64 / self.fps.max(1) as i64;
            sample.SetSampleTime(self.epoch.elapsed().as_nanos() as i64 / 100)?;
            sample.SetSampleDuration(dur)?;
            Ok(sample)
        }
    }

    /// GPU 上的 NV12 纹理（不复制，编码器直接读）
    fn texture_input(&self, tex: &ID3D11Texture2D) -> Result<IMFSample> {
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)
                .context("MFCreateDXGISurfaceBuffer")?;
            // 不设置长度的话有的编码器当它是空的
            if let Ok(b2) = buf.cast::<IMF2DBuffer>()
                && let Ok(len) = b2.GetContiguousLength()
            {
                buf.SetCurrentLength(len)?;
            }
            self.sample(&buf)
        }
    }

    fn make_input(&self, bgra: &[u8]) -> Result<IMFSample> {
        let (w, h) = (self.info.width, self.info.height);
        let y_len = (w * h) as usize;
        let len = y_len + y_len / 2;
        unsafe {
            let buf = MFCreateMemoryBuffer(len as u32)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            let dst = std::slice::from_raw_parts_mut(ptr, len);
            let (y, uv) = dst.split_at_mut(y_len);
            let mut img = YuvBiPlanarImageMut {
                y_plane: yuvutils_rs::BufferStoreMut::Borrowed(y),
                y_stride: w,
                uv_plane: yuvutils_rs::BufferStoreMut::Borrowed(uv),
                uv_stride: w,
                width: w,
                height: h,
            };
            let r = yuvutils_rs::bgra_to_yuv_nv12(
                &mut img,
                bgra,
                w * 4,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            );
            buf.Unlock()?;
            r.map_err(|e| anyhow!("color conversion failed: {e:?}"))?;
            buf.SetCurrentLength(len as u32)?;
            self.sample(&buf)
        }
    }

    /// 交给编码器一帧，等它编完
    fn submit(&mut self, sample: IMFSample, force_key: bool) -> Result<Option<EncodedFrame>> {
        let ltr = std::mem::take(&mut self.next_ltr);
        if let Some(api) = &self.codec_api {
            unsafe {
                if force_key {
                    let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                } else if self.ltr_slots > 0 {
                    // 低 16 位是可以参考的槽位，高 16 位 1：之后的帧也不再参考这之前的短期参考帧、别的槽位
                    if let Some(i) = ltr.recover {
                        api.SetValue(
                            &CODECAPI_AVEncVideoUseLTRFrame,
                            &VARIANT::from(1u32 << i | 1 << 16),
                        )
                        .context("CODECAPI_AVEncVideoUseLTRFrame")?;
                    }
                    if let Some(i) = ltr.mark {
                        api.SetValue(&CODECAPI_AVEncVideoMarkLTRFrame, &VARIANT::from(i as u32))
                            .context("CODECAPI_AVEncVideoMarkLTRFrame")?;
                    }
                }
            }
        }
        if self.events.is_none() {
            unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
            return self.pull();
        }

        let deadline = Instant::now() + Duration::from_millis(1500);
        while self.need_input == 0 {
            self.pump(deadline)?;
        }
        unsafe { self.transform.ProcessInput(0, &sample, 0) }.context("ProcessInput")?;
        self.need_input -= 1;
        // 低延迟模式下每输入一帧都会立即输出一帧
        loop {
            while self.have_output == 0 {
                self.pump(deadline)?;
            }
            self.have_output -= 1;
            if let Some(f) = self.pull()? {
                return Ok(Some(f));
            }
        }
    }

    /// 取一帧输出。`Ok(None)` 表示需要更多输入；流格式变化时会自动重新协商。
    fn pull(&mut self) -> Result<Option<EncodedFrame>> {
        for _ in 0..3 {
            let sample = if self.provides_samples {
                None
            } else {
                unsafe {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                    Some(s)
                }
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = unsafe { self.transform.ProcessOutput(0, &mut out, &mut status) };
            let sample = unsafe { ManuallyDrop::take(&mut out[0].pSample) };
            drop(unsafe { ManuallyDrop::take(&mut out[0].pEvents) });
            match r {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    unsafe {
                        let ty = self.transform.GetOutputAvailableType(0, 0)?;
                        self.transform.SetOutputType(0, &ty, 0)?;
                        self.seq_header = Self::sequence_header(&self.transform);
                    }
                    if self.events.is_some() {
                        // 异步 MFT 会再发一次 HaveOutput
                        return Ok(None);
                    }
                    continue;
                }
                Err(e) => bail!("ProcessOutput: {e}"),
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            return unsafe { self.read_sample(&sample) }.map(Some);
        }
        bail!("the hardware encoder keeps changing its output format")
    }

    unsafe fn read_sample(&mut self, sample: &IMFSample) -> Result<EncodedFrame> {
        unsafe {
            let buf = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buf.Lock(&mut ptr, None, Some(&mut len))?;
            let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buf.Unlock()?;
            let clean = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            // 低 16 位：这一帧标进了哪个槽位（0xFFFF：没有标）；高 16 位：参考了哪些槽位
            let ltr = sample
                .GetUINT32(&MFSampleExtension_LongTermReferenceFrameInfo)
                .ok()
                .map(|v| LtrReport {
                    marked: (v & 0xffff != 0xffff).then_some((v & 0xffff) as u8),
                    used: (v >> 16) as u16,
                });
            let key = match self.info.codec {
                Codec::H264 => clean || annexb_nal_types(&data, false).any(|t| t == 5),
                Codec::H265 => {
                    clean || annexb_nal_types(&data, true).any(|t| (16..=21).contains(&t))
                }
                Codec::AV1 | Codec::Unknown(_) => clean,
            };
            // 部分编码器只把参数集放在媒体类型里，关键帧前补上，保证控制端可以从任意关键帧开始解码
            if key && !self.seq_header.is_empty() {
                let has_ps = match self.info.codec {
                    Codec::H264 => annexb_nal_types(&data, false).any(|t| t == 7),
                    Codec::H265 => annexb_nal_types(&data, true).any(|t| t == 33),
                    // AV1 编码器会在关键帧中自带 Sequence Header OBU
                    Codec::AV1 | Codec::Unknown(_) => true,
                };
                if !has_ps {
                    let mut v = Vec::with_capacity(self.seq_header.len() + data.len());
                    v.extend_from_slice(&self.seq_header);
                    v.extend_from_slice(&data);
                    data = v;
                }
            }
            Ok(EncodedFrame { data, key, ltr })
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn ltr_slots(&self) -> u8 {
        self.ltr_slots
    }

    fn set_ltr(&mut self, ctl: LtrControl) {
        self.next_ltr = ctl;
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == self.kbps {
            return;
        }
        self.kbps = kbps;
        if let Some(api) = &self.codec_api {
            let bps = kbps.saturating_mul(1000);
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps));
                let _ = api.SetValue(
                    &CODECAPI_AVEncCommonMaxBitRate,
                    &VARIANT::from(bps.saturating_mul(3) / 2),
                );
            }
        }
    }

    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        if self.gpu.is_some() {
            bail!("this encoder takes textures on the GPU");
        }
        let sample = self.make_input(bgra)?;
        self.submit(sample, force_key)
    }

    fn takes_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    fn encode_gpu(&mut self, frame: &GpuFrame, force_key: bool) -> Result<Option<EncodedFrame>> {
        let input = self
            .gpu
            .as_mut()
            .ok_or_else(|| anyhow!("this encoder takes frames in memory"))?;
        let tex = input.convert(frame)?;
        let sample = self.texture_input(&tex)?;
        self.submit(sample, force_key)
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(s) = self.transform.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::capture::{Rotation, create_device};
    use std::sync::Arc;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1,
    };

    /// 每块显卡（不含软件渲染）上带视频支持的设备。没有显卡的机器上为空
    fn sources() -> Vec<(String, Arc<D3dSource>)> {
        let mut out = Vec::new();
        unsafe {
            let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
                return out;
            };
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = adapter.GetDesc1() else {
                    continue;
                };
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    continue;
                }
                let name = String::from_utf16_lossy(&desc.Description)
                    .trim_end_matches('\0')
                    .to_string();
                match create_device(&adapter) {
                    Ok((d, c, true)) => out.push((name, Arc::new(D3dSource::new(d, c).unwrap()))),
                    Ok(_) => println!("{name}: no video support"),
                    Err(e) => println!("{name}: {e:#}"),
                }
            }
        }
        out
    }

    /// GPU 上的画面（没有旋转过的方向）：左半边红、右半边蓝
    fn two_colors(src: &Arc<D3dSource>, w: u32, h: u32, rotation: Rotation) -> GpuFrame {
        let g = GpuFrame::new(src.clone(), w, h, rotation).unwrap();
        let mut px = vec![0u8; (w * h * 4) as usize];
        for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = i as u32 % w;
            *p = if x < w / 2 {
                [0, 0, 255, 255]
            } else {
                [255, 0, 0, 255]
            };
        }
        unsafe {
            src.context()
                .UpdateSubresource(g.texture(), 0, None, px.as_ptr().cast(), w * 4, 0);
        }
        g
    }

    /// NV12 纹理读回来：(Y 平面, UV 平面)，都是紧密排列的
    fn read_nv12(src: &D3dSource, tex: &ID3D11Texture2D, w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        unsafe {
            let mut staging = None;
            src.device()
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            let ctx = src.context();
            ctx.CopyResource(&staging, tex);
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut m))
                .unwrap();
            let pitch = m.RowPitch as usize;
            let base = m.pData as *const u8;
            let rows = |offset: usize, n: u32| -> Vec<u8> {
                (0..n as usize)
                    .flat_map(|y| {
                        std::slice::from_raw_parts(base.add(offset + y * pitch), w as usize)
                            .to_vec()
                    })
                    .collect()
            };
            // UV 平面紧接在 Y 平面后面
            let planes = (rows(0, h), rows(pitch * h as usize, h / 2));
            ctx.Unmap(&staging, 0);
            planes
        }
    }

    /// 视频处理器缩放、旋转、转换颜色：BT.709 有限范围（和内存那条路的 yuvutils 一样），
    /// 旋转方向和读回内存时转的（`capture/dxgi.rs` 的 copy_rotated）一致
    #[test]
    fn video_processor_scales_rotates_and_converts() {
        // BT.709 有限范围：红 Y=63 U=102 V=240；蓝 Y=32 U=240 V=118
        const RED: [u8; 3] = [63, 102, 240];
        const BLUE: [u8; 3] = [32, 240, 118];
        for (name, src) in sources() {
            for (rotation, (w, h)) in [
                (Rotation::None, (128u32, 64u32)),
                (Rotation::R90, (64, 128)),
                (Rotation::R180, (128, 64)),
                (Rotation::R270, (64, 128)),
            ] {
                // 256×128 的画面缩小一半
                let g = two_colors(&src, 256, 128, rotation);
                let mut input = match GpuInput::new(&g, w, h, 30) {
                    Ok(i) => i,
                    Err(e) if rotation != Rotation::None => {
                        println!("{name} {rotation:?}: {e:#}");
                        continue;
                    }
                    Err(e) => panic!("{name}: {e:#}"),
                };
                let tex = input.convert(&g).unwrap();
                let (y, uv) = read_nv12(&src, &tex, w, h);
                let at = |x: u32, yy: u32| {
                    let c = ((yy / 2) * w + (x / 2) * 2) as usize;
                    [y[(yy * w + x) as usize], uv[c], uv[c + 1]]
                };
                // 用户看到的方向：顺时针转 90° 时画面的左边到了上边
                let (first, second) = match rotation {
                    Rotation::None => ((w / 4, h / 2), (w * 3 / 4, h / 2)),
                    Rotation::R90 => ((w / 2, h / 4), (w / 2, h * 3 / 4)),
                    Rotation::R180 => ((w * 3 / 4, h / 2), (w / 4, h / 2)),
                    Rotation::R270 => ((w / 2, h * 3 / 4), (w / 2, h / 4)),
                };
                let close = |got: [u8; 3], want: [u8; 3]| {
                    got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 3)
                };
                let (red, blue) = (at(first.0, first.1), at(second.0, second.1));
                println!("{name} {rotation:?}: red {red:?} blue {blue:?}");
                assert!(close(red, RED), "{name} {rotation:?}: red is {red:?}");
                assert!(close(blue, BLUE), "{name} {rotation:?}: blue is {blue:?}");
            }
        }
    }

    /// 每个硬件编码器的长期参考帧：打印它报告了什么（调查用）
    #[test]
    #[ignore]
    fn ltr_probe() {
        ensure_init().unwrap();
        let (w, h) = (640u32, 360u32);
        let frame = |i: u32| {
            let mut px = vec![0u8; (w * h * 4) as usize];
            for (j, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (j as u32 % w, j as u32 / w);
                *p = [
                    ((x + i * 7) % 256) as u8,
                    ((y + i * 3) % 256) as u8,
                    (x ^ y) as u8,
                    255,
                ];
            }
            px
        };
        for codec in [Codec::H264, Codec::H265, Codec::AV1] {
            for activate in enum_hardware(codec) {
                let name = friendly_name(&activate);
                let mut e = match unsafe {
                    MfEncoder::open(activate.clone(), &name, codec, w, h, 30, 2000, None, None)
                } {
                    Ok(e) => e,
                    Err(err) => {
                        println!("{name} {codec:?}: {err:#}");
                        continue;
                    }
                };
                println!("{name} {codec:?}: ltr_slots {}", e.ltr_slots);
                e.ltr_slots = LTR_SLOTS;
                for i in 0..16u32 {
                    let ctl = match i {
                        5 => LtrControl {
                            mark: Some(1),
                            recover: None,
                        },
                        8 => LtrControl {
                            mark: Some(0),
                            recover: None,
                        },
                        12 => LtrControl {
                            mark: None,
                            recover: Some(1),
                        },
                        _ => LtrControl::default(),
                    };
                    e.set_ltr(ctl);
                    match e.encode(&frame(i), i == 0) {
                        Ok(Some(f)) => println!(
                            "  {i}: key {} {} bytes {:?} ctl {ctl:?}",
                            f.key,
                            f.data.len(),
                            f.ltr
                        ),
                        Ok(None) => println!("  {i}: none"),
                        Err(err) => println!("  {i}: {err:#}"),
                    }
                }
            }
        }
    }

    /// ffmpeg 严格模式（`-err_detect explode`：码流有一点问题就报错）解出来的每一帧的 MD5。没装 ffmpeg 时为 `None`
    fn ffmpeg_md5(codec: Codec, stream: &[u8], tag: &str) -> Option<Vec<String>> {
        let path = std::env::temp_dir().join(format!("airhand-{}-{tag}.bin", std::process::id()));
        std::fs::write(&path, stream).unwrap();
        let format = match codec {
            Codec::H265 => "hevc",
            Codec::AV1 => "obu",
            _ => "h264",
        };
        // HEVC 的参考帧集里可以列着不再用来预测、只是留着的帧（RefPicSetStFoll），恢复帧就列着被放弃的帧：标准允许它们不在，
        // ffmpeg 的严格模式却当它是错。HEVC 只看解出来的对不对
        let strict: &[&str] = if codec == Codec::H265 {
            &[]
        } else {
            &["-err_detect", "explode"]
        };
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(strict)
            .args(["-f", format, "-i"])
            .arg(&path)
            .args(["-f", "framemd5", "-"])
            .output();
        let _ = std::fs::remove_file(&path);
        let out = out.ok()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "ffmpeg ({tag}): {stderr}");
        if !stderr.is_empty() {
            println!("  ffmpeg ({codec:?} {tag}): {}", stderr.trim());
        }
        assert!(
            strict.is_empty() || stderr.is_empty(),
            "ffmpeg ({tag}): {stderr}"
        );
        let text = String::from_utf8(out.stdout).unwrap();
        Some(
            text.lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| l.rsplit(',').next().map(|m| m.trim().to_string()))
                .collect(),
        )
    }

    /// 长期参考帧恢复（参考帧失效）：丢掉中间几帧，从长期参考帧恢复的那一帧和之后的帧照样解得出来，
    /// 而且和一帧不丢时解出来的一模一样（它们只参考了那个长期参考帧）。每个支持长期参考帧的硬件编码器都检查它报告的；
    /// 装了 ffmpeg 的话再用它解一遍（浏览器的软件 H.264 解码也是它）。OpenH264 的解码器不支持这样用长期参考帧
    /// （一帧不丢也报参考帧丢了），不能拿来对照
    #[test]
    fn ltr_recovery_decodes_without_the_lost_frames() {
        ensure_init().unwrap();
        let (w, h) = (640u32, 360u32);
        let picture = |i: u32| {
            let mut px = vec![0u8; (w * h * 4) as usize];
            for (j, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let (x, y) = (j as u32 % w, j as u32 / w);
                *p = [
                    ((x + i * 9) % 256) as u8,
                    ((y + i * 5) % 256) as u8,
                    ((x ^ y) + i) as u8,
                    255,
                ];
            }
            px
        };
        let mut tested = 0;
        let encoders = [Codec::H264, Codec::H265, Codec::AV1]
            .into_iter()
            .flat_map(|c| enum_hardware(c).into_iter().map(move |a| (c, a)));
        for (codec, activate) in encoders {
            let name = format!("{} ({codec:?})", friendly_name(&activate));
            let Ok(mut e) = (unsafe {
                MfEncoder::open(activate.clone(), &name, codec, w, h, 30, 3000, None, None)
            }) else {
                continue;
            };
            if e.ltr_slots() == 0 {
                println!("{name}: no long-term reference frames");
                continue;
            }
            // 第 6 帧标进 1 号槽位；第 7、8、9 帧"丢了"；第 10 帧只参考 1 号槽位（第 6 帧）
            let mut frames = Vec::new();
            for i in 0..15u32 {
                let ctl = match i {
                    6 => LtrControl {
                        mark: Some(1),
                        recover: None,
                    },
                    10 => LtrControl {
                        mark: None,
                        recover: Some(1),
                    },
                    _ => LtrControl::default(),
                };
                e.set_ltr(ctl);
                let f = e.encode(&picture(i), i == 0).unwrap().expect("no output");
                assert_eq!(f.key, i == 0, "{name}: frame {i}");
                if i == 6 {
                    assert_eq!(f.ltr.unwrap().marked, Some(1), "{name}");
                }
                if i == 10 {
                    assert_eq!(f.ltr.unwrap().used, 0b10, "{name}");
                }
                frames.push(f.data);
            }
            let stream = |skip: &[usize]| -> Vec<u8> {
                let kept = frames.iter().enumerate().filter(|(i, _)| !skip.contains(i));
                kept.flat_map(|(_, f)| f.clone()).collect()
            };
            let (Some(all), Some(lossy)) = (
                ffmpeg_md5(codec, &stream(&[]), "full"),
                ffmpeg_md5(codec, &stream(&[7, 8, 9]), "lossy"),
            ) else {
                println!("{name}: ffmpeg isn't installed, not decoding");
                continue;
            };
            assert_eq!((all.len(), lossy.len()), (15, 12), "{name}");
            assert_eq!(all[..7], lossy[..7], "{name}");
            assert_eq!(
                all[10..],
                lossy[7..],
                "{name}: the frames after the recovery decode differently without frames 7-9"
            );
            println!("{name}: recovered from a long-term reference frame");
            tested += 1;
        }
        println!("{tested} encoders decoded");
    }

    /// 每块显卡上：同一块显卡的硬件编码器直接编码 GPU 上的画面；只要有硬件 H.264 编码器，至少一块显卡上要成功
    #[test]
    fn gpu_zero_copy_on_every_adapter() {
        let mut any = false;
        for (name, src) in sources() {
            for codec in [Codec::H264, Codec::H265, Codec::AV1] {
                let g = two_colors(&src, 1920, 1080, Rotation::None);
                let mut e = match MfEncoder::open_any(codec, 1280, 720, 30, 4000, Some(&g)) {
                    Ok(e) => e,
                    Err(e) => {
                        println!("{name} {codec:?}: {e:#}");
                        continue;
                    }
                };
                assert!(e.takes_gpu());
                any |= codec == Codec::H264;
                let mut sizes = Vec::new();
                for i in 0..6 {
                    let t = Instant::now();
                    let f = e
                        .encode_gpu(&g, i == 0 || i == 4)
                        .unwrap()
                        .expect("no output");
                    assert!(!f.data.is_empty());
                    if i == 0 || i == 4 {
                        assert!(f.key, "{name} {codec:?}: frame {i} isn't a keyframe");
                    }
                    sizes.push((f.data.len(), t.elapsed()));
                }
                // 内存里的画面不收
                assert!(e.encode(&[0; 1280 * 720 * 4], false).is_err());
                println!("{name} {codec:?}: {} {sizes:?}", e.info.name);
            }
        }
        let hw = has_hardware(Codec::H264);
        assert!(
            any || !hw,
            "there's a hardware H.264 encoder but GPU zero-copy didn't work"
        );
    }
}
