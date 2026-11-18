//! 被控端会话的写任务。每个显示器用自己的密钥，每一帧独立开流。声音使用数据报。
//! 画面、声音走哪条路由直连任务决定（[`Paths::media`]，见 `crate::direct`），这里每一帧、每一包看一眼。
//!
//! 画面的每一路（多屏同时显示时每个显示器一路，见 `airhand_proto::session` 的"多屏"）一个写任务、一个加密端，
//! 写在自己的通道上（`SessionLane::Video(第几路)`）；一路的写法和只有一路时一样。
//!

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use airhand_proto::SessionLane;
use airhand_proto::datagram;
use airhand_proto::media::VideoHeader;
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{SendDatagramError, SendStream};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::debug;

use super::queue::LaneReceiver;
use crate::audio::Bundler;
use crate::lanes::open_lane;
use crate::paths::{Paths, send_datagram};
use crate::secure::Sealer;
use crate::tasks::try_join_all;
use crate::transport::{WRITE_TIMEOUT, seal, write_lane};

/// 本端各条通道的写任务要的东西
pub(super) struct Writers {
    pub session_id: u64,
    pub paths: Paths,
    pub control: (SendStream, LaneReceiver, Sealer),
    pub bulk: (SendStream, LaneReceiver, Sealer),
    /// 画面的每一路（第 `i` 项是第 `i` 路）
    pub video: Vec<(Arc<FrameStreams>, mpsc::Receiver<Bytes>, Sealer)>,
    pub audio: (mpsc::Receiver<Bytes>, Sealer),
}

/// 所有的写任务，一个出错就都结束
pub(super) async fn write_all(w: Writers) -> Result<()> {
    let Writers {
        session_id,
        paths,
        control: (control, ctl_rx, ctl_sealer),
        bulk: (bulk, bulk_rx, bulk_sealer),
        video,
        audio: (audio_rx, audio_sealer),
    } = w;
    let video = video
        .into_iter()
        .enumerate()
        .map(|(stream, (video, rx, sealer))| {
            let to = Target {
                session_id,
                stream: stream as u8,
                paths: paths.clone(),
            };
            async move { write_frames(&video, rx, sealer, to).await }
        })
        .collect();
    tokio::try_join!(
        write_lane(control, ctl_rx, ctl_sealer, Some(paths.clone())),
        write_lane(bulk, bulk_rx, bulk_sealer, None),
        try_join_all(video),
        write_audio(paths, session_id, audio_rx, audio_sealer)
    )?;
    Ok(())
}

/// 直连上一帧画面这么久写不进去，就当它断了（这一帧改从中转发）
const DIRECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// 忘掉确认丢失的旧流句柄后 QUIC 仍会送完它。
const MAX_FRAME_STREAMS: usize = 4 * crate::flow::MAX_WINDOW as usize;

/// 在途的帧流（见模块说明）：按发出的先后，带着帧序号
#[derive(Default)]
pub(crate) struct FrameStreams(Mutex<VecDeque<(u32, SendStream)>>);

impl FrameStreams {
    fn add(&self, seq: u32, s: SendStream) {
        let mut q = self.0.lock().unwrap();
        if q.len() == MAX_FRAME_STREAMS {
            q.pop_front();
        }
        q.push_back((seq, s));
    }

    /// 控制端确认到了第 `seq` 帧（累积）：它和它之前的帧流不用管了
    pub(crate) fn acked(&self, seq: u32) {
        let mut q = self.0.lock().unwrap();
        while q
            .front()
            .is_some_and(|(s, _)| s.wrapping_sub(seq) as i32 <= 0)
        {
            q.pop_front();
        }
    }

    #[cfg(test)]
    pub(crate) fn add_for_test(&self, seq: u32, s: SendStream) {
        self.add(seq, s);
    }

    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// 控制端等不到了：第 `after` 帧之后的帧流都放弃（reset，不再重传）。返回放弃了几条
    pub(crate) fn abandon(&self, after: u32) -> usize {
        let mut q = self.0.lock().unwrap();
        let before = q.len();
        q.retain_mut(|(seq, s)| {
            let keep = seq.wrapping_sub(after) as i32 <= 0;
            if !keep {
                let _ = s.reset(0u32.into());
            }
            keep
        });
        before - q.len()
    }
}

/// 画面发给谁、是第几路、走哪条路
struct Target {
    session_id: u64,
    stream: u8,
    paths: Paths,
}

