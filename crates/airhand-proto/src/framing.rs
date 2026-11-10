//! 长度前缀帧：`u32 LE 长度 | 负载`。
//!
//! 负载是什么：
//!
//! * 信令流上的第一条消息（版本握手）：永远不变的 postcard 编码（见 [`crate::signal`] 的"版本握手"）；
//! * 其它信令消息、端到端握手、会话的控制消息：CBOR（[`encode`] / [`decode`]）；
//! * 画面流、声音流：媒体包（见 [`crate::media`]），原样读出来交给界面。
//!
//! 会话流上的负载在端到端握手之后都是密文（见 `airhand_link::secure`），解密之后才是上面的格式。
//!
//! 这里只有编解码，不碰 IO，也不依赖任何异步运行时：从流里读写帧在 `airhand_link::wire`（它用的就是这里的
//! [`payload_len`] / [`decode`]）。手里已经有一段字节的（网页里的 wasm、fuzz）用 [`split`] / [`take_msg`]：
//! 和 `airhand_link::wire::read_msg` 走的是同一段解码代码。
//!
//! # 兼容规则
//!
//! 消息是 minicbor 派生的 CBOR：每个枚举变体、每个字段都有一个**显式的编号**（`#[n(…)]`），线上只有编号，
//! 没有名字，也和声明的先后无关。编号一旦发布就不能改、不能复用。
//!
//! * **不认识的消息跳过，不断开**：一条消息**本身**（顶层的枚举变体）不认识，[`decode`] 返回 [`Decoded::Unknown`]，
//!   读的一方跳过它（帧有长度，跳过不会错位）。认识这条消息、但是里面有解不出来的东西（包括没标明可以跳过的地方
//!   出现了不认识的变体）是协议错误，照样断开：那是 bug，不能悄悄吞掉——例如被控端的 `Hello` 里多了一个新的枚举值，
//!   旧的控制端要是把整条 `Hello` 跳过，会话就卡在那里，也没有任何报错；
//! * **加字段**：只能加 `Option` 字段（或者带 `#[cbor(default)]` 的）。旧版本不认识新字段，跳过；
//!   新版本读旧版本的消息，缺的字段是 `None`。必填的字段要永远存在；
//! * **枚举加值**：只有编号的枚举都是开放枚举（`open_enum!`，见 [`crate::cbor`]），不认识的值是 `Unknown(原值)`，
//!   放在哪里都不会出错，由收的一方决定怎么对待；
//! * **带字段的枚举加变体**：作为一整条消息时，旧版本跳过整条消息；作为字段时写成 `Option<枚举>` 并且用
//!   [`crate::cbor::opt`]（不认识的是 `None`）；在列表里用 [`crate::cbor::open_vec`]，只跳过不认识的元素。
//!   只有这三个地方能跳过，别的地方不能放带字段的枚举（为什么见 [`crate::cbor`]）；
//! * **对方可能不认识的新消息**：只在对方的能力位（[`crate::Features`]）表明它认识时才发。
//!
//! 只有改不下去的时候（例如要删掉必填字段）才加版本号（[`crate::SIGNAL_VERSION`] / [`crate::SESSION_VERSION`]）。

use minicbor::{Decode, Encode};

use crate::cbor;

/// 长度前缀的字节数（`u32 LE`）
pub const PREFIX: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("encoding error: {0}")]
    Decode(#[from] minicbor::decode::Error),
    #[error("encoding error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("message too large: {0} bytes")]
    TooLarge(usize),
}

/// 把一条 CBOR 消息编码成带长度前缀的帧。
pub fn encode<T: Encode<()>>(msg: &T) -> Vec<u8> {
    encode_with_spare(msg, 0)
}

/// 同 [`encode`]，多留 `spare` 字节的容量：加密时在后面追加认证标签不用重新分配
pub fn encode_with_spare<T: Encode<()>>(msg: &T, spare: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64 + spare);
    buf.extend_from_slice(&[0u8; 4]);
    minicbor::encode(msg, &mut buf).expect("writing to a Vec can't fail");
    let len = buf.len() - 4;
    buf[..4].copy_from_slice(&(len as u32).to_le_bytes());
    buf
}

/// 一条消息的负载（不带长度前缀，CBOR）
pub fn to_vec<T: Encode<()>>(msg: &T) -> Vec<u8> {
    minicbor::to_vec(msg).expect("writing to a Vec can't fail")
}

