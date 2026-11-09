//! 长度前缀帧：`u32 LE 长度 | 负载`。
//!
//! 负载是什么：
//!
//! * 信令流上的第一条消息（版本握手）：永远不变的 postcard 编码（见 [`crate::signal`] 的"版本握手"）；
//! * 其它信令消息、端到端握手、会话的控制消息：CBOR（[`write_msg`] / [`read_msg`]）；
//! * 画面流、声音流：媒体包（见 [`crate::media`]），原样读出来交给界面（[`read_packet`]）。
//!
//! 会话流上的负载在端到端握手之后都是密文（见 `airhand_link::secure`），解密之后才是上面的格式。
//!
//! # 兼容规则
//!
//! 消息是 minicbor 派生的 CBOR：每个枚举变体、每个字段都有一个**显式的编号**（`#[n(…)]`），线上只有编号，
//! 没有名字，也和声明的先后无关。编号一旦发布就不能改、不能复用。
//!
//! * **不认识的消息跳过，不断开**：一条消息（顶层的枚举变体）不认识、或者里面有不认识的枚举变体，[`decode`] 返回
//!   [`Decoded::Unknown`]，读的一方跳过它（帧有长度，跳过不会错位）。认识但是解不出来的是协议错误，照样断开
//!   （那是 bug，不能悄悄吞掉）；
//! * **加字段**：只能加 `Option` 字段（或者带 `#[cbor(default)]` 的）。旧版本不认识新字段，跳过；
//!   新版本读旧版本的消息，缺的字段是 `None`。必填的字段要永远存在；
//! * **枚举加变体**：作为一整条消息时，旧版本跳过整条消息；作为字段时写成 `Option<枚举>`（不认识的是 `None`），
//!   或者用 [`crate::cbor::or_default`] 换成默认值；在列表里用 [`crate::cbor::open_vec`]，只跳过不认识的元素；
//! * **对方可能不认识的新消息**：只在对方的能力位（[`crate::Features`]）表明它认识时才发。
//!
//! 只有改不下去的时候（例如要删掉必填字段）才加版本号（[`crate::SIGNAL_VERSION`] / [`crate::SESSION_VERSION`]）。

use minicbor::{Decode, Encode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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
    /// 对方版本新增的消息（或者里面有新增的枚举变体）：跳过
    Unknown,
}

/// 解码一条消息的负载（不带长度前缀）。不认识的返回 [`Decoded::Unknown`]，解不出来的是错误（见模块说明）
pub fn decode<'b, T: Decode<'b, ()>>(payload: &'b [u8]) -> Result<Decoded<T>, FrameError> {
    match minicbor::decode(payload) {
        Ok(m) => Ok(Decoded::Msg(m)),
        Err(e) if e.is_unknown_variant() => Ok(Decoded::Unknown),
        Err(e) => Err(e.into()),
    }
}

/// 解码一条一定认识的消息（握手这类没有"跳过"可言的）
pub fn decode_known<'b, T: Decode<'b, ()>>(payload: &'b [u8]) -> Result<T, FrameError> {
    Ok(minicbor::decode(payload)?)
}

/// 写一帧：长度前缀 + `payload`
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> Result<(), FrameError> {
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf).await?;
    Ok(())
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Encode<()>,
{
    w.write_all(&encode(msg)).await?;
    Ok(())
}

/// 读长度前缀。对端在帧边界处正常关闭时返回 `Ok(None)`，关在半截里是错误
async fn read_len<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
) -> Result<Option<usize>, FrameError> {
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        let n = r.read(&mut len[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        got += n;
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > max {
        return Err(FrameError::TooLarge(len));
    }
    Ok(Some(len))
}

/// 读一帧的负载放进 `buf`（由调用方复用）。对端在帧边界处正常关闭时返回 `Ok(false)`
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
    buf: &mut Vec<u8>,
) -> Result<bool, FrameError> {
    let Some(len) = read_len(r, max).await? else {
        return Ok(false);
    };
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    Ok(true)
}

/// 读一条消息，不认识的跳过（见模块说明）。对端在消息边界处正常关闭时返回 `Ok(None)`。
///
/// `buf` 由调用方复用，避免每条消息都分配内存。
pub async fn read_msg<R, T>(
    r: &mut R,
    max: usize,
    buf: &mut Vec<u8>,
) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: for<'b> Decode<'b, ()>,
{
    loop {
        if !read_frame(r, max, buf).await? {
            return Ok(None);
        }
        match decode(buf)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown => {
                tracing::debug!(len = buf.len(), "skipping a message from a newer version");
            }
        }
    }
}

