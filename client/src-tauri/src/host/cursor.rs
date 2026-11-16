//! 一个会话的光标。被控端只有一个光标，画面却可能有好几路（同时看好几个显示器，见 `airhand_proto::session` 的"多屏"）：
//! 每一路的采集都报光标（Windows 的桌面复制按显示器报），各报各的会互相打架——光标在第 1 块显示器上拖窗口
//! （画进画面里的软件光标），第 0 块显示器却一直报"显示着、在别的显示器上"，控制端就在两种状态之间来回跳；
//! 形状也只有光标所在的那一块显示器报得对。
//!
//! 所以只听光标所在的那一路（采集报的 [`CursorSnapshot::here`](super::capture::CursorSnapshot::here)）：
//!
//! * 只有一路画面时照旧，它报什么发什么（光标在别的显示器上时是"显示着"，控制端照常用绝对坐标，一动就回来）；
//! * 好几路时，光标在哪一路的显示器上就听哪一路（[`SharedCursor::owner`]）。它离开了（到了别的显示器上）就不再归它，
//!   它报的"在别的显示器上"发出去，然后等光标所在的那一路报；光标到了没在看的显示器上时就停在"显示着"；
//! * 看不出在哪里（隐藏了，X11、macOS 只有一个全局的光标状态）的时候每一路报的都一样，谁报都行。
//!
//! 形状按 [`CursorCache`] 的槽位发：一路刚开始归它管时把它的形状重发一遍（多半已经在控制端的槽位里了，只发引用）。

use airhand_link::host::{CursorCache, Outbox, SendError};
use airhand_proto::session::{CursorShape, CursorVisibility};

/// 一个会话的光标：形状缓存，以及现在听哪一路（见模块说明）
#[derive(Default)]
pub struct SharedCursor {
    cache: CursorCache,
    /// 光标在哪一路画面的显示器上
    owner: Option<u8>,
    /// 现在有几路画面
    streams: usize,
}

/// 一路报的光标怎么处理了
#[derive(Debug, PartialEq, Eq)]
pub enum Reported {
    /// 发出去了
    Sent,
    /// 不归这一路管（光标在别的显示器上），没发。之后归它管时要把形状重发一遍
    Ignored,
}

impl SharedCursor {
    /// 现在有几路画面（看的显示器变了）
    pub fn set_streams(&mut self, streams: usize) {
        self.streams = streams;
        if self.owner.is_some_and(|o| o as usize >= streams) {
            self.owner = None;
        }
    }

    /// 第 `stream` 路的采集报了光标的最新状态（`here`：光标在它那块显示器上）。发不出去时同 [`CursorCache::send`]
    pub fn report(
        &mut self,
        stream: u8,
        here: bool,
        out: &Outbox,
        visibility: CursorVisibility,
        shape: Option<&CursorShape>,
    ) -> Result<Reported, SendError> {
        if self.streams > 1 {
            if here {
                self.owner = Some(stream);
            } else if self.owner == Some(stream) {
                // 光标离开了这一路的显示器：它报的"在别的显示器上"发出去，之后等光标所在的那一路报
                self.owner = None;
            } else if self.owner.is_some() {
                return Ok(Reported::Ignored);
            }
        }
        self.cache.send(out, visibility, shape)?;
        Ok(Reported::Sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_link::host::queue;
    use airhand_proto::framing;
    use airhand_proto::session::HostMsg;

    fn visibility(m: HostMsg) -> CursorVisibility {
        match m {
            HostMsg::Cursor { visibility, .. } => visibility,
            other => panic!("{other:?}"),
        }
    }

    /// 两路画面：只听光标所在的那一路；它离开了先发它说的"在别的显示器上"，之后另一路接手
    #[test]
    fn only_the_display_with_the_cursor_counts() {
        let (out, mut control, _bulk) = queue::channel();
        let mut c = SharedCursor::default();
        c.set_streams(2);
        let sent = |control: &mut queue::LaneReceiver| -> Vec<CursorVisibility> {
            std::iter::from_fn(|| control.try_recv())
                .map(|b| visibility(framing::decode_known(&b[framing::PREFIX..]).unwrap()))
                .collect()
        };
        use CursorVisibility::*;
        // 光标在第 1 块显示器上拖窗口（画进画面里了），第 0 块一直报"在别的显示器上"
        assert_eq!(c.report(1, true, &out, InFrame, None), Ok(Reported::Sent));
        assert_eq!(c.report(0, false, &out, Shown, None), Ok(Reported::Ignored));
        assert_eq!(sent(&mut control), [InFrame]);
        // 光标回到第 0 块：它接手
        assert_eq!(c.report(0, true, &out, Shown, None), Ok(Reported::Sent));
        assert_eq!(c.report(1, false, &out, Shown, None), Ok(Reported::Ignored));
        assert_eq!(sent(&mut control), [Shown]);
        // 到了没在看的显示器上：第 0 块说"在别的显示器上"，之后谁说都一样
        assert_eq!(c.report(0, false, &out, Shown, None), Ok(Reported::Sent));
        assert_eq!(c.report(1, false, &out, Shown, None), Ok(Reported::Sent));
        // 隐藏了（游戏）：每一路都说隐藏
        assert_eq!(c.report(1, true, &out, Hidden, None), Ok(Reported::Sent));
        assert_eq!(c.report(0, true, &out, Hidden, None), Ok(Reported::Sent));
        assert_eq!(sent(&mut control).last(), Some(&Hidden));
        // 只剩一路：它报什么发什么
        c.set_streams(1);
        assert_eq!(c.report(0, false, &out, Shown, None), Ok(Reported::Sent));
    }
}
