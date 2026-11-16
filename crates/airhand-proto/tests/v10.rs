//! v10 新增的线上格式：帧流的会话头、画面包头的参考帧扩展、`ControllerDatagram::Lost`。
//! 和 v9 的样例一样，发布之后不要随实现重新生成（见 `v9.rs`）。
use airhand_proto::media::VideoHeader;
use airhand_proto::session::{Codec, ControllerDatagram};
use airhand_proto::{SessionHeader, SessionLane, features, framing};

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
