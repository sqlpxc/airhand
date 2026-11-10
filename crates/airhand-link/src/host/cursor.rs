//! 控制端缓存了哪些光标形状（见 [`airhand_proto::session::CURSOR_SLOTS`]）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use super::{Outbox, SendError};
use airhand_proto::session::{CURSOR_SLOTS, CursorRef, CursorShape, HostMsg};

/// 槽位里是哪个形状
#[derive(Clone, Copy)]
struct Entry {
    /// 形状内容的哈希
    hash: u64,
    /// 最近一次用到的时刻（换掉最久没用的那个）
    used: u64,
    /// 这个槽位的第几代图像（槽位复用时变大，见 [`CursorRef`]）
    generation: u64,
}

/// 发给一个控制端的光标形状缓存：控制端按槽位存形状（[`CURSOR_SLOTS`] 个），这里记着每个槽位里是什么。
/// 只在一个会话里有效，会话里换显示器、重建编码管线时接着用。
///
/// 槽位满了换掉最久没用到的那个。用形状内容的 64 位哈希认形状，几十个形状里撞上的概率可以忽略
pub struct CursorCache {
    slots: Vec<Option<Entry>>,
    clock: u64,
    /// 控制端现在用的形状（发出去了图像的）
    current: Option<CursorRef>,
}

impl Default for CursorCache {
    fn default() -> Self {
        Self {
            slots: vec![None; CURSOR_SLOTS as usize],
            clock: 0,
            current: None,
        }
    }
}

impl CursorCache {
    /// 发送光标的最新状态：控制端还没有的形状先把图像排进大块通道，可见性和形状引用走控制通道（只留最新值）。
    /// 大块通道排满了（返回 `Err(Full)`）时可见性照样发出去，形状保持上一个，图像下一轮再试
    pub fn send(
        &mut self,
        out: &Outbox,
        visible: bool,
        shape: Option<&CursorShape>,
    ) -> Result<(), SendError> {
        let mut result = Ok(());
        if let Some(shape) = shape {
            let (reference, new) = self.place(shape);
            if new {
                result = out.send(&HostMsg::CursorImage {
                    reference,
                    image: shape.clone(),
                });
                if result.is_err() {
                    // 控制端没收到它：下次还得带图像
                    self.slots[reference.slot as usize] = None;
                }
            }
            if result.is_ok() {
                self.current = Some(reference);
            }
        }
        out.send(&HostMsg::Cursor {
            visible,
            shape: self.current,
        })?;
        result
    }

    /// 光标换成了 `shape`：它在哪个槽位，控制端是不是还没有它（要发图像）
    fn place(&mut self, shape: &CursorShape) -> (CursorRef, bool) {
        let mut h = DefaultHasher::new();
        shape.hash(&mut h);
        let hash = h.finish();
        self.clock += 1;
        let reference = |slot: usize, e: &Entry| CursorRef {
            slot: slot as u8,
            generation: e.generation,
        };
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|e| e.hash == hash))
        {
            let e = self.slots[i].as_mut().expect("found above");
            e.used = self.clock;
            return (reference(i, e), false);
        }
        let i = self
            .slots
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                (0..self.slots.len())
                    .min_by_key(|&i| self.slots[i].map_or(0, |e| e.used))
                    .unwrap_or(0)
            });
        let e = Entry {
            hash,
            used: self.clock,
            generation: self.clock,
        };
        self.slots[i] = Some(e);
        (reference(i, &e), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::framing;

    fn shape(n: u8) -> CursorShape {
        CursorShape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![n; 4],
        }
    }

    fn decode(b: &bytes::Bytes) -> HostMsg {
        framing::decode_known(&b[framing::PREFIX..]).unwrap()
    }

    /// 图像走大块通道、状态走控制通道；大块通道满了，可见性照样不等，图像下一轮重发
    #[test]
    fn images_are_retried_without_delaying_visibility() {
        let (out, mut control, mut bulk) = super::super::queue::channel();
        let mut cache = CursorCache::default();
        cache.send(&out, true, Some(&shape(1))).unwrap();
        assert!(matches!(
            decode(&bulk.try_recv().unwrap()),
            HostMsg::CursorImage { .. }
        ));
        assert!(matches!(
            decode(&control.try_recv().unwrap()),
            HostMsg::Cursor {
                visible: true,
                shape: Some(_)
            }
        ));
        for _ in 0..32 {
            out.send(&HostMsg::Clipboard { text: "x".into() }).unwrap();
        }
        assert_eq!(
            cache.send(&out, false, Some(&shape(2))),
            Err(SendError::Full)
        );
        // 可见性到了；形状还是控制端已经有的那一个
        let HostMsg::Cursor {
            visible: false,
            shape: Some(kept),
        } = decode(&control.try_recv().unwrap())
        else {
            panic!()
        };
        while bulk.try_recv().is_some() {}
        cache.send(&out, false, Some(&shape(2))).unwrap();
        let HostMsg::CursorImage { reference, image } = decode(&bulk.try_recv().unwrap()) else {
            panic!()
        };
        assert_eq!(image, shape(2));
        assert_ne!(reference, kept);
    }

    #[test]
    fn each_shape_is_sent_once() {
        let mut c = CursorCache::default();
        // 动画光标：同几个形状循环
        let first: Vec<(CursorRef, bool)> = (0..8).map(|n| c.place(&shape(n))).collect();
        assert!(first.iter().all(|(_, new)| *new));
        for round in 0..3 {
            for n in 0..8 {
                assert_eq!(
                    c.place(&shape(n)),
                    (first[n as usize].0, false),
                    "round {round}"
                );
            }
        }
        // 槽位满了：换掉最久没用的（现在是 0 号形状），代际变了
        for n in 8..CURSOR_SLOTS {
            assert!(c.place(&shape(n)).1);
        }
        for n in 1..8 {
            c.place(&shape(n));
        }
        let (r, new) = c.place(&shape(200));
        assert!(new);
        assert_eq!(r.slot, first[0].0.slot);
        assert!(r.generation > first[0].0.generation);
        assert!(c.place(&shape(0)).1);
    }
}
