//! 控制端缓存了哪些光标形状（见 [`airhand_proto::session::CURSOR_SLOTS`]）。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use airhand_proto::session::{CURSOR_SLOTS, CursorShape, CursorSlot};

/// 发给一个控制端的光标形状缓存：控制端按槽位存形状（[`CURSOR_SLOTS`] 个），这里记着每个槽位里是什么
/// （见 [`CursorSlot`]）。只在一个会话里有效，会话里换显示器、重建编码管线时接着用。
///
/// 槽位满了换掉最久没用到的那个。用形状内容的 64 位哈希认形状，几十个形状里撞上的概率可以忽略
pub struct CursorCache {
    /// 每个槽位里的形状（哈希）和最近一次用到的时刻
    slots: Vec<Option<(u64, u64)>>,
    clock: u64,
}

impl Default for CursorCache {
    fn default() -> Self {
        Self {
            slots: vec![None; CURSOR_SLOTS as usize],
            clock: 0,
        }
    }
}

impl CursorCache {
    /// 光标换成了 `shape`：控制端已经有它的话只给槽位，否则挑一个槽位、带上图像
    pub fn place(&mut self, shape: &CursorShape) -> CursorSlot {
        let mut h = DefaultHasher::new();
        shape.hash(&mut h);
        let hash = h.finish();
        self.clock += 1;
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.is_some_and(|(h, _)| h == hash))
        {
            self.slots[i] = Some((hash, self.clock));
            return CursorSlot {
                slot: i as u8,
                image: None,
            };
        }
        let i = self
            .slots
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                (0..self.slots.len())
                    .min_by_key(|&i| self.slots[i].map_or(0, |(_, used)| used))
                    .unwrap_or(0)
            });
        self.slots[i] = Some((hash, self.clock));
        CursorSlot {
            slot: i as u8,
            image: Some(shape.clone()),
        }
    }

    /// 带着图像的 `slot` 没能发出去（例如编码管线正在停下）：控制端没有它，下次还得带图像
    pub fn forget(&mut self, slot: &CursorSlot) {
        if slot.image.is_some()
            && let Some(s) = self.slots.get_mut(slot.slot as usize)
        {
            *s = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_cache_sends_each_shape_once() {
        let shape = |n: u8| CursorShape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![n; 4],
        };
        let mut c = CursorCache::default();
        // 动画光标：同几个形状循环
        let first: Vec<CursorSlot> = (0..8).map(|n| c.place(&shape(n))).collect();
        assert!(first.iter().all(|s| s.image.is_some()));
        for round in 0..3 {
            for n in 0..8 {
                let s = c.place(&shape(n));
                assert_eq!(
                    (s.slot, s.image.is_none()),
                    (first[n as usize].slot, true),
                    "round {round}"
                );
            }
        }
        // 槽位满了：换掉最久没用的（现在是 0 号形状）
        for n in 8..CURSOR_SLOTS {
            assert!(c.place(&shape(n)).image.is_some());
        }
        for n in 1..8 {
            c.place(&shape(n));
        }
        let s = c.place(&shape(200));
        assert_eq!(s.slot, first[0].slot);
        assert!(s.image.is_some());
        assert!(c.place(&shape(0)).image.is_some());
        // 没发出去的要忘掉，下次还带图像
        let s = c.place(&shape(201));
        c.forget(&s);
        assert!(c.place(&shape(201)).image.is_some());
    }
}
