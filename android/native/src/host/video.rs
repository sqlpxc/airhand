//! 被控端的画面分发：Kotlin 那边只有一个编码器（手机只有一块屏幕，MediaProjection 也只能建一个虚拟显示器），
//! 编出来的帧发给所有会话。和桌面版的编码管线（`client/src-tauri/src/host/pipeline.rs`）用同样的低延迟办法：
//!
//! * **在途控制**：每个会话单独限制已发送、未确认的字节（[`Flow`]，与桌面版相同）；只要有一个会话能接收就继续编码。
//!   跟不上的会话跳过帧，恢复后从关键帧重新加入；所有会话都堵住时才等待；
//! * **码率**：每个会话各自按网络上的排队和带宽估计（与桌面版相同），共享的编码器用还在接收的会话里最高的那个：
//!   慢的观看者跳帧，不拉低别人的画质；
//! * 新加入的会话从关键帧开始收（[`Out::need_key`]）。
//!
//! Kotlin 的渲染线程每一轮调用 [`VideoHub::tick`]：设置变了（帧率、分辨率上限、编码格式、码率）
//! 或者需要关键帧时返回一个 JSON，见 [`Tick`]。

use airhand_link::flow::{FlowState as Flow, frame_bytes};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use airhand_proto::media::{self, VideoHeader};
use airhand_proto::session::{Codec, QualitySettings};
use bytes::Bytes;
use serde::Serialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::queue::Queue;

/// 每个会话的画面队列（打包好的画面包）。在途控制之下一般只有一两帧；容量和在途帧数的上限一样，
/// 网络慢、写不进流的时候在途的帧也都放得下，不会因为队列满了丢帧、再去要关键帧
pub const QUEUE: usize = airhand_link::flow::MAX_WINDOW as usize;

pub struct VideoHub {
    st: Mutex<State>,
    cv: Condvar,
    events: Arc<Queue<String>>,
}

struct State {
    sessions: HashMap<u64, Out>,
    settings: QualitySettings,
    /// 设置的版本：每次设置变化加 1
    version: u32,
    /// 上次通过 tick 告诉 Kotlin 的设置版本 / 码率
    told_version: u32,
    told_kbps: u32,
    force_key: bool,
    seq: u32,
    /// H.264 / HEVC 的参数集（MediaCodec 单独给出的 codec config），拼在每个关键帧前面
    config: Vec<u8>,
}

struct Out {
    tx: mpsc::Sender<Bytes>,
    flow: Flow,
    settings: QualitySettings,
    /// 还没收到过关键帧（刚加入，或者队列满了丢过帧）：非关键帧不发给它
    need_key: bool,
}

/// [`VideoHub::tick`] 的结果
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tick {
    pub version: u32,
    pub fps: u8,
    pub max_short: u16,
    pub codec: Codec,
    /// 当前目标码率：还在接收的会话里按网络估计得最高的（见模块说明）
    pub kbps: u32,
    /// 需要马上编一个关键帧
    pub key: bool,
}

impl VideoHub {
    pub fn new(events: Arc<Queue<String>>) -> Self {
        let settings = QualitySettings::default();
        Self {
            st: Mutex::new(State {
                sessions: HashMap::new(),
                settings,
                version: 1,
                told_version: 0,
                told_kbps: 0,
                force_key: false,
                seq: 0,
                config: Vec::new(),
            }),
            cv: Condvar::new(),
            events,
        }
    }

    fn report_active(&self, active: bool) {
        info!(active, "video wanted");
        self.events
            .push(json!({ "type": "video", "active": active }).to_string());
    }

    /// 会话开始看画面
    pub fn subscribe(&self, id: u64, tx: mpsc::Sender<Bytes>, settings: QualitySettings) {
        let mut st = self.st.lock().unwrap();
        st.sessions.insert(
            id,
            Out {
                tx,
                flow: Flow::default(),
                settings: settings.sanitized(),
                need_key: true,
            },
        );
        st.merge_settings();
        st.force_key = true;
        let first = st.sessions.len() == 1;
        drop(st);
        if first {
            self.report_active(true);
        }
        self.cv.notify_all();
    }

    #[cfg(test)]
    fn add(&self, id: u64, tx: mpsc::Sender<Bytes>) {
        self.subscribe(id, tx, QualitySettings::default());
    }

