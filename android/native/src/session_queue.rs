//! 控制端到 Kotlin 的会话信箱。视频和控制事件可靠、有字节预算；音频独立，只保留最近几包。
//! 不能静默丢弃等待 Kotlin ACK 的视频，否则发送端窗口会永远关闭。可靠队列满时显式结束会话。
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use airhand_link::Problem;
use airhand_link::controller::{SessionEvent, SessionUi, UiClosed};
use airhand_proto::media::{self, Kind};

const RELIABLE_MESSAGES: usize = 256;
const RELIABLE_BYTES: usize = 32 * 1024 * 1024;
const AUDIO_MESSAGES: usize = 16;

#[derive(Default)]
struct State {
    reliable: VecDeque<Vec<u8>>,
    bytes: usize,
    audio: VecDeque<Vec<u8>>,
    stats: Option<Vec<u8>>,
    terminal: Option<Vec<u8>>,
    closed: bool,
}

impl State {
    fn end(&mut self, event: SessionEvent) {
        self.reliable.clear();
        self.bytes = 0;
        self.audio.clear();
        self.stats = None;
        self.terminal = Some(serde_json::to_vec(&event).expect("session event encodes"));
        self.closed = true;
    }

    fn ready(&self) -> bool {
        self.closed || !self.reliable.is_empty() || !self.audio.is_empty() || self.stats.is_some()
    }
}

#[derive(Default)]
pub(crate) struct SessionQueue {
    state: Mutex<State>,
    ready: Condvar,
}

impl SessionQueue {
    fn reliable(&self, bytes: Vec<u8>) -> Result<(), UiClosed> {
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return Err(UiClosed);
        }
        if s.reliable.len() >= RELIABLE_MESSAGES || bytes.len() > RELIABLE_BYTES - s.bytes {
            tracing::warn!(
                queued_bytes = s.bytes,
                queued_messages = s.reliable.len(),
                "Android session consumer fell behind"
            );
            s.end(SessionEvent::Closed {
                reason: Problem::failed("the Android session queue exceeded its budget"),
                retry: true,
            });
            drop(s);
            self.ready.notify_all();
            return Err(UiClosed);
        }
        s.bytes += bytes.len();
        s.reliable.push_back(bytes);
        drop(s);
        self.ready.notify_one();
        Ok(())
    }

    pub fn pop(&self, timeout: Duration) -> Option<Vec<u8>> {
        let s = self.state.lock().unwrap();
        let (mut s, _) = self
            .ready
            .wait_timeout_while(s, timeout, |s| !s.ready())
            .unwrap();
        if let Some(end) = s.terminal.take() {
            return Some(end);
        }
        if let Some(bytes) = s.reliable.pop_front() {
            s.bytes -= bytes.len();
            return Some(bytes);
        }
        s.stats.take().or_else(|| s.audio.pop_front())
    }

    pub fn close(&self) {
        let mut s = self.state.lock().unwrap();
        *s = State {
            closed: true,
            ..State::default()
        };
        drop(s);
        self.ready.notify_all();
    }
}

impl SessionUi for SessionQueue {
    fn packet(&self, packet: Vec<u8>) -> Result<(), UiClosed> {
        if Kind::of(&packet) != Some(Kind::Audio) {
            return self.reliable(packet);
        }
        let mut s = self.state.lock().unwrap();
        if s.closed {
            return Err(UiClosed);
        }
        // 音频不占用可靠队列的额度，不挤掉视频、Hello 或 Bye。
        if packet.len() > media::MAX_AUDIO_PACKET {
            return Ok(());
        }
        if s.audio.len() == AUDIO_MESSAGES {
            s.audio.pop_front();
        }
        s.audio.push_back(packet);
        drop(s);
        self.ready.notify_one();
        Ok(())
    }

    fn event(&self, event: SessionEvent) -> Result<(), UiClosed> {
        match event {
            SessionEvent::Closed { .. } | SessionEvent::Stats { .. } => {
                let mut s = self.state.lock().unwrap();
                if s.closed {
                    return Err(UiClosed);
                }
                if matches!(event, SessionEvent::Closed { .. }) {
                    s.end(event);
                } else {
                    s.stats = Some(serde_json::to_vec(&event).map_err(|_| UiClosed)?);
                }
                drop(s);
                self.ready.notify_all();
                Ok(())
            }
            _ => self.reliable(serde_json::to_vec(&event).map_err(|_| UiClosed)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_link::flow::{FlowState, MAX_WINDOW};
    use airhand_proto::media::{AudioHeader, VideoHeader};
    use airhand_proto::session::Codec;

    #[test]
    fn stalled_kotlin_can_resume_and_ack_video_after_audio_overflows() {
        let q = SessionQueue::default();
        let mut flow = FlowState::default();
        q.event(SessionEvent::AwaitingApproval).unwrap();
        // 在途的帧最多这么多
        for seq in 1..=MAX_WINDOW {
            let p = VideoHeader {
                seq,
                key: seq == 1,
                codec: Codec::H264,
                width: 1920,
                height: 1080,
                capture_us: 0,
            }
            .encode()
            .to_vec();
            flow.on_sent(seq, p.len());
            q.packet(p).unwrap();
        }
        for seq in 1..=2048 {
            q.packet(
                AudioHeader {
                    seq,
                    start: seq == 1,
                    capture_us: seq as u64 * 20_000,
                }
                .encode()
                .to_vec(),
            )
            .unwrap();
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&q.pop(Duration::ZERO).unwrap()).unwrap()["type"],
            "awaitingApproval"
        );
        let mut audio = Vec::new();
        while let Some(p) = q.pop(Duration::ZERO) {
            if let Some(h) = VideoHeader::decode(&p) {
                flow.on_ack(h.seq, None);
            } else if let Some(h) = AudioHeader::decode(&p) {
                audio.push(h.seq);
            }
        }
        assert_eq!(flow.inflight(), 0);
        assert_eq!(audio.len(), AUDIO_MESSAGES);
        assert_eq!(audio.last(), Some(&2048));
    }

    #[test]
    fn reliable_overflow_delivers_a_terminal_event_even_when_full() {
        for large in [false, true] {
            let q = SessionQueue::default();
            if large {
                q.reliable(vec![0; RELIABLE_BYTES]).unwrap();
            } else {
                for _ in 0..RELIABLE_MESSAGES {
                    q.event(SessionEvent::AwaitingApproval).unwrap();
                }
            }
            assert!(q.event(SessionEvent::AwaitingApproval).is_err());
            assert!(q.packet(vec![1]).is_err());
            let end: serde_json::Value =
                serde_json::from_slice(&q.pop(Duration::ZERO).unwrap()).unwrap();
            assert_eq!(end["type"], "closed");
            assert_eq!(end["retry"], true);
            assert!(q.pop(Duration::ZERO).is_none());
        }
    }

    #[test]
    fn closing_wakes_a_blocked_consumer() {
        let q = std::sync::Arc::new(SessionQueue::default());
        let reader = q.clone();
        let task = std::thread::spawn(move || reader.pop(Duration::from_secs(10)));
        q.close();
        assert!(task.join().unwrap().is_none());
    }
}