/// 帧流（见模块说明）：每一帧加密后开一条帧流写进去、结束这条流，记进 `streams`。
/// 直连上写不进去：这一帧改从中转发，告诉直连任务。
/// 中转上开不了流（连接断了、对方一直不给开新流）是错误，会话结束；开了流却写不进去（对方叫停了这条流）只是丢了这一帧，
/// 控制端会报告等不到它。
async fn write_frames(
    streams: &FrameStreams,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    to: Target,
) -> Result<()> {
    let Target {
        session_id,
        stream,
        paths,
    } = to;
    let lane = SessionLane::Video(stream);
    while let Some(frame) = rx.recv().await {
        let h = frame
            .get(4..)
            .and_then(VideoHeader::decode_any)
            .context("not a video packet")?;
        let seq = h.seq;
        let sealed = seal(frame, &mut sealer)?;
        if let Some(conn) = paths.media() {
            // 开流、写这一帧加起来最多等这么久：直连卡住了就改从中转发，不先在开流上等好几秒
            let deadline = Instant::now() + DIRECT_WRITE_TIMEOUT;
            let written = async {
                let mut s = tokio::time::timeout_at(deadline, open_lane(&conn, session_id, lane))
                    .await
                    .context("timed out opening a frame stream on the direct path")??;
                write_frame(&mut s, sealed.clone(), deadline)
                    .await
                    .context("the direct path stalled")?;
                anyhow::Ok(s)
            };
            match written.await {
                Ok(s) => {
                    streams.add(seq, s);
                    continue;
                }
                Err(e) => {
                    debug!(stream, "writing video to the direct path failed: {e:#}");
                    paths.report_broken(&conn);
                }
            }
        }
        let mut s = open_lane(paths.relay(), session_id, lane).await?;
        match write_frame(&mut s, sealed, Instant::now() + WRITE_TIMEOUT).await {
            Ok(()) => streams.add(seq, s),
            Err(e) if paths.relay().close_reason().is_some() => return Err(e),
            Err(e) => debug!(stream, seq, "a video frame couldn't be written: {e:#}"),
        }
    }
    Ok(())
}

/// 往一条帧流写完这一帧、结束这条流，最多等到 `deadline`。写不完就 reset：没发出去的丢掉、不再重传。
/// 只是丢掉流句柄的话 quinn 会正常结束它：已经排进去的半帧照样在（卡住了的）路上发完，对方读到半截就结束了，
/// 白占一份带宽
async fn write_frame(s: &mut SendStream, sealed: Bytes, deadline: Instant) -> Result<()> {
    let written = match tokio::time::timeout_at(deadline, s.write_chunk(sealed)).await {
        Ok(r) => r.map_err(anyhow::Error::from),
        Err(_) => Err(anyhow::anyhow!("timed out writing a video frame")),
    }
    .and_then(|()| s.finish().map_err(Into::into));
    if written.is_err() {
        let _ = s.reset(0u32.into());
    }
    written
}

/// 声音的数据报：发送缓冲排满了最多等这么久，等不到这一包就丢了
const AUDIO_WAIT: Duration = Duration::from_millis(40);