    pub fn remove(&self, id: u64) {
        let mut st = self.st.lock().unwrap();
        let removed = st.sessions.remove(&id).is_some();
        st.merge_settings();
        let empty = st.sessions.is_empty();
        drop(st);
        if removed && empty {
            self.report_active(false);
        }
        self.cv.notify_all();
    }

    pub fn ack(&self, id: u64, seq: u32, arrived: Option<u64>) {
        let mut st = self.st.lock().unwrap();
        let frame = st.frame_bytes();
        if let Some(o) = st.sessions.get_mut(&id) {
            o.flow.on_ack(seq, arrived);
            if o.need_key && o.flow.can_send(frame) {
                st.force_key = true;
            }
        }
        drop(st);
        self.cv.notify_all();
    }

    pub fn ack_stalled(&self, id: u64) -> bool {
        self.st
            .lock()
            .unwrap()
            .sessions
            .get(&id)
            .is_some_and(|o| o.flow.ack_stalled())
    }

    /// 只修改该会话的需求；共享编码参数按所有观看者合并，不由最后一次修改覆盖。
    pub fn set_settings(&self, id: u64, s: QualitySettings) {
        let mut st = self.st.lock().unwrap();
        if let Some(o) = st.sessions.get_mut(&id) {
            o.settings = s.sanitized();
            st.merge_settings();
        }
        drop(st);
        self.cv.notify_all();
    }

    pub fn request_keyframe(&self) {
        self.st.lock().unwrap().force_key = true;
    }

    /// 渲染线程每一轮调用：需要 Kotlin 做点什么（设置变了、码率变了、要关键帧）时返回 [`Tick`]
    pub fn tick(&self) -> Option<Tick> {
        let mut st = self.st.lock().unwrap();
        let kbps = st.kbps();
        if st.version == st.told_version && kbps == st.told_kbps && !st.force_key {
            return None;
        }
        st.told_version = st.version;
        st.told_kbps = kbps;
        let key = std::mem::take(&mut st.force_key);
        Some(Tick {
            version: st.version,
            fps: st.settings.fps,
            max_short: st.settings.max_short,
            codec: st.settings.codec,
            kbps,
            key,
        })
    }

    /// 等到至少一个会话有窗口。所有会话都堵住才算一次全局阻塞。
    pub fn wait_window(&self, timeout: Duration) -> bool {
        let mut st = self.st.lock().unwrap();
        // Defensive cleanup as well as the session's RAII guard: a closed
        // transport can never acknowledge any outstanding frames.
        let was_active = !st.sessions.is_empty();
        let before = st.sessions.len();
        st.sessions.retain(|_, o| !o.tx.is_closed());
        if st.sessions.len() != before {
            st.merge_settings();
        }
        if was_active && st.sessions.is_empty() {
            self.report_active(false);
        }
        let (mut st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| !s.window_open())
            .unwrap();
        let open = st.window_open();
        if !open {
            // 编码器要等：每个会话的码率控制都把它算作拥塞的迹象（见 airhand_link::flow）
            for o in st.sessions.values_mut() {
                o.flow.note_blocked();
            }
        }
        open
    }

