//! v9 的固定线上字节。不要随协议实现重新生成：兼容改动必须继续读写旧消息，新增消息另加样例。
use airhand_proto::{
    Features, framing,
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
fn v9_existing_messages_keep_their_wire_encoding() {
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
        },
        "820282071a0001e240",
    );
    golden(ClientMsg::Rekey { secret: None }, "820480");
    golden(HostMsg::Pong { t: 99 }, "8203811863");
    golden(HostMsg::Bye(ByeReason::Kicked), "82098100");
    golden(
        HostMsg::Cursor(CursorState {
            visible: false,
            shape: None,
        }),
        "82078181f4",
    );
    golden(HostMsg::Clipboard { text: "hi".into() }, "820881626869");
    golden(
        ControllerMsg::Input {
            t: 5,
            events: vec![InputEvent::ReleaseAll],
        },
        "8201820581820680",
    );
    golden(
        ControllerMsg::Settings(QualitySettings::default()),
        "82038185181e190fa0190438f500",
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

#[test]
fn rekey_confirmation_is_an_optional_field() {
    #[derive(Encode, Decode)]
    enum Old {
        #[n(4)]
        Rekey,
    }
    let old = framing::to_vec(&Old::Rekey);
    assert!(matches!(
        framing::decode_known::<ClientMsg>(&old).unwrap(),
        ClientMsg::Rekey { secret: None }
    ));
    let new = ClientMsg::Rekey {
        secret: Some([7; 16]),
    };
    assert!(matches!(
        framing::decode_known::<Old>(&framing::to_vec(&new)).unwrap(),
        Old::Rekey
    ));
    golden(new, "8204815007070707070707070707070707070707");
}

#[test]
fn cursor_extension_uses_new_numbers() {
    golden(
        HostMsg::CursorUpdate {
            visible: false,
            shape: Some(CursorRef {
                slot: 2,
                generation: 7,
            }),
        },
        "820a82f4820207",
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
        "820b8282020785010100004401020304",
    );
    assert_eq!(
        airhand_proto::features::session::CURSOR_UPDATES,
        Features::from_bits(1)
    );
}
