//! 被控端的画面分发：Kotlin 那边只有一个编码器（手机只有一块屏幕，MediaProjection 也只能建一个虚拟显示器），
//! 编出来的帧发给所有会话。和桌面版的编码管线（`client/src-tauri/src/host/pipeline.rs`）用同样的低延迟办法：
//!
//! * **在途帧控制**：每个会话单独限制已发送、未确认的帧；只要有一个会话能接收就继续编码。
//!   跟不上的会话跳过帧，恢复后从关键帧重新加入；所有会话都堵住时才等待；
//! * **自适应码率**：按阻塞次数和 RTT 膨胀程度下调 / 恢复码率（[`Abr`]，与桌面版相同）；
//! * 新加入的会话从关键帧开始收（[`Out::need_key`]）。
//!
//! Kotlin 的渲染线程每一轮调用 [`VideoHub::tick`]：设置变了（帧率、分辨率上限、编码格式、码率）
//! 或者需要关键帧时返回一个 JSON，见 [`Tick`]。

use airhand_link::flow::{Abr, FlowState as Flow};
use std::collections::{HashMap, VecDeque};
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
    host_codecs: Vec<Codec>,
    source: (u32, u32),
    selections: VecDeque<(u32, HashMap<u64, airhand_link::video::SelectedVideo>)>,
}

struct Out {
    tx: mpsc::Sender<Bytes>,
    flow: Flow,
    settings: QualitySettings,
    request_id: u32,
    /// 还没收到过关键帧（刚加入，或者队列满了丢过帧）：非关键帧不发给它
    need_key: bool,
    model: Option<Arc<airhand_link::video::VideoSession>>,
    failed: bool,
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
                host_codecs: vec![Codec::H264, Codec::H265, Codec::AV1],
                source: (0, 0),
                selections: VecDeque::new(),
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
    #[cfg(test)]
    pub fn subscribe(&self, id: u64, tx: mpsc::Sender<Bytes>, settings: QualitySettings) {
        self.subscribe_model(id, tx, settings, None).unwrap();
    }

