//! 长度前缀帧：`u32 LE 长度 | 负载`。
//!
//! 控制流和信令的负载是 postcard 编码的消息（[`write_msg`] / [`read_msg`]）；
//! 画面流和声音流的负载是媒体包（见 [`crate::media`]），原样读出来交给界面（[`read_packet`]）。

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("encoding error: {0}")]
    Codec(#[from] postcard::Error),
    #[error("message too large: {0} bytes")]
    TooLarge(usize),
}

/// 把消息编码为带长度前缀的字节串。
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    encode_with_capacity(msg, 32)
}

/// 同 [`encode`]，按预估大小一次分配好（视频帧这类大消息用，避免多次扩容）。
pub fn encode_with_capacity<T: Serialize>(msg: &T, cap: usize) -> Result<Vec<u8>, FrameError> {
    let mut buf = Vec::with_capacity(cap + 32);
    buf.extend_from_slice(&[0u8; 4]);
    let mut buf = postcard::to_extend(msg, buf)?;
    let len = buf.len() - 4;
    buf[..4].copy_from_slice(&(len as u32).to_le_bytes());
    Ok(buf)
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let buf = encode(msg)?;
    w.write_all(&buf).await?;
    Ok(())
}

/// 解码一条消息的负载（不带长度前缀）
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    Ok(postcard::from_bytes(payload)?)
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

