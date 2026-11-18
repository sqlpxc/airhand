//! 控制端解密被控端的控制、大块、视频帧流和音频数据报。
//! 控制数据报，解密之后交出来（[`HostReceiver`]）。被控端的探测（`HostDatagram::Probe`）在这里就从原来的那条路弹回去，
//! 不交出来。
//!
//! 画面的每一路（多屏同时显示时每个显示器一路，见 `airhand_proto::session` 的"多屏"）一个 [`Merge`]，各自在一个任务里
//! 合并、等缺着的帧，交到同一个信箱：一路等着重传、一路来得很快，谁也不挡谁。

use std::collections::VecDeque;

use airhand_proto::SessionLane;
use airhand_proto::datagram;
use airhand_proto::session::{ControllerDatagram, HostDatagram, HostMsg};
use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::audio::Unbundler;
use crate::lanes::Via;
use crate::merge::{Merge, Video, VideoStats};
use crate::paths::DatagramSender;
use crate::secure::Opener;
use crate::tasks::OwnedTask;
use crate::transport::{next_msg, open_datagram};

/// 被控端发来的一样东西（已经解密）
#[derive(Debug)]
pub enum FromHost {
    /// 控制通道、大块通道上的消息
    Control(HostMsg),
    /// 回应 Ping（`HostDatagram::Pong`）：`t` 是 Ping 里的时刻
    Pong(u64),
    /// 第 `stream` 路的画面包（`airhand_proto::media` 格式：头 + 码流），解得了，原样交给界面。`arrived`：开始从网络上
    /// 到达的时刻（控制端的时钟，微秒，见 [`crate::wire::read_packet_timed`]），确认时带给被控端（见 `ControllerDatagram::Ack`）。
    /// `direct`：从直连来的
    Video {
        stream: u8,
        packet: Vec<u8>,
        arrived: u64,
        direct: bool,
    },
    /// 第几路画面上别的事：解不了的帧、等不到的帧、要关键帧（见 [`crate::merge::Video`]）
    VideoEvent(u8, Video),
    /// 声音包（`airhand_proto::media` 格式：头 + Opus 包），原样交给界面。一个数据报里带着之前几包的副本，
    /// 拆开之后只交出没收过的（丢了的包这样补回来，见 `crate::audio`），一个一个、按采集先后
    Audio(Vec<u8>),
}

/// 控制端收被控端的各条通道、声音数据报和控制数据报。
///
/// 每条通道的流由 [`Merge`] 的读任务读（读任务活到流结束，取消 [`HostReceiver::recv`] 不会切断半帧），
/// 这里只从几个互相独立的信箱里取：画面、剪贴板再多也挡不住控制消息和 Pong
pub struct HostReceiver {
    control: Merge,
    bulk: Merge,
    /// 画面的每一路
    video: VideoLanes,
    /// 声音数据报（整个数据报，还是密文）和它的解密端
    audio: (mpsc::Receiver<Bytes>, Opener),
    /// 控制数据报：从哪条路来、整个数据报，和它的解密端
    datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
    /// 弹回探测
    echo: DatagramSender,
    bulk_open: bool,
    audio_open: bool,
    datagrams_open: bool,
    unbundler: Unbundler,
    /// 从声音数据报里拆出来、还没交出去的声音包
    audio_out: VecDeque<Vec<u8>>,
}

impl HostReceiver {
    /// `control` / `bulk`：两条通道的接收端；`video`：画面的每一路（第 `i` 项是第 `i` 路）；
    /// `audio` / `datagrams`：发给这个会话的声音、控制数据报和它们的解密端；
    /// `echo`：本端的控制数据报，探测从收到它的那条路弹回去
    pub(crate) fn new(
        control: Merge,
        bulk: Merge,
        video: Vec<Merge>,
        audio: (mpsc::Receiver<Bytes>, Opener),
        datagrams: (mpsc::Receiver<(Via, Bytes)>, Opener),
        echo: DatagramSender,
    ) -> Self {
        Self {
            control,
            bulk,
            video: VideoLanes::start(video),
            audio,
            datagrams,
            echo,
            bulk_open: true,
            audio_open: true,
            datagrams_open: true,
            unbundler: Unbundler::default(),
            audio_out: VecDeque::new(),
        }
    }