/// 解码的结果
#[derive(Debug)]
pub enum Decoded<T> {
    Msg(T),
    /// 对方版本新增的消息（顶层的变体不认识）：跳过。带着变体的编号（日志用）
    Unknown(i64),
}

/// 解码一条消息的负载（不带长度前缀）。消息本身不认识的返回 [`Decoded::Unknown`]，
/// 其它解不出来的（包括里面没标明可以跳过的地方有不认识的变体）是错误（见模块说明）
pub fn decode<'b, T: Decode<'b, ()>>(payload: &'b [u8]) -> Result<Decoded<T>, FrameError> {
    match minicbor::decode(payload) {
        Ok(m) => Ok(Decoded::Msg(m)),
        Err(e) if cbor::unknown_here(&e, payload) => Ok(Decoded::Unknown(
            cbor::variant_of(payload).unwrap_or_default(),
        )),
        Err(e) => Err(cbor::strict(e).into()),
    }
}

/// 解码一条一定认识的消息（握手这类没有"跳过"可言的）
pub fn decode_known<'b, T: Decode<'b, ()>>(payload: &'b [u8]) -> Result<T, FrameError> {
    minicbor::decode(payload).map_err(|e| cbor::strict(e).into())
}

/// 一帧：长度前缀 + `payload`
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(PREFIX + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// 长度前缀说负载有多长；超过 `max` 是错误（对方说多长就分配多长的话，一个前缀就能让这边分配 4 GiB）
pub fn payload_len(prefix: [u8; PREFIX], max: usize) -> Result<usize, FrameError> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

/// [`split`] 切下来的：`(负载, 剩下的字节)`；还不够一帧时为 `None`
pub type Split<'b> = Option<(&'b [u8], &'b [u8])>;

/// 从 `buf` 开头切下一帧：`(负载, 剩下的字节)`。还不够一帧（要等更多数据）时返回 `Ok(None)`
pub fn split(buf: &[u8], max: usize) -> Result<Split<'_>, FrameError> {
    let Some((prefix, rest)) = buf.split_first_chunk::<PREFIX>() else {
        return Ok(None);
    };
    let len = payload_len(*prefix, max)?;
    Ok(rest.split_at_checked(len))
}

