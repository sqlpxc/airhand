//! 控制端的输入晚到了多久（见 `airhand_proto::session` 的"过时的输入"）。

use std::time::{Duration, Instant};

use airhand_proto::session::INPUT_MAX_AGE;
use tracing::warn;

/// 判断控制端的输入晚到了多久（见 `airhand_proto::session` 的"过时的输入"）。
///
/// 两边的时钟不同步，只能比"比平时晚了多少"：`收到的时刻 − t` 是两边时钟之差加上这一路的耽搁，
/// 记下它的最小值当作基准（时钟之差 + 最快的那一次），超出基准的部分就是这条消息额外耽搁了多久——
/// 在控制端的队列里、在 QUIC 的发送缓冲里（断网时往里写不会阻塞）、在服务器上。Ping 和输入都带着 `t`，都算样本。
///
/// * 基准每秒最多上调 1 ms（[`InputClock::DRIFT`]），跟得上两边时钟的漂移（通常不到 0.01%），
///   断网再久，恢复后涌来的旧消息也远远超出基准；
/// * 连续 [`InputClock::RESYNC`] 收到的全是晚到的消息，说明这一路本身变慢了（例如换了网络），而不是积压：
///   以现在为准重新取基准，不能一直丢下去。
pub struct InputClock {
    epoch: Instant,
    /// 基准（微秒）和取它的时刻
    base: Option<(i64, Instant)>,
    /// 从什么时候起收到的全是晚到的消息
    late_since: Option<Instant>,
}

impl Default for InputClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
            base: None,
            late_since: None,
        }
    }
}

impl InputClock {
    /// 基准每过这么多微秒最多上调 1 微秒
    const DRIFT: i64 = 1000;
    const RESYNC: Duration = Duration::from_secs(5);

    /// 记下一条带着控制端时刻 `t` 的消息（Ping、输入），`now` 是收到它的时刻。返回它是不是过时了（只对输入有意义）
    pub fn stale(&mut self, t: u64, now: Instant) -> bool {
        let offset = now.saturating_duration_since(self.epoch).as_micros() as i64 - t as i64;
        let base = match self.base {
            Some((b, at)) => {
                (b + now.saturating_duration_since(at).as_micros() as i64 / Self::DRIFT).min(offset)
            }
            None => offset,
        };
        let late = Duration::from_micros((offset - base) as u64) > INPUT_MAX_AGE;
        let since = match (late, self.late_since) {
            (false, _) => None,
            (true, since) => Some(since.unwrap_or(now)),
        };
        if since.is_some_and(|s| now.saturating_duration_since(s) > Self::RESYNC) {
            warn!(
                "input has been arriving late for {:?}, the network path got slower: resynchronizing",
                Self::RESYNC
            );
            self.base = Some((offset, now));
            self.late_since = None;
            return false;
        }
        self.base = Some((base, now));
        self.late_since = since;
        late
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 控制端的时钟比被控端晚 1000 秒、正常时这一路 30 ms
    fn clock_at(start: Instant) -> InputClock {
        InputClock {
            epoch: start,
            ..InputClock::default()
        }
    }

    const SKEW: u64 = 1_000_000_000;

    #[test]
    fn input_that_waited_out_a_network_outage_is_stale() {
        let start = Instant::now();
        let mut c = clock_at(start);
        // 控制端在 sent 时刻（被控端时钟，毫秒）发出、被控端在 at 时刻收到
        let mut msg =
            |sent: u64, at: u64| c.stale(SKEW + sent * 1000, start + Duration::from_millis(at));
        // 时快时慢的正常网络
        for i in 0..100 {
            assert!(!msg(i * 100, i * 100 + 30 + (i % 7) * 20));
        }
        // 断网 30 秒：这期间的 Ping 和输入都积在 QUIC 的发送缓冲里，恢复后一起到
        let back = 10_000 + 30_000;
        assert!(msg(10_000, back));
        assert!(msg(25_000, back + 1));
        assert!(msg(37_000, back + 2));
        // 恢复前 1 秒才发的：不到 INPUT_MAX_AGE，照样执行
        assert!(!msg(39_000, back + 3));
        // 恢复之后新发的
        assert!(!msg(39_990, back + 40));
        assert!(!msg(40_100, 40_100 + 35));
    }

    #[test]
    fn follows_clock_drift_and_a_slower_path() {
        let start = Instant::now();
        let mut c = clock_at(start);
        // 控制端的时钟每秒慢 0.05 ms（50 ppm），跑一个小时
        for s in 0..3600u64 {
            let sent = SKEW + s * 1_000_000 - s * 50;
            assert!(!c.stale(sent, start + Duration::from_millis(s * 1000 + 30)));
        }
        // 换了一条慢 3 秒的网络：丢一阵子（不超过 RESYNC），然后重新取基准
        let mut dropped = 0;
        for i in 0..100u64 {
            let at = 3_700_000 + i * 100;
            if c.stale(
                SKEW + (at - 3_030) * 1000 - 3600 * 50,
                start + Duration::from_millis(at),
            ) {
                dropped += 1;
            }
        }
        assert!(dropped > 0 && dropped <= 51, "{dropped}");
        assert!(!c.stale(
            SKEW + (3_720_000 - 3_030) * 1000 - 3600 * 50,
            start + Duration::from_millis(3_720_000)
        ));
    }
}
