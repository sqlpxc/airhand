//! 被控端一个会话的写任务：控制、大块通道（两端共用的 [`write_lane`]），画面（[`write_video`]）和声音（[`write_audio`]）。
//! 画面、声音走哪条路由直连任务决定（[`Paths::media`]，见 `crate::direct`），这里每一帧、每一包看一眼。
//!
//! 画面的每一路（多屏同时显示时每个显示器一路，见 `airhand_proto::session` 的"多屏"）一个写任务、一个加密端，
//! 写在自己的通道上（`SessionLane::Video(第几路)`）；一路的写法和只有一路时一样。
//!
//! 画面有两种写法（见 `airhand_proto::session` 的"帧流"）：
//!
//! * 一条画面流（[`Video::Stream`]）：每条路上一条（发第一帧时才开），帧一个接一个写进去；
//! * 帧流（[`Video::Frames`]，双方都支持、中转也支持时）：每帧开一条流，写完就结束。写好的流记在 [`FrameStreams`] 里，
//!   控制端确认了就不用管了；控制端等不到了（`ControllerDatagram::Lost`），之后的帧整条流放弃（reset），
//!   不再重传过时的数据；
//! * 帧流时，增量帧还可以走纠错的数据报（双方都支持时，见 `airhand_proto::session` 的"前向纠错"）：
//!   每帧问一次 [`Chooser`]，它说走纠错就把密封单元切开、配上冗余（[`crate::fec::fragments`]），一块一个数据报。
//!   发送缓冲排满了就等（[`crate::paths::send_datagram`]）：画面多了不挤掉声音，声音也不挤掉画面的块。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use airhand_proto::SessionLane;
use airhand_proto::datagram;
use airhand_proto::framing::PREFIX;
use airhand_proto::media::VideoHeader;
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, SendDatagramError, SendStream};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::debug;

use super::queue::LaneReceiver;
use crate::audio::Bundler;
use crate::fec::{self, Chooser};
use crate::lanes::{open_frame, open_lane};
use crate::paths::{Paths, send_datagram};
use crate::secure::Sealer;
use crate::tasks::try_join_all;
use crate::transport::{WRITE_TIMEOUT, seal, write_lane, write_sealed};

/// 本端各条通道的写任务要的东西
pub(super) struct Writers {
    pub session_id: u64,
    pub paths: Paths,
    pub control: (SendStream, LaneReceiver, Sealer),
    pub bulk: (SendStream, LaneReceiver, Sealer),
    /// 画面的每一路（第 `i` 项是第 `i` 路）
    pub video: Vec<(Video, mpsc::Receiver<Bytes>, Sealer)>,
    pub audio: (mpsc::Receiver<Bytes>, Sealer),
}

