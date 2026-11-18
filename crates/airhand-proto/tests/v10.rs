//! v10 新增的线上格式：帧流的会话头、画面包头的参考帧扩展、`ControllerDatagram::Lost`；
//! 帧流交付报告和多屏反馈。
//! 剪贴板里的文件（文件通道、`FileMsg`，画面和大块通道的优先级因此从 0 提到 1）。
//! 和 v9 的样例一样，发布之后不要随实现重新生成（见 `v9.rs`）。
use airhand_proto::media::VideoHeader;
use airhand_proto::session::{
    Codec, ControllerDatagram, ControllerMsg, InputEvent, QualitySettings, StreamView, VideoReport,
};
use airhand_proto::{SessionHeader, SessionLane, features, framing};

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn frame_stream_headers_keep_their_wire_encoding() {
    assert!(
        airhand_proto::signal::Versions::SIGNAL
            .common(airhand_proto::signal::Versions { min: 9, max: 9 })
            .is_none()
    );
    let h = SessionHeader::new(0x0102_0304_0506_0708, SessionLane::Video(0));
    assert_eq!(hex(&h.encode()), "41485339080706050403020101010100");
}

#[test]
fn the_reference_extension_keeps_its_wire_encoding() {
    let h = VideoHeader {
        seq: 0x10,
        stream: 0,
        key: false,
        codec: Codec::H264,
        width: 8,
        height: 4,
        capture_us: 1,
        reference: Some(0x0f),
    };
    assert_eq!(
        hex(&h.encode()),
        "0100001e100000000800040000000000010000000000000001040f000000"
    );
}

#[test]
fn frames_messages_keep_their_wire_encoding() {
    assert_eq!(
        hex(&framing::to_vec(&ControllerDatagram::Lost {
            decoded: 99,
            stream: 0
        })),
        "8203811863"
    );
}

#[test]
fn multi_display_formats_keep_their_wire_encoding() {
    assert_eq!(airhand_proto::MAX_STREAMS, 4);
    // 画面的第 1 路是 3 号通道
    let h = SessionHeader::new(0x0102_0304_0506_0708, SessionLane::Video(1));
    assert_eq!(hex(&h.encode()), "41485339080706050403020103010100");
    // 画面包头的第 12 个字节
    let v = VideoHeader {
        seq: 1,
        stream: 2,
        key: true,
        codec: Codec::H264,
        width: 8,
        height: 4,
        capture_us: 1,
        reference: None,
    };
    assert_eq!(
        hex(&v.encode()),
        "010100180100000008000400020000000100000000000000"
    );
    let ack = ControllerDatagram::Ack {
        seq: 5,
        arrived: 6,
        stream: 1,
    };
    assert_eq!(
        hex(&framing::to_vec(&ack)),
        "8201830506 01".replace(' ', "")
    );
    let lost = ControllerDatagram::Lost {
        decoded: 7,
        stream: 3,
    };
    assert_eq!(hex(&framing::to_vec(&lost)), "8203820703");
    let keyframe = ControllerMsg::RequestKeyframe {
        after: 5,
        stream: 2,
    };
    assert_eq!(hex(&framing::to_vec(&keyframe)), "8204820502");
    let view = ControllerMsg::View {
        streams: vec![
            StreamView::display(1),
            StreamView {
                display: 0,
                max_short: 720,
            },
        ],
    };
    assert_eq!(
        hex(&framing::to_vec(&view)),
        "820b81828201008200 1902d0".replace(' ', "")
    );
    let mv = ControllerMsg::Input {
        t: 0,
        events: vec![InputEvent::MouseMove {
            x: 0.5,
            y: 0.25,
            stream: 1,
        }],
    };
    assert_eq!(
        hex(&framing::to_vec(&mv)),
        "8201820081820083fa3f000000fa3e80000001"
    );
}

#[test]
fn file_formats_keep_their_wire_encoding() {
    use airhand_proto::files::{FileEntry, FileError, FileKind, FileMsg, reset};
    use airhand_proto::session::HostMsg;
    assert_eq!(features::session::FILES.bits(), 8);
    // 文件通道是 6 号，只用帧流，优先级 0
    let h = SessionHeader::new(0x0102_0304_0506_0708, SessionLane::File);
    assert_eq!(hex(&h.encode()), "41485339080706050403020106000100");
    assert_eq!((reset::CANCELLED, reset::RELAY_LIMIT), (0x10, 0x20));
    let offer = HostMsg::Files(Some(FileMsg::Offer {
        clip: 1,
        items: vec![FileEntry {
            path: vec!["a".into()],
            kind: FileKind::File,
            size: 3,
            mtime: None,
            target: None,
        }],
    }));
    assert_eq!(
        hex(&framing::to_vec(&offer)),
        "820d818200820181838161610003"
    );
    let fetch = ControllerMsg::Files(Some(FileMsg::Fetch {
        fetch: 2,
        clip: 1,
        path: vec!["a".into()],
        offset: 0,
        size: 3,
        mtime: Some(4),
        until: 5,
    }));
    assert_eq!(
        hex(&framing::to_vec(&fetch)),
        "820c818204870201816161000304 05".replace(' ', "")
    );
    let failed = HostMsg::Files(Some(FileMsg::Failed {
        fetch: 2,
        error: FileError::TooLarge,
    }));
    assert_eq!(hex(&framing::to_vec(&failed)), "820d818207820204");
}

#[test]
fn stream_reports_and_quality_keep_their_wire_encoding() {
    let report = VideoReport {
        frames: 60,
        stalled: 2,
    };
    let first = ControllerDatagram::Report { report, stream: 0 };
    assert_eq!(hex(&framing::to_vec(&first)), "82048182183c02");
    let second = ControllerDatagram::Report { report, stream: 2 };
    assert_eq!(hex(&framing::to_vec(&second)), "82048282183c0202");
    let quality = QualitySettings::default();
    assert_eq!(
        hex(&framing::to_vec(&ControllerMsg::Settings(quality))),
        "82038185181e190fa0190438f500"
    );
    let cursor = QualitySettings {
        draw_cursor: true,
        ..quality
    };
    assert_eq!(
        hex(&framing::to_vec(&ControllerMsg::Settings(cursor))),
        "82038186181e190fa0190438f500f5"
    );
}
