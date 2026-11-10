//! 从流里读写长度前缀帧（`u32 LE 长度 | 负载`）。
//!
//! 帧的格式、消息的编解码和兼容规则都在 [`airhand_proto::framing`]：那边只吃字节、不碰 IO（所以协议库不依赖 tokio，
//! 能编译成 wasm，解析对方数据的函数也能直接拿去 fuzz）。这里只是把它接到异步的流上：
//! 长度前缀由 [`framing::payload_len`] 检查，消息由 [`framing::decode`] 解码，和 [`framing::take_msg`] 是同一段代码。
//!
//! 读一帧**不是取消安全的**（读到一半被 `select!` 取消，流上的帧边界就丢了）：放在单独的读任务里，
//! 或者只用超时包住、超时就放弃这条流。

use airhand_proto::framing::{self, Decoded, FrameError, PREFIX};
use airhand_proto::minicbor::{Decode, Encode};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

/// 写一帧：长度前缀 + `payload`
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    payload: &[u8],
) -> Result<(), WireError> {
    w.write_all(&framing::frame(payload)).await?;
    Ok(())
}

pub async fn write_msg<W, T>(w: &mut W, msg: &T) -> Result<(), WireError>
where
    W: AsyncWrite + Unpin,
    T: Encode<()>,
{
    w.write_all(&framing::encode(msg)).await?;
    Ok(())
}

/// 读长度前缀。对端在帧边界处正常关闭时返回 `Ok(None)`，关在半截里是错误
async fn read_len<R: AsyncRead + Unpin>(r: &mut R, max: usize) -> Result<Option<usize>, WireError> {
    let mut prefix = [0u8; PREFIX];
    let mut got = 0;
    while got < PREFIX {
        let n = r.read(&mut prefix[got..]).await?;
        if n == 0 {
            if got == 0 {
                return Ok(None);
            }
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
        got += n;
    }
    Ok(Some(framing::payload_len(prefix, max)?))
}

/// 读一帧的负载放进 `buf`（由调用方复用）。对端在帧边界处正常关闭时返回 `Ok(false)`
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    max: usize,
    buf: &mut Vec<u8>,
) -> Result<bool, WireError> {
    let Some(len) = read_len(r, max).await? else {
        return Ok(false);
    };
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    Ok(true)
}

/// 读一条消息，不认识的跳过（见 [`airhand_proto::framing`] 的"兼容规则"）。对端在消息边界处正常关闭时返回 `Ok(None)`。
///
/// `buf` 由调用方复用，避免每条消息都分配内存。
pub async fn read_msg<R, T>(
    r: &mut R,
    max: usize,
    buf: &mut Vec<u8>,
) -> Result<Option<T>, WireError>
where
    R: AsyncRead + Unpin,
    T: for<'b> Decode<'b, ()>,
{
    loop {
        if !read_frame(r, max, buf).await? {
            return Ok(None);
        }
        match framing::decode(buf)? {
            Decoded::Msg(m) => return Ok(Some(m)),
            Decoded::Unknown(variant) => {
                tracing::debug!(
                    variant,
                    len = buf.len(),
                    "skipping a message from a newer version"
                );
            }
        }
    }
}

/// 读取一帧的原始负载（媒体包），读进一块新内存：之后原样交给界面，不再拷贝。
/// 对端在帧边界处正常关闭时返回 `Ok(None)`
///
/// 直接读进未初始化的空间（`read_buf`），不先把整块内存清零：画面包可能有几 MiB，每帧都清零一遍是白做的
pub async fn read_packet<R>(r: &mut R, max: usize) -> Result<Option<Vec<u8>>, WireError>
where
    R: AsyncRead + Unpin,
{
    Ok(read_packet_timed(r, max, || ()).await?.map(|(p, ())| p))
}

