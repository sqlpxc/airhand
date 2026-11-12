//! 被控端一个会话的写任务：控制、大块通道（两端共用的 [`write_lane`]），画面（[`write_video`]）和声音（[`write_audio`]）。
//! 画面、声音走哪条路由直连任务决定（[`Paths::media`]，见 `crate::direct`），这里每一帧、每一包看一眼。

use airhand_proto::SessionLane;
use airhand_proto::datagram;
use anyhow::{Context, Result};
use bytes::Bytes;
use quinn::{Connection, SendDatagramError, SendStream};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::debug;

use super::queue::LaneReceiver;
use crate::audio::Bundler;
use crate::lanes::open_lane;
use crate::paths::Paths;
use crate::secure::Sealer;
use crate::transport::{seal, write_lane, write_sealed};

/// 本端各条通道的写任务要的东西
pub(super) struct Writers {
    pub session_id: u64,
    pub paths: Paths,
    pub control: (SendStream, LaneReceiver, Sealer),
    pub bulk: (SendStream, LaneReceiver, Sealer),
    pub video: (SendStream, mpsc::Receiver<Bytes>, Sealer),
    pub audio: (mpsc::Receiver<Bytes>, Sealer),
}

/// 所有的写任务，一个出错就都结束
pub(super) async fn write_all(w: Writers) -> Result<((), (), (), ())> {
    let Writers {
        session_id,
        paths,
        control: (control, ctl_rx, ctl_sealer),
        bulk: (bulk, bulk_rx, bulk_sealer),
        video: (video, video_rx, video_sealer),
        audio: (audio_rx, audio_sealer),
    } = w;
    tokio::try_join!(
        write_lane(control, ctl_rx, ctl_sealer, Some(paths.clone())),
        write_lane(bulk, bulk_rx, bulk_sealer, None),
        write_video(video, video_rx, video_sealer, session_id, paths.clone()),
        write_audio(paths, session_id, audio_rx, audio_sealer)
    )
}

/// 直连上一帧画面这么久写不进去，就当它断了（这一帧改从中转发）
const DIRECT_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// 画面：`rx` 里排好队的画面包加密后写进现在走的那条路（[`Paths::media`]）。
/// 换路只在两帧之间生效，写到一半的帧照样写完：直连上已经写进去的帧照样送到，接收端按计数排好（见 [`crate::merge`]）。
/// 往直连上写不进去（断了、堵死了）就放弃直连上的这条流（流里还没送到的帧随之丢了），这一帧改写进中转，
/// 并告诉直连任务（[`Paths::report_broken`]）：它当直连断了（要关键帧、码率控制不再等那些帧）。中转上写不进去是错误：会话结束
async fn write_video(
    mut relay: SendStream,
    mut rx: mpsc::Receiver<Bytes>,
    mut sealer: Sealer,
    session_id: u64,
    paths: Paths,
) -> Result<()> {
    // 直连上画面的流：哪条连接上的（`stable_id`）
    let mut direct: Option<(usize, SendStream)> = None;
    while let Some(frame) = rx.recv().await {
        let sealed = seal(frame, &mut sealer)?;
        if let Some(conn) = paths.media() {
            match write_direct(&conn, &mut direct, session_id, sealed.clone()).await {
                Ok(()) => continue,
                Err(e) => {
                    debug!("writing video to the direct path failed: {e:#}");
                    if let Some((_, mut s)) = direct.take() {
                        let _ = s.reset(0u32.into());
                    }
                    paths.report_broken(&conn);
                }
            }
        }
        write_sealed(&mut relay, sealed).await?;
    }
    relay.finish()?;
    if let Some((_, mut s)) = direct {
        let _ = s.finish();
    }
    Ok(())
}

/// 往直连上写一帧画面（这条连接上还没有画面的流就开一条）
async fn write_direct(
    conn: &Connection,
    direct: &mut Option<(usize, SendStream)>,
    session_id: u64,
    sealed: Bytes,
) -> Result<()> {
    if direct
        .as_ref()
        .is_none_or(|(id, _)| *id != conn.stable_id())
    {
        let s = open_lane(conn, session_id, SessionLane::Video).await?;
        *direct = Some((conn.stable_id(), s));
    }
    let (_, s) = direct.as_mut().expect("opened above");
    tokio::time::timeout(DIRECT_WRITE_TIMEOUT, s.write_chunk(sealed))
        .await
        .context("the direct path stalled")??;
    Ok(())
}

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
        match conn.send_datagram(Bytes::from(d)) {
            Ok(()) => {}
            Err(SendDatagramError::ConnectionLost(e)) if direct.is_none() => return Err(e.into()),
            // 直连断了（直连任务会把路改回中转）、发送缓冲满了：这一包丢了就丢了
            Err(e) => debug!("dropped an audio packet: {e}"),
        }
    }
    Ok(())
}