    fn subscribe_model(
        &self,
        id: u64,
        tx: mpsc::Sender<Bytes>,
        settings: QualitySettings,
        model: Option<Arc<airhand_link::video::VideoSession>>,
    ) -> anyhow::Result<()> {
        let mut st = self.st.lock().unwrap();
        let allowed = model.as_ref().and_then(|m| m.receiver());
        anyhow::ensure!(
            st.host_codecs.iter().any(|c| {
                allowed.as_ref().is_none_or(|r| r.capability(*c).is_some())
                    && st.sessions.values().all(|o| {
                        o.model
                            .as_ref()
                            .and_then(|m| m.receiver())
                            .is_none_or(|r| r.capability(*c).is_some())
                    })
            }),
            "no common codec for the shared Android encoder"
        );
        st.sessions.insert(
            id,
            Out {
                tx,
                flow: Flow::default(),
                settings: settings.sanitized(),
                request_id: 0,
                need_key: true,
                model,
                failed: false,
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
        Ok(())
    }

    pub fn source(&self, width: u32, height: u32, codecs: Vec<Codec>) {
        let mut st = self.st.lock().unwrap();
        st.source = (width, height);
        st.host_codecs = codecs;
        st.merge_settings();
    }

    pub fn subscribe_video(
        &self,
        id: u64,
        tx: mpsc::Sender<Bytes>,
        settings: QualitySettings,
        model: Arc<airhand_link::video::VideoSession>,
    ) -> anyhow::Result<()> {
        self.subscribe_model(id, tx, settings, Some(model))
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

    pub fn ack(&self, id: u64, seq: u32) {
        let mut st = self.st.lock().unwrap();
        let fps = st.settings.fps;
        if let Some(o) = st.sessions.get_mut(&id) {
            o.flow.on_ack(seq);
            if o.need_key && o.flow.can_send(fps, 1) {
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
            .is_some_and(|o| o.failed || o.flow.ack_stalled())
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
        if st.settings.adaptive {
            let (srtt, min_rtt) = st.available_rtt();
            st.abr.tick(srtt, min_rtt);
        } else {
            st.abr.current = st.settings.bitrate_kbps;
        }
        let kbps = st.abr.current;
        if st.version == st.told_version && kbps == st.told_kbps && !st.force_key {
            return None;
        }
        st.told_version = st.version;
        st.remember_selection();
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
            st.abr.blocked += 1;
        }
        open
    }

    /// 编码器的输出。`config`：参数集（不是画面），存起来拼到之后的关键帧前面
    #[cfg(test)]
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
        let (fps, kbps, version) = {
            let mut st = self.st.lock().unwrap();
            st.remember_selection();
            (st.settings.fps, st.abr.current, st.version)
        };
        self.push_with_parameters(
            data, key, config, capture_us, codec, width, height, fps, kbps, version,
        );
    }

    /// Parameters describe the encoder that produced this packet, not a pending request.
    #[allow(clippy::too_many_arguments)]
    pub fn push_with_parameters(
        &self,
        data: &[u8],
        key: bool,
        config: bool,
        capture_us: u64,
        codec: Codec,
        width: u16,
        height: u16,
        encoder_fps: u8,
        encoder_kbps: u32,
        selection_version: u32,
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
        let config = config.to_vec();
        let base_size = media::HEADER_LEN + config.len() + data.len();
        if base_size + media::VIDEO_CONTEXT_LEN - media::HEADER_LEN > media::MAX_VIDEO_PACKET {
            return;
        }
        let mut want_key = false;
        let fps = st.settings.fps;
        let state = &mut *st;
        let selections = state
            .selections
            .iter()
            .find(|(v, _)| *v == selection_version)
            .map(|(_, s)| s);
        for (id, o) in state.sessions.iter_mut() {
            if o.failed {
                continue;
            }
            let size = base_size
                + if o.model.as_ref().is_some_and(|m| m.enabled()) {
                    media::VIDEO_CONTEXT_LEN - media::HEADER_LEN
                } else {
                    0
                };
            if !o.flow.can_send(fps, size) {
                o.need_key = true;
                continue;
            }
            if o.need_key && !key {
                want_key = true;
                continue;
            }
            let encoded_header = match &o.model {
                Some(model) => {
                    let Some(mut selected) = selections.as_ref().and_then(|s| s.get(id)).copied()
                    else {
                        o.need_key = true;
                        want_key = true;
                        continue;
                    };
                    selected.settings.fps = encoder_fps;
                    match model.header(&header, selected, encoder_kbps) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!(session = id, "video contract rejected output: {e:#}");
                            o.failed = true;
                            continue;
                        }
                    }
                }
                None => header.encode().to_vec(),
            };
            let bytes = Bytes::from(media::frame(&encoded_header, &[&config, data]));
            match o.tx.try_send(bytes) {
                Ok(()) => {
                    o.need_key = false;
                    o.flow.on_sent(seq, size);
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
        let mut st = self.st.lock().unwrap();
        st.config.clear();
        for o in st.sessions.values() {
            if let Some(m) = &o.model {
                m.restart();
            }
        }
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
    fn remember_selection(&mut self) {
        if self
            .selections
            .back()
            .is_some_and(|(v, _)| *v == self.version)
        {
            return;
        }
        let selected = self
            .sessions
            .iter()
            .map(|(&id, o)| {
                let s = airhand_link::video::SelectedVideo {
                    request_id: o.request_id,
                    settings: o.settings,
                };
                (id, s)
            })
            .collect();
        self.selections.push_back((self.version, selected));
        while self.selections.len() > 4 {
            self.selections.pop_front();
        }
    }

    /// 单编码器：帧率和码率满足前台需求，尺寸服从最严格上限；编码偏好不一致时用共同基线 H.264。
    fn merge_settings(&mut self) {
        // A view can change without changing the merged encoder parameters. Kotlin must still
        // observe that revision before output is associated with the new request.
        self.version = self.version.wrapping_add(1);
        for o in self.sessions.values_mut() {
            if let Some(m) = &o.model
                && let Ok(selected) =
                    m.select(o.settings, &self.host_codecs, self.source.0, self.source.1)
            {
                o.settings = selected.settings;
                o.request_id = selected.request_id;
            }
        }
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
        let common: Vec<Codec> = self
            .host_codecs
            .iter()
            .copied()
            .filter(|c| {
                self.sessions.values().all(|o| {
                    o.model
                        .as_ref()
                        .and_then(|m| m.receiver())
                        .is_none_or(|r| r.capability(*c).is_some())
                })
            })
            .collect();
        if !common.contains(&s.codec) {
            if let Some(codec) = [Codec::AV1, Codec::H265, Codec::H264]
                .into_iter()
                .find(|c| common.contains(c))
            {
                s.codec = codec;
            }
        }
        for o in self.sessions.values() {
            if let Some(receiver) = o.model.as_ref().and_then(|m| m.receiver())
                && let Some(limited) = receiver.select(s, &common, self.source.0, self.source.1)
            {
                s = limited;
            }
        }
        if self.settings != s {
            self.settings = s;
            self.abr.reset(s.bitrate_kbps);
            info!(?s, "shared quality settings updated");
        }
    }

    fn window_open(&self) -> bool {
        self.sessions.is_empty()
            || self
                .sessions
                .values()
                .any(|o| o.flow.can_send(self.settings.fps, 1))
    }

    /// 使用仍在接收的最快会话，慢订阅者不能把其它人的共享码率一起拉低。
    fn available_rtt(&self) -> (f64, f64) {
        self.sessions
            .values()
            .filter(|o| !o.need_key)
            .map(|o| o.flow.rtt())
            .filter(|r| r.0 > 0.0)
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::session::HostMsg;

    #[test]
    fn shared_encoder_honors_receiver_limits_and_output_keeps_its_applied_request() {
        use airhand_link::host::queue;
        use airhand_link::video::VideoSession;
        use airhand_proto::video::*;
        let (hub, _) = hub();
        hub.source(1920, 1080, vec![Codec::H264, Codec::AV1]);
        let (out, mut control, _bulk) = queue::channel();
        let model = Arc::new(VideoSession::new(
            Some(VideoOffer {
                receiver: ReceiverCapabilities {
                    codecs: vec![DecodeCapability {
                        codec: Codec::H264,
                        max_width: Some(1280),
                        max_height: Some(720),
                        max_fps: Some(30),
                    }],
                },
                view: ViewRequest {
                    request_id: 0,
                    preference: QualitySettings {
                        fps: 60,
                        codec: Codec::AV1,
                        max_short: 0,
                        ..Default::default()
                    },
                },
            }),
            out,
        ));
        let (tx, mut rx) = mpsc::channel(QUEUE);
        hub.subscribe_video(1, tx, QualitySettings::default(), model.clone())
            .unwrap();
        let first = hub.tick().unwrap();
        assert_eq!(
            (first.codec, first.max_short, first.fps),
            (Codec::H264, 720, 30)
        );
        hub.push_with_parameters(
            &[1],
            true,
            false,
            0,
            Codec::H264,
            1280,
            720,
            30,
            4000,
            first.version,
        );
        let packet = rx.try_recv().unwrap();
        let initial = VideoHeader::context(&packet[4..]).unwrap();
        let msg: HostMsg =
            airhand_proto::framing::decode_known(&control.try_recv().unwrap()[4..]).unwrap();
        assert!(
            matches!(msg, HostMsg::VideoConfig(c) if c.request_id == 0 && c.context == initial)
        );
        let request = ViewRequest {
            request_id: 1,
            preference: QualitySettings {
                fps: 15,
                ..Default::default()
            },
        };
        assert!(model.view(request));
        hub.set_settings(1, request.preference);
        // Old encoder output arriving before Kotlin consumes its next tick still belongs to view 0.
        hub.push_with_parameters(
            &[1],
            false,
            false,
            1,
            Codec::H264,
            1280,
            720,
            30,
            4000,
            first.version,
        );
        let packet = rx.try_recv().unwrap();
        assert_eq!(VideoHeader::context(&packet[4..]), Some(initial));
        assert!(control.try_recv().is_none());
        let second = hub.tick().unwrap();
        assert_eq!(second.fps, 15);
        hub.reset_config();
        hub.push_with_parameters(
            &[1],
            true,
            false,
            2,
            Codec::H264,
            1280,
            720,
            15,
            4000,
            second.version,
        );
        let packet = rx.try_recv().unwrap();
        let next = VideoHeader::context(&packet[4..]).unwrap();
        assert!(next.config_id > initial.config_id);
        let msg: HostMsg =
            airhand_proto::framing::decode_known(&control.try_recv().unwrap()[4..]).unwrap();
        assert!(
            matches!(msg, HostMsg::VideoConfig(c) if c.request_id == 1 && c.fps == 15 && c.context == next)
        );

        // Adding a viewer with no common codec cannot make existing viewers receive undecodable output.
        let (out, _c, _b) = queue::channel();
        let other = Arc::new(VideoSession::new(
            Some(VideoOffer {
                receiver: ReceiverCapabilities {
                    codecs: vec![DecodeCapability {
                        codec: Codec::AV1,
                        max_width: None,
                        max_height: None,
                        max_fps: None,
                    }],
                },
                view: ViewRequest {
                    request_id: 0,
                    preference: Default::default(),
                },
            }),
            out,
        ));
        let (tx, _rx) = mpsc::channel(QUEUE);
        assert!(
            hub.subscribe_video(2, tx, Default::default(), other)
                .is_err()
        );
        assert_eq!(hub.st.lock().unwrap().sessions.len(), 1);
    }

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
            hub.push(&[1], i == 0, false, i, Codec::H264, 8, 8);
            hub.ack(1, decode(&fast_rx.try_recv().unwrap()).seq);
        }
        let mut last = 0;
        for _ in 0..3 {
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
        hub.ack(2, last);
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