/// 读取一帧的原始负载（媒体包），读进一块新内存：之后原样交给界面，不再拷贝。
/// 对端在帧边界处正常关闭时返回 `Ok(None)`
///
/// 直接读进未初始化的空间（`read_buf`），不先把整块内存清零：画面包可能有几 MiB，每帧都清零一遍是白做的
pub async fn read_packet<R>(r: &mut R, max: usize) -> Result<Option<Vec<u8>>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let Some(len) = read_len(r, max).await? else {
        return Ok(None);
    };
    let mut buf = Vec::with_capacity(len);
    while buf.len() < len {
        // take 限制住这一帧剩下的长度，不会读进下一帧；buf 的容量正好够，不会再分配
        let left = (len - buf.len()) as u64;
        if (&mut *r).take(left).read_buf(&mut buf).await? == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
    }
    debug_assert_eq!(buf.capacity(), len);
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::*;
    use crate::signal::*;

    #[tokio::test]
    async fn roundtrip() {
        let msgs = vec![
            HostMsg::Pong { t: 99 },
            HostMsg::Notice(Notice::CaptureFailed {
                detail: "AcquireNextFrame: 0x887A0026".into(),
            }),
            HostMsg::Notice(Notice::MissingPermissions {
                screen: true,
                input: false,
            }),
            HostMsg::Notice(Notice::AudioFailed {
                detail: "no default audio output device".into(),
            }),
            HostMsg::Notice(Notice::AudioRecovered),
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
            write_msg(&mut wire, m).await.unwrap();
        }
        let mut r = &wire[..];
        let mut buf = Vec::new();
        for m in &msgs {
            let back: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
        let c: Option<HostMsg> = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap();
        assert!(c.is_none());
    }

    /// 对方版本新增的消息：跳过，后面的照常读；认识但解不出来的是错误
    #[tokio::test]
    async fn unknown_messages_are_skipped() {
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
            Burnt,
        }
        let mut wire = encode(&Future::Hologram { depth: 3 });
        wire.extend(encode(&FutureNotice::Notice(Smell::Burnt)));
        wire.extend(encode(&HostMsg::Pong { t: 5 }));
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let m: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(m, HostMsg::Pong { t: 5 }));

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
        let r: Result<Option<HostMsg>, _> = read_msg(&mut &wire[..], 1 << 20, &mut buf).await;
        assert!(matches!(r, Err(FrameError::Decode(_))));
    }

    #[tokio::test]
    async fn packets_are_read_raw() {
        use crate::media::{AudioHeader, HEADER_LEN, frame};
        let h = AudioHeader {
            seq: 1,
            start: false,
            capture_us: 5,
        };
        let mut wire = frame(&h.encode(), &[&[1, 2]]);
        wire.extend_from_slice(&frame(&h.encode(), &[]));
        let mut r = &wire[..];
        let a = read_packet(&mut r, 1024).await.unwrap().unwrap();
        assert_eq!(a.len(), HEADER_LEN + 2);
        assert_eq!(AudioHeader::decode(&a), Some(h));
        assert_eq!(
            read_packet(&mut r, 1024).await.unwrap().unwrap().len(),
            HEADER_LEN
        );
        assert!(read_packet(&mut r, 1024).await.unwrap().is_none());
        // 关在半截里
        let mut r = &wire[..6];
        assert!(read_packet(&mut r, 1024).await.is_err());
        assert!(matches!(
            read_packet(&mut &wire[..], 10).await,
            Err(FrameError::TooLarge(26))
        ));
        // 一个字节一个字节地到（流上的数据通常是分几次到的）：照样拼成完整的一帧，不多读下一帧
        let mut slow = tokio_test_reader(&wire);
        let a = read_packet(&mut slow, 1024).await.unwrap().unwrap();
        assert_eq!(a.len(), HEADER_LEN + 2);
        assert_eq!(a.capacity(), a.len());
        let b = read_packet(&mut slow, 1024).await.unwrap().unwrap();
        assert_eq!(b.len(), HEADER_LEN);
        assert!(read_packet(&mut slow, 1024).await.unwrap().is_none());
    }

    /// 每次最多读出一个字节的读取端
    fn tokio_test_reader(data: &[u8]) -> impl AsyncRead + Unpin + '_ {
        struct OneByte<'a>(&'a [u8]);
        impl AsyncRead for OneByte<'_> {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if let Some((&b, rest)) = self.0.split_first()
                    && buf.remaining() > 0
                {
                    buf.put_slice(&[b]);
                    self.0 = rest;
                }
                std::task::Poll::Ready(Ok(()))
            }
        }
        OneByte(data)
    }

    #[tokio::test]
    async fn signal_roundtrip() {
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
            ClientMsg::Rekey,
            ClientMsg::Release,
            ClientMsg::Register {
                reclaim: Some(reclaim),
                keep: false,
                rekey: true,
            },
        ];
        let mut buf = Vec::new();
        for m in client {
            let wire = encode(&m);
            let back: ClientMsg = read_msg(&mut &wire[..], 1024, &mut buf)
                .await
                .unwrap()
                .unwrap();
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
            let back: ServerMsg = read_msg(&mut &wire[..], 1024, &mut buf)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
    }

    /// 会话开始、等待同意、触摸和系统操作
    #[tokio::test]
    async fn session_start_and_touch_roundtrip() {
        let mut wire = Vec::new();
        write_msg(&mut wire, &HostMsg::AwaitingApproval)
            .await
            .unwrap();
        write_msg(&mut wire, &HostMsg::Bye(ByeReason::Declined))
            .await
            .unwrap();
        write_msg(
            &mut wire,
            &HostMsg::Hello(HostHello {
                hostname: "Pixel".into(),
                os: "Android 15".into(),
                version: "0.1.0".into(),
                displays: vec![],
                current_display: 0,
                codecs: vec![Codec::H264, Codec::AV1],
                platform: Platform::Android,
                touch: true,
                actions: vec![SystemAction::Back, SystemAction::VolumeDown],
            }),
        )
        .await
        .unwrap();
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let a: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(a, HostMsg::AwaitingApproval));
        let b: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(b, HostMsg::Bye(ByeReason::Declined)));
        let h: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
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
        let input: ControllerMsg = read_msg(&mut &wire[..], 1 << 20, &mut buf)
            .await
            .unwrap()
            .unwrap();
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

    #[tokio::test]
    async fn too_large() {
        let wire = encode(&ControllerMsg::Input {
            t: 0,
            events: vec![InputEvent::Text {
                text: "x".repeat(100),
            }],
        });
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let res: Result<Option<ControllerMsg>, _> = read_msg(&mut r, 10, &mut buf).await;
        assert!(matches!(res, Err(FrameError::TooLarge(_))));
    }
}
