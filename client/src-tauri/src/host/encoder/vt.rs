//! macOS VideoToolbox 硬件编码（Apple 芯片的媒体引擎 / Intel 核显），支持 H.264 和 HEVC。
//!
//! * 实时模式、低延迟码率控制（H.264）、无 B 帧、超长 GOP（只在需要时插入关键帧）；
//! * 画面的两种来路：ScreenCaptureKit 已经缩放好的 NV12（GPU 零拷贝，`CVPixelBuffer` 原样交给编码器，见
//!   `capture/sck.rs`）；内存里的 BGRA 直接转换进编码器自己缓冲池里的 NV12 缓冲（IOSurface），没有多余拷贝；
//! * 编码器是异步的：提交一帧后等它完成（`CompleteFrames`），变成"一进一出"的同步调用；
//! * 输出是 AVCC / HVCC（长度前缀），转换成控制端使用的 Annex-B，关键帧前面带上参数集。

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use airhand_proto::session::{Codec, EncoderInfo};
use anyhow::{Result, anyhow, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMFormatDescription, CMSampleBuffer, CMTime, CMVideoCodecType,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMTimeInvalid, kCMVideoCodecType_AV1,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferPool, CVPixelBufferUnlockBaseAddress, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTCopySupportedPropertyDictionaryForEncoder, VTEncodeInfoFlags,
    VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_TransferFunction,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};
use yuvutils_rs::{YuvBiPlanarImageMut, YuvConversionMode, YuvRange, YuvStandardMatrix};

use super::{EncodedFrame, VideoEncoder, annexb_nal_types};
use crate::host::capture::GpuFrame;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

fn codec_type(c: Codec) -> CMVideoCodecType {
    match c {
        // 不认识的格式不会走到这里：Encoder::new 只用认识的格式
        Codec::H264 | Codec::Unknown(_) => kCMVideoCodecType_H264,
        Codec::H265 => kCMVideoCodecType_HEVC,
        Codec::AV1 => kCMVideoCodecType_AV1,
    }
}

/// 只要硬件编码器
fn hardware_spec(low_latency: bool) -> CFRetained<CFDictionary> {
    let yes: &CFType = CFBoolean::new(true);
    let mut keys: Vec<&CFString> =
        vec![unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }];
    if low_latency {
        keys.push(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl });
    }
    let values = vec![yes; keys.len()];
    opaque(CFDictionary::<CFString, CFType>::from_slices(
        &keys, &values,
    ))
}

fn opaque(d: CFRetained<CFDictionary<CFString, CFType>>) -> CFRetained<CFDictionary> {
    // 带类型参数的字典和不带的是同一种对象
    unsafe { CFRetained::cast_unchecked(d) }
}

/// 本机是否有这种格式的硬件编码器（Apple 芯片：H.264 / HEVC）。
pub fn has_hardware(codec: Codec) -> bool {
    // AV1 的输出是 OBU 而不是 NAL，这里只实现了 H.264 / HEVC 的打包（目前的 Apple 芯片也只能硬件解码 AV1）
    if codec == Codec::AV1 {
        return false;
    }
    let spec = hardware_spec(false);
    let mut id: *const CFString = std::ptr::null();
    let mut props: *const CFDictionary = std::ptr::null();
    let status = unsafe {
        VTCopySupportedPropertyDictionaryForEncoder(
            1920,
            1080,
            codec_type(codec),
            Some(&spec),
            &mut id,
            &mut props,
        )
    };
    // 两个输出都是 +1 引用，交给 CFRetained 释放
    unsafe {
        let _ = NonNull::new(id.cast_mut()).map(|p| CFRetained::from_raw(p));
        let _ = NonNull::new(props.cast_mut()).map(|p| CFRetained::from_raw(p));
    }
    status == 0
}

type Output = Option<Result<Option<EncodedFrame>, String>>;

