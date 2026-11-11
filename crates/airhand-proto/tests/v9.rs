//! v9 的固定线上字节。发布之后不要随协议实现重新生成：兼容改动必须继续读写这些消息，新增消息另加样例。
use airhand_proto::{
    Features, Role, framing,
    handshake::*,
    minicbor::{Decode, Encode},
    session::*,
    signal::*,
};

fn golden<T: Encode<()> + for<'b> Decode<'b, ()>>(value: T, hex: &str) {
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    let wire: Vec<u8> = pairs
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(
        framing::to_vec(&value),
        wire,
        "published message numbers or field positions changed"
    );
    let decoded: T = framing::decode_known(&wire).unwrap();
    assert_eq!(framing::to_vec(&decoded), wire);
}

#[test]
fn signal_messages_keep_their_wire_encoding() {
    golden(
        ClientHello {
            min: 9,
            max: 9,
            role: Role::Host,
            name: "PC".into(),
            os: "W".into(),
            features: Features::NONE,
        },
        "a6000901090200036250430461570500",
    );
    golden(
        ServerHello {
            min: 9,
            max: 9,
            version: Some(9),
            features: Features::NONE,
        },
        "a40009010902090300",
    );
    golden(
        ServerHello {
            min: 9,
            max: 9,
            version: None,
            features: Features::NONE,
        },
        "a3000901090300",
    );
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
}

#[test]
fn session_messages_keep_their_wire_encoding() {
    golden(HostDatagram::Pong { t: 99 }, "8200811863");
    golden(HostDatagram::Probe { t: 7 }, "82018107");
    golden(ControllerDatagram::Ping { t: 5 }, "82008105");
    golden(ControllerDatagram::Echo { t: 7 }, "82028107");
    golden(
        ControllerDatagram::Ack {
            seq: 99,
            arrived: 1000,
        },
        "82018218631903e8",
    );
    golden(HostMsg::Bye(ByeReason::Kicked), "82098100");
    golden(HostMsg::Clipboard { text: "hi".into() }, "820881626869");
    golden(
        HostMsg::Cursor {
            visible: false,
            shape: None,
        },
        "820781f4",
    );
    golden(
        HostMsg::Cursor {
            visible: false,
            shape: Some(CursorRef {
                slot: 2,
                generation: 7,
            }),
        },
        "820782f4820207",
    );
    golden(
        HostMsg::CursorImage {
            reference: CursorRef {
                slot: 2,
                generation: 7,
            },
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
    golden(ControllerMsg::RequestKeyframe { after: 5 }, "82048105");
    golden(
        ControllerMsg::Settings(QualitySettings::default()),
        "82038185181e190fa0190438f500",
    );
}

#[test]
fn handshake_messages_keep_their_wire_encoding() {
    golden(
        HandshakeInit {
            pattern: Pattern::Known,
            pake: None,
            noise: vec![1, 2, 3],
        },
        "a200000243010203",
    );
    golden(
        HostVerdict {
            version: Some(1),
            features: Features::NONE,
            refused: None,
            min: 1,
            max: 1,
        },
        "a40001010003010401",
    );
    golden(Confirm::default(), "a0");
}
