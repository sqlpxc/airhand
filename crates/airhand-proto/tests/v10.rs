//! 当前 v10 格式的固定字节样例，覆盖握手、流头、CBOR 消息和媒体包头。
use airhand_proto::handshake::{
    Confirm, ControllerIntro, HandshakeInit, HostVerdict, Pattern, Refusal,
};
use airhand_proto::media::VideoHeader;
use airhand_proto::minicbor::{Decode, Encode};
use airhand_proto::session::*;
use airhand_proto::signal::{ClientHello, ClientMsg, ServerHello};
use airhand_proto::{Role, SESSION_VERSION, SessionHeader, SessionLane, framing};

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

fn golden<T: Encode<()> + for<'b> Decode<'b, ()>>(value: T, expected: &str) {
    let wire = framing::to_vec(&value);
    assert_eq!(hex(&wire), expected);
    let decoded: T = framing::decode_known(&wire).unwrap();
    assert_eq!(framing::to_vec(&decoded), wire);
}

#[test]
fn signal_and_handshake_layout() {
    golden(
        ClientHello::new(Role::Host, "PC".into(), "W".into()),
        "840a006250436157",
    );
    golden(ServerHello::new(None), "810a");
    golden(ServerHello::new(Some(0)), "820a00");
    golden(
        ClientMsg::Register {
            reclaim: None,
            keep: true,
            rekey: false,
        },
        "820183f6f5f4",
    );
    golden(
        ClientMsg::Connect {
            req_id: 7,
            code: 123456,
            resume: None,
            init: vec![1, 2],
        },
        "820284071a0001e240f6420102",
    );
    golden(
        ClientMsg::Rekey { secret: [7; 16] },
        "8204815007070707070707070707070707070707",
    );
    golden(
        HandshakeInit {
            pattern: Pattern::Known,
            pake: None,
            noise: vec![1, 2, 3],
        },
        "a200000243010203",
    );
    golden(
        ControllerIntro {
            version: SESSION_VERSION,
            name: "PC".into(),
            os: "W".into(),
            settings: QualitySettings::default(),
            audio: true,
            view: vec![],
        },
        "8601625043615785181e190fa0190438f500f580",
    );
    golden(
        HostVerdict {
            version: SESSION_VERSION,
            refused: None,
        },
        "8101",
    );
    golden(
        HostVerdict {
            version: SESSION_VERSION,
            refused: Some(Refusal::PairingRequired),
        },
        "820101",
    );
    golden(Confirm::default(), "a0");
}

#[test]
fn session_messages_layout() {
    golden(HostDatagram::Pong { t: 99 }, "8200811863");
    golden(HostDatagram::Probe { t: 7 }, "82018107");
    golden(ControllerDatagram::Ping { t: 5 }, "82008105");
    golden(ControllerDatagram::Echo { t: 7 }, "82028107");
    golden(
        ControllerDatagram::Ack {
            seq: 99,
            arrived: 1000,
            stream: 0,
        },
        "82018218631903e8",
    );
    golden(HostMsg::Bye(ByeReason::Kicked), "82098100");
    golden(HostMsg::Clipboard { text: "hi".into() }, "820881626869");
    golden(
        HostMsg::Cursor {
            visibility: CursorVisibility::Hidden,
            shape: None,
        },
        "82078100",
    );
    let reference = CursorRef {
        slot: 2,
        generation: 7,
    };
    golden(
        HostMsg::Cursor {
            visibility: CursorVisibility::Shown,
            shape: Some(reference),
        },
        "82078201820207",
    );
    golden(
        HostMsg::Cursor {
            visibility: CursorVisibility::InFrame,
            shape: None,
        },
        "82078102",
    );
    golden(
        HostMsg::CursorImage {
            reference,
            image: CursorShape {
                width: 1,
                height: 1,
                hot_x: 0,
                hot_y: 0,
                rgba: vec![1, 2, 3, 4],
            },
        },
        "820a8282020785010100004401020304",
    );
    golden(
        ControllerMsg::Input {
            t: 5,
            events: vec![InputEvent::ReleaseAll],
        },
        "8201820581820680",
    );
    golden(
        ControllerMsg::RequestKeyframe {
            after: 5,
            stream: 0,
        },
        "82048105",
    );
    golden(
        HostMsg::Mic {
            state: MicState::Ready,
            detail: None,
        },
        "820c8102",
    );
    golden(
        HostMsg::Mic {
            state: MicState::Failed,
            detail: Some("x".into()),
        },
        "820c82046178",
    );
    for (state, bytes) in [
        (MicState::SteamMissing, "820c8105"),
        (MicState::Preparing, "820c8106"),
        (MicState::SetupFailed, "820c8107"),
    ] {
        golden(
            HostMsg::Mic {
                state,
                detail: None,
            },
            bytes,
        );
    }
    golden(ControllerMsg::Mic { enabled: true }, "820a81f5");
}

#[test]
fn lane_headers_keep_their_wire_encoding() {
    let id = 0x0102_0304_0506_0708;
    assert_eq!(
        hex(&SessionHeader::new(id, SessionLane::Control).encode()),
        "080706050403020100"
    );
    assert_eq!(
        hex(&SessionHeader::new(id, SessionLane::Bulk).encode()),
        "080706050403020101"
    );
    assert_eq!(
        hex(&SessionHeader::new(id, SessionLane::Video(0)).encode()),
        "080706050403020102"
    );
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
    assert_eq!(hex(&h.encode()), "080706050403020103");
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
    // 文件通道编号为 6。
    let h = SessionHeader::new(0x0102_0304_0506_0708, SessionLane::File);
    assert_eq!(hex(&h.encode()), "080706050403020106");
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