/// 编码完成的回调（在 VideoToolbox 的线程上调用）。`refcon` 指向编码器里的 `Mutex<Output>`。
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let slot = unsafe { &*(refcon as *const (Mutex<Output>, Codec)) };
    let r = if status != 0 {
        Err(format!("encoding failed: OSStatus {status}"))
    } else {
        match unsafe { sample.as_ref() } {
            // 编码器丢弃了这一帧
            None => Ok(None),
            Some(sb) => to_annexb(sb, slot.1).map(Some),
        }
    };
    *slot.0.lock().unwrap() = Some(r);
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    /// 回调写入结果的地方（地址必须固定：交给了 VideoToolbox）
    out: Box<(Mutex<Output>, Codec)>,
    info: EncoderInfo,
    kbps: u32,
    epoch: Instant,
    last_pts: i64,
    /// 紧密排列的 UV 平面（见 [`Self::pixel_buffer`]）
    uv_scratch: Vec<u8>,
    /// 直接编码系统缩放好的画面（GPU 零拷贝）：画面的尺寸就是编码尺寸
    gpu: bool,
}

impl VtEncoder {
    /// `gpu`：采集给的是系统缩放好的画面（[`GpuFrame`]），尺寸和编码尺寸一样时直接编码它们
    pub fn new(
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
        gpu: Option<&GpuFrame>,
    ) -> Result<Self> {
        if codec == Codec::AV1 {
            bail!("VideoToolbox can't encode AV1");
        }
        let out = Box::new((Mutex::new(None), codec));
        let source = {
            let nv12 = CFNumber::new_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32);
            let (width, height) = (CFNumber::new_i32(w as i32), CFNumber::new_i32(h as i32));
            let iosurface = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
            opaque(CFDictionary::<CFString, CFType>::from_slices(
                &[
                    unsafe { kCVPixelBufferPixelFormatTypeKey },
                    unsafe { kCVPixelBufferWidthKey },
                    unsafe { kCVPixelBufferHeightKey },
                    unsafe { kCVPixelBufferIOSurfacePropertiesKey },
                ],
                &[&nv12, &width, &height, &iosurface],
            ))
        };
        // 低延迟码率控制（为视频会议设计：没有缓冲、码率更平稳）。不支持的格式退回普通的实时模式
        let mut session = None;
        let mut low_latency = codec == Codec::H264;
        for attempt in [low_latency, false] {
            let spec = hardware_spec(attempt);
            let mut ptr: *mut VTCompressionSession = std::ptr::null_mut();
            let status = unsafe {
                VTCompressionSession::create(
                    None,
                    w as i32,
                    h as i32,
                    codec_type(codec),
                    Some(&spec),
                    Some(&source),
                    None,
                    Some(on_output),
                    (&raw const *out).cast_mut().cast(),
                    NonNull::from(&mut ptr),
                )
            };
            if let (0, Some(p)) = (status, NonNull::new(ptr)) {
                session = Some(unsafe { CFRetained::from_raw(p) });
                low_latency = attempt;
                break;
            }
            tracing::debug!(
                ?codec,
                low_latency = attempt,
                status,
                "VTCompressionSessionCreate failed"
            );
            if !attempt {
                bail!("can't create the VideoToolbox {codec:?} encoder (OSStatus {status})");
            }
        }
        let session = session.ok_or_else(|| anyhow!("no VideoToolbox session"))?;

