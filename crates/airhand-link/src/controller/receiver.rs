//! 控制端收被控端的东西：控制、大块、画面三条通道，声音数据报和控制数据报，解密之后交出来（[`HostReceiver`]）。
//! 被控端的探测（`HostDatagram::Probe`）在这里就从原来的那条路弹回去，不交出来。

use airhand_proto::SessionLane;
use airhand_proto::datagram;
use airhand_proto::media::{AudioHeader, Kind};
use airhand_proto::session::{ControllerDatagram, HostDatagram, HostMsg};
use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::mpsc;
use tracing::debug;

use crate::lanes::Via;
use crate::merge::Merge;
use crate::paths::DatagramSender;
use crate::secure::Opener;
use crate::transport::{next_msg, open_datagram};

/// 被控端发来的一样东西（已经解密）
#[derive(Debug)]
pub enum FromHost {
    /// 控制通道、大块通道上的消息
    Control(HostMsg),
    /// 回应 Ping（`HostDatagram::Pong`）：`t` 是 Ping 里的时刻
    Pong(u64),
    /// 画面包（`airhand_proto::media` 格式：头 + 码流），原样交给界面。`arrived`：开始从网络上到达的时刻
    /// （控制端的时钟，微秒，见 [`crate::wire::read_packet_timed`]），确认时带给被控端（见 `ControllerDatagram::Ack`）。
    /// `direct`：从直连来的；`gap`：前面有帧没等到（断在了直连上），要一个关键帧（见 [`crate::merge`]）
    Video {
        packet: Vec<u8>,
        arrived: u64,
        direct: bool,
        gap: bool,
    },
    /// 声音包（`airhand_proto::media` 格式：头 + Opus 包），原样交给界面
    Audio(Vec<u8>),
}

/// 控制端收被控端的三条通道、声音数据报和控制数据报。
///
/// 每条通道的流由 [`Merge`] 的读任务读（读任务活到流结束，取消 [`HostReceiver::recv`] 不会切断半帧），
/// 这里只从几个互相独立的信箱里取：画面、剪贴板再多也挡不住控制消息和 Pong
pub struct HostReceiver {
    control: Merge,
    bulk: Merge,
    video: Merge,
    /// 声音数据报（整个数据报，还是密文）和它的解密端
    audio: (mpsc::Receiver<Bytes>, Opener),
    /// 控制数据报：从哪条路来、整个数据报，和它的解密端
    datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
    /// 弹回探测
    echo: DatagramSender,
    bulk_open: bool,
    video_open: bool,
    audio_open: bool,
    datagrams_open: bool,
    audio_order: AudioOrder,
}

impl HostReceiver {
    /// `control` / `bulk` / `video`：三条通道的接收端；`audio` / `datagrams`：发给这个会话的声音、控制数据报
    /// 和它们的解密端；`echo`：本端的控制数据报，探测从收到它的那条路弹回去
    pub(crate) fn new(
        control: Merge,
        bulk: Merge,
        video: Merge,
        audio: (mpsc::Receiver<Bytes>, Opener),
        datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
        echo: DatagramSender,
    ) -> Self {
        Self {
            control,
            bulk,
            video,
            audio,
            datagrams,
            echo,
            bulk_open: true,
            video_open: true,
            audio_open: true,
            datagrams_open: true,
            audio_order: AudioOrder::default(),
        }
    }

