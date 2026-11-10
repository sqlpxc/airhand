//! macOS `CGEvent` 注入（需要"辅助功能"权限，没有时系统会悄悄丢掉事件）。
//!
//! * 按键按**物理键码**注入，远端的键盘布局 / 输入法照常工作；修饰键的状态由我们自己维护并写进每个事件；
//! * 按住鼠标键移动时发"拖动"事件，连续点击带上点击次数（否则双击、拖动窗口都不起作用）；
//! * 滚轮按像素发送（触控板的平滑滚动也能还原），120 = 一格 ≈ 3 行；
//! * 坐标：显示器内的归一化坐标 → 全局坐标（点）。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use airhand_proto::session::{DisplayInfo, InputEvent, MouseButton};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventType, CGMouseButton, CGScrollEventUnit,
};
use tracing::warn;

use super::mackeys::{self, Modifier};
use crate::host::macos::{self, Display};

/// 浏览器滚轮的单位：1 像素 ≈ 1.2（见控制端 session.ts），120 = 一格
const UNITS_PER_PIXEL: f64 = 1.2;
/// 一格（100 像素）≈ 3 行，与 Windows 默认的"一次滚动 3 行"一致
const PIXELS_PER_LINE: f64 = 100.0 / 3.0;
/// 连续点击的判定：时间间隔和位置偏差（系统默认的双击间隔是 0.5 秒）
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;
/// 显示器排列缓存的有效期
const DISPLAY_CACHE: Duration = Duration::from_secs(2);
/// 一个事件最多带 20 个 UTF-16 字符（系统的限制）
const TEXT_CHUNK: usize = 20;
/// 检查"辅助功能"权限的间隔（检查要和系统服务通信，不能每个事件都查）
const PERMISSION_CHECK: Duration = Duration::from_secs(10);

// 设备无关的修饰键标志（`kCGEventFlagMask*`）和左右键的设备相关位（`NX_DEVICE*KEYMASK`）
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;
const FLAG_FN: u64 = 0x0080_0000;
const FLAG_CAPS: u64 = 0x0001_0000;

fn modifier_flags(m: Modifier, right: bool) -> u64 {
    match (m, right) {
        (Modifier::Shift, false) => FLAG_SHIFT | 0x02,
        (Modifier::Shift, true) => FLAG_SHIFT | 0x04,
        (Modifier::Control, false) => FLAG_CONTROL | 0x01,
        (Modifier::Control, true) => FLAG_CONTROL | 0x2000,
        (Modifier::Option, false) => FLAG_OPTION | 0x20,
        (Modifier::Option, true) => FLAG_OPTION | 0x40,
        (Modifier::Command, false) => FLAG_COMMAND | 0x08,
        (Modifier::Command, true) => FLAG_COMMAND | 0x10,
        (Modifier::Function, _) => FLAG_FN,
    }
}

/// `CGEventSource` 只在持有注入器的那个任务里使用
struct Source(CFRetained<CGEventSource>);
unsafe impl Send for Source {}

/// 连续点击的计数
struct Clicks {
    button: MouseButton,
    at: Instant,
    pos: CGPoint,
    count: i64,
}

#[derive(Default)]
pub struct MacInjector {
    source: Option<Source>,
    /// 按下的键（虚拟键码）
    keys: HashSet<u16>,
    buttons: HashSet<MouseButton>,
    /// Caps Lock 的锁定状态
    caps: bool,
    pos: Option<CGPoint>,
    clicks: Option<Clicks>,
    /// 还不够一像素的滚动量 (水平, 垂直)
    wheel: (f64, f64),
    displays: Vec<Display>,
    displays_at: Option<Instant>,
    permission_checked: Option<Instant>,
    /// 已经提示过没有权限（授权之后又收回会再提示一次）
    warned: bool,
}