        let mut me = Self {
            session,
            out,
            info: EncoderInfo {
                codec,
                name: format!(
                    "VideoToolbox {}",
                    match codec {
                        Codec::H264 | Codec::Unknown(_) => "H.264",
                        Codec::H265 => "HEVC",
                        Codec::AV1 => "AV1",
                    }
                ),
                hardware: true,
                width: w,
                height: h,
            },
            kbps: 0,
            epoch: Instant::now(),
            last_pts: -1,
            uv_scratch: Vec::new(),
            gpu: gpu.is_some_and(|g| g.size() == (w, h)),
        };
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        let profile: &CFType = match codec {
            Codec::H265 => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
            _ => unsafe { kVTProfileLevel_H264_High_AutoLevel },
        };
        let fps_num = CFNumber::new_i32(fps.max(1) as i32);
        // 不主动插入关键帧（控制端需要时会请求）
        let gop = CFNumber::new_i32(i32::MAX);
        unsafe {
            me.set(kVTCompressionPropertyKey_RealTime, yes)?;
            me.set(kVTCompressionPropertyKey_AllowFrameReordering, no)?;
            me.set(kVTCompressionPropertyKey_ProfileLevel, profile)?;
            me.set(kVTCompressionPropertyKey_ExpectedFrameRate, &fps_num)?;
            if me
                .set(kVTCompressionPropertyKey_MaxKeyFrameInterval, &gop)
                .is_err()
            {
                me.set(
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    &CFNumber::new_i32(100_000),
                )?;
            }
            // 与软件编码器、Media Foundation 一致：BT.709、有限范围
            me.set(
                kVTCompressionPropertyKey_ColorPrimaries,
                kCVImageBufferColorPrimaries_ITU_R_709_2,
            )?;
            me.set(
                kVTCompressionPropertyKey_TransferFunction,
                kCVImageBufferTransferFunction_ITU_R_709_2,
            )?;
            me.set(
                kVTCompressionPropertyKey_YCbCrMatrix,
                kCVImageBufferYCbCrMatrix_ITU_R_709_2,
            )?;
        }
        me.set_bitrate(kbps);
        let status = unsafe { me.session.prepare_to_encode_frames() };
        if status != 0 {
            bail!("VideoToolbox can't prepare the encoder (OSStatus {status})");
        }
        tracing::debug!(?codec, low_latency, "VideoToolbox session created");
        Ok(me)
    }

    fn set(&self, key: &CFString, value: &CFType) -> Result<()> {
        let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
        if status != 0 {
            bail!("VTSessionSetProperty({key}): OSStatus {status}");
        }
        Ok(())
    }

    /// 从编码器的缓冲池取一个 NV12 缓冲，把 BGRA 转换进去。
    ///
    /// 宽度没有对齐时（例如 1662、1590）CoreVideo 会在每行末尾补字节，而 yuvutils 0.8 的 NV12 只接受紧密排列的
    /// UV 平面（Y 平面没有这个限制）：UV 先转换进紧密排列的缓冲区，再逐行复制进去。
    /// 不这样的话每帧都转换失败，连续失败三次后会话改用软件编码。
    fn pixel_buffer(&mut self, bgra: &[u8]) -> Result<CFRetained<CVPixelBuffer>> {
        let (w, h) = (self.info.width, self.info.height);
        let pool = unsafe { self.session.pixel_buffer_pool() }
            .ok_or_else(|| anyhow!("the encoder has no pixel buffer pool"))?;
        let mut ptr: *mut CVPixelBuffer = std::ptr::null_mut();
        let ret =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut ptr)) };
        let pb = match (ret, NonNull::new(ptr)) {
            (0, Some(p)) => unsafe { CFRetained::from_raw(p) },
            _ => bail!("can't get a pixel buffer (CVReturn {ret})"),
        };
        let flags = CVPixelBufferLockFlags::empty();
        if unsafe { CVPixelBufferLockBaseAddress(&pb, flags) } != 0 {
            bail!("can't lock the pixel buffer");
        }
        let scratch = &mut self.uv_scratch;
        let r = (|| {
            let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 0);
            let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, 1);
            let y_rows = CVPixelBufferGetHeightOfPlane(&pb, 0);
            let uv_rows = CVPixelBufferGetHeightOfPlane(&pb, 1);
            let y_ptr = CVPixelBufferGetBaseAddressOfPlane(&pb, 0) as *mut u8;
            let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(&pb, 1) as *mut u8;
            if y_ptr.is_null() || uv_ptr.is_null() || y_rows < h as usize {
                bail!("unexpected pixel buffer layout");
            }
            // UV 平面每行的有效字节数和行数（4:2:0，U、V 交错）
            let (uv_row, uv_h) = ((w as usize).div_ceil(2) * 2, (h as usize).div_ceil(2));
            if uv_stride < uv_row || uv_rows < uv_h {
                bail!("unexpected pixel buffer layout");
            }
            let (y, uv) = unsafe {
                (
                    std::slice::from_raw_parts_mut(y_ptr, y_stride * y_rows),
                    std::slice::from_raw_parts_mut(uv_ptr, uv_stride * uv_rows),
                )
            };
            let packed = uv_stride == uv_row && uv_rows == uv_h;
            if !packed {
                scratch.resize(uv_row * uv_h, 0);
            }
            let mut img = YuvBiPlanarImageMut {
                y_plane: yuvutils_rs::BufferStoreMut::Borrowed(y),
                y_stride: y_stride as u32,
                uv_plane: yuvutils_rs::BufferStoreMut::Borrowed(if packed {
                    &mut *uv
                } else {
                    scratch.as_mut_slice()
                }),
                uv_stride: if packed { uv_stride } else { uv_row } as u32,
                width: w,
                height: h,
            };
            yuvutils_rs::bgra_to_yuv_nv12(
                &mut img,
                bgra,
                w * 4,
                YuvRange::Limited,
                YuvStandardMatrix::Bt709,
                YuvConversionMode::Balanced,
            )
            .map_err(|e| anyhow!("color conversion failed: {e:?}"))?;
            if !packed {
                for (dst, src) in uv.chunks_mut(uv_stride).zip(scratch.chunks(uv_row)) {
                    dst[..uv_row].copy_from_slice(src);
                }
            }
            Ok(())
        })();
        unsafe { CVPixelBufferUnlockBaseAddress(&pb, flags) };
        r.map(|()| pb)
    }
}

