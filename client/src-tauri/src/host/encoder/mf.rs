//! Windows Media Foundation 硬件编码（NVENC / Intel Quick Sync / AMD AMF 都以硬件 MFT 形式提供）。
//!
//! * 低延迟模式、无 B 帧、超长 GOP（只在需要时插入关键帧）；
//! * BGRA 直接转换进 MF 缓冲区（NV12），没有多余拷贝；
//! * 硬件 MFT 通常是异步的，这里用事件轮询把它变成"一进一出"的同步调用。

use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use airhand_proto::session::{Codec, EncoderInfo};
use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR};
use yuvutils_rs::{YuvBiPlanarImageMut, YuvConversionMode, YuvRange, YuvStandardMatrix};

use super::{EncodedFrame, VideoEncoder, annexb_nal_types};

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

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

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
}

impl MfEncoder {
    pub fn new(codec: Codec, w: u32, h: u32, fps: u8, kbps: u32) -> Result<Self> {
        ensure_init()?;
        let mut last_err = anyhow!("no hardware encoder available");
        for activate in enum_hardware(codec) {
            let name = friendly_name(&activate);
            match unsafe { Self::open(activate.clone(), &name, codec, w, h, fps, kbps) } {
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

    unsafe fn open(
        activate: IMFActivate,
        name: &str,
        codec: Codec,
        w: u32,
        h: u32,
        fps: u8,
        kbps: u32,
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

            let codec_api: Option<ICodecAPI> = transform.cast().ok();
            let bps = kbps.saturating_mul(1000);
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
                },
                seq_header,
                need_input: 0,
                have_output: 0,
                provides_samples,
                out_size: osi.cbSize.max(w * h * 3 / 2),
                kbps,
                fps,
                epoch: Instant::now(),
            };
            // 冒烟测试：编码一帧黑屏，确保这个编码器在当前参数下真的能工作
            let black = vec![0u8; (w * h * 4) as usize];
            enc.encode(&black, true).context("test encode failed")?;
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
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let dur = 10_000_000i64 / self.fps.max(1) as i64;
            sample.SetSampleTime(self.epoch.elapsed().as_nanos() as i64 / 100)?;
            sample.SetSampleDuration(dur)?;
            Ok(sample)
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
            Ok(EncodedFrame { data, key })
        }
    }
}

impl VideoEncoder for MfEncoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
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
        if force_key && let Some(api) = &self.codec_api {
            unsafe {
                let _ = api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
        }
        let sample = self.make_input(bgra)?;
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