    /// 编码器的输出。`config`：参数集（不是画面），存起来拼到之后的关键帧前面
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &self,
        data: &[u8],
        key: bool,
        config: bool,
        capture_us: u64,
        codec: Codec,
        width: u16,
        height: u16,
    ) {
        let mut st = self.st.lock().unwrap();
        if config {
            st.config = data.to_vec();
            return;
        }
        if st.sessions.is_empty() {
            return;
        }
        st.seq = st.seq.wrapping_add(1);
        let seq = st.seq;
        let header = VideoHeader {
            seq,
            key,
            codec,
            width,
            height,
            capture_us,
        };
        // 关键帧前面拼上参数集：和画面一起一次拷进包里
        let config: &[u8] = if key && codec != Codec::AV1 && !starts_with_parameter_set(data, codec)
        {
            &st.config
        } else {
            &[]
        };
        let bytes = Bytes::from(media::frame(&header.encode(), &[config, data]));
        let mut want_key = false;
        for (id, o) in st.sessions.iter_mut() {
            if !o.flow.can_send(bytes.len() - 4) {
                // 跟不上：跳过这一帧，之后从关键帧重新开始
                o.flow.note_blocked();
                o.need_key = true;
                continue;
            }
            if o.need_key && !key {
                want_key = true;
                continue;
            }
            match o.tx.try_send(bytes.clone()) {
                Ok(()) => {
                    o.need_key = false;
                    o.flow.on_sent(seq, bytes.len() - 4);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    // 丢了一帧，之后的帧解不出来：从下一个关键帧重新开始
                    debug!(session = id, "video queue full, waiting for a keyframe");
                    o.need_key = true;
                    want_key = true;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
        if want_key {
            st.force_key = true;
        }
    }

    /// 编码器重建了（格式 / 分辨率变了）：旧的参数集作废
    pub fn reset_config(&self) {
        self.st.lock().unwrap().config.clear();
    }
}

/// 关键帧本身是不是已经带着参数集（有的编码器会自己在 IDR 前面重复 SPS/PPS）
fn starts_with_parameter_set(data: &[u8], codec: Codec) -> bool {
    let start = if data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return false;
    };
    let Some(&h) = data.get(start) else {
        return false;
    };
    match codec {
        Codec::H264 => h & 0x1f == 7,                      // SPS
        Codec::H265 => matches!((h >> 1) & 0x3f, 32 | 33), // VPS / SPS
        // AV1 的关键帧自带 Sequence Header；不认识的格式不会走到这里（Kotlin 只编认识的格式）
        Codec::AV1 | Codec::Unknown(_) => true,
    }
}

impl State {
    /// 单编码器：帧率和码率满足前台需求，尺寸服从最严格上限；编码偏好不一致时用共同基线 H.264。
    fn merge_settings(&mut self) {
        let mut all = self.sessions.values().map(|o| o.settings);
        let Some(mut s) = all.next() else { return };
        for other in all {
            s.fps = s.fps.max(other.fps);
            s.bitrate_kbps = s.bitrate_kbps.max(other.bitrate_kbps);
            s.max_short = match (s.max_short, other.max_short) {
                (0, n) | (n, 0) => n,
                (a, b) => a.min(b),
            };
            s.adaptive |= other.adaptive;
            if s.codec != other.codec {
                s.codec = Codec::H264;
            }
        }
        if self.settings != s {
            self.settings = s;
            self.version = self.version.wrapping_add(1);
            info!(?s, "shared quality settings updated");
        }
        // 共享的编码器按合并后的码率编：每个会话都按它估计（码率变了从它重新开始，新加入的会话也是）
        for o in self.sessions.values_mut() {
            o.flow.set_target(s.bitrate_kbps, s.adaptive);
        }
    }

    /// 共享的编码器该用的码率：还在接收的会话里估计得最高的，慢的观看者不能把其它人的画质一起拉低
    /// （它跳帧，恢复后从关键帧重新加入）
    fn kbps(&self) -> u32 {
        let receiving = self.sessions.values().filter(|o| !o.need_key);
        receiving
            .map(|o| o.flow.bitrate_kbps())
            .max()
            .or_else(|| self.sessions.values().map(|o| o.flow.bitrate_kbps()).max())
            .unwrap_or(self.settings.bitrate_kbps)
    }

    /// 平均一帧多大（按编码器现在的码率）
    fn frame_bytes(&self) -> usize {
        frame_bytes(self.kbps(), self.settings.fps)
    }

    fn window_open(&self) -> bool {
        let frame = self.frame_bytes();
        self.sessions.is_empty() || self.sessions.values().any(|o| o.flow.can_send(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> (VideoHub, Arc<Queue<String>>) {
        let events = Arc::new(Queue::new(64));
        (VideoHub::new(events.clone()), events)
    }

    struct Frame {
        seq: u32,
        key: bool,
        data: Vec<u8>,
    }

    /// 线上的画面包：长度前缀 + 头 + 码流
    fn decode(b: &Bytes) -> Frame {
        assert_eq!(
            u32::from_le_bytes(b[..4].try_into().unwrap()) as usize,
            b.len() - 4
        );
        let h = VideoHeader::decode(&b[4..]).unwrap();
        Frame {
            seq: h.seq,
            key: h.key,
            data: b[4 + media::HEADER_LEN..].to_vec(),
        }
    }

    #[test]
    fn new_sessions_start_from_a_keyframe() {
        let (hub, events) = hub();
        let (tx, mut rx) = mpsc::channel(QUEUE);
        hub.add(1, tx);
        assert_eq!(
            events.pop(Duration::ZERO).unwrap(),
            r#"{"active":true,"type":"video"}"#
        );
        // 加入时要求关键帧
        let t = hub.tick().unwrap();
        assert!(t.key);
        assert_eq!(t.codec, Codec::H264);
        assert!(hub.tick().is_none());

        // 参数集 + 非关键帧：都不发
        hub.push(&[0, 0, 0, 1, 0x67, 1, 2], false, true, 0, Codec::H264, 8, 8);
        hub.push(&[0, 0, 0, 1, 0x41, 9], false, false, 1, Codec::H264, 8, 8);
        assert!(rx.try_recv().is_err());
        // 关键帧：前面拼上参数集
        hub.push(&[0, 0, 0, 1, 0x65, 9], true, false, 2, Codec::H264, 8, 8);
        let v = decode(&rx.try_recv().unwrap());
        assert!(v.key);
        assert_eq!(v.data, [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65, 9]);
        // 之后的非关键帧照常发
        hub.push(&[0, 0, 0, 1, 0x41, 9], false, false, 3, Codec::H264, 8, 8);
        let v2 = decode(&rx.try_recv().unwrap());
        assert_eq!(v2.seq, v.seq + 1);
        // 自带参数集的关键帧不重复拼
        hub.push(
            &[0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x65],
            true,
            false,
            4,
            Codec::H264,
            8,
            8,
        );
        assert_eq!(decode(&rx.try_recv().unwrap()).data.len(), 12);

        hub.remove(1);
        assert_eq!(
            events.pop(Duration::ZERO).unwrap(),
            r#"{"active":false,"type":"video"}"#
        );
    }

    /// 一帧占在途字节上限的 2/5：发了两帧之后就放不下平均大小的下一帧了
    fn big() -> Vec<u8> {
        vec![0; Flow::default().window_bytes() * 2 / 5]
    }

    #[test]
    fn inflight_window_blocks_until_acked() {
        let (hub, _) = hub();
        let (tx, mut rx) = mpsc::channel(QUEUE);
        hub.add(7, tx);
        hub.push(&big(), true, false, 0, Codec::AV1, 8, 8);
        assert!(hub.wait_window(Duration::ZERO));
        hub.push(&big(), false, false, 0, Codec::AV1, 8, 8);
        assert!(!hub.wait_window(Duration::from_millis(5)));
        // 放不下的帧不发
        hub.push(&big(), false, false, 0, Codec::AV1, 8, 8);
        let seqs: Vec<u32> = (0..2)
            .map(|_| decode(&rx.try_recv().unwrap()).seq)
            .collect();
        assert!(rx.try_recv().is_err());
        let inflight = || hub.st.lock().unwrap().sessions[&7].flow.inflight();
        // 累积确认；旧的 / 没发过的序号不影响
        hub.ack(7, seqs[1] + 100, None);
        hub.ack(7, seqs[0], None);
        hub.ack(7, seqs[0], None);
        assert_eq!(inflight(), 1);
        hub.ack(7, seqs[1], None);
        assert_eq!(inflight(), 0);
        assert!(hub.wait_window(Duration::ZERO));
    }

    #[test]
    fn full_queue_drops_to_the_next_keyframe() {
        let (hub, _) = hub();
        let (tx, mut rx) = mpsc::channel(1);
        hub.add(1, tx);
        let _ = hub.tick();
        hub.push(&[1], true, false, 0, Codec::AV1, 8, 8);
        hub.push(&[2], false, false, 0, Codec::AV1, 8, 8); // 队列满，丢掉
        assert!(hub.tick().unwrap().key);
        rx.try_recv().unwrap();
        hub.push(&[3], false, false, 0, Codec::AV1, 8, 8); // 等关键帧，不发
        assert!(rx.try_recv().is_err());
        hub.push(&[4], true, false, 0, Codec::AV1, 8, 8);
        assert_eq!(decode(&rx.try_recv().unwrap()).data, [4]);
    }

    #[test]
    fn closed_subscriber_cannot_block_new_session() {
        let (hub, _) = hub();
        let (tx, rx) = mpsc::channel(QUEUE);
        hub.add(1, tx);
        for i in 0..3 {
            hub.push(&big(), i == 0, false, 0, Codec::AV1, 8, 8);
        }
        assert!(!hub.wait_window(Duration::ZERO));
        drop(rx);
        let (tx, _rx) = mpsc::channel(QUEUE);
        hub.add(2, tx);
        assert!(hub.wait_window(Duration::ZERO));
    }

    #[test]
    fn settings_changes_are_reported_once() {
        let (hub, _) = hub();
        let (tx, _rx) = mpsc::channel(QUEUE);
        hub.add(1, tx);
        let first = hub.tick().unwrap();
        assert_eq!(first.kbps, 4000);
        assert!(hub.tick().is_none());
        hub.set_settings(
            1,
            QualitySettings {
                fps: 60,
                bitrate_kbps: 8000,
                max_short: 720,
                adaptive: false,
                codec: Codec::H265,
            },
        );
        let t = hub.tick().unwrap();
        assert_eq!(
            (t.fps, t.kbps, t.max_short, t.codec),
            (60, 8000, 720, Codec::H265)
        );
        assert!(t.version > first.version);
        assert!(hub.tick().is_none());
    }

    #[test]
    fn slow_viewer_does_not_block_fast_viewer_and_recovers_at_a_keyframe() {
        let (hub, _) = hub();
        let (fast, mut fast_rx) = mpsc::channel(QUEUE);
        let (slow, mut slow_rx) = mpsc::channel(QUEUE);
        hub.add(1, fast);
        hub.add(2, slow);
        let _ = hub.tick();
        for i in 0..12 {
            assert!(hub.wait_window(Duration::ZERO));
            hub.push(&big(), i == 0, false, i, Codec::H264, 8, 8);
            hub.ack(1, decode(&fast_rx.try_recv().unwrap()).seq, None);
        }
        let mut last = 0;
        for _ in 0..2 {
            last = decode(&slow_rx.try_recv().unwrap()).seq;
        }
        assert!(
            slow_rx.try_recv().is_err(),
            "slow viewer stays within its window"
        );
        assert!(
            hub.tick().is_none(),
            "a stalled viewer must not cause a keyframe storm"
        );
        hub.ack(2, last, None);
        assert!(hub.tick().unwrap().key);
        hub.push(&[2], false, false, 12, Codec::H264, 8, 8);
        assert!(
            slow_rx.try_recv().is_err(),
            "delta frames after a gap are not decodable"
        );
        hub.push(&[3], true, false, 13, Codec::H264, 8, 8);
        assert!(decode(&slow_rx.try_recv().unwrap()).key);
    }

    #[test]
    fn viewers_keep_their_settings_and_share_a_decodable_format() {
        let (hub, _) = hub();
        let (a, _ar) = mpsc::channel(QUEUE);
        let (b, _br) = mpsc::channel(QUEUE);
        let foreground = QualitySettings {
            fps: 60,
            codec: Codec::AV1,
            max_short: 1080,
            ..Default::default()
        };
        hub.subscribe(1, a, foreground);
        hub.subscribe(
            2,
            b,
            QualitySettings {
                fps: 5,
                codec: Codec::H264,
                max_short: 720,
                ..Default::default()
            },
        );
        let settings = hub.tick().unwrap();
        assert_eq!(
            (settings.fps, settings.codec, settings.max_short),
            (60, Codec::H264, 720)
        );
        hub.set_settings(
            2,
            QualitySettings {
                fps: 5,
                codec: Codec::H265,
                max_short: 720,
                ..Default::default()
            },
        );
        assert_eq!(hub.st.lock().unwrap().settings.codec, Codec::H264);
        hub.remove(2);
        assert_eq!(hub.st.lock().unwrap().settings, foreground);
    }

    #[test]
    fn parameter_set_detection() {
        assert!(starts_with_parameter_set(&[0, 0, 1, 0x67], Codec::H264));
        assert!(!starts_with_parameter_set(&[0, 0, 1, 0x65], Codec::H264));
        assert!(starts_with_parameter_set(
            &[0, 0, 0, 1, 0x40, 1],
            Codec::H265
        )); // VPS
        assert!(!starts_with_parameter_set(
            &[0, 0, 0, 1, 0x26, 1],
            Codec::H265
        )); // IDR
        assert!(!starts_with_parameter_set(&[1, 2, 3], Codec::H264));
    }
}
