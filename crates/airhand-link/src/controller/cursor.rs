//! 合并跨通道的光标状态与图像。界面仍收到原来的媒体包，三端不需要各写一份乱序处理。
use airhand_proto::session::{CURSOR_SLOTS, CursorRef, CursorShape, CursorSlot, CursorState};

struct Image {
    generation: u64,
    shape: CursorShape,
    delivered: bool,
}

pub(super) struct CursorReceiver {
    visible: bool,
    wanted: Option<CursorRef>,
    slots: Vec<Option<Image>>,
}

impl Default for CursorReceiver {
    fn default() -> Self {
        Self {
            visible: true,
            wanted: None,
            slots: (0..CURSOR_SLOTS).map(|_| None).collect(),
        }
    }
}

impl CursorReceiver {
    pub fn update(&mut self, visible: bool, shape: Option<CursorRef>) -> CursorState {
        self.visible = visible;
        self.wanted = shape.filter(|r| r.slot < CURSOR_SLOTS);
        self.current()
    }

    pub fn image(&mut self, reference: CursorRef, image: CursorShape) -> Option<CursorState> {
        let slot = self.slots.get_mut(reference.slot as usize)?;
        // 先验证尺寸，不让坏图像进入前端缓存。
        let pixels = usize::from(image.width) * usize::from(image.height);
        if pixels == 0 || pixels.checked_mul(4)? != image.rgba.len() {
            return None;
        }
        if slot
            .as_ref()
            .is_some_and(|s| s.generation >= reference.generation)
        {
            return None;
        }
        *slot = Some(Image {
            generation: reference.generation,
            shape: image,
            delivered: false,
        });
        // 图像本身不改变可见性或选择；晚到的旧图像只能入缓存。
        (self.wanted == Some(reference)).then(|| self.current())
    }

    fn current(&mut self) -> CursorState {
        let shape = self.wanted.and_then(|r| {
            let image = self.slots.get_mut(r.slot as usize)?.as_mut()?;
            if image.generation != r.generation {
                return None;
            }
            let data = (!image.delivered).then(|| image.shape.clone());
            image.delivered = true;
            Some(CursorSlot {
                slot: r.slot,
                image: data,
            })
        });
        CursorState {
            visible: self.visible,
            shape,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(n: u8) -> CursorShape {
        CursorShape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![n; 4],
        }
    }

    #[test]
    fn visibility_arrives_without_waiting_for_image_and_late_images_cannot_undo_it() {
        let mut c = CursorReceiver::default();
        let r = CursorRef {
            slot: 0,
            generation: 1,
        };
        assert!(c.update(true, Some(r)).shape.is_none());
        let hidden = c.update(false, Some(r));
        assert!(!hidden.visible && hidden.shape.is_none());
        let arrival = c.image(r, image(1)).unwrap();
        assert!(!arrival.visible);
        assert_eq!(arrival.shape.unwrap().image, Some(image(1)));
        let shown = c.update(true, Some(r));
        assert!(shown.visible);
        assert!(
            shown.shape.unwrap().image.is_none(),
            "frontend already has this image"
        );
    }

    #[test]
    fn reuse_of_a_slot_does_not_display_an_old_generation() {
        let mut c = CursorReceiver::default();
        let old = CursorRef {
            slot: 0,
            generation: 1,
        };
        let new = CursorRef {
            slot: 0,
            generation: 2,
        };
        c.update(true, Some(new));
        assert!(c.image(old, image(1)).is_none());
        let fresh = c.image(new, image(2)).unwrap();
        assert_eq!(fresh.shape.unwrap().image, Some(image(2)));
        assert!(c.image(old, image(1)).is_none());
        assert!(c.update(false, Some(old)).shape.is_none());
    }

    #[test]
    fn images_can_arrive_before_their_control_reference_and_cache_stays_bounded() {
        let mut c = CursorReceiver::default();
        let a = CursorRef {
            slot: 0,
            generation: 5,
        };
        let b = CursorRef {
            slot: 1,
            generation: 6,
        };
        assert!(c.image(a, image(1)).is_none());
        assert!(c.image(b, image(2)).is_none());
        assert_eq!(c.update(true, Some(b)).shape.unwrap().image, Some(image(2)));
        assert_eq!(c.update(true, Some(a)).shape.unwrap().image, Some(image(1)));
        assert!(
            c.image(
                CursorRef {
                    slot: 255,
                    generation: 7
                },
                image(3)
            )
            .is_none()
        );
        assert!(
            c.update(
                false,
                Some(CursorRef {
                    slot: 255,
                    generation: 7
                })
            )
            .shape
            .is_none()
        );
        assert_eq!(c.slots.len(), CURSOR_SLOTS as usize);
    }
}