/// 从 `buf` 开头取下一条消息，不认识的跳过（见模块说明），`buf` 前移到这条消息后面。
/// 剩下的不够一帧时返回 `Ok(None)`（`buf` 停在那一帧开头）
pub fn take_msg<'b, T: Decode<'b, ()>>(
    buf: &mut &'b [u8],
    max: usize,
) -> Result<Option<T>, FrameError> {
    loop {
        let Some((payload, rest)) = split(buf, max)? else {
            return Ok(None);
        };
        *buf = rest;
        match decode(payload)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown(variant) => {
                tracing::debug!(
                    variant,
                    len = payload.len(),
                    "skipping a message from a newer version"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::*;
    use crate::signal::*;

    /// 下一条消息（一定有）
    fn next<'b, T: Decode<'b, ()>>(r: &mut &'b [u8]) -> T {
        take_msg(r, 1 << 20).unwrap().unwrap()
    }

    #[test]
    fn roundtrip() {
        let msgs = vec![
            HostMsg::Pong { t: 99 },
            HostMsg::Notice(Some(Notice::CaptureFailed {
                detail: "AcquireNextFrame: 0x887A0026".into(),
            })),
            HostMsg::Notice(Some(Notice::MissingPermissions {
                screen: true,
                input: false,
            })),
            HostMsg::Notice(Some(Notice::AudioFailed {
                detail: "no default audio output device".into(),
            })),
            HostMsg::Notice(Some(Notice::AudioRecovered)),
            HostMsg::Bye(ByeReason::CodeChanged),
            HostMsg::Cursor(CursorState {
                visible: false,
                shape: None,
            }),
            HostMsg::Cursor(CursorState {
                visible: true,
                shape: Some(CursorSlot {
                    slot: 3,
                    image: None,
                }),
            }),
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            wire.extend(encode(m));
        }
        let mut r = &wire[..];
        for m in &msgs {
            let back: HostMsg = next(&mut r);
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
        let c: Option<HostMsg> = take_msg(&mut r, 1 << 20).unwrap();
        assert!(c.is_none());
        assert!(r.is_empty());
    }

    /// 一帧一帧地切：不够一帧时等更多数据，不动 `buf`；长度前缀超过上限是错误
    #[test]
    fn frames_are_split_at_their_boundaries() {
        let mut wire = frame(b"abc");
        wire.extend(frame(b""));
        wire.extend(frame(b"de"));
        assert_eq!(wire[..PREFIX], [3, 0, 0, 0]);
        let (a, rest) = split(&wire, 16).unwrap().unwrap();
        assert_eq!(a, b"abc");
        let (b, rest) = split(rest, 16).unwrap().unwrap();
        assert!(b.is_empty());
        let (c, rest) = split(rest, 16).unwrap().unwrap();
        assert_eq!(c, b"de");
        assert!(rest.is_empty());
        // 每一种截断：都是"还不够"
        for cut in 0..PREFIX + 3 {
            assert!(split(&wire[..cut], 16).unwrap().is_none(), "{cut}");
        }
        assert!(matches!(split(&wire, 2), Err(FrameError::TooLarge(3))));
        // 上限只看长度前缀：负载还没到也一样报错（不会先分配）
        assert!(matches!(
            split(&[0xff, 0xff, 0xff, 0xff], 1 << 20),
            Err(FrameError::TooLarge(0xffff_ffff))
        ));
        assert_eq!(payload_len([0, 1, 0, 0], 256).unwrap(), 256);

        // 取消息：停在不完整的那一帧开头
        let mut wire = encode(&HostMsg::Pong { t: 1 });
        let whole = wire.len();
        wire.extend(encode(&HostMsg::Pong { t: 2 }));
        let mut r = &wire[..wire.len() - 1];
        assert!(matches!(next(&mut r), HostMsg::Pong { t: 1 }));
        assert!(take_msg::<HostMsg>(&mut r, 1 << 20).unwrap().is_none());
        assert_eq!(r.len(), wire.len() - 1 - whole);
    }

    /// 对方版本新增的消息：跳过，后面的照常读；新的提示是 None；认识但解不出来的是错误
    #[test]
    fn unknown_messages_are_skipped() {
        #[derive(minicbor::Encode)]
        enum Future {
            #[n(200)]
            Hologram {
                #[n(0)]
                depth: u32,
            },
        }
        #[derive(minicbor::Encode)]
        enum FutureNotice {
            #[n(5)]
            Notice(#[n(0)] Smell),
        }
        #[derive(minicbor::Encode)]
        enum Smell {
            #[n(99)]
            Burnt {
                #[n(0)]
                how: u32,
            },
        }
        let mut wire = encode(&Future::Hologram { depth: 3 });
        wire.extend(encode(&FutureNotice::Notice(Smell::Burnt { how: 1 })));
        wire.extend(encode(&HostMsg::Pong { t: 5 }));
        let mut r = &wire[..];
        let m: HostMsg = next(&mut r);
        assert!(matches!(m, HostMsg::Notice(None)), "{m:?}");
        let m: HostMsg = next(&mut r);
        assert!(matches!(m, HostMsg::Pong { t: 5 }));
        let Decoded::<HostMsg>::Unknown(v) =
            decode(&to_vec(&Future::Hologram { depth: 3 })).unwrap()
        else {
            panic!()
        };
        assert_eq!(v, 200);

        // Pong 的必填字段是个字符串：认识的消息解不出来，是错误
        #[derive(minicbor::Encode)]
        enum Broken {
            #[n(3)]
            Pong {
                #[n(0)]
                t: String,
            },
        }
        let wire = encode(&Broken::Pong { t: "x".into() });
        let r: Result<Option<HostMsg>, _> = take_msg(&mut &wire[..], 1 << 20);
        assert!(matches!(r, Err(FrameError::Decode(_))));
    }

    /// 认识的消息里、没标明可以跳过的地方有不认识的变体：是错误，不是把整条消息悄悄跳过
    /// （例如 Hello 里多了一个新的带字段的枚举，忘了标明：旧版本要是跳过整条 Hello，会话就卡住了，也没有报错）
    #[test]
    fn unknown_variants_inside_known_messages_are_errors() {
        #[derive(minicbor::Encode, minicbor::Decode, Debug)]
        enum Old {
            #[n(1)]
            Hello(#[n(0)] OldShape),
        }
        #[derive(minicbor::Encode, minicbor::Decode, Debug)]
        enum OldShape {
            #[n(0)]
            Square {
                #[n(0)]
                side: u32,
            },
        }
        #[derive(minicbor::Encode)]
        enum New {
            #[n(1)]
            Hello(#[n(0)] NewShape),
        }
        #[derive(minicbor::Encode)]
        enum NewShape {
            #[n(1)]
            Circle {
                #[n(0)]
                r: u32,
            },
        }
        let wire = to_vec(&New::Hello(NewShape::Circle { r: 1 }));
        assert!(matches!(decode::<Old>(&wire), Err(FrameError::Decode(_))));
        // 消息本身不认识才跳过
        #[derive(minicbor::Encode)]
        enum Newer {
            #[n(2)]
            Bye,
        }
        assert!(matches!(
            decode::<Old>(&to_vec(&Newer::Bye)),
            Ok(Decoded::Unknown(2))
        ));
    }

    #[test]
    fn signal_roundtrip() {
        let token = [7u8; 16];
        let reclaim = Reclaim {
            code: 123_456,
            secret: [5; 16],
        };
        let client = vec![
            ClientMsg::Connect {
                req_id: 4,
                code: 123_456,
                resume: Some(token),
            },
            ClientMsg::Connect {
                req_id: 5,
                code: 654_321,
                resume: None,
            },
            ClientMsg::Rekey { secret: None },
            ClientMsg::Release,
            ClientMsg::Register {
                reclaim: Some(reclaim),
                keep: false,
                rekey: true,
            },
        ];
        for m in client {
            let wire = encode(&m);
            let back: ClientMsg = take_msg(&mut &wire[..], 1024).unwrap().unwrap();
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
        let peer = |ip: &str| Peer {
            name: "PC".into(),
            ip: ip.parse().unwrap(),
        };
        for m in [
            ServerMsg::connect_result(
                5,
                Ok(SessionGrant {
                    session_id: 1,
                    host_name: "PC".into(),
                    host_os: "Windows".into(),
                    resume: token,
                }),
            ),
            ServerMsg::connect_result(6, Err(ConnectError::Offline)),
            ServerMsg::connect_result(7, Err(ConnectError::Revoked)),
            ServerMsg::connect_result(8, Err(ConnectError::Busy)),
            ServerMsg::connect_result(10, Err(ConnectError::Unknown(77))),
            ServerMsg::Incoming {
                session_id: 9,
                peer: peer("10.0.0.7"),
            },
            ServerMsg::Incoming {
                session_id: 9,
                peer: peer("2001:db8::1"),
            },
            ServerMsg::HostOnline { code: 123_456 },
            ServerMsg::Rekeyed { secret: [3; 16] },
            ServerMsg::Registered {
                code: 123_456,
                secret: [4; 16],
            },
        ] {
            let wire = encode(&m);
            let back: ServerMsg = take_msg(&mut &wire[..], 1024).unwrap().unwrap();
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
    }

    /// 会话开始、等待同意、触摸和系统操作
    #[test]
    fn session_start_and_touch_roundtrip() {
        let mut wire = Vec::new();
        wire.extend(encode(&HostMsg::AwaitingApproval));
        wire.extend(encode(&HostMsg::Bye(ByeReason::Declined)));
        wire.extend(encode(&HostMsg::Hello(HostHello {
            hostname: "Pixel".into(),
            os: "Android 15".into(),
            version: "0.1.0".into(),
            displays: vec![],
            current_display: 0,
            codecs: vec![Codec::H264, Codec::AV1],
            platform: Platform::Android,
            touch: true,
            actions: vec![SystemAction::Back, SystemAction::VolumeDown],
        })));
        let mut r = &wire[..];
        let a: HostMsg = next(&mut r);
        assert!(matches!(a, HostMsg::AwaitingApproval));
        let b: HostMsg = next(&mut r);
        assert!(matches!(b, HostMsg::Bye(ByeReason::Declined)));
        let h: HostMsg = next(&mut r);
        assert!(
            matches!(h, HostMsg::Hello(ref h) if h.platform == Platform::Android && h.touch
            && h.actions == [SystemAction::Back, SystemAction::VolumeDown]
            && h.codecs == [Codec::H264, Codec::AV1])
        );

        let events = vec![
            InputEvent::Touch {
                id: 1,
                phase: TouchPhase::Down,
                x: 0.25,
                y: 0.5,
            },
            InputEvent::System {
                action: SystemAction::Recents,
            },
            InputEvent::MouseDelta { dx: -3, dy: 70_000 },
            InputEvent::Key {
                code: "KeyA".into(),
                down: true,
            },
            InputEvent::Wheel { dx: 0, dy: -120 },
            InputEvent::ReleaseAll,
        ];
        let wire = encode(&ControllerMsg::Input {
            t: 1 << 40,
            events: events.clone(),
        });
        let input: ControllerMsg = next(&mut &wire[..]);
        let ControllerMsg::Input { t, events: got } = input else {
            panic!()
        };
        assert_eq!(t, 1 << 40);
        assert_eq!(got, events);
    }

    /// 新版本的控制端发了旧被控端不认识的输入事件：只跳过那一个，同一批里的其它照常执行
    #[test]
    fn unknown_input_events_are_skipped_one_by_one() {
        #[derive(minicbor::Encode)]
        enum NewEvent {
            #[n(1)]
            MouseDelta {
                #[n(0)]
                dx: i32,
                #[n(1)]
                dy: i32,
            },
            #[n(77)]
            Pen {
                #[n(0)]
                pressure: f32,
            },
        }
        #[derive(minicbor::Encode)]
        enum NewMsg {
            #[n(1)]
            Input {
                #[n(0)]
                t: u64,
                #[n(1)]
                events: Vec<NewEvent>,
            },
        }
        let wire = to_vec(&NewMsg::Input {
            t: 9,
            events: vec![
                NewEvent::Pen { pressure: 0.5 },
                NewEvent::MouseDelta { dx: 1, dy: 2 },
            ],
        });
        let Decoded::Msg(ControllerMsg::Input { t, events }) = decode(&wire).unwrap() else {
            panic!()
        };
        assert_eq!(t, 9);
        assert_eq!(events, [InputEvent::MouseDelta { dx: 1, dy: 2 }]);
    }

    #[test]
    fn too_large() {
        let wire = encode(&ControllerMsg::Input {
            t: 0,
            events: vec![InputEvent::Text {
                text: "x".repeat(100),
            }],
        });
        let mut r = &wire[..];
        let res: Result<Option<ControllerMsg>, _> = take_msg(&mut r, 10);
        assert!(matches!(res, Err(FrameError::TooLarge(_))));
    }

    /// 服务器在认证之前就要解析的东西（版本握手、信令消息、会话头）：什么样的字节都不能让它崩溃、
    /// 也不能让它按对方说的长度去分配。这里只是把每条合法消息的每一位翻一遍、每个位置截一遍的冒烟测试，
    /// 正经的 fuzz 直接拿这几个函数当入口（它们都只吃字节，不碰 IO）
    #[test]
    fn hostile_bytes_never_panic() {
        use crate::{MAX_SIGNAL_MSG, Role, SessionHeader, SessionLane};
        let hello = ClientHello::new(Role::Host, "PC".into(), "Windows 11".into()).encode();
        let mut signal = Vec::new();
        for m in [
            ClientMsg::Register {
                reclaim: Some(Reclaim {
                    code: 123_456,
                    secret: [5; 16],
                }),
                keep: true,
                rekey: false,
            },
            ClientMsg::Connect {
                req_id: 4,
                code: 123_456,
                resume: Some([7; 16]),
            },
            ClientMsg::Rekey {
                secret: Some([5; 16]),
            },
            ClientMsg::Release,
        ] {
            signal.extend(encode(&m));
        }
        let header = SessionHeader::new(7, Role::Controller, SessionLane::Bulk).encode();

        let try_all = |hello: &[u8], signal: &[u8], header: &[u8; SessionHeader::LEN]| {
            let _ = ClientHello::decode(hello);
            let mut r = signal;
            while let Ok(Some(_)) = take_msg::<ClientMsg>(&mut r, MAX_SIGNAL_MSG) {}
            let _ = SessionHeader::decode(header);
        };
        for cut in 0..=hello.len() {
            try_all(&hello[..cut], &signal, &header);
        }
        for cut in 0..=signal.len() {
            try_all(&hello, &signal[..cut], &header);
        }
        // 翻每一位，再把每个字节换成几个边界值（长度、标签、数组头最容易出事的取值）
        type All = (Vec<u8>, Vec<u8>, [u8; SessionHeader::LEN]);
        let mutate = |pick: fn(&mut All) -> &mut [u8]| {
            let mut all = (hello.clone(), signal.clone(), header);
            for i in 0..pick(&mut all).len() {
                let original = pick(&mut all)[i];
                let flips = (0..8).map(|bit| original ^ (1 << bit));
                for b in flips.chain([0x00, 0x1f, 0x7f, 0x9f, 0xbf, 0xff]) {
                    pick(&mut all)[i] = b;
                    try_all(&all.0, &all.1, &all.2);
                }
                pick(&mut all)[i] = original;
            }
        };
        mutate(|a| &mut a.0);
        mutate(|a| &mut a.1);
        mutate(|a| &mut a.2);
    }
}
