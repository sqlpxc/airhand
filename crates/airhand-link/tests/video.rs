#![cfg(feature = "client")]
use airhand_link::{host::queue, video::VideoSession};
use airhand_proto::{
    framing,
    media::VideoHeader,
    session::{Codec, HostMsg, QualitySettings},
    video::*,
};

fn offer() -> VideoOffer {
    VideoOffer {
        receiver: ReceiverCapabilities {
            codecs: vec![DecodeCapability {
                codec: Codec::H264,
                max_width: Some(1920),
                max_height: Some(1080),
                max_fps: Some(60),
            }],
        },
        view: ViewRequest {
            request_id: 0,
            preference: QualitySettings::default(),
        },
    }
}
fn header(seq: u32) -> VideoHeader {
    VideoHeader {
        seq,
        key: true,
        codec: Codec::H264,
        width: 1920,
        height: 1080,
        capture_us: 5,
    }
}
fn feedback(context: VideoContext, seq: u32) -> VideoFeedback {
    VideoFeedback {
        context,
        sample_id: 1,
        received_seq: seq,
        decoded_seq: Some(seq),
        presented_seq: Some(seq),
        pending_frames: 0,
        oldest_pending_ms: 0,
        decode_time_us: 7000,
        dropped_frames: 0,
        decoder_errors: 0,
    }
}

#[test]
fn configs_identify_actual_output_and_stale_feedback_cannot_affect_a_rebuilt_encoder() {
    let (out, mut control, _bulk) = queue::channel();
    let video = VideoSession::new(Some(offer()), out);
    let selected = video
        .select(Default::default(), &[Codec::H264], 1920, 1080)
        .unwrap();
    let bytes = video.header(&header(8), selected, 3000).unwrap();
    let context = VideoHeader::context(&bytes).unwrap();
    let config: HostMsg = framing::decode_known(&control.try_recv().unwrap()[4..]).unwrap();
    assert!(
        matches!(config, HostMsg::VideoConfig(c) if c.context == context && c.bitrate_kbps == 3000 && c.request_id == 0)
    );
    assert!(video.feedback(feedback(context, 8)));
    assert!(!video.feedback(feedback(context, 8)), "duplicate sample");
    let mut future = feedback(context, 9);
    future.sample_id = 2;
    assert!(!video.feedback(future));
    video.restart();
    let next = VideoHeader::context(&video.header(&header(9), selected, 3000).unwrap()).unwrap();
    assert_ne!(next, context);
    assert!(!video.feedback(feedback(context, 8)));
    assert!(video.latest_feedback().is_none());
    assert!(video.feedback(feedback(next, 9)));
    assert_eq!(video.latest_feedback().unwrap().context, next);
}

#[test]
fn telemetry_does_not_ack_frames_and_view_updates_keep_receiver_limits() {
    let (out, _control, _bulk) = queue::channel();
    let video = VideoSession::new(Some(offer()), out);
    let mut flow = airhand_link::flow::FlowState::default();
    flow.on_sent(1, 100);
    let request = ViewRequest {
        request_id: 1,
        preference: QualitySettings {
            fps: 120,
            max_short: 0,
            ..Default::default()
        },
    };
    assert!(video.view(request));
    assert!(!video.view(request));
    let s = video
        .select(Default::default(), &[Codec::H264], 3840, 2160)
        .unwrap();
    assert_eq!(
        (s.request_id, s.settings.fps, s.settings.max_short),
        (1, 60, 1080)
    );
    let context = VideoHeader::context(&video.header(&header(1), s, 4000).unwrap()).unwrap();
    assert!(video.feedback(feedback(context, 1)));
    assert_eq!(flow.inflight(), 1);
    flow.on_ack(1);
    assert_eq!(flow.inflight(), 0);
    let mut oversize = header(2);
    oversize.width = 3840;
    assert!(video.header(&oversize, s, 4000).is_err());
}

#[test]
fn legacy_session_has_no_new_events_or_header_and_ignores_feedback() {
    let (out, mut control, _bulk) = queue::channel();
    let video = VideoSession::new(None, out);
    let h = header(1);
    let s = video
        .select(Default::default(), &[Codec::H264], 1920, 1080)
        .unwrap();
    assert_eq!(video.header(&h, s, 4000).unwrap(), h.encode());
    assert!(control.try_recv().is_none());
    assert!(!video.feedback(feedback(
        VideoContext {
            config_id: 1,
            instance: Default::default()
        },
        1
    )));
}