impl VideoEncoder for VtEncoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == self.kbps {
            return;
        }
        self.kbps = kbps;
        let bps = kbps.saturating_mul(1000).min(i32::MAX as u32) as i32;
        // 平均码率 + 上限：每秒最多比平均高 50%，场景切换时也不会一下子塞满网络
        let bytes = CFNumber::new_i64(bps as i64 * 3 / 2 / 8);
        let secs = CFNumber::new_f64(1.0);
        let limits =
            objc2_core_foundation::CFArray::<CFNumber>::from_retained_objects(&[bytes, secs]);
        unsafe {
            if let Err(e) = self.set(
                kVTCompressionPropertyKey_AverageBitRate,
                &CFNumber::new_i32(bps),
            ) {
                tracing::warn!("{e:#}");
            }
            // 有的编码器（例如低延迟模式）不支持上限，忽略即可
            if let Err(e) = self.set(kVTCompressionPropertyKey_DataRateLimits, &limits) {
                tracing::debug!("{e:#}");
            }
        }
    }

    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        let pb = self.pixel_buffer(bgra)?;
        self.submit(&pb, force_key)
    }

    fn takes_gpu(&self) -> bool {
        self.gpu
    }

    fn encode_gpu(&mut self, frame: &GpuFrame, force_key: bool) -> Result<Option<EncodedFrame>> {
        let (w, h) = (self.info.width, self.info.height);
        if frame.size() != (w, h) {
            bail!("the frame is {:?}, the encoder {w}x{h}", frame.size());
        }
        self.submit(frame.pixel_buffer(), force_key)
    }
}