impl MacInjector {
    pub fn handle(&mut self, events: &[InputEvent], display: &DisplayInfo) {
        self.check_permission();
        for ev in events {
            match ev {
                InputEvent::MouseMove { x, y } => {
                    let p = self.global_point(display, *x, *y);
                    self.mouse_move(p);
                }
                InputEvent::MouseDelta { dx, dy } => self.mouse_delta(*dx, *dy),
                InputEvent::MouseButton { button, down } => {
                    // 不认识的按钮（对方版本新增的）HostSession 已经滤掉了，这里再防一手
                    if !button.is_known() {
                        continue;
                    }
                    if *down {
                        self.buttons.insert(*button);
                    } else if !self.buttons.remove(button) {
                        continue;
                    }
                    self.mouse_button(*button, *down);
                }
                InputEvent::Wheel { dx, dy } => self.wheel(*dx as f64, *dy as f64),
                InputEvent::Key { code, down } => {
                    let Some(kc) = mackeys::lookup(code) else {
                        continue;
                    };
                    let repeat = if *down {
                        !self.keys.insert(kc)
                    } else if self.keys.remove(&kc) {
                        false
                    } else {
                        // 没按下过的键不发送抬起，避免干扰本地用户
                        continue;
                    };
                    if kc == 0x39 && *down && !repeat {
                        self.caps = !self.caps;
                    }
                    self.key(kc, *down, repeat);
                }
                InputEvent::Text { text } => self.type_text(text),
                InputEvent::ReleaseAll => self.release_all(),
                // 桌面被控端不声明触摸和系统操作（HostHello），控制端不会发
                InputEvent::Touch { .. } | InputEvent::System { .. } => {}
            }
        }
    }

    pub fn release_all(&mut self) {
        // 先抬起普通键，再抬起修饰键（修饰键抬起时 flags 跟着清掉）
        let mut keys: Vec<u16> = self.keys.iter().copied().collect();
        keys.sort_by_key(|&k| mackeys::modifier(k).is_some());
        for kc in keys {
            self.keys.remove(&kc);
            self.key(kc, false, false);
        }
        for b in std::mem::take(&mut self.buttons) {
            self.mouse_button(b, false);
        }
        self.wheel = (0.0, 0.0);
    }

    fn check_permission(&mut self) {
        if self
            .permission_checked
            .is_some_and(|t| t.elapsed() < PERMISSION_CHECK)
        {
            return;
        }
        self.permission_checked = Some(Instant::now());
        let allowed = macos::input_allowed();
        if !allowed && !self.warned {
            warn!(
                "the Accessibility permission isn't granted, mouse and keyboard input is ignored (System Settings → Privacy & Security → Accessibility)"
            );
        }
        self.warned = !allowed;
    }

