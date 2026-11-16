//! 软件编码兜底：OpenH264（屏幕内容实时模式）。

use std::time::Instant;

use airhand_proto::session::{Codec, EncoderInfo};
use anyhow::{Result, anyhow};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, QpRange,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSource;
use yuvutils_rs::{YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

use super::{EncodedFrame, VideoEncoder};

struct I420 {
    width: u32,
    height: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }
    fn strides(&self) -> (usize, usize, usize) {
        let c = self.width.div_ceil(2) as usize;
        (self.width as usize, c, c)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

pub struct OpenH264Encoder {
    enc: Encoder,
    yuv: I420,
    info: EncoderInfo,
    kbps: u32,
    epoch: Instant,
}

impl OpenH264Encoder {
    pub fn new(w: u32, h: u32, fps: u8, kbps: u32) -> Result<Self> {
        // 码率越高，允许的最低 QP 越低（画质上限越高）
        let min_qp = if kbps >= 15_000 {
            10
        } else if kbps >= 6_000 {
            14
        } else {
            18
        };
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            .skip_frames(false)
            .complexity(Complexity::Low)
            .qp(QpRange::new(min_qp, 42))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId)
            .intra_frame_period(IntraFramePeriod::from_num_frames(0))
            .scene_change_detect(true)
            .adaptive_quantization(false)
            .background_detection(false)
            .vui(VuiConfig::bt709());
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg)
            .map_err(|e| anyhow!("can't initialize OpenH264: {e}"))?;
        let (cw, ch) = (w.div_ceil(2) as usize, h.div_ceil(2) as usize);
        Ok(Self {
            enc,
            yuv: I420 {
                width: w,
                height: h,
                y: vec![0; (w * h) as usize],
                u: vec![128; cw * ch],
                v: vec![128; cw * ch],
            },
            info: EncoderInfo {
                codec: Codec::H264,
                name: "OpenH264".into(),
                hardware: false,
                width: w,
                height: h,
                // 第几路画面由编码管线填（见 pipeline）
                stream: 0,
            },
            kbps,
            epoch: Instant::now(),
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn info(&self) -> &EncoderInfo {
        &self.info
    }

    fn set_bitrate(&mut self, kbps: u32) {
        if kbps == self.kbps {
            return;
        }
        self.kbps = kbps;
        let mut info = openh264_sys2::SBitrateInfo {
            iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
            iBitrate: kbps.saturating_mul(1000).min(i32::MAX as u32) as i32,
        };
        unsafe {
            let api = self.enc.raw_api();
            api.set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                (&raw mut info).cast(),
            );
            api.set_option(
                openh264_sys2::ENCODER_OPTION_MAX_BITRATE,
                (&raw mut info).cast(),
            );
        }
    }

    fn encode(&mut self, bgra: &[u8], force_key: bool) -> Result<Option<EncodedFrame>> {
        let (w, h) = (self.yuv.width, self.yuv.height);
        let cw = w.div_ceil(2);
        let mut planar = YuvPlanarImageMut {
            y_plane: yuvutils_rs::BufferStoreMut::Borrowed(&mut self.yuv.y),
            y_stride: w,
            u_plane: yuvutils_rs::BufferStoreMut::Borrowed(&mut self.yuv.u),
            u_stride: cw,
            v_plane: yuvutils_rs::BufferStoreMut::Borrowed(&mut self.yuv.v),
            v_stride: cw,
            width: w,
            height: h,
        };
        yuvutils_rs::bgra_to_yuv420(
            &mut planar,
            bgra,
            w * 4,
            YuvRange::Limited,
            YuvStandardMatrix::Bt709,
            YuvConversionMode::Balanced,
        )
        .map_err(|e| anyhow!("color conversion failed: {e:?}"))?;

        if force_key {
            self.enc.force_intra_frame();
        }
        let ts = openh264::Timestamp::from_millis(self.epoch.elapsed().as_millis() as u64);
        let bs = self
            .enc
            .encode_at(&self.yuv, ts)
            .map_err(|e| anyhow!("encoding failed: {e}"))?;
        let ft = bs.frame_type();
        if matches!(ft, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let mut data = Vec::with_capacity(64 * 1024);
        bs.write_vec(&mut data);
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(EncodedFrame {
            data,
            key: matches!(ft, FrameType::IDR | FrameType::I),
            ltr: None,
        }))
    }
}