/// 一路画面怎么写（见模块说明）
pub(super) enum Video {
    /// 每条路上一条画面流
    Stream,
    /// 每帧一条帧流；双方都支持纠错时有 `fec`：增量帧可能走纠错的数据报
    Frames {
        streams: Arc<FrameStreams>,
        fec: Option<Arc<Chooser>>,
    },
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
            async move {
                match video {
                    Video::Stream => write_video(rx, sealer, to).await,
                    Video::Frames { streams, fec } => {
                        write_frames(&streams, fec.as_deref(), rx, sealer, to).await
                    }
                }
            }
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

/// 一路画面：`rx` 里排好队的画面包加密后写进现在走的那条路（[`Paths::media`]）。
/// 换路只在两帧之间生效，写到一半的帧照样写完：直连上已经写进去的帧照样送到，接收端按计数排好（见 [`crate::merge`]）。
/// 往直连上写不进去（断了、堵死了）就放弃直连上的这条流（流里还没送到的帧随之丢了），这一帧改写进中转，
/// 并告诉直连任务（[`Paths::report_broken`]）：它当直连断了（要关键帧、码率控制不再等那些帧）。中转上写不进去是错误：会话结束。
/// 中转上的流发第一帧时才开：只看一个显示器时别的路不开流
async fn write_video(mut rx: mpsc::Receiver<Bytes>, mut sealer: Sealer, to: Target) -> Result<()> {
    let Target {
        session_id,
        stream,
        paths,
    } = to;
    let lane = SessionLane::Video(stream);
    let mut relay: Option<SendStream> = None;
    // 直连上画面的流：哪条连接上的（`stable_id`）
    let mut direct: Option<(usize, SendStream)> = None;
    while let Some(frame) = rx.recv().await {
        let sealed = seal(frame, &mut sealer)?;
        if let Some(conn) = paths.media() {
            match write_direct(&conn, &mut direct, session_id, lane, sealed.clone()).await {
                Ok(()) => continue,
                Err(e) => {
                    debug!(stream, "writing video to the direct path failed: {e:#}");
                    if let Some((_, mut s)) = direct.take() {
                        let _ = s.reset(0u32.into());
                    }
                    paths.report_broken(&conn);
                }
            }
        }
        if relay.is_none() {
            relay = Some(open_lane(paths.relay(), session_id, lane).await?);
        }
        write_sealed(relay.as_mut().expect("opened above"), sealed).await?;
    }
    if let Some(mut r) = relay {
        r.finish()?;
    }
    if let Some((_, mut s)) = direct {
        let _ = s.finish();
    }
    Ok(())
}

/// 往直连上写一帧画面（这条连接上还没有这一路画面的流就开一条）
async fn write_direct(
    conn: &Connection,
    direct: &mut Option<(usize, SendStream)>,
    session_id: u64,
    lane: SessionLane,
    sealed: Bytes,
) -> Result<()> {
    if direct
        .as_ref()
        .is_none_or(|(id, _)| *id != conn.stable_id())
    {
        let s = open_lane(conn, session_id, lane).await?;
        *direct = Some((conn.stable_id(), s));
    }
    let (_, s) = direct.as_mut().expect("opened above");
    tokio::time::timeout(DIRECT_WRITE_TIMEOUT, s.write_chunk(sealed))
        .await
        .context("the direct path stalled")??;
    Ok(())
}

/// 最多记着这么多条还没确认的帧流（在途的帧不超过 `flow::MAX_WINDOW`，多出来的是确认丢了的，忘掉就是了：
/// 忘掉一条流不会放弃它，QUIC 照样送完）
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
/// 直连上写不进去：这一帧改从中转发，告诉直连任务（和 [`write_video`] 一样）。
/// 中转上开不了流（连接断了、对方一直不给开新流）是错误，会话结束；开了流却写不进去（对方叫停了这条流）只是丢了这一帧，
/// 控制端会报告等不到它。
///
/// 有 `fec` 时增量帧可能走纠错的数据报（见模块说明）：只有参考的就是前一帧的才行（恢复帧、关键帧总是走帧流）
async fn write_frames(
    streams: &FrameStreams,
    fec: Option<&Chooser>,
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
    // 上一帧的序号、是不是走的纠错（下一帧的数据报里要说，见 airhand_proto::fec 的 PREV_FEC）
    let mut prev: Option<u32> = None;
    let mut prev_fec = false;
    while let Some(frame) = rx.recv().await {
        let h = frame
            .get(4..)
            .and_then(VideoHeader::decode_any)
            .context("not a video packet")?;
        let seq = h.seq;
        let eligible = !h.key && h.reference.is_some() && h.reference == prev;
        prev = Some(seq);
        let sealed = seal(frame, &mut sealer)?;
        if let Some(c) = fec {
            c.sent(seq, Instant::now());
            if let Some(r) = c.plan(sealed.len() - PREFIX, eligible) {
                let d = fec::fragments(session_id, stream, &sealed[PREFIX..], prev_fec, r)?;
                prev_fec = true;
                send_fragments(&paths, d).await?;
                continue;
            }
        }
        prev_fec = false;
        if let Some(conn) = paths.media() {
            let written = async {
                let mut s = open_frame(&conn, session_id, lane).await?;
                tokio::time::timeout(DIRECT_WRITE_TIMEOUT, s.write_chunk(sealed.clone()))
                    .await
                    .context("the direct path stalled")??;
                s.finish()?;
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
        let mut s = open_frame(paths.relay(), session_id, lane).await?;
        let written = async {
            tokio::time::timeout(WRITE_TIMEOUT, s.write_chunk(sealed))
                .await
                .context("session writer stalled")??;
            s.finish()?;
            anyhow::Ok(())
        };
        match written.await {
            Ok(()) => streams.add(seq, s),
            Err(e) if paths.relay().close_reason().is_some() => return Err(e),
            Err(e) => debug!(stream, seq, "a video frame couldn't be written: {e:#}"),
        }
    }
    Ok(())
}

/// 纠错的一帧（切好的数据报，见 [`crate::fec::fragments`]）发在画面走的那条路上（[`Paths::media`]）。
/// 发送缓冲排满了就等，不挤掉排着的（见 [`crate::paths::send_datagram`]）。
/// 直连上发不出去（断了、堵死了）：剩下的改从中转发，告诉直连任务（和帧流一样）。中转断了是错误
async fn send_fragments(paths: &Paths, frags: Vec<Bytes>) -> Result<()> {
    let mut frags = frags.into_iter().peekable();
    if let Some(conn) = paths.media() {
        while let Some(d) = frags.peek() {
            match tokio::time::timeout(DIRECT_WRITE_TIMEOUT, conn.send_datagram_wait(d.clone()))
                .await
            {
                Ok(Ok(())) => {
                    frags.next();
                }
                r => {
                    debug!("sending video datagrams on the direct path failed: {r:?}");
                    paths.report_broken(&conn);
                    break;
                }
            }
        }
    }
    for d in frags {
        match tokio::time::timeout(WRITE_TIMEOUT, paths.relay().send_datagram_wait(d)).await {
            Ok(Ok(())) => {}
            Ok(Err(SendDatagramError::ConnectionLost(e))) => return Err(e.into()),
            Ok(Err(e)) => debug!("dropped a video datagram: {e}"),
            Err(_) => anyhow::bail!("session writer stalled"),
        }
    }
    Ok(())
}

/// 声音的数据报：发送缓冲排满了（多半是画面的纠错数据报）最多等这么久，等不到这一包就丢了
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
    use crate::lanes::{Router, Via};
    use crate::merge::{Merge, Policy, Video as Out};
    use airhand_proto::media;
    use airhand_proto::session::{Codec, VideoTransport};

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

    /// 合并交出来的下一帧：(序号, 是不是纠错拼回来的)
    async fn next_frame(m: &mut Merge) -> (u32, bool) {
        loop {
            let v = tokio::time::timeout(Duration::from_secs(5), m.next_video())
                .await
                .expect("no frame")
                .unwrap()
                .unwrap();
            if let Out::Frame(f) = v {
                return (VideoHeader::decode(&f.data).unwrap().seq, f.fec);
            }
        }
    }

    /// 真的 QUIC，两路画面（多屏同时显示）：每一路的帧流走自己的通道、纠错数据报按头里的第几路分开，
    /// 控制端的两个合并各自按顺序交出自己那一路的帧，谁也不混进谁
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
            merges.push(
                Merge::new(
                    SessionLane::Video(stream),
                    Policy::Video,
                    opener,
                    std::mem::replace(
                        &mut inbox.video[stream as usize],
                        mpsc::unbounded_channel().1,
                    ),
                )
                .with_datagrams(std::mem::replace(
                    &mut inbox.fec[stream as usize],
                    mpsc::channel(1).1,
                )),
            );
            let streams = Arc::new(FrameStreams::default());
            let chooser = Arc::new(Chooser::new(VideoTransport::Fec));
            let (tx, rx) = mpsc::channel(64);
            senders.push(tx);
            let to = Target {
                session_id: SID,
                stream,
                paths: Paths::new(client.clone()),
            };
            writers.push(tokio::spawn(async move {
                write_frames(&streams, Some(&chooser), rx, sealer, to).await
            }));
        }
        // 两路交替着发：关键帧走帧流，之后的增量帧走纠错的数据报
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
                assert_eq!(f.fec, seq > 1, "stream {stream} frame {seq}");
            }
        }
        drop(senders);
        for w in writers {
            w.await.unwrap().unwrap();
        }
    }

    /// 真的 QUIC：写任务固定用纠错时，增量帧走纠错的数据报，关键帧、恢复帧（参考的不是前一帧）走帧流；
    /// 控制端的合并把两种按顺序交出来。改成帧流之后都走帧流
    #[tokio::test]
    async fn delta_frames_go_as_datagrams_and_the_rest_as_streams() {
        const SID: u64 = 77;
        let (client, server) = crate::tests::quic_pair().await;
        let (router, _task) = Router::start(&server);
        let (mut inbox, _registration) = router.register(SID);
        let (sealer, opener) = crate::secure::test_lane(false);
        let mut m = Merge::new(
            SessionLane::Video(0),
            Policy::Video,
            opener,
            inbox.video.remove(0),
        )
        .with_datagrams(inbox.fec.remove(0));
        let streams = Arc::new(FrameStreams::default());
        let chooser = Arc::new(Chooser::new(VideoTransport::Fec));
        let (tx, rx) = mpsc::channel(64);
        let writer = {
            let (streams, chooser) = (streams.clone(), chooser.clone());
            let to = Target {
                session_id: SID,
                stream: 0,
                paths: Paths::new(client.clone()),
            };
            tokio::spawn(
                async move { write_frames(&streams, Some(&chooser), rx, sealer, to).await },
            )
        };
        // 关键帧、增量帧（大小不一）、从第 3 帧恢复的一帧、接着的增量帧
        tx.send(video(1, true, None, 30_000)).await.unwrap();
        for seq in 2..=6 {
            tx.send(video(
                seq,
                false,
                Some(seq - 1),
                300 * seq as usize * seq as usize,
            ))
            .await
            .unwrap();
        }
        tx.send(video(7, false, Some(3), 2000)).await.unwrap();
        tx.send(video(8, false, Some(7), 2000)).await.unwrap();
        let mut got = Vec::new();
        for _ in 1..=8 {
            got.push(next_frame(&mut m).await);
        }
        assert_eq!(
            got,
            [
                (1, false),
                (2, true),
                (3, true),
                (4, true),
                (5, true),
                (6, true),
                (7, false),
                (8, true)
            ]
        );
        // 改成只用帧流
        chooser.set_mode(VideoTransport::Streams);
        tx.send(video(9, false, Some(8), 2000)).await.unwrap();
        assert_eq!(next_frame(&mut m).await, (9, false));
        drop(tx);
        writer.await.unwrap().unwrap();
    }

    /// 自动，真的 QUIC 走一条丢 5% 的包、往返 60 ms 的路：帧流上的帧常常读到一半停下来等重传，控制端的报告里
    /// 卡住的帧多了，被控端改用纠错；之后的增量帧走数据报，丢了块也拼得回来，不再等重传
    #[tokio::test]
    async fn auto_switches_to_fec_on_a_lossy_path() {
        use airhand_proto::session::REPORT_EVERY;
        const SID: u64 = 5;
        let (client, server, _relay) =
            crate::tests::lossy_quic_pair(0.05, Duration::from_millis(30)).await;
        let (router, _task) = Router::start(&server);
        let (mut inbox, _registration) = router.register(SID);
        let (sealer, opener) = crate::secure::test_lane(false);
        let mut m = Merge::new(
            SessionLane::Video(0),
            Policy::Video,
            opener,
            inbox.video.remove(0),
        )
        .with_datagrams(inbox.fec.remove(0));
        m.set_rtt(60.0);
        let stats = m.stats();
        let streams = Arc::new(FrameStreams::default());
        let chooser = Arc::new(Chooser::new(VideoTransport::Auto));
        let (tx, rx) = mpsc::channel(64);
        let _writer = {
            let (streams, chooser) = (streams.clone(), chooser.clone());
            let to = Target {
                session_id: SID,
                stream: 0,
                paths: Paths::new(client.clone()),
            };
            crate::tasks::OwnedTask(tokio::spawn(async move {
                write_frames(&streams, Some(&chooser), rx, sealer, to).await
            }))
        };
        // 被控端：60 fps，每帧 15 KB（十几个包），每秒一个关键帧（这里没有恢复：补不回来的帧之后解不了的跳过，到关键帧为止）
        let _source = crate::tasks::OwnedTask(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(16));
            for seq in 1u32.. {
                tick.tick().await;
                let key = seq % 60 == 1;
                let f = video(seq, key, (!key).then(|| seq - 1), 15_000);
                if tx.send(f).await.is_err() {
                    break;
                }
            }
        }));
        // 控制端：收画面、确认；每 REPORT_EVERY 把报告交给被控端（真的会话里是控制数据报）
        let started = Instant::now();
        let mut report_at = started + REPORT_EVERY;
        let (mut frames, mut fec_frames, mut switched_at) = (0u32, 0u32, None);
        let mut fec = airhand_proto::session::VideoReport::default();
        while started.elapsed() < Duration::from_secs(15) && fec_frames < 120 {
            if let Ok(v) = tokio::time::timeout_at(report_at, m.next_video()).await {
                if let Out::Frame(f) = v.unwrap().unwrap() {
                    frames += 1;
                    fec_frames += u32::from(f.fec);
                    let seq = VideoHeader::decode(&f.data).unwrap().seq;
                    chooser.acked(seq, Instant::now());
                }
                continue;
            }
            report_at += REPORT_EVERY;
            let r = stats.take();
            fec.fragments += r.fragments;
            fec.missing += r.missing;
            fec.unrecovered += r.unrecovered;
            chooser.report(r, Instant::now());
            if switched_at.is_none() && chooser.using() {
                switched_at = Some(started.elapsed());
            }
        }
        let switched = switched_at.expect("auto never switched to forward error correction");
        println!(
            "switched after {switched:?}; {frames} frames, {fec_frames} rebuilt from datagrams; \
             {} of {} blocks lost, {} frames unrecovered",
            fec.missing, fec.fragments, fec.unrecovered
        );
        // 刚过了 DWELL 就换了（之前卡住的帧已经够多了）
        assert!(switched < Duration::from_secs(8), "{switched:?}");
        assert!(
            fec_frames >= 120,
            "only {fec_frames} frames came as datagrams"
        );
        // 真的丢了块（大约 5%），冗余补上了绝大多数帧
        assert!(fec.missing * 100 >= fec.fragments * 2, "{fec:?}");
        assert!(fec.unrecovered * 20 <= fec_frames, "{fec:?}");
    }

    /// 自动，真的 QUIC 走一条不丢包、往返 60 ms 的路：大的关键帧本来就比跟在后面的小帧晚到（拥塞窗口还小的时候
    /// 还会停下来等确认），这不算等重传：一直用帧流
    #[tokio::test]
    async fn auto_stays_on_streams_without_loss() {
        use airhand_proto::session::REPORT_EVERY;
        const SID: u64 = 6;
        let (client, server, _relay) =
            crate::tests::lossy_quic_pair(0.0, Duration::from_millis(30)).await;
        let (router, _task) = Router::start(&server);
        let (mut inbox, _registration) = router.register(SID);
        let (sealer, opener) = crate::secure::test_lane(false);
        let mut m = Merge::new(
            SessionLane::Video(0),
            Policy::Video,
            opener,
            inbox.video.remove(0),
        )
        .with_datagrams(inbox.fec.remove(0));
        m.set_rtt(60.0);
        let stats = m.stats();
        let streams = Arc::new(FrameStreams::default());
        let chooser = Arc::new(Chooser::new(VideoTransport::Auto));
        let (tx, rx) = mpsc::channel(64);
        let _writer = {
            let (streams, chooser) = (streams.clone(), chooser.clone());
            let to = Target {
                session_id: SID,
                stream: 0,
                paths: Paths::new(client.clone()),
            };
            crate::tasks::OwnedTask(tokio::spawn(async move {
                write_frames(&streams, Some(&chooser), rx, sealer, to).await
            }))
        };
        // 每秒一个 150 KB 的关键帧，其余 15 KB
        let _source = crate::tasks::OwnedTask(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(16));
            for seq in 1u32.. {
                tick.tick().await;
                let key = seq % 60 == 1;
                let size = if key { 150_000 } else { 15_000 };
                let f = video(seq, key, (!key).then(|| seq - 1), size);
                if tx.send(f).await.is_err() {
                    break;
                }
            }
        }));
        let started = Instant::now();
        let mut report_at = started + REPORT_EVERY;
        let mut total = airhand_proto::session::VideoReport::default();
        while started.elapsed() < Duration::from_millis(6500) {
            if let Ok(v) = tokio::time::timeout_at(report_at, m.next_video()).await {
                if let Out::Frame(f) = v.unwrap().unwrap() {
                    let seq = VideoHeader::decode(&f.data).unwrap().seq;
                    chooser.acked(seq, Instant::now());
                }
                continue;
            }
            report_at += REPORT_EVERY;
            let r = stats.take();
            total.frames += r.frames;
            total.stalled += r.stalled;
            chooser.report(r, Instant::now());
            assert!(!chooser.using(), "switched on a clean network: {total:?}");
        }
        println!("{} frames, {} stalled", total.frames, total.stalled);
        assert!(total.frames >= 300, "{total:?}");
    }

    /// 真的 QUIC 数据报，一路上丢了七分之一：按丢包率配了冗余的帧都拼得回来，不用重传
    #[tokio::test]
    async fn fec_frames_survive_lost_datagrams() {
        let (client, server) = crate::tests::quic_pair().await;
        let paths = Paths::new(client.clone());
        let (mut sealer, opener) = crate::secure::test_lane(false);
        let (_streams, no_streams) = mpsc::unbounded_channel();
        let (dtx, drx) = mpsc::channel(4096);
        let mut m = Merge::new(SessionLane::Video(0), Policy::Video, opener, no_streams)
            .with_datagrams(drx);
        let lossy = tokio::spawn(async move {
            let mut n = 0u32;
            while let Ok(d) = server.read_datagram().await {
                n += 1;
                if !n.is_multiple_of(7) {
                    let _ = dtx.send((Via::Relay, d)).await;
                }
            }
        });
        const FRAMES: u32 = 30;
        let mut prev_fec = false;
        for seq in 1..=FRAMES {
            let f = video(seq, seq == 1, (seq > 1).then(|| seq - 1), 12_000);
            let sealed = seal(f, &mut sealer).unwrap();
            let (_, k) = airhand_proto::fec::shard_size(sealed.len() - PREFIX);
            let r = fec::repair_count(k, 1.0 / 7.0);
            let d = fec::fragments(1, 0, &sealed[PREFIX..], prev_fec, r).unwrap();
            prev_fec = true;
            send_fragments(&paths, d).await.unwrap();
        }
        for seq in 1..=FRAMES {
            assert_eq!(next_frame(&mut m).await, (seq, true));
        }
        let report = m.stats().take();
        assert_eq!(report.unrecovered, 0);
        lossy.abort();
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