impl VtEncoder {
    /// 交给编码器一帧，等它编完
    fn submit(&mut self, pb: &CVPixelBuffer, force_key: bool) -> Result<Option<EncodedFrame>> {
        // 时间戳用真实时间：码率控制据此分配每帧的比特数（画面静止一段时间后的那一帧可以多用一些）
        let pts = (self.epoch.elapsed().as_micros() as i64).max(self.last_pts + 1);
        self.last_pts = pts;
        let props = force_key.then(|| {
            let yes: &CFType = CFBoolean::new(true);
            opaque(CFDictionary::<CFString, CFType>::from_slices(
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[yes],
            ))
        });
        *self.out.0.lock().unwrap() = None;
        let status = unsafe {
            self.session.encode_frame(
                pb,
                CMTime::new(pts, 1_000_000),
                kCMTimeInvalid,
                props.as_deref(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame: OSStatus {status}");
        }
        // 等这一帧编码完（回调在返回之前一定会被调用）
        let status = unsafe { self.session.complete_frames(kCMTimeInvalid) };
        if status != 0 {
            bail!("VTCompressionSessionCompleteFrames: OSStatus {status}");
        }
        match self.out.0.lock().unwrap().take() {
            Some(Ok(f)) => Ok(f),
            Some(Err(e)) => Err(anyhow!(e)),
            None => Ok(None),
        }
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        // 先让会话停下来（不会再调用回调），`out` 才能释放
        unsafe { self.session.invalidate() };
    }
}

/// 编码输出（长度前缀的 NAL）→ Annex-B；关键帧前面加上参数集（SPS/PPS，HEVC 还有 VPS）。
fn to_annexb(sb: &CMSampleBuffer, codec: Codec) -> Result<EncodedFrame, String> {
    let desc = unsafe { sb.format_description() };
    let (params, nal_len) = match &desc {
        Some(d) => parameter_sets(d, codec),
        None => (Vec::new(), 4),
    };
    let block = unsafe { sb.data_buffer() }.ok_or("the encoded frame has no data")?;
    let len = unsafe { block.data_length() };
    let mut raw = vec![0u8; len];
    if len > 0 {
        let status = unsafe {
            block.copy_data_bytes(0, len, NonNull::new_unchecked(raw.as_mut_ptr().cast()))
        };
        if status != 0 {
            return Err(format!("CMBlockBufferCopyDataBytes: OSStatus {status}"));
        }
    }
    let body = length_prefixed_to_annexb(&raw, nal_len)?;
    let hevc = codec == Codec::H265;
    let key =
        annexb_nal_types(&body, hevc).any(|t| if hevc { (16..=23).contains(&t) } else { t == 5 });
    let mut data = Vec::with_capacity(body.len() + if key { 256 } else { 0 });
    if key {
        for p in &params {
            data.extend_from_slice(&START_CODE);
            data.extend_from_slice(p);
        }
    }
    data.extend_from_slice(&body);
    Ok(EncodedFrame { data, key })
}

/// 格式描述里的参数集，以及 NAL 长度字段的字节数。
fn parameter_sets(desc: &CMFormatDescription, codec: Codec) -> (Vec<Vec<u8>>, usize) {
    let get = |i: usize,
               ptr: &mut *const u8,
               size: &mut usize,
               count: &mut usize,
               nal: &mut i32| unsafe {
        match codec {
            Codec::H265 => {
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(desc, i, ptr, size, count, nal)
            }
            _ => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(desc, i, ptr, size, count, nal),
        }
    };
    let (mut ptr, mut size, mut count, mut nal) = (std::ptr::null(), 0usize, 0usize, 4i32);
    if get(0, &mut ptr, &mut size, &mut count, &mut nal) != 0 {
        return (Vec::new(), 4);
    }
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        if get(i, &mut ptr, &mut size, &mut 0, &mut 0) == 0 && !ptr.is_null() {
            sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
        }
    }
    (sets, nal.clamp(1, 4) as usize)
}

/// 每个 NAL 前面是 `nal_len` 字节的大端长度 → 每个 NAL 前面是起始码。
fn length_prefixed_to_annexb(raw: &[u8], nal_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(raw.len() + 16);
    let mut off = 0;
    while off + nal_len <= raw.len() {
        let size = raw[off..off + nal_len]
            .iter()
            .fold(0usize, |n, &b| (n << 8) | b as usize);
        off += nal_len;
        if size == 0 {
            continue;
        }
        let end = off
            .checked_add(size)
            .filter(|&e| e <= raw.len())
            .ok_or("truncated NAL unit in the encoder output")?;
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&raw[off..end]);
        off = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_to_annexb() {
        let raw = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        assert_eq!(
            length_prefixed_to_annexb(&raw, 4).unwrap(),
            vec![0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]
        );
        assert_eq!(
            length_prefixed_to_annexb(&[0, 2, 0x65, 0xaa], 2).unwrap(),
            vec![0, 0, 0, 1, 0x65, 0xaa]
        );
        assert!(length_prefixed_to_annexb(&[0, 0, 0, 9, 0x65], 4).is_err());
    }

    /// 宽度没有对齐（CoreVideo 在行末补字节）时颜色转换也要成功，而且每一行都对
    #[test]
    fn converts_unaligned_widths() {
        if !has_hardware(Codec::H264) {
            println!("没有硬件编码器，跳过");
            return;
        }
        // 纯红色：BT.709 有限范围 Y=63、U=102、V=240
        for (w, h) in [(1590u32, 994u32), (1662, 1080), (1728, 1080), (722, 406)] {
            let mut enc = VtEncoder::new(Codec::H264, w, h, 30, 4000, None).unwrap();
            let red = [0u8, 0, 255, 255].repeat((w * h) as usize);
            let pb = enc.pixel_buffer(&red).unwrap();
            let ro = CVPixelBufferLockFlags::ReadOnly;
            assert_eq!(unsafe { CVPixelBufferLockBaseAddress(&pb, ro) }, 0);
            let plane = |i: usize, rows: usize, row: usize| {
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&pb, i);
                let p = CVPixelBufferGetBaseAddressOfPlane(&pb, i) as *const u8;
                let all = unsafe { std::slice::from_raw_parts(p, stride * rows) };
                (
                    stride,
                    all.chunks(stride)
                        .map(|r| r[..row].to_vec())
                        .collect::<Vec<_>>(),
                )
            };
            let (y_stride, y) = plane(0, h as usize, w as usize);
            let (uv_stride, uv) = plane(1, h as usize / 2, w as usize);
            unsafe { CVPixelBufferUnlockBaseAddress(&pb, ro) };
            println!("{w}x{h}: Y 每行 {y_stride} 字节，UV 每行 {uv_stride} 字节");
            assert!(
                y.iter().flatten().all(|&v| v.abs_diff(63) <= 1),
                "{w}x{h} Y"
            );
            for row in &uv {
                for p in row.as_chunks::<2>().0 {
                    assert!(
                        p[0].abs_diff(102) <= 1 && p[1].abs_diff(240) <= 1,
                        "{w}x{h} UV {p:?}"
                    );
                }
            }
            assert!(
                enc.encode(&red, true).unwrap().is_some(),
                "{w}x{h} 没有编出画面"
            );
        }
    }