    /// 往返时延（毫秒，Ping / Pong 量的）：画面（每一路）据此决定缺着的帧等多久（见 [`crate::merge`]）
    pub fn set_rtt(&mut self, rtt_ms: f64) {
        self.video.rtt.send_replace(rtt_ms);
    }

    /// 画面每一路收得怎么样（第 `i` 项是第 `i` 路）：定时取走、报告给被控端（`ControllerDatagram::Report`，
    /// 见 [`crate::merge`] 的"报告"）
    pub fn video_stats(&self) -> Vec<VideoStats> {
        self.video.stats.clone()
    }

    /// 下一样东西。控制消息优先；控制通道结束了就是会话结束了（其它通道先结束不算）。可以放在 `select!` 里
    pub async fn recv(&mut self) -> Result<FromHost> {
        loop {
            // 一个声音数据报里补回来的几包（最多几个）先交完
            if let Some(p) = self.audio_out.pop_front() {
                return Ok(FromHost::Audio(p));
            }
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
                    Some(d) => self.open_audio(&d),
                    None => self.audio_open = false,
                },
                v = self.video.rx.recv(), if self.video.open > 0 => match v {
                    Some((stream, f)) => match f? {
                        Some(Video::Frame(f)) => {
                            return Ok(FromHost::Video {
                                stream,
                                packet: f.data,
                                arrived: f.arrived,
                                direct: f.direct,
                            });
                        }
                        Some(other) => return Ok(FromHost::VideoEvent(stream, other)),
                        None => self.video.open -= 1,
                    },
                    None => self.video.open = 0,
                },
            }
        }
    }

    /// 声音数据报：解开，拆出里面没收过的声音包（放进 `audio_out`）。解不开的（被篡改、重放的）整个丢掉
    fn open_audio(&mut self, d: &[u8]) {
        let Some(sealed) = d.get(datagram::HEADER..) else {
            return;
        };
        let mut plain = sealed.to_vec();
        if let Err(e) = self.audio.1.open(&mut plain) {
            debug!("dropped an audio datagram: {e:#}");
            return;
        }
        let recovered = self.unbundler.unbundle(&plain, &mut self.audio_out);
        if recovered > 0 {
            debug!(
                recovered,
                "recovered audio packets that didn't arrive on their own"
            );
        }
    }
}

/// 一路画面交出来的：第几路、[`Merge::next_video`] 的结果（`Ok(None)`：这一路结束了）
type VideoOut = (u8, Result<Option<Video>>);

/// 画面的每一路一个任务（见模块说明），交到同一个信箱
struct VideoLanes {
    rx: mpsc::Receiver<VideoOut>,
    /// 往返时延告诉每一路
    rtt: watch::Sender<f64>,
    stats: Vec<VideoStats>,
    /// 还没结束的路
    open: usize,
    _tasks: Vec<OwnedTask<()>>,
}

impl VideoLanes {
    /// 每一路放着没交出去的最多这么多：合并本来就替它放着合并好的帧，这里只是转一手
    const QUEUE: usize = 2;

    fn start(video: Vec<Merge>) -> Self {
        let (tx, rx) = mpsc::channel(Self::QUEUE * video.len().max(1));
        let (rtt, _) = watch::channel(0.0);
        let stats = video.iter().map(Merge::stats).collect();
        let open = video.len();
        let tasks = video
            .into_iter()
            .enumerate()
            .map(|(stream, merge)| {
                OwnedTask(tokio::spawn(pump(
                    stream as u8,
                    merge,
                    tx.clone(),
                    rtt.subscribe(),
                )))
            })
            .collect();
        Self {
            rx,
            rtt,
            stats,
            open,
            _tasks: tasks,
        }
    }
}

/// 一路画面：合并好的交出去，直到这一路结束（或者出错）。往返时延变了就告诉合并（缺着的帧等多久）
async fn pump(
    stream: u8,
    mut merge: Merge,
    tx: mpsc::Sender<VideoOut>,
    mut rtt: watch::Receiver<f64>,
) {
    loop {
        tokio::select! {
            changed = rtt.changed() => {
                if changed.is_err() {
                    return;
                }
                merge.set_rtt(*rtt.borrow_and_update());
            }
            v = merge.next_video() => {
                let end = !matches!(v, Ok(Some(_)));
                if tx.send((stream, v)).await.is_err() || end {
                    return;
                }
            }
        }
    }
}