    /// 事件源（retain 一份返回，避免和 `&mut self` 冲突）
    fn source(&mut self) -> Option<CFRetained<CGEventSource>> {
        if self.source.is_none() {
            let s = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)?;
            // 默认注入之后会屏蔽本地键鼠 0.25 秒；远程控制时本地用户也可能在操作，不屏蔽
            CGEventSource::set_local_events_suppression_interval(Some(&s), 0.0);
            CGEventSource::set_pixels_per_line(Some(&s), PIXELS_PER_LINE);
            self.source = Some(Source(s));
        }
        self.source.as_ref().map(|s| s.0.clone())
    }

    /// 当前按住的修饰键对应的标志
    fn flags(&self) -> u64 {
        let mut f = self
            .keys
            .iter()
            .filter_map(|&k| mackeys::modifier(k))
            .fold(0, |f, (m, right)| f | modifier_flags(m, right));
        if self.caps {
            f |= FLAG_CAPS;
        }
        f
    }

    fn post(&self, ev: &CGEvent, flags: u64) {
        CGEvent::set_flags(Some(ev), CGEventFlags(flags));
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(ev));
    }

    // ───────────── 鼠标 ─────────────

    /// 显示器内的归一化坐标 → 全局坐标（点）。
    fn global_point(&mut self, d: &DisplayInfo, x: f32, y: f32) -> CGPoint {
        let stale = self.displays_at.is_none_or(|t| t.elapsed() > DISPLAY_CACHE);
        if stale || self.displays.len() <= d.index as usize {
            self.displays = macos::displays();
            self.displays_at = Some(Instant::now());
        }
        match self.displays.get(d.index as usize) {
            Some(m) => to_points(m, x, y),
            // 列不出显示器时按控制端给出的位置大致换算
            None => CGPoint::new(
                d.x as f64 + x.clamp(0.0, 1.0) as f64 * d.width as f64,
                d.y as f64 + y.clamp(0.0, 1.0) as f64 * d.height as f64,
            ),
        }
    }

    fn cursor(&self) -> CGPoint {
        self.pos.unwrap_or_else(|| {
            // 还没有移动过：用当前光标位置
            CGEvent::new(None)
                .map(|e| CGEvent::location(Some(&e)))
                .unwrap_or(CGPoint::new(0.0, 0.0))
        })
    }

    fn mouse_move(&mut self, p: CGPoint) {
        let prev = self.cursor();
        self.pos = Some(p);
        // 相对移动量（游戏等程序用它）
        let (dx, dy) = ((p.x - prev.x).round() as i64, (p.y - prev.y).round() as i64);
        self.post_move(p, dx, dy);
    }

    /// 相对移动（被控端光标隐藏时，见 `airhand_proto::session` 的模块说明）：从光标现在真正所在的位置移过去，
    /// 事件里的移动量就是控制端给的。游戏读的是移动量，光标被游戏固定住时也照样有
    fn mouse_delta(&mut self, dx: i32, dy: i32) {
        // 之后的移动、点击都从光标真正所在的位置算（系统会把光标限制在屏幕里，游戏也会把它拉回去）
        self.pos = None;
        let cur = self.cursor();
        let p = CGPoint::new(cur.x + dx as f64, cur.y + dy as f64);
        self.post_move(p, dx as i64, dy as i64);
    }

    /// 发一个移动事件（按住键时是拖动），带上移动量
    fn post_move(&mut self, p: CGPoint, dx: i64, dy: i64) {
        // 按住某个键移动 = 拖动（按住多个键时以左键为准）
        let (ty, button) = if self.buttons.contains(&MouseButton::Left) {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left)
        } else if self.buttons.contains(&MouseButton::Right) {
            (CGEventType::RightMouseDragged, CGMouseButton::Right)
        } else if let Some(b) = self.buttons.iter().next() {
            (CGEventType::OtherMouseDragged, cg_button(*b))
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left)
        };
        let flags = self.flags();
        let src = self.source();
        let Some(ev) = CGEvent::new_mouse_event(src.as_deref(), ty, p, button) else {
            return;
        };
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaX, dx);
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventDeltaY, dy);
        self.post(&ev, flags);
    }

    fn mouse_button(&mut self, b: MouseButton, down: bool) {
        let p = self.cursor();
        let count = if down {
            self.click_count(b, p)
        } else {
            self.clicks
                .as_ref()
                .filter(|c| c.button == b)
                .map_or(1, |c| c.count)
        };
        let ty = match (b, down) {
            (MouseButton::Left, true) => CGEventType::LeftMouseDown,
            (MouseButton::Left, false) => CGEventType::LeftMouseUp,
            (MouseButton::Right, true) => CGEventType::RightMouseDown,
            (MouseButton::Right, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        let flags = self.flags();
        let src = self.source();
        let button = cg_button(b);
        let Some(ev) = CGEvent::new_mouse_event(src.as_deref(), ty, p, button) else {
            return;
        };
        CGEvent::set_integer_value_field(Some(&ev), CGEventField::MouseEventClickState, count);
        CGEvent::set_integer_value_field(
            Some(&ev),
            CGEventField::MouseEventButtonNumber,
            button.0 as i64,
        );
        self.post(&ev, flags);
    }

    /// 这次按下是第几次连续点击（同一个键、时间间隔和位置都很近时累加）
    fn click_count(&mut self, b: MouseButton, p: CGPoint) -> i64 {
        let now = Instant::now();
        let count = match &self.clicks {
            Some(c)
                if c.button == b
                    && now.duration_since(c.at) <= MULTI_CLICK_TIME
                    && (c.pos.x - p.x).abs() <= MULTI_CLICK_DISTANCE
                    && (c.pos.y - p.y).abs() <= MULTI_CLICK_DISTANCE =>
            {
                c.count + 1
            }
            _ => 1,
        };
        self.clicks = Some(Clicks {
            button: b,
            at: now,
            pos: p,
            count,
        });
        count
    }

    fn wheel(&mut self, dx: f64, dy: f64) {
        // 换方向时丢弃反方向的余量
        let acc = |a: f64, d: f64| if a * d < 0.0 { d } else { a + d };
        self.wheel.0 = acc(self.wheel.0, dx / UNITS_PER_PIXEL);
        self.wheel.1 = acc(self.wheel.1, dy / UNITS_PER_PIXEL);
        let (px, py) = (self.wheel.0.trunc(), self.wheel.1.trunc());
        if px == 0.0 && py == 0.0 {
            return;
        }
        self.wheel.0 -= px;
        self.wheel.1 -= py;
        let flags = self.flags();
        let src = self.source();
        // wheel1：正数 = 向上；wheel2：正数 = 向左（我们的 dx 正数是向右）
        let Some(ev) = CGEvent::new_scroll_wheel_event2(
            src.as_deref(),
            CGScrollEventUnit::Pixel,
            2,
            py as i32,
            -px as i32,
            0,
        ) else {
            return;
        };
        self.post(&ev, flags);
    }

    // ───────────── 键盘 ─────────────

    fn key(&mut self, kc: u16, down: bool, repeat: bool) {
        let flags = self.flags() | mackeys::extra_flags(kc);
        let src = self.source();
        let Some(ev) = CGEvent::new_keyboard_event(src.as_deref(), kc, down) else {
            return;
        };
        if repeat {
            CGEvent::set_integer_value_field(Some(&ev), CGEventField::KeyboardEventAutorepeat, 1);
        }
        self.post(&ev, flags);
    }

    /// "以键盘输入方式发送"：换行、Tab 按真实的键，其余字符直接以 Unicode 字符串发送（不受键盘布局和
    /// 输入法影响，中文、表情都可以）。
    fn type_text(&mut self, text: &str) {
        let mut chunk: Vec<u16> = Vec::with_capacity(TEXT_CHUNK + 1);
        for ch in text.chars() {
            let special = match ch {
                '\r' | '\n' => Some(0x24u16), // Return
                '\t' => Some(0x30),           // Tab
                c if c.is_control() => continue,
                _ => None,
            };
            if let Some(kc) = special {
                self.unicode(&chunk);
                chunk.clear();
                self.tap(kc);
                continue;
            }
            let mut buf = [0u16; 2];
            let units = ch.encode_utf16(&mut buf);
            if chunk.len() + units.len() > TEXT_CHUNK {
                self.unicode(&chunk);
                chunk.clear();
            }
            chunk.extend_from_slice(units);
        }
        self.unicode(&chunk);
    }

    fn tap(&mut self, kc: u16) {
        let src = self.source();
        for down in [true, false] {
            if let Some(ev) = CGEvent::new_keyboard_event(src.as_deref(), kc, down) {
                self.post(&ev, 0);
            }
        }
    }

    fn unicode(&mut self, units: &[u16]) {
        if units.is_empty() {
            return;
        }
        let src = self.source();
        for down in [true, false] {
            if let Some(ev) = CGEvent::new_keyboard_event(src.as_deref(), 0, down) {
                unsafe {
                    CGEvent::keyboard_set_unicode_string(
                        Some(&ev),
                        units.len() as _,
                        units.as_ptr(),
                    )
                };
                // 不带修饰键：按住的 Command 之类不会把文字变成快捷键
                self.post(&ev, 0);
            }
        }
    }
}

fn cg_button(b: MouseButton) -> CGMouseButton {
    match b {
        MouseButton::Left => CGMouseButton::Left,
        MouseButton::Right => CGMouseButton::Right,
        MouseButton::Middle => CGMouseButton::Center,
        MouseButton::Back => CGMouseButton(3),
        MouseButton::Forward => CGMouseButton(4),
        // 不会用到：不认识的按钮不注入（见 handle）
        MouseButton::Unknown(_) => CGMouseButton::Left,
    }
}

/// 显示器内的归一化坐标 → 全局坐标（点）。
fn to_points(m: &Display, x: f32, y: f32) -> CGPoint {
    // 最右 / 最下的一个像素，而不是显示器外面
    let px = 1.0 / m.scale().max(1.0);
    CGPoint::new(
        m.x + x.clamp(0.0, 1.0) as f64 * (m.points_w - px).max(0.0),
        m.y + y.clamp(0.0, 1.0) as f64 * (m.points_h - px).max(0.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn display() -> Display {
        Display {
            id: 2,
            x: -1512.0,
            y: 0.0,
            points_w: 1512.0,
            points_h: 982.0,
            width: 3024,
            height: 1964,
            primary: false,
        }
    }

    #[test]
    fn coordinates() {
        let m = display();
        let p = to_points(&m, 0.0, 0.0);
        assert_eq!((p.x, p.y), (-1512.0, 0.0));
        let p = to_points(&m, 1.0, 1.0);
        assert_eq!((p.x, p.y), (-0.5, 981.5));
        let p = to_points(&m, 2.0, -1.0);
        assert_eq!((p.x, p.y), (-0.5, 0.0));
    }

    #[test]
    fn modifiers_and_clicks() {
        let mut inj = MacInjector::default();
        assert_eq!(inj.flags(), 0);
        inj.keys.insert(0x38); // 左 Shift
        inj.keys.insert(0x36); // 右 Command
        inj.keys.insert(0x00);
        assert_eq!(inj.flags(), FLAG_SHIFT | 0x02 | FLAG_COMMAND | 0x10);
        inj.caps = true;
        assert_eq!(inj.flags() & FLAG_CAPS, FLAG_CAPS);

        let p = CGPoint::new(100.0, 100.0);
        assert_eq!(inj.click_count(MouseButton::Left, p), 1);
        assert_eq!(
            inj.click_count(MouseButton::Left, CGPoint::new(102.0, 99.0)),
            2
        );
        assert_eq!(inj.click_count(MouseButton::Left, p), 3);
        assert_eq!(inj.click_count(MouseButton::Right, p), 1, "换了键重新计数");
        assert_eq!(
            inj.click_count(MouseButton::Right, CGPoint::new(200.0, 100.0)),
            1,
            "离得太远"
        );
    }

    /// 需要"辅助功能"权限：把鼠标移到主显示器中心，然后恢复原位置。
    /// `cargo test --lib mac_move_to_center -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mac_move_to_center() {
        if !macos::input_allowed() {
            println!("没有辅助功能权限，跳过");
            return;
        }
        let here = || CGEvent::location(CGEvent::new(None).as_deref());
        let before = here();
        let d = crate::host::capture::displays()
            .into_iter()
            .find(|d| d.primary)
            .unwrap();
        let m = macos::display(d.index).unwrap();
        let mut inj = MacInjector::default();
        inj.handle(&[InputEvent::MouseMove { x: 0.5, y: 0.5 }], &d);
        std::thread::sleep(Duration::from_millis(100));
        let after = here();
        inj.handle(
            &[InputEvent::MouseMove {
                x: ((before.x - m.x) / m.points_w) as f32,
                y: ((before.y - m.y) / m.points_h) as f32,
            }],
            &d,
        );
        let (ex, ey) = (m.x + m.points_w / 2.0, m.y + m.points_h / 2.0);
        println!("移动后 ({}, {})，期望 ({ex}, {ey})", after.x, after.y);
        assert!((after.x - ex).abs() <= 1.0 && (after.y - ey).abs() <= 1.0);
    }
}
