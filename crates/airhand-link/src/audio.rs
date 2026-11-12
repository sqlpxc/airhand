//! 声音数据报的冗余（格式见 `airhand_proto::media` 的"声音数据报"）：被控端每个数据报带上之前几包的副本
//! （[`Bundler`]），控制端拆开、按采集时刻只留没收过的（[`Unbundler`]）。丢了的包这样就补回来了，不用等重传；
//! 乱序到的数据报也一样：晚到的那包在后一个数据报里已经有了。

use std::collections::VecDeque;

use airhand_proto::framing;
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::media::{self, AudioHeader};
use bytes::Bytes;

/// 每个声音数据报最多带上之前这么多包的副本（放得下的话）：连着丢了这么多个数据报也补得回来。
/// 一包 Opus 约 240 字节，一个数据报三包约 0.8 KB，每秒 50 个约 40 KB（320 kbps），和画面比可以忽略
pub(crate) const REDUNDANCY: usize = 2;

/// 被控端：拼声音数据报的明文（一个会话一个）
#[derive(Default)]
pub(crate) struct Bundler {
    /// 最近发出去的包（不带长度前缀），从新到旧
    recent: VecDeque<Bytes>,
}

impl Bundler {
    /// 这一包（[`media::frame`] 编好的，带长度前缀）连同之前几包的副本：要加密发出去的数据报明文。
    /// 单独一包都放不进一个数据报（[`media::MAX_AUDIO_PACKET`]）时为 `None`，这一包丢掉
    pub fn bundle(&mut self, framed: Bytes) -> Option<Vec<u8>> {
        let packet = framed.slice(framing::PREFIX.min(framed.len())..);
        // 静音之后的第一包：之前的那几包是上一段声音的，补过去就是在新的声音前面放一段过时的
        if AudioHeader::decode(&packet).is_none_or(|h| h.start) {
            self.recent.clear();
        }
        let plain = media::audio_datagram(&packet, self.recent.iter().map(|p| &p[..]))?;
        self.recent.push_front(packet);
        self.recent.truncate(REDUNDANCY);
        Some(plain)
    }
}

/// 控制端：拆开声音数据报的明文（一个会话一个）
#[derive(Default)]
pub(crate) struct Unbundler {
    order: AudioOrder,
}

