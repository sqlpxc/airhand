//! Video negotiation and receiver telemetry. These are independent of the UI and the codec API.
//!
//! `ReceiverCapabilities` contains hard limits; `ViewRequest` contains preferences. A host reports
//! the selected `VideoConfig`, and every extended video packet identifies that config. Feedback is
//! cumulative within that context and never acknowledges transport bytes. `presented_seq` means
//! submitted to the renderer (possibly scheduled), not measured physical scanout.

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::session::{Codec, QualitySettings};

pub const MAX_VIDEO_CAPABILITIES: usize = 16;
pub const FEEDBACK_INTERVAL_MS: u64 = 200;

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct DecodeCapability {
    #[n(0)]
    pub codec: Codec,
    /// None means unknown, not a promise that every size is supported.
    #[n(1)]
    #[serde(default)]
    pub max_width: Option<u16>,
    #[n(2)]
    #[serde(default)]
    pub max_height: Option<u16>,
    #[n(3)]
    #[serde(default)]
    pub max_fps: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct ReceiverCapabilities {
    #[n(0)]
    pub codecs: Vec<DecodeCapability>,
}

impl ReceiverCapabilities {
    pub fn valid(&self) -> bool {
        !self.codecs.is_empty()
            && self.codecs.len() <= MAX_VIDEO_CAPABILITIES
            && self.codecs.iter().enumerate().all(|(i, c)| {
                c.max_width.is_none_or(|n| n >= 64)
                    && c.max_height.is_none_or(|n| n >= 64)
                    && c.max_fps.is_none_or(|n| (1..=120).contains(&n))
                    && !self.codecs[..i].iter().any(|old| old.codec == c.codec)
            })
    }

    pub fn capability(&self, codec: Codec) -> Option<&DecodeCapability> {
        self.codecs.iter().find(|c| c.codec == codec)
    }

    /// Intersect hard receiver limits with the request and the host's available formats.
    /// No common format is an explicit failure; never silently select an unsupported codec.
    pub fn select(
        &self,
        request: QualitySettings,
        host: &[Codec],
        width: u32,
        height: u32,
    ) -> Option<QualitySettings> {
        let mut s = request.sanitized();
        s.codec = [s.codec, Codec::AV1, Codec::H265, Codec::H264]
            .into_iter()
            .find(|c| c.is_known() && host.contains(c) && self.capability(*c).is_some())?;
        let cap = self.capability(s.codec)?;
        if let Some(fps) = cap.max_fps {
            s.fps = s.fps.min(fps);
        }
        if width > 0 && height > 0 {
            let scale = cap
                .max_width
                .map_or(1.0, |w| f64::from(w) / f64::from(width))
                .min(
                    cap.max_height
                        .map_or(1.0, |h| f64::from(h) / f64::from(height)),
                )
                .min(1.0);
            if scale < 1.0 {
                let short = (f64::from(width.min(height)) * scale)
                    .floor()
                    .clamp(1.0, f64::from(u16::MAX)) as u16;
                s.max_short = if s.max_short == 0 {
                    short
                } else {
                    s.max_short.min(short)
                };
            }
        }
        Some(s)
    }

    pub fn accepts(&self, codec: Codec, width: u32, height: u32, fps: u8) -> bool {
        self.capability(codec).is_some_and(|c| {
            c.max_width.is_none_or(|n| width <= u32::from(n))
                && c.max_height.is_none_or(|n| height <= u32::from(n))
                && c.max_fps.is_none_or(|n| fps <= n)
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct ViewRequest {
    /// Monotonically increasing within a session. Zero is the handshake's initial request.
    #[n(0)]
    pub request_id: u32,
    /// Desired output, distinct from the receiver's hard limits.
    #[n(1)]
    pub preference: QualitySettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct VideoOffer {
    #[n(0)]
    pub receiver: ReceiverCapabilities,
    #[n(1)]
    pub view: ViewRequest,
}

/// A logical media stream and its transport instance. The existing fixed video lane uses (0, 0).
/// Nonzero instances require BOTH future signal and session MEDIA_INSTANCES capabilities, which
/// are reserved but not advertised. A replacement must use independent authenticated nonce space.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct MediaInstance {
    #[n(0)]
    pub stream_id: u32,
    #[n(1)]
    pub generation: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct VideoContext {
    /// Nonzero, monotonically increasing configuration ID, never reused in a session.
    #[n(0)]
    pub config_id: u32,
    #[n(1)]
    pub instance: MediaInstance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct VideoConfig {
    #[n(0)]
    pub context: VideoContext,
    #[n(1)]
    pub request_id: u32,
    #[n(2)]
    pub codec: Codec,
    #[n(3)]
    pub width: u16,
    #[n(4)]
    pub height: u16,
    /// Selected encoder limits; measured throughput is reported separately.
    #[n(5)]
    pub fps: u8,
    #[n(6)]
    pub bitrate_kbps: u32,
}

impl VideoConfig {
    pub fn valid(&self) -> bool {
        self.context.config_id != 0
            && self.width >= 64
            && self.height >= 64
            && (1..=120).contains(&self.fps)
            && (100..=100_000).contains(&self.bitrate_kbps)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode, Serialize, Deserialize)]
#[cbor(map)]
#[serde(rename_all = "camelCase")]
pub struct VideoFeedback {
    #[n(0)]
    pub context: VideoContext,
    /// Increasing sample ID within this context. Counters are cumulative, so coalescing is safe.
    #[n(1)]
    pub sample_id: u32,
    #[n(2)]
    pub received_seq: u32,
    #[n(3)]
    pub decoded_seq: Option<u32>,
    #[n(4)]
    pub presented_seq: Option<u32>,
    /// Work awaiting decoder output, excluding intentional A/V playout waiting.
    #[n(5)]
    pub pending_frames: u32,
    /// Age of the oldest pending decode, including codec processing and internal queuing.
    #[n(6)]
    pub oldest_pending_ms: u32,
    /// Smoothed time from submission to decoder output, not pure hardware execution time.
    #[n(7)]
    pub decode_time_us: u32,
    #[n(8)]
    pub dropped_frames: u32,
    #[n(9)]
    pub decoder_errors: u32,
}

impl VideoFeedback {
    pub fn valid(&self) -> bool {
        let at_most = |seq: u32, end: u32| end.wrapping_sub(seq) as i32 >= 0;
        self.context.config_id != 0
            && self.pending_frames <= 1024
            && self.oldest_pending_ms <= 60_000
            && self.decode_time_us <= 60_000_000
            && self
                .decoded_seq
                .is_none_or(|s| at_most(s, self.received_seq))
            && self
                .presented_seq
                .is_none_or(|s| self.decoded_seq.is_some_and(|d| at_most(s, d)))
    }
}