/// 同 [`read_packet`]，另外返回读完长度前缀时 `clock()` 的值：这一帧开始到达的时刻。
/// 读任务闲着的话就是第一个字节到的时刻；前面的还没读完的话是读完前面那一帧的时刻（它已经在排队了）。
/// 不含这一帧自己传了多久：被控端拿它算排队时延（见 `airhand_link::flow`）
pub async fn read_packet_timed<R, T>(
    r: &mut R,
    max: usize,
    clock: impl FnOnce() -> T,
) -> Result<Option<(Vec<u8>, T)>, WireError>
where
    R: AsyncRead + Unpin,
{
    let Some(len) = read_len(r, max).await? else {
        return Ok(None);
    };
    let at = clock();
    let mut buf = Vec::with_capacity(len);
    while buf.len() < len {
        // take 限制住这一帧剩下的长度，不会读进下一帧；buf 的容量正好够，不会再分配
        let left = (len - buf.len()) as u64;
        if (&mut *r).take(left).read_buf(&mut buf).await? == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
        }
    }
    debug_assert_eq!(buf.capacity(), len);
    Ok(Some((buf, at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::{ByeReason, ControllerMsg, HostMsg, InputEvent};

    /// 每次最多读出一个字节的读取端（流上的数据通常是分几次到的）
    fn one_byte_at_a_time(data: &[u8]) -> impl AsyncRead + Unpin + '_ {
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

    /// 写出去的就是 `framing::encode` 的字节；读回来的和 `framing::take_msg` 一样（包括跳过不认识的消息）
    #[tokio::test]
    async fn messages_roundtrip_over_a_stream() {
        use airhand_proto::minicbor;
        #[derive(minicbor::Encode)]
        enum Future {
            #[n(200)]
            Hologram {
                #[n(0)]
                depth: u32,
            },
        }
        let mut wire = Vec::new();
        write_msg(&mut wire, &HostMsg::Pong { t: 99 })
            .await
            .unwrap();
        write_msg(&mut wire, &Future::Hologram { depth: 3 })
            .await
            .unwrap();
        write_msg(&mut wire, &HostMsg::Bye(ByeReason::CodeChanged))
            .await
            .unwrap();
        let mut expected = framing::encode(&HostMsg::Pong { t: 99 });
        expected.extend(framing::encode(&Future::Hologram { depth: 3 }));
        expected.extend(framing::encode(&HostMsg::Bye(ByeReason::CodeChanged)));
        assert_eq!(wire, expected);

        let mut buf = Vec::new();
        for mut r in [
            Box::new(&wire[..]) as Box<dyn AsyncRead + Unpin>,
            Box::new(one_byte_at_a_time(&wire)),
        ] {
            let m: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
            assert!(matches!(m, HostMsg::Pong { t: 99 }));
            let m: HostMsg = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap().unwrap();
            assert!(matches!(m, HostMsg::Bye(ByeReason::CodeChanged)));
            let end: Option<HostMsg> = read_msg(&mut r, 1 << 20, &mut buf).await.unwrap();
            assert!(end.is_none());
        }
    }

    /// 流在帧边界处结束是正常结束，关在半截里（前缀里、负载里）是错误
    #[tokio::test]
    async fn a_stream_cut_inside_a_frame_is_an_error() {
        let mut wire = Vec::new();
        write_frame(&mut wire, b"abcdef").await.unwrap();
        assert_eq!(wire, framing::frame(b"abcdef"));
        let mut buf = Vec::new();
        assert!(read_frame(&mut &wire[..], 16, &mut buf).await.unwrap());
        assert_eq!(buf, b"abcdef");
        assert!(!read_frame(&mut &wire[..0], 16, &mut buf).await.unwrap());
        for cut in 1..wire.len() {
            let r = read_frame(&mut &wire[..cut], 16, &mut buf).await;
            assert!(matches!(r, Err(WireError::Io(_))), "{cut}: {r:?}");
        }
    }

    /// 太长的帧在读负载之前就拒绝（不按对方说的长度分配）
    #[tokio::test]
    async fn too_large() {
        let wire = framing::encode(&ControllerMsg::Input {
            t: 0,
            events: vec![InputEvent::Text {
                text: "x".repeat(100),
            }],
        });
        let mut buf = Vec::new();
        let res: Result<Option<ControllerMsg>, _> = read_msg(&mut &wire[..], 10, &mut buf).await;
        assert!(matches!(
            res,
            Err(WireError::Frame(FrameError::TooLarge(_)))
        ));
        assert!(buf.is_empty());
        // 只有前缀、说自己有 4 GiB
        let res = read_packet(&mut &[0xffu8, 0xff, 0xff, 0xff][..], 1 << 20).await;
        assert!(matches!(
            res,
            Err(WireError::Frame(FrameError::TooLarge(0xffff_ffff)))
        ));
    }

    #[tokio::test]
    async fn packets_are_read_raw() {
        use airhand_proto::media::{AudioHeader, HEADER_LEN, frame};
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
            Err(WireError::Frame(FrameError::TooLarge(26)))
        ));
        // 一个字节一个字节地到：照样拼成完整的一帧，不多读下一帧
        let mut slow = one_byte_at_a_time(&wire);
        let a = read_packet(&mut slow, 1024).await.unwrap().unwrap();
        assert_eq!(a.len(), HEADER_LEN + 2);
        assert_eq!(a.capacity(), a.len());
        let b = read_packet(&mut slow, 1024).await.unwrap().unwrap();
        assert_eq!(b.len(), HEADER_LEN);
        assert!(read_packet(&mut slow, 1024).await.unwrap().is_none());
    }
}