    #[test]
    fn hardware_probe() {
        let h264 = has_hardware(Codec::H264);
        println!(
            "H.264 {h264}, HEVC {}, AV1 {}",
            has_hardware(Codec::H265),
            has_hardware(Codec::AV1)
        );
    }

    /// 编码出的码流：Annex-B，第一帧是带参数集的关键帧，之后是 P 帧，强制关键帧有效
    #[test]
    fn encodes_annexb() {
        for codec in [Codec::H264, Codec::H265] {
            if !has_hardware(codec) {
                println!("{codec:?}: 没有硬件编码器，跳过");
                continue;
            }
            let (w, h) = (1280u32, 720u32);
            let mut enc = VtEncoder::new(codec, w, h, 30, 4000, None).unwrap();
            let mut frame = vec![0u8; (w * h * 4) as usize];
            let hevc = codec == Codec::H265;
            let mut kinds = Vec::new();
            for i in 0..12u32 {
                // 每帧画面都不一样（移动的色块），保证编码器有东西可编
                for (p, px) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    let (x, y) = (p as u32 % w, p as u32 / w);
                    let on = (x / 32 + y / 32 + i) % 2 == 0;
                    *px = if on {
                        [200, 80, 30, 255]
                    } else {
                        [20, 40, 220, 255]
                    };
                }
                let t = Instant::now();
                let f = enc
                    .encode(&frame, i == 0 || i == 8)
                    .unwrap()
                    .expect("编码器丢了帧");
                let types: Vec<u8> = annexb_nal_types(&f.data, hevc).collect();
                assert!(f.data.starts_with(&START_CODE), "不是 Annex-B");
                println!(
                    "{codec:?} #{i}: {} 字节，{:?}，NAL {types:?}",
                    f.data.len(),
                    t.elapsed()
                );
                kinds.push(f.key);
                if f.key {
                    // 参数集：H.264 SPS(7)/PPS(8)，HEVC VPS(32)/SPS(33)/PPS(34)
                    let need: &[u8] = if hevc { &[32, 33, 34] } else { &[7, 8] };
                    for n in need {
                        assert!(types.contains(n), "关键帧缺少参数集 {n}");
                    }
                }
            }
            assert!(kinds[0] && kinds[8], "请求的关键帧没有生效: {kinds:?}");
            assert!(
                !kinds[1..8].iter().any(|&k| k),
                "出现了多余的关键帧: {kinds:?}"
            );
            enc.set_bitrate(1000);
            assert!(enc.encode(&frame, false).unwrap().is_some());
        }
    }
}