    /// 下一样东西。控制消息优先；控制通道结束了就是会话结束了（其它通道先结束不算）。可以放在 `select!` 里
    pub async fn recv(&mut self) -> Result<FromHost> {
        loop {
            tokio::select! {
                biased;
                m = next_msg::<HostMsg>(&mut self.control, SessionLane::Control) => {
                    return Ok(FromHost::Control(m?.context("control stream ended")?));
                }
                d = self.datagrams.0.recv(), if self.datagrams_open => match d {
                    Some((via, d)) => match open_datagram(&mut self.datagrams.1, &d) {
                        Some(HostDatagram::Pong { t }) => return Ok(FromHost::Pong(t)),
                        Some(HostDatagram::Probe { t }) => {
                            self.echo.send_on(via, &ControllerDatagram::Echo { t });
                        }
                        None => {}
                    },
                    None => self.datagrams_open = false,
                },
                m = next_msg::<HostMsg>(&mut self.bulk, SessionLane::Bulk), if self.bulk_open => match m? {
                    Some(m) => return Ok(FromHost::Control(m)),
                    None => self.bulk_open = false,
                },
                d = self.audio.0.recv(), if self.audio_open => match d {
                    Some(d) => {
                        if let Some(p) = self.open_audio(&d) {
                            return Ok(FromHost::Audio(p));
                        }
                    }
                    None => self.audio_open = false,
                },
                f = self.video.next(), if self.video_open => match f? {
                    Some(f) => {
                        return Ok(FromHost::Video {
                            packet: f.data,
                            arrived: f.arrived,
                            direct: f.direct,
                            gap: f.gap,
                        });
                    }
                    None => self.video_open = false,
                },
            }
        }
    }

    /// 声音数据报：解开、检查是声音包、不是迟到的。解不开的（被篡改、重放的）、迟到的都丢掉这一包
    fn open_audio(&mut self, d: &[u8]) -> Option<Vec<u8>> {
        let mut p = d.get(datagram::HEADER..)?.to_vec();
        if let Err(e) = self.audio.1.open(&mut p) {
            debug!("dropped an audio packet: {e:#}");
            return None;
        }
        (Kind::of(&p) == Some(Kind::Audio) && self.audio_order.accept(&p)).then_some(p)
    }
}

/// 数据报可能乱序：采集时刻在一个会话内不倒退，即使采集器重启、序号从头算也一样。
/// `start` 只描述静音段边界，不能让迟到的首包重置顺序。相同采集时刻用序号消歧（允许绕回）。
#[derive(Default)]
struct AudioOrder {
    last: Option<AudioHeader>,
}

impl AudioOrder {
    fn accept(&mut self, packet: &[u8]) -> bool {
        let Some(h) = AudioHeader::decode(packet) else {
            return false;
        };
        if let Some(last) = self.last
            && (h.capture_us < last.capture_us
                || (h.capture_us == last.capture_us && h.seq.wrapping_sub(last.seq) as i32 <= 0))
        {
            return false;
        }
        self.last = Some(h);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(seq: u32, start: bool, capture_us: u64) -> Vec<u8> {
        AudioHeader {
            seq,
            start,
            capture_us,
        }
        .encode()
        .to_vec()
    }

    #[test]
    fn late_audio_cannot_rewind_playback_even_when_it_starts_a_segment() {
        let mut o = AudioOrder::default();
        // 首包丢了或迟到：先收到了第三包。
        assert!(o.accept(&audio(3, false, 60_000)));
        assert!(!o.accept(&audio(1, true, 20_000)));
        assert!(!o.accept(&audio(2, false, 40_000)));
        assert!(!o.accept(&audio(3, false, 60_000)));
        assert!(o.accept(&audio(4, false, 80_000)));
        // 采集器重启，首包也可能丢失：用采集时钟分辨新流与迟到的旧流。
        assert!(o.accept(&audio(2, false, 1_000_000)));
        assert!(!o.accept(&audio(1, true, 980_000)));
        assert!(!o.accept(&audio(5, false, 100_000)));
        assert!(o.accept(&audio(3, false, 1_020_000)));
        // 相同采集时刻（时钟精度不够）仍能按序号丢重复、识别绕回。
        let mut o = AudioOrder::default();
        assert!(o.accept(&audio(u32::MAX, true, 0)));
        assert!(o.accept(&audio(0, false, 0)));
        assert!(!o.accept(&audio(u32::MAX, true, 0)));
        assert!(!o.accept(&audio(0, true, 0)));
        assert!(!o.accept(&[]));
    }
}
