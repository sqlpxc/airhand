use airhand_proto::{
    Features, features, framing,
    handshake::ControllerIntro,
    media::{self, VideoHeader},
    session::{Codec, ControllerMsg, HostMsg, QualitySettings},
    video::*,
};

fn receiver() -> ReceiverCapabilities {
    ReceiverCapabilities {
        codecs: vec![DecodeCapability {
            codec: Codec::H264,
            max_width: Some(1920),
            max_height: Some(1080),
            max_fps: Some(30),
        }],
    }
}

#[test]
fn receiver_limits_and_view_preferences_have_distinct_meanings() {
    let r = receiver();
    assert!(r.valid());
    let want = QualitySettings {
        codec: Codec::AV1,
        fps: 120,
        max_short: 0,
        ..Default::default()
    };
    let selected = r
        .select(want, &[Codec::AV1, Codec::H264], 3840, 2160)
        .unwrap();
    assert_eq!(
        (selected.codec, selected.fps, selected.max_short),
        (Codec::H264, 30, 1080)
    );
    let portrait = r.select(want, &[Codec::H264], 1080, 2400).unwrap();
    assert_eq!(portrait.max_short, 486);
    assert!(r.select(want, &[Codec::AV1], 1920, 1080).is_none());
    assert!(!r.accepts(Codec::H264, 1920, 1080, 60));
    assert!(!r.accepts(Codec::H265, 1920, 1080, 30));
    let mut bad = r.clone();
    bad.codecs.push(bad.codecs[0].clone());
    assert!(!bad.valid());
    bad.codecs.clear();
    assert!(!bad.valid());
}

#[test]
fn video_context_extends_without_changing_the_legacy_header_or_payload() {
    let h = VideoHeader {
        seq: u32::MAX,
        key: true,
        codec: Codec::H264,
        width: 1920,
        height: 1080,
        capture_us: 50,
    };
    let context = VideoContext {
        config_id: 7,
        instance: MediaInstance::default(),
    };
    assert_eq!(h.encode_with_context(None), h.encode());
    let wire = media::frame(&h.encode_with_context(Some(context)), &[b"video"]);
    let packet = &wire[4..];
    assert_eq!(VideoHeader::decode(packet), Some(h));
    assert_eq!(VideoHeader::context(packet), Some(context));
    assert_eq!(media::payload(packet), Some(b"video".as_slice()));
    assert_eq!(&packet[24..36], &[7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut short = packet.to_vec();
    short[3] = 24;
    assert!(VideoHeader::context(&short).is_none());
}

#[test]
fn handshake_video_offer_is_optional_and_old_decoders_skip_it() {
    #[derive(minicbor::Encode, minicbor::Decode)]
    #[cbor(map)]
    struct OldIntro {
        #[n(0)]
        min: u16,
        #[n(1)]
        max: u16,
        #[n(2)]
        features: Features,
        #[n(3)]
        name: String,
        #[n(4)]
        os: String,
        #[n(5)]
        settings: QualitySettings,
        #[n(6)]
        audio: bool,
    }
    let old = OldIntro {
        min: 1,
        max: 1,
        features: Features::NONE,
        name: "old".into(),
        os: "test".into(),
        settings: Default::default(),
        audio: false,
    };
    let mut new: ControllerIntro = framing::decode_known(&framing::to_vec(&old)).unwrap();
    assert!(new.video.is_none());
    new.video = Some(VideoOffer {
        receiver: receiver(),
        view: ViewRequest {
            request_id: 0,
            preference: old.settings,
        },
    });
    let back: OldIntro = framing::decode_known(&framing::to_vec(&new)).unwrap();
    assert_eq!(back.settings, old.settings);
    assert_eq!(back.name, "old");
}

#[test]
fn new_video_messages_are_skippable_and_replacement_is_not_advertised() {
    #[derive(minicbor::Decode)]
    #[allow(dead_code)]
    enum OldController {
        #[n(2)]
        Ack {
            #[n(0)]
            seq: u32,
        },
    }
    #[derive(minicbor::Decode)]
    #[allow(dead_code)]
    enum OldHost {
        #[n(3)]
        Pong {
            #[n(0)]
            t: u64,
        },
    }
    let request = ControllerMsg::View(ViewRequest {
        request_id: 1,
        preference: Default::default(),
    });
    assert!(matches!(
        framing::decode::<OldController>(&framing::to_vec(&request)).unwrap(),
        framing::Decoded::Unknown(9)
    ));
    let config = HostMsg::VideoConfig(VideoConfig {
        context: VideoContext {
            config_id: 1,
            instance: Default::default(),
        },
        request_id: 0,
        codec: Codec::H264,
        width: 64,
        height: 64,
        fps: 30,
        bitrate_kbps: 1000,
    });
    assert!(matches!(
        framing::decode::<OldHost>(&framing::to_vec(&config)).unwrap(),
        framing::Decoded::Unknown(12)
    ));
    assert!(!features::session::SUPPORTED.has(features::session::MEDIA_INSTANCES));
    assert!(!features::signal::SUPPORTED.has(features::signal::MEDIA_INSTANCES));
    assert!(features::session::SUPPORTED.has(features::session::VIDEO_MODEL));
}

#[test]
fn feedback_distinguishes_receipt_decode_and_render_progress_even_at_sequence_wrap() {
    let mut f = VideoFeedback {
        context: VideoContext {
            config_id: 1,
            instance: Default::default(),
        },
        sample_id: 1,
        received_seq: 0,
        decoded_seq: Some(u32::MAX),
        presented_seq: None,
        pending_frames: 1,
        oldest_pending_ms: 5,
        decode_time_us: 7000,
        dropped_frames: 0,
        decoder_errors: 0,
    };
    assert!(f.valid());
    f.presented_seq = Some(0);
    assert!(!f.valid());
    f.decoded_seq = Some(0);
    assert!(f.valid());
    f.oldest_pending_ms = 60_001;
    assert!(!f.valid());
}
