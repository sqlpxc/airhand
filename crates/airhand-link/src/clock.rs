//! 控制端的时钟（微秒）：Ping 和输入里的 `t`、确认里画面的到达时刻都是它。被控端只拿它和自己的时钟相减，
//! 两边的时钟不需要同步（见 `airhand_proto::session` 的"过时的输入"、`crate::flow`）。

use std::sync::LazyLock;
use std::time::Instant;

/// 进程里第一次调用以来过了多少微秒
pub fn now_us() -> u64 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_micros() as u64
}
