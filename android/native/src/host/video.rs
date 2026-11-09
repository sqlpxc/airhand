//! 被控端的画面分发：Kotlin 那边只有一个编码器（手机只有一块屏幕，MediaProjection 也只能建一个虚拟显示器），
//! 编出来的帧发给所有会话。和桌面版的编码管线（`client/src-tauri/src/host/pipeline.rs`）用同样的低延迟办法：
//!
//! * **在途帧控制**：每个会话记录已发送、未确认的帧，超过窗口（≈ 基础 RTT × 帧率 + 2）时
//!   Kotlin 的渲染线程在 [`VideoHub::wait_window`] 里等，期间新画面覆盖旧画面，不会排队；
//! * **自适应码率**：按阻塞次数和 RTT 膨胀程度下调 / 恢复码率（[`Abr`]，与桌面版相同）；
//! * 新加入的会话从关键帧开始收（[`Out::need_key`]）。
//!
//! Kotlin 的渲染线程每一轮调用 [`VideoHub::tick`]：设置变了（帧率、分辨率上限、编码格式、码率）
//! 或者需要关键帧时返回一个 JSON，见 [`Tick`]。

use airhand_link::flow::{Abr, FlowState as Flow};
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

/// 每个会话的画面队列（打包好的画面包）。在途帧控制之下一般只有一两帧；容量和在途帧窗口的上限一样，
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
    abr: Abr,
    /// H.264 / HEVC 的参数集（MediaCodec 单独给出的 codec config），拼在每个关键帧前面
    config: Vec<u8>,
}

struct Out {
    tx: mpsc::Sender<Bytes>,
    flow: Flow,
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
    /// 当前目标码率（自适应码率调整后的）
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
                abr: Abr::new(settings.bitrate_kbps),
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
    pub fn add(&self, id: u64, tx: mpsc::Sender<Bytes>) {
        let mut st = self.st.lock().unwrap();
        st.sessions.insert(
            id,
            Out {
                tx,
                flow: Flow::default(),
                need_key: true,
            },
        );
        st.force_key = true;
        let first = st.sessions.len() == 1;
        drop(st);
        if first {
            self.report_active(true);
        }
        self.cv.notify_all();
    }

    pub fn remove(&self, id: u64) {
        let mut st = self.st.lock().unwrap();
        let removed = st.sessions.remove(&id).is_some();
        let empty = st.sessions.is_empty();
        drop(st);
        if removed && empty {
            self.report_active(false);
        }
        self.cv.notify_all();
    }

    pub fn ack(&self, id: u64, seq: u32) {
        let mut st = self.st.lock().unwrap();
        if let Some(o) = st.sessions.get_mut(&id) {
            o.flow.on_ack(seq);
        }
        drop(st);
        self.cv.notify_all();
    }

    /// 控制端改了画质。多个控制端时以最后一次修改为准
    pub fn set_settings(&self, s: QualitySettings) {
        let s = s.sanitized();
        let mut st = self.st.lock().unwrap();
        if st.settings != s {
            st.settings = s;
            st.version = st.version.wrapping_add(1);
            st.abr.reset(s.bitrate_kbps);
            info!(?s, "quality settings updated");
        }
    }

    pub fn request_keyframe(&self) {
        self.st.lock().unwrap().force_key = true;
    }

    /// 渲染线程每一轮调用：需要 Kotlin 做点什么（设置变了、码率变了、要关键帧）时返回 [`Tick`]
    pub fn tick(&self) -> Option<Tick> {
        let mut st = self.st.lock().unwrap();
        if st.settings.adaptive {
            let (srtt, min_rtt) = st.worst_rtt();
            st.abr.tick(srtt, min_rtt);
        } else {
            st.abr.current = st.settings.bitrate_kbps;
        }
        let kbps = st.abr.current;
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

    /// 等到所有会话的在途帧都低于窗口。返回 false 表示超时（算一次阻塞，自适应码率据此下调）
    pub fn wait_window(&self, timeout: Duration) -> bool {
        let mut st = self.st.lock().unwrap();
        // Defensive cleanup as well as the session's RAII guard: a closed
        // transport can never acknowledge any outstanding frames.
        let was_active = !st.sessions.is_empty();
        st.sessions.retain(|_, o| !o.tx.is_closed());
        if was_active && st.sessions.is_empty() {
            self.report_active(false);
        }
        let fps = st.settings.fps;
        let (mut st, _) = self
            .cv
            .wait_timeout_while(st, timeout, |s| !s.window_open(fps))
            .unwrap();
        let open = st.window_open(fps);
        if !open {
            st.abr.blocked += 1;
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
            if o.need_key && !key {
                want_key = true;
                continue;
            }
            match o.tx.try_send(bytes.clone()) {
                Ok(()) => {
                    o.need_key = false;
                    o.flow.on_sent(seq);
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
        Codec::AV1 => true,
    }
}

impl State {
    fn window_open(&self, fps: u8) -> bool {
        self.sessions
            .values()
            .all(|o| o.need_key || o.flow.inflight() < o.flow.window(fps))
    }

    /// 最慢的会话的 (平滑 RTT, 基础 RTT)
    fn worst_rtt(&self) -> (f64, f64) {
        self.sessions
            .values()
            .map(|o| o.flow.rtt())
            .fold((0.0, 0.0), |acc, r| if r.0 > acc.0 { r } else { acc })
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

    #[test]
    fn inflight_window_blocks_until_acked() {
        let (hub, _) = hub();
        let (tx, mut rx) = mpsc::channel(QUEUE);
        hub.add(7, tx);
        hub.push(&[1], true, false, 0, Codec::AV1, 8, 8);
        assert!(hub.wait_window(Duration::ZERO));
        hub.push(&[2], false, false, 0, Codec::AV1, 8, 8);
        hub.push(&[3], false, false, 0, Codec::AV1, 8, 8);
        // 还没有 RTT 样本时窗口是 3 帧
        assert!(!hub.wait_window(Duration::from_millis(5)));
        let seqs: Vec<u32> = (0..3)
            .map(|_| decode(&rx.try_recv().unwrap()).seq)
            .collect();
        hub.ack(7, seqs[1]);
        assert!(hub.wait_window(Duration::ZERO));
        // 累积确认；旧的 / 没发过的序号不影响
        hub.ack(7, seqs[0]);
        hub.ack(7, seqs[2] + 100);
        hub.ack(7, seqs[2]);
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
            hub.push(&[1], i == 0, false, 0, Codec::AV1, 8, 8);
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
        let first = hub.tick().unwrap();
        assert_eq!(first.kbps, 4000);
        assert!(hub.tick().is_none());
        hub.set_settings(QualitySettings {
            fps: 60,
            bitrate_kbps: 8000,
            max_short: 720,
            adaptive: false,
            codec: Codec::H265,
        });
        let t = hub.tick().unwrap();
        assert_eq!(
            (t.fps, t.kbps, t.max_short, t.codec),
            (60, 8000, 720, Codec::H265)
        );
        assert!(t.version > first.version);
        assert!(hub.tick().is_none());
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
