//! Per-session video contract. The transport ACK remains in `flow`; decoder telemetry never
//! releases a send window. Producers report actual parameters, independent of viewer preferences.
use std::sync::Mutex;
use std::time::{Duration, Instant};

use airhand_proto::media::VideoHeader;
use airhand_proto::session::{Codec, HostMsg, QualitySettings};
use airhand_proto::video::{
    MediaInstance, VideoConfig, VideoContext, VideoFeedback, VideoOffer, ViewRequest,
};
use anyhow::{Result, ensure};

use crate::host::Outbox;

#[derive(Debug, Clone, Copy)]
pub struct SelectedVideo {
    pub request_id: u32,
    pub settings: QualitySettings,
}

struct State {
    offer: Option<VideoOffer>,
    config: Option<VideoConfig>,
    next_config: u32,
    restart: bool,
    first_seq: u32,
    last_seq: u32,
    feedback: Option<(VideoFeedback, Instant)>,
}

pub struct VideoSession {
    state: Mutex<State>,
    out: Outbox,
}

impl VideoSession {
    pub fn new(offer: Option<VideoOffer>, out: Outbox) -> Self {
        Self {
            state: Mutex::new(State {
                offer,
                config: None,
                next_config: 1,
                restart: false,
                first_seq: 0,
                last_seq: 0,
                feedback: None,
            }),
            out,
        }
    }

    pub fn receiver(&self) -> Option<airhand_proto::video::ReceiverCapabilities> {
        self.state
            .lock()
            .unwrap()
            .offer
            .as_ref()
            .map(|o| o.receiver.clone())
    }

    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().offer.is_some()
    }

    pub fn view(&self, mut request: ViewRequest) -> bool {
        let mut s = self.state.lock().unwrap();
        let Some(offer) = &mut s.offer else {
            return false;
        };
        if request.request_id <= offer.view.request_id {
            return false;
        }
        request.preference = request.preference.sanitized();
        offer.view = request;
        true
    }

    pub fn select(
        &self,
        fallback: QualitySettings,
        host: &[Codec],
        width: u32,
        height: u32,
    ) -> Result<SelectedVideo> {
        let s = self.state.lock().unwrap();
        match &s.offer {
            None => Ok(SelectedVideo {
                request_id: 0,
                settings: fallback.sanitized(),
            }),
            Some(offer) => {
                let settings = offer
                    .receiver
                    .select(offer.view.preference, host, width, height)
                    .ok_or_else(|| anyhow::anyhow!("no common video codec"))?;
                Ok(SelectedVideo {
                    request_id: offer.view.request_id,
                    settings,
                })
            }
        }
    }

    /// Rebuilding even an identically configured encoder gives feedback a new context.
    pub fn restart(&self) {
        self.state.lock().unwrap().restart = true;
    }

    /// Called before exposing a frame to the receiver. Control and video may arrive in either
    /// order; the context in the packet is authoritative for associating feedback.
    pub fn header(
        &self,
        h: &VideoHeader,
        selected: SelectedVideo,
        bitrate_kbps: u32,
    ) -> Result<Vec<u8>> {
        let mut s = self.state.lock().unwrap();
        let Some(offer) = &s.offer else {
            return Ok(h.encode().to_vec());
        };
        ensure!(
            offer.receiver.accepts(
                h.codec,
                h.width.into(),
                h.height.into(),
                selected.settings.fps
            ),
            "encoder output exceeds the receiver's video capabilities"
        );
        let mut config = VideoConfig {
            context: s.config.map_or(VideoContext::default(), |c| c.context),
            request_id: selected.request_id,
            codec: h.codec,
            width: h.width,
            height: h.height,
            fps: selected.settings.fps,
            bitrate_kbps,
        };
        if s.restart || s.config != Some(config) {
            config.context = VideoContext {
                config_id: s.next_config,
                instance: MediaInstance::default(),
            };
            let next = s
                .next_config
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("video configuration IDs exhausted"))?;
            ensure!(config.valid(), "invalid encoder video configuration");
            self.out.send(&HostMsg::VideoConfig(config))?;
            s.next_config = next;
            s.config = Some(config);
            s.restart = false;
            s.first_seq = h.seq;
            s.feedback = None;
        }
        s.last_seq = h.seq;
        Ok(h.encode_with_context(Some(config.context)))
    }

    /// Reject old contexts, duplicate samples and progress outside frames issued in this context.
    pub fn feedback(&self, feedback: VideoFeedback) -> bool {
        if !feedback.valid() {
            return false;
        }
        let progress = |old: Option<u32>, new: Option<u32>| {
            old.is_none_or(|old| new.is_some_and(|new| new.wrapping_sub(old) as i32 >= 0))
        };
        let mut s = self.state.lock().unwrap();
        if s.config.is_none_or(|c| c.context != feedback.context)
            || s.feedback.is_some_and(|(old, _)| {
                feedback.sample_id <= old.sample_id
                    || (feedback.received_seq.wrapping_sub(old.received_seq) as i32) < 0
                    || !progress(old.decoded_seq, feedback.decoded_seq)
                    || !progress(old.presented_seq, feedback.presented_seq)
                    || feedback.dropped_frames < old.dropped_frames
                    || feedback.decoder_errors < old.decoder_errors
            })
        {
            return false;
        }
        let in_range = |seq: u32| {
            seq.wrapping_sub(s.first_seq) as i32 >= 0 && s.last_seq.wrapping_sub(seq) as i32 >= 0
        };
        if !in_range(feedback.received_seq)
            || feedback.decoded_seq.is_some_and(|seq| !in_range(seq))
            || feedback.presented_seq.is_some_and(|seq| !in_range(seq))
        {
            return false;
        }
        s.feedback = Some((feedback, Instant::now()));
        true
    }

    /// Stale telemetry must not drive a future adaptive policy after the receiver stops reporting.
    pub fn latest_feedback(&self) -> Option<VideoFeedback> {
        self.state
            .lock()
            .unwrap()
            .feedback
            .filter(|(_, at)| at.elapsed() <= Duration::from_secs(2))
            .map(|(f, _)| f)
    }
}
