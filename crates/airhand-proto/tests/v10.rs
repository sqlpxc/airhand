//! v10 新增的线上格式：帧流的会话头、画面包头的参考帧扩展、`ControllerDatagram::Lost`；
//! 前向纠错的数据报、`ControllerDatagram::Report`、画质设置里的传输方式。
//! 和 v9 的样例一样，发布之后不要随实现重新生成（见 `v9.rs`）。
use airhand_proto::fec::{Fragment, PREV_FEC};
use airhand_proto::media::VideoHeader;
use airhand_proto::session::{
    Codec, ControllerDatagram, ControllerMsg, QualitySettings, VideoReport, VideoTransport,
};
use airhand_proto::{SessionHeader, SessionLane, datagram, features, framing};

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn frame_stream_headers_keep_their_wire_encoding() {
    assert_eq!(airhand_proto::SIGNAL_FRAMES, 10);
    let h = SessionHeader::frame(0x0102_0304_0506_0708, SessionLane::Video);
    assert_eq!(hex(&h.encode()), "41485339080706050403020101000100");
    // v9 的会话头：第 14 个字节是 0
    let v9 = SessionHeader::new(0x0102_0304_0506_0708, SessionLane::Video);
    assert_eq!(hex(&v9.encode()), "41485339080706050403020101000000");
}

#[test]
fn the_reference_extension_keeps_its_wire_encoding() {
    let h = VideoHeader {
        seq: 0x10,
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
    assert_eq!(features::session::FRAMES.bits(), 2);
    assert_eq!(
        hex(&framing::to_vec(&ControllerDatagram::Lost { decoded: 99 })),
        "8203811863"
    );
}

#[test]
fn fec_formats_keep_their_wire_encoding() {
    assert_eq!(features::session::FEC.bits(), 4);
    assert_eq!(datagram::VIDEO, 2);
    let shard = [0xaa, 0xbb];
    let f = Fragment {
        counter: 0x0102_0304_0506_0708,
        len: 2,
        index: 1,
        k: 1,
        r: 1,
        flags: PREV_FEC,
        shard: &shard,
    };
    assert_eq!(
        hex(&f.encode(0x1122_3344_5566_7788)),
        "887766554433221102080706050403020102000000010001000100\
         01aabb"
    );
    let report = ControllerDatagram::Report(VideoReport {
        frames: 60,
        stalled: 2,
        fragments: 300,
        missing: 4,
        unrecovered: 1,
    });
    assert_eq!(hex(&framing::to_vec(&report)), "82048185183c0219012c0401");
    // 传输方式是画质设置的第 6 项；自动不写。写了传输方式时，前面低延迟的优先项也占着它那一位（0）
    let q = QualitySettings {
        transport: VideoTransport::Fec,
        ..QualitySettings::default()
    };
    assert_eq!(
        hex(&framing::to_vec(&ControllerMsg::Settings(q))),
        "82038187181e190fa0190438f5000002"
    );
    let auto = QualitySettings::default();
    assert_eq!(
        hex(&framing::to_vec(&ControllerMsg::Settings(auto))),
        "82038185181e190fa0190438f500"
    );
}