impl Unbundler {
    /// 解密后的数据报明文里没收过的声音包，按采集先后放进 `out`（每个多留了加密开销的容量：
    /// 和原地解密的画面包一样，交给界面时可以在原缓冲里加东西，见 `console::ConsoleUi`）。
    /// 返回其中有几包是补回来的（之前的数据报丢了、或者还没到）。格式不对的整个丢掉
    pub fn unbundle(&mut self, plain: &[u8], out: &mut VecDeque<Vec<u8>>) -> usize {
        let Some(packets) = media::audio_packets(plain) else {
            tracing::debug!(len = plain.len(), "dropped a malformed audio datagram");
            return 0;
        };
        let newest = packets.len() - 1;
        let mut recovered = 0;
        for (i, p) in packets.into_iter().enumerate() {
            // 不是声音包的（以后的版本可能放别的）、副本、迟到的都不要
            if !self.order.accept(p) {
                continue;
            }
            if i < newest {
                recovered += 1;
            }
            let mut packet = Vec::with_capacity(p.len() + SEAL_OVERHEAD);
            packet.extend_from_slice(p);
            out.push_back(packet);
        }
        recovered
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

    /// 被控端发队列里的一包：`seq` 每包 20 ms，`start` 的那包前面停顿过
    fn framed(seq: u32, start: bool, opus: usize) -> Bytes {
        let h = AudioHeader {
            seq,
            start,
            capture_us: seq as u64 * 20_000,
        };
        Bytes::from(media::frame(&h.encode(), &[&vec![seq as u8; opus]]))
    }

    fn seqs(out: &VecDeque<Vec<u8>>) -> Vec<u32> {
        out.iter()
            .map(|p| AudioHeader::decode(p).unwrap().seq)
            .collect()
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

    /// 连着丢了不超过 [`REDUNDANCY`] 个数据报：下一个数据报把缺的补上，界面收到的一包不少、顺序不乱
    #[test]
    fn lost_datagrams_are_recovered_from_the_next_one() {
        let mut tx = Bundler::default();
        let datagrams: Vec<Vec<u8>> = (1..=20)
            .map(|seq| tx.bundle(framed(seq, seq == 1, 240)).unwrap())
            .collect();
        let mut rx = Unbundler::default();
        let mut out = VecDeque::new();
        let mut recovered = 0;
        // 丢了 3；5、6；9、10、11（多于能补的）
        for (i, d) in datagrams.iter().enumerate() {
            if ![3, 5, 6, 9, 10, 11].contains(&(i + 1)) {
                recovered += rx.unbundle(d, &mut out);
            }
        }
        let want: Vec<u32> = (1..=20).filter(|&s| s != 9).collect();
        assert_eq!(seqs(&out), want);
        assert_eq!(recovered, 1 + 2 + 2);
        // 交给界面的还是一个一个的声音包，多留了原地加东西的容量
        assert!(out.iter().all(|p| p.len() == media::HEADER_LEN + 240));
        assert!(out.iter().all(|p| p.capacity() >= p.len() + SEAL_OVERHEAD));
    }

    /// 乱序、重复到的数据报：先到的那个已经带来了晚到的那一包，晚到的整个是副本
    #[test]
    fn reordered_and_duplicate_datagrams_play_each_packet_once() {
        let mut tx = Bundler::default();
        let d: Vec<Vec<u8>> = (1..=4)
            .map(|seq| tx.bundle(framed(seq, seq == 1, 100)).unwrap())
            .collect();
        let mut rx = Unbundler::default();
        let mut out = VecDeque::new();
        for i in [0, 2, 1, 2, 3, 3] {
            rx.unbundle(&d[i], &mut out);
        }
        assert_eq!(seqs(&out), [1, 2, 3, 4]);
    }

    /// 静音之后的第一包不带上一段声音的副本：上一段最后丢了的那包不会在新的声音前面晚一大截播出来
    #[test]
    fn copies_do_not_cross_a_silence() {
        let mut tx = Bundler::default();
        let before: Vec<Vec<u8>> = (1..=3)
            .map(|seq| tx.bundle(framed(seq, seq == 1, 100)).unwrap())
            .collect();
        // 停了一阵（序号接着走，采集时刻跳过去了）
        let after = tx.bundle(framed(100, true, 100)).unwrap();
        assert_eq!(media::audio_packets(&after).unwrap().len(), 1);
        let next = tx.bundle(framed(101, false, 100)).unwrap();
        assert_eq!(media::audio_packets(&next).unwrap().len(), 2);
        let mut rx = Unbundler::default();
        let mut out = VecDeque::new();
        rx.unbundle(&before[0], &mut out);
        rx.unbundle(&after, &mut out);
        assert_eq!(seqs(&out), [1, 100]);
    }

    /// 单独一包都放不进数据报的丢掉，也不当成之后的副本；副本放不下就少带几包
    #[test]
    fn oversized_packets_are_dropped_and_copies_shrink_to_fit() {
        let mut tx = Bundler::default();
        assert!(
            tx.bundle(framed(1, true, media::MAX_OPUS_PACKET + 1))
                .is_none()
        );
        let first = tx.bundle(framed(2, false, 500)).unwrap();
        assert_eq!(media::audio_packets(&first).unwrap().len(), 1);
        let d = tx.bundle(framed(3, false, 300)).unwrap();
        assert_eq!(media::audio_packets(&d).unwrap().len(), 2);
        let d = tx.bundle(framed(4, false, 300)).unwrap();
        // 三包要 3 × (4 + 24) + 500 + 300 + 300 字节：放不下，只带上一包
        assert_eq!(media::audio_packets(&d).unwrap().len(), 2);
        assert!(d.len() <= media::MAX_AUDIO_DATAGRAM);
        let full = tx.bundle(framed(5, false, media::MAX_OPUS_PACKET)).unwrap();
        assert_eq!(full.len(), media::MAX_AUDIO_DATAGRAM);
    }

    /// 格式不对的数据报整个丢掉；里面不是声音包的跳过
    #[test]
    fn malformed_datagrams_and_unknown_packets_are_skipped() {
        let mut rx = Unbundler::default();
        let mut out = VecDeque::new();
        assert_eq!(rx.unbundle(&[1, 2, 3], &mut out), 0);
        let video = airhand_proto::media::VideoHeader {
            seq: 1,
            key: true,
            codec: airhand_proto::session::Codec::H264,
            width: 2,
            height: 2,
            capture_us: 0,
        }
        .encode();
        let plain =
            media::audio_datagram(&framed(2, true, 10)[framing::PREFIX..], [&video[..]]).unwrap();
        rx.unbundle(&plain, &mut out);
        assert_eq!(seqs(&out), [2]);
    }
}