/// 声音：`rx` 里排好队的声音包（`media::frame` 编好的，带长度前缀）连同之前几包的副本（[`Bundler`]）
/// 加密后发成数据报，发在画面走的那条路上（[`Paths::media`]）。单独一包都放不进一个数据报的
/// （`media::MAX_AUDIO_PACKET`）丢掉
async fn write_audio(
    paths: Paths,
    session_id: u64,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
) -> Result<()> {
    let mut bundler = Bundler::default();
    while let Some(framed) = rx.recv().await {
        let len = framed.len();
        let Some(plain) = bundler.bundle(framed) else {
            debug!(len, "dropped an audio packet too large for a datagram");
            continue;
        };
        let direct = paths.media();
        let conn = direct.as_ref().unwrap_or(paths.relay());
        let d = sealer.seal_datagram(session_id, datagram::AUDIO, &plain)?;
        match send_datagram(conn, Bytes::from(d), AUDIO_WAIT).await {
            Ok(()) => {}
            Err(SendDatagramError::ConnectionLost(e)) if direct.is_none() => return Err(e.into()),
            // 直连断了（直连任务会把路改回中转）、发送缓冲满了：这一包丢了就丢了
            Err(e) => debug!("dropped an audio packet: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::Router;
    use crate::merge::{Merge, Video as Out};
    use airhand_proto::media;
    use airhand_proto::session::Codec;
    use tokio::time::Instant;

    /// 一帧画面（带长度前缀的明文，写任务收的那种）
    fn video(seq: u32, key: bool, reference: Option<u32>, size: usize) -> Bytes {
        let h = VideoHeader {
            seq,
            stream: 0,
            key,
            codec: Codec::H264,
            width: 8,
            height: 8,
            capture_us: 0,
            reference,
        };
        Bytes::from(media::frame(&h.encode(), &[&vec![seq as u8; size]]))
    }

    /// 多屏交替发送，每个显示器的关键帧和增量帧在各自的通道上按依赖交付。
    #[tokio::test]
    async fn two_streams_travel_independently() {
        const SID: u64 = 78;
        let (client, server) = crate::tests::quic_pair().await;
        let (router, _task) = Router::start(&server);
        let (mut inbox, _registration) = router.register(SID);
        let mut merges = Vec::new();
        let mut senders = Vec::new();
        let mut writers = Vec::new();
        for stream in 0..2u8 {
            let (sealer, opener) = crate::secure::test_lane(false);
            merges.push(Merge::new(
                SessionLane::Video(stream),
                opener,
                std::mem::replace(
                    &mut inbox.video[stream as usize],
                    mpsc::unbounded_channel().1,
                ),
            ));
            let streams = Arc::new(FrameStreams::default());
            let (tx, rx) = mpsc::channel(64);
            senders.push(tx);
            let to = Target {
                session_id: SID,
                stream,
                paths: Paths::new(client.clone()),
            };
            writers.push(tokio::spawn(async move {
                write_frames(&streams, rx, sealer, to).await
            }));
        }
        // 两路交替发送，每一路先发关键帧。
        for seq in 1..=5u32 {
            for (stream, tx) in senders.iter().enumerate() {
                let key = seq == 1;
                let size = if key { 20_000 } else { 1500 * (stream + 1) };
                let mut f = video(seq, key, (!key).then(|| seq - 1), size).to_vec();
                f[4 + 12] = stream as u8;
                tx.send(Bytes::from(f)).await.unwrap();
            }
        }
        for (stream, m) in merges.iter_mut().enumerate() {
            for seq in 1..=5u32 {
                let f = loop {
                    let v = tokio::time::timeout(Duration::from_secs(5), m.next_video())
                        .await
                        .expect("no frame")
                        .unwrap()
                        .unwrap();
                    if let Out::Frame(f) = v {
                        break f;
                    }
                };
                let h = VideoHeader::decode(&f.data).unwrap();
                assert_eq!((h.stream, h.seq), (stream as u8, seq));
            }
        }
        drop(senders);
        for w in writers {
            w.await.unwrap().unwrap();
        }
    }

    /// 直连卡住了（对方不读，流量控制的额度用完了）：这一帧的流要 reset，不能正常结束
    /// （正常结束的话已经排进去的半帧照样在卡住的路上发完，对方读到半截），这一帧改从中转完整地发
    #[tokio::test]
    async fn a_stalled_direct_write_is_reset_and_the_frame_goes_through_the_relay() {
        use crate::lanes::Via;
        use airhand_proto::SessionHeader;
        const SID: u64 = 11;
        let (relay, relay_peer) = crate::tests::quic_pair().await;
        let (direct, direct_peer) = crate::tests::quic_pair().await;
        let paths = Paths::new(relay.clone());
        let mirror = paths.attach(direct.clone(), direct.open_uni().await.unwrap());
        drop(mirror);
        paths.set_media(Via::Direct);
        let broken = paths.broken();
        let (sealer, _opener) = crate::secure::test_lane(false);
        let (tx, rx) = mpsc::channel(1);
        let to = Target {
            session_id: SID,
            stream: 0,
            paths: paths.clone(),
        };
        let _writer = crate::tasks::OwnedTask(tokio::spawn(async move {
            write_frames(&FrameStreams::default(), rx, sealer, to).await
        }));
        // 比直连上一条流的接收窗口（16 MiB）大
        let frame = video(1, true, None, 20 << 20);
        let sealed_len = frame.len() + airhand_proto::handshake::SEAL_OVERHEAD;
        tx.send(frame).await.unwrap();
        async fn header(recv: &mut quinn::RecvStream) -> Option<SessionHeader> {
            let mut b = [0u8; SessionHeader::LEN];
            recv.read_exact(&mut b).await.ok()?;
            SessionHeader::decode(&b)
        }
        // 直连上：读完会话头就不读了。attach 拿的那条控制流（上面丢掉了，没有数据就结束了）先到，跳过它
        let mut stalled = loop {
            let mut s = direct_peer.accept_uni().await.unwrap();
            if header(&mut s).await == Some(SessionHeader::new(SID, SessionLane::Video(0))) {
                break s;
            }
        };
        // 中转上：完整的一帧
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            let mut r = relay_peer.accept_uni().await.unwrap();
            assert_eq!(
                header(&mut r).await,
                Some(SessionHeader::new(SID, SessionLane::Video(0)))
            );
            r.read_to_end(usize::MAX).await.unwrap()
        })
        .await
        .expect("the frame didn't come through the relay");
        assert_eq!(got.len(), sealed_len);
        assert!(
            broken.has_changed().unwrap(),
            "the direct path wasn't reported"
        );
        // 直连上的那条被放弃了：读下去是 reset，不是正常结束的半帧
        let rest = tokio::time::timeout(Duration::from_secs(5), stalled.read_to_end(usize::MAX))
            .await
            .expect("the stalled stream neither ended nor was reset");
        assert!(
            matches!(
                rest,
                Err(quinn::ReadToEndError::Read(quinn::ReadError::Reset(_)))
            ),
            "{:?}",
            rest.map(|b| b.len())
        );
    }

    /// 帧流，真的 QUIC 走一条丢 2% 的包、往返 60 ms 的路：丢了包的帧等 QUIC 重传就到了，控制端不能抢在重传之前报告
    /// 等不到（报告了被控端就要放弃在途的帧、编恢复帧，编码器没有长期参考帧时还是关键帧）。
    /// 这里被控端不理会报告，每一帧最后都会到：报告了的都是报早了的
    #[tokio::test]
    async fn losses_are_not_reported_before_the_retransmission() {
        use airhand_proto::session::REPORT_EVERY;
        const SID: u64 = 9;
        const RTT_MS: f64 = 60.0;
        let (client, server, _relay) =
            crate::tests::lossy_quic_pair(0.02, Duration::from_secs_f64(RTT_MS / 2000.0)).await;
        let (router, _task) = Router::start(&server);
        let (mut inbox, _registration) = router.register(SID);
        let (sealer, opener) = crate::secure::test_lane(false);
        let mut m = Merge::new(SessionLane::Video(0), opener, inbox.video.remove(0));
        m.set_rtt(RTT_MS);
        let stats = m.stats();
        let (tx, rx) = mpsc::channel(64);
        let _writer = {
            let to = Target {
                session_id: SID,
                stream: 0,
                paths: Paths::new(client.clone()),
            };
            crate::tasks::OwnedTask(tokio::spawn(async move {
                write_frames(&FrameStreams::default(), rx, sealer, to).await
            }))
        };
        // 60 fps，每帧 15 KB（十几个包），只有第一帧是关键帧
        let _source = crate::tasks::OwnedTask(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(16));
            for seq in 1u32.. {
                tick.tick().await;
                let f = video(seq, seq == 1, (seq > 1).then(|| seq - 1), 15_000);
                if tx.send(f).await.is_err() {
                    break;
                }
            }
        }));
        let started = Instant::now();
        let mut report_at = started + REPORT_EVERY;
        let (mut frames, mut stalled, mut reported, mut keyframes) = (0u32, 0u32, 0u32, 0u32);
        // 一次缺帧只算一次报告（之后定时重报的不算）
        let mut reporting = false;
        while started.elapsed() < Duration::from_secs(6) {
            match tokio::time::timeout_at(report_at, m.next_video()).await {
                Ok(v) => match v.unwrap().unwrap() {
                    Out::Frame(_) => {
                        frames += 1;
                        reporting = false;
                    }
                    Out::Lost { .. } => {
                        reported += u32::from(!reporting);
                        reporting = true;
                    }
                    Out::Keyframe { .. } => keyframes += 1,
                    Out::Skipped { .. } => {}
                },
                Err(_) => {
                    report_at += REPORT_EVERY;
                    stalled += stats.take().stalled;
                }
            }
        }
        println!(
            "{frames} frames, {stalled} stalled, {reported} reported lost too early, {keyframes} keyframe requests"
        );
        assert!(frames >= 250, "only {frames} frames");
        assert!(
            stalled >= 10,
            "the path should lose packets: {stalled} stalled"
        );
        assert!(
            reported * 10 <= stalled,
            "{reported} of {stalled} stalled frames were reported lost"
        );
        assert_eq!(keyframes, 0);
    }

    /// 帧流，真的 QUIC，往返 60 ms：丢 2% 的包时，帧等前面的帧重传、到达时刻跟着晚（和以前一条有序流时一样，
    /// 见 crate::merge 的"到达时刻"），码率控制当它是排队、把码率降下来；同一条路不丢包时一直是画质设置的码率
    #[tokio::test]
    async fn waiting_for_retransmissions_lowers_the_bitrate() {
        use crate::flow::{Flow, frame_bytes};
        use std::sync::atomic::AtomicBool;
        const SID: u64 = 10;
        const TARGET: u32 = 8_000;
        let mut got = Vec::new();
        for loss in [0.0, 0.02] {
            let (client, server, _relay) =
                crate::tests::lossy_quic_pair(loss, Duration::from_millis(30)).await;
            // 先跑一阵大块数据：拥塞窗口从很小开始涨，画面一开始就按 8 Mbps 发的话头几帧要在发送端排队、
            // 码率降下来再慢慢涨回去（本来就是排队，以前一条有序流时也一样），不是这个测试要看的
            let drain = {
                let server = server.clone();
                tokio::spawn(async move {
                    let mut r = server.accept_uni().await.unwrap();
                    r.read_to_end(usize::MAX).await.unwrap();
                })
            };
            let mut warm = client.open_uni().await.unwrap();
            warm.write_all(&vec![0u8; 2_000_000]).await.unwrap();
            warm.finish().unwrap();
            drain.await.unwrap();
            let (router, _task) = Router::start(&server);
            let (mut inbox, _registration) = router.register(SID);
            let (sealer, opener) = crate::secure::test_lane(false);
            let mut m = Merge::new(SessionLane::Video(0), opener, inbox.video.remove(0));
            let flow = Arc::new(Flow::default());
            flow.set_target(TARGET, true);
            let (tx, rx) = mpsc::channel(64);
            let _writer = {
                let to = Target {
                    session_id: SID,
                    stream: 0,
                    paths: Paths::new(client.clone()),
                };
                crate::tasks::OwnedTask(tokio::spawn(async move {
                    write_frames(&FrameStreams::default(), rx, sealer, to).await
                }))
            };
            // 被控端：60 fps，一帧的大小按码率控制给的码率，在途窗口不够就跳过这一帧（和编码管线一样）
            let _source = {
                let flow = flow.clone();
                crate::tasks::OwnedTask(tokio::spawn(async move {
                    let stop = AtomicBool::new(false);
                    let mut tick = tokio::time::interval(Duration::from_millis(16));
                    let mut prev = None;
                    loop {
                        tick.tick().await;
                        if !flow.wait_window(60, &stop, Duration::ZERO) {
                            continue;
                        }
                        let size = frame_bytes(flow.bitrate_kbps(), 60);
                        let seq = flow.sent(size);
                        let f = video(seq, prev.is_none(), prev, size);
                        prev = Some(seq);
                        if tx.send(f).await.is_err() {
                            break;
                        }
                    }
                }))
            };
            // 控制端：交出来的帧（和跳过的）带着到达时刻确认
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                let Ok(v) = tokio::time::timeout(Duration::from_millis(500), m.next_video()).await
                else {
                    continue;
                };
                match v.unwrap().unwrap() {
                    Out::Frame(f) => {
                        let seq = VideoHeader::decode(&f.data).unwrap().seq;
                        flow.ack(seq, Some(f.arrived));
                    }
                    Out::Skipped { seq, arrived } => flow.ack(seq, Some(arrived)),
                    Out::Lost { .. } | Out::Keyframe { .. } => {}
                }
            }
            got.push(flow.bitrate_kbps());
        }
        println!(
            "bitrate without loss {} kbps, with 2% loss {} kbps",
            got[0], got[1]
        );
        assert_eq!(got[0], TARGET);
        // 只看每帧自己开始到达的时刻时也会降一些（丢了帧开头的包时它自己晚到），大约到 3.6 Mbps；
        // 加上等前面的帧之后降到 1.5～2.3 Mbps
        assert!(got[1] * 10 <= TARGET * 3, "{got:?}");
    }

    /// 确认了的帧流不用管了（序号会绕回）；放弃的是控制端解到的那一帧之后的
    #[tokio::test]
    async fn frame_streams_are_forgotten_when_acked_and_reset_when_abandoned() {
        let (client, _server) = crate::tests::quic_pair().await;
        let streams = FrameStreams::default();
        for seq in [u32::MAX, 0, 1, 2] {
            let mut s = client.open_uni().await.unwrap();
            s.write_all(&seq.to_le_bytes()).await.unwrap();
            streams.add(seq, s);
        }
        streams.acked(0);
        let left = |s: &FrameStreams| {
            s.0.lock()
                .unwrap()
                .iter()
                .map(|(q, _)| *q)
                .collect::<Vec<_>>()
        };
        assert_eq!(left(&streams), [1, 2]);
        assert_eq!(streams.abandon(1), 1);
        assert_eq!(left(&streams), [1]);
        assert_eq!(streams.abandon(1), 0);
    }
}