/// 读取一条消息。对端在消息边界处正常关闭时返回 `Ok(None)`。
///
/// `buf` 由调用方复用，避免每条消息都分配内存。
pub async fn read_msg<R, T>(
    r: &mut R,
    max: usize,
    buf: &mut Vec<u8>,
) -> Result<Option<T>, FrameError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let Some(len) = read_len(r, max).await? else {
        return Ok(None);
    };
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    Ok(Some(decode(buf)?))
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
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_msg(&mut wire, m).await.unwrap();
        }
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let b: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(b, HostMsg::Pong { t: 99 }));
        let n: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(
            matches!(n, HostMsg::Notice(Notice::CaptureFailed { ref detail }) if detail == "AcquireNextFrame: 0x887A0026")
        );
        let p: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(
            p,
            HostMsg::Notice(Notice::MissingPermissions {
                screen: true,
                input: false
            })
        ));
        let a: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(
            matches!(a, HostMsg::Notice(Notice::AudioFailed { ref detail }) if detail == "no default audio output device")
        );
        let a: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(a, HostMsg::Notice(Notice::AudioRecovered)));
        let bye: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(bye, HostMsg::Bye(ByeReason::CodeChanged)));
        let c: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(matches!(
            c,
            HostMsg::Cursor(CursorState {
                visible: false,
                shape: None
            })
        ));
        let c: Option<HostMsg> = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap();
        assert!(c.is_none());
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
        let m = ServerMsg::ConnectResult {
            req_id: 3,
            result: Err(ConnectError::Busy),
        };
        let wire = encode(&m).unwrap();
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let back: ServerMsg = read_msg(&mut r, 1024, &mut buf).await.unwrap().unwrap();
        assert!(matches!(
            back,
            ServerMsg::ConnectResult {
                req_id: 3,
                result: Err(ConnectError::Busy)
            }
        ));

        for ip in ["10.0.0.7", "2001:db8::1"] {
            let peer = Peer {
                name: "PC".into(),
                ip: ip.parse().unwrap(),
            };
            let wire = encode(&ServerMsg::Incoming {
                session_id: 9,
                peer: peer.clone(),
            })
            .unwrap();
            let back: ServerMsg = read_msg(&mut &wire[..], 1024, &mut buf)
                .await
                .unwrap()
                .unwrap();
            assert!(
                matches!(back, ServerMsg::Incoming { session_id: 9, peer: ref p } if *p == peer)
            );
        }

        // 重连：带凭证的请求、带凭证的会话授权、"不在线"、"重新上线了"
        let token = [7u8; 16];
        let wire = encode(&ClientMsg::Connect {
            req_id: 4,
            code: 123_456,
            resume: Some(token),
        })
        .unwrap();
        let back: ClientMsg = read_msg(&mut &wire[..], 1024, &mut buf)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            back,
            ClientMsg::Connect { req_id: 4, code: 123_456, resume: Some(t) } if t == token
        ));
        let wire = encode(&ClientMsg::Rekey).unwrap();
        let back: ClientMsg = read_msg(&mut &wire[..], 1024, &mut buf)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(back, ClientMsg::Rekey));
        for m in [
            ServerMsg::ConnectResult {
                req_id: 5,
                result: Ok(SessionGrant {
                    session_id: 1,
                    host_name: "PC".into(),
                    host_os: "Windows".into(),
                    resume: token,
                }),
            },
            ServerMsg::ConnectResult {
                req_id: 6,
                result: Err(ConnectError::Offline),
            },
            ServerMsg::HostOnline { code: 123_456 },
            ServerMsg::Rekeyed { secret: [3; 16] },
        ] {
            let wire = encode(&m).unwrap();
            let back: ServerMsg = read_msg(&mut &wire[..], 1024, &mut buf)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(format!("{back:?}"), format!("{m:?}"));
        }
    }

    /// 版本握手的消息永远不能变（见 signal 模块说明）。这个测试锁定它们的编码：
    /// 如果它失败了，说明改动会让其它版本的客户端 / 服务器连"版本不一致"都无法得知。
    #[test]
    fn version_handshake_wire_is_stable() {
        let hello = ClientMsg::Hello {
            version: 7,
            role: crate::Role::Controller,
            name: "A".into(),
            os: "B".into(),
        };
        // 变体 0 | version=7 | Role::Controller=1 | "A" | "B"
        assert_eq!(
            postcard::to_stdvec(&hello).unwrap(),
            [0, 7, 1, 1, b'A', 1, b'B']
        );
        let host = ClientMsg::Hello {
            version: 7,
            role: crate::Role::Host,
            name: String::new(),
            os: String::new(),
        };
        assert_eq!(postcard::to_stdvec(&host).unwrap(), [0, 7, 0, 0, 0]);
        assert_eq!(
            postcard::to_stdvec(&ServerMsg::Welcome { version: 7 }).unwrap(),
            [0, 7]
        );
        assert_eq!(
            postcard::to_stdvec(&ServerMsg::Incompatible { server_version: 7 }).unwrap(),
            [1, 7]
        );
        assert_eq!(crate::ALPN, b"airhand/1");
    }

    /// 会话开始、等待同意、触摸和系统操作（协议版本 7 新增）
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
            && h.actions == [SystemAction::Back, SystemAction::VolumeDown])
        );

        let msgs = vec![
            ControllerMsg::Hello(ControllerHello { device: [9; 16] }),
            ControllerMsg::Input(vec![
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
            ]),
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_msg(&mut wire, m).await.unwrap();
        }
        let mut r = &wire[..];
        let hello: ControllerMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        assert!(
            matches!(hello, ControllerMsg::Hello(ControllerHello { device }) if device == [9; 16])
        );
        let input: ControllerMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
        let ControllerMsg::Input(events) = input else {
            panic!()
        };
        assert_eq!(
            events,
            [
                InputEvent::Touch {
                    id: 1,
                    phase: TouchPhase::Down,
                    x: 0.25,
                    y: 0.5
                },
                InputEvent::System {
                    action: SystemAction::Recents
                },
                InputEvent::MouseDelta { dx: -3, dy: 70_000 },
            ]
        );
    }

    #[tokio::test]
    async fn too_large() {
        let wire = encode(&ControllerMsg::Input(vec![InputEvent::Text {
            text: "x".repeat(100),
        }]))
        .unwrap();
        let mut r = &wire[..];
        let mut buf = Vec::new();
        let res: Result<Option<ControllerMsg>, _> = read_msg(&mut r, 10, &mut buf).await;
        assert!(matches!(res, Err(FrameError::TooLarge(_))));
    }
}
