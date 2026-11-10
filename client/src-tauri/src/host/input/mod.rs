//! 输入注入。每个会话一个 [`Injector`]，记录按下的键/鼠标键，会话结束时全部释放，避免粘键。
//!
//! 注入是同步的，有的还很慢（X11 上输入文字）：会话不直接用 [`Injector`]，而是把输入交给自己的注入线程
//! （[`InjectorThread`]），`Injector` 在那个线程上创建、使用、释放。

#[cfg(target_os = "linux")]
mod evdev;
#[cfg(windows)]
mod keymap;
#[cfg(target_os = "macos")]
mod mackeys;
#[cfg(target_os = "macos")]
mod macos;
mod thread;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
mod x11;

use airhand_proto::session::{DisplayInfo, InputEvent};

pub use thread::InjectorThread;

pub struct Injector {
    #[cfg(windows)]
    inner: windows::WinInjector,
    #[cfg(target_os = "linux")]
    inner: x11::X11Injector,
    #[cfg(target_os = "macos")]
    inner: macos::MacInjector,
}

impl Injector {
    pub fn new() -> Self {
        Self {
            #[cfg(windows)]
            inner: windows::WinInjector::default(),
            #[cfg(target_os = "linux")]
            inner: x11::X11Injector::default(),
            #[cfg(target_os = "macos")]
            inner: macos::MacInjector::default(),
        }
    }

    /// 处理一批输入事件。`display` 为当前画面对应的显示器（用于坐标换算）。
    #[allow(unused_variables)]
    pub fn handle(&mut self, events: &[InputEvent], display: &DisplayInfo) {
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        self.inner.handle(events, display);
    }

    pub fn release_all(&mut self) {
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        self.inner.release_all();
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use ::windows::Win32::Foundation::POINT;
    use ::windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    fn cursor() -> (i32, i32) {
        let mut p = POINT::default();
        crate::host::dpi_aware(|| unsafe { GetCursorPos(&mut p).unwrap() });
        (p.x, p.y)
    }

    /// 把鼠标移到主显示器中心，然后恢复原位置。
    #[test]
    #[ignore]
    fn move_to_center() {
        let d = crate::host::capture::displays()
            .into_iter()
            .find(|d| d.primary)
            .unwrap();
        println!("display {d:?}");

        let before = cursor();
        let mut inj = Injector::new();
        inj.handle(&[InputEvent::MouseMove { x: 0.5, y: 0.5 }], &d);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let after = cursor();
        crate::host::dpi_aware(|| unsafe { SetCursorPos(before.0, before.1).unwrap() });
        println!("cursor after = {after:?}");
        let (ex, ey) = (d.x + d.width as i32 / 2, d.y + d.height as i32 / 2);
        assert!(
            (after.0 - ex).abs() <= 2 && (after.1 - ey).abs() <= 2,
            "期望 ({ex},{ey}) 实际 {after:?}"
        );
    }
}

/// 需要 X 服务器（例如 `Xvfb :99 &` 然后 `DISPLAY=:99`）。会在屏幕上开一个窗口接收按键。
/// `cargo test --lib x11_input -- --ignored --nocapture`
#[cfg(all(test, target_os = "linux"))]
mod x11_tests {
    use std::time::{Duration, Instant};

    use airhand_proto::session::MouseButton;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass,
    };
    use x11rb::wrapper::ConnectionExt as _;

    use super::*;
    use crate::host::x11::X11;

    /// 开一个全屏窗口并聚焦，收集它收到的按键（翻译成字符）和鼠标事件。
    struct Probe {
        x: X11,
        win: u32,
    }

    impl Probe {
        fn new() -> Self {
            let x = X11::connect().expect("需要 X 服务器（DISPLAY）");
            let s = x.screen();
            let win = x.conn.generate_id().unwrap();
            let mask = EventMask::KEY_PRESS
                | EventMask::KEY_RELEASE
                | EventMask::BUTTON_PRESS
                | EventMask::POINTER_MOTION;
            x.conn
                .create_window(
                    0,
                    win,
                    s.root,
                    0,
                    0,
                    s.width_in_pixels,
                    s.height_in_pixels,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    0,
                    &CreateWindowAux::new().event_mask(mask).override_redirect(1),
                )
                .unwrap();
            x.conn.map_window(win).unwrap();
            x.sync().unwrap();
            x.conn
                .set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME)
                .unwrap();
            x.sync().unwrap();
            Self { x, win }
        }

        /// 收集 `dur` 内的事件：(按下的字符, 某个键码按下的次数, 鼠标键, 最后的指针位置)
        fn collect(
            &self,
            dur: Duration,
            count_keycode: u8,
        ) -> (String, u32, Vec<u8>, Option<(i16, i16)>) {
            let (mut text, mut presses, mut buttons, mut pos) =
                (String::new(), 0, Vec::new(), None);
            let deadline = Instant::now() + dur;
            while Instant::now() < deadline {
                let Some(ev) = self.x.conn.poll_for_event().unwrap() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                match ev {
                    Event::KeyPress(e) => {
                        if e.detail == count_keycode {
                            presses += 1;
                        }
                        // 按事件发生时的键盘映射翻译（Shift 选第二列）
                        let m = self
                            .x
                            .conn
                            .get_keyboard_mapping(e.detail, 1)
                            .unwrap()
                            .reply()
                            .unwrap();
                        let state = u16::from(e.state);
                        let col = if state & 1 != 0 { 1 } else { 0 };
                        let ks = m
                            .keysyms
                            .get(col)
                            .copied()
                            .filter(|&k| k != 0)
                            .unwrap_or(m.keysyms[0]);
                        // Caps Lock
                        let ks = if state & 2 != 0 && (0x61..=0x7a).contains(&ks) {
                            ks - 0x20
                        } else {
                            ks
                        };
                        let c = match ks {
                            0xff0d => Some('\n'),
                            0x20..=0xff => char::from_u32(ks),
                            k if k & 0xff00_0000 == 0x0100_0000 => char::from_u32(k & 0xff_ffff),
                            _ => None,
                        };
                        text.extend(c);
                    }
                    Event::ButtonPress(e) => buttons.push(e.detail),
                    Event::MotionNotify(e) => pos = Some((e.root_x, e.root_y)),
                    _ => {}
                }
            }
            (text, presses, buttons, pos)
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let _ = self.x.conn.destroy_window(self.win);
            let _ = self.x.conn.sync();
        }
    }

    fn keymap(x: &X11) -> Vec<u32> {
        let s = x.conn.setup();
        let r = x
            .conn
            .get_keyboard_mapping(s.min_keycode, s.max_keycode - s.min_keycode + 1)
            .unwrap();
        r.reply().unwrap().keysyms
    }

    #[test]
    #[ignore]
    fn x11_input() {
        let probe = Probe::new();
        let original_keymap = keymap(&probe.x);
        let d = crate::host::capture::displays()
            .into_iter()
            .find(|d| d.primary)
            .unwrap();
        let mut inj = Injector::new();

        // 鼠标
        inj.handle(&[InputEvent::MouseMove { x: 0.5, y: 0.25 }], &d);
        inj.handle(
            &[InputEvent::MouseButton {
                button: MouseButton::Right,
                down: true,
            }],
            &d,
        );
        inj.handle(
            &[InputEvent::MouseButton {
                button: MouseButton::Right,
                down: false,
            }],
            &d,
        );
        inj.handle(
            &[
                InputEvent::Wheel { dx: 0, dy: 60 },
                InputEvent::Wheel { dx: 0, dy: 60 },
            ],
            &d,
        );
        inj.handle(&[InputEvent::Wheel { dx: -120, dy: -240 }], &d);
        let (_, _, buttons, pos) = probe.collect(Duration::from_millis(300), 0);
        let (ex, ey) = (
            d.x + (d.width as i32 - 1) / 2,
            d.y + (d.height as i32 - 1) / 4,
        );
        let pos = pos.expect("没有收到鼠标移动");
        assert!(
            (pos.0 as i32 - ex).abs() <= 1 && (pos.1 as i32 - ey).abs() <= 1,
            "期望 ({ex},{ey}) 实际 {pos:?}"
        );
        assert_eq!(
            buttons,
            vec![3, 4, 5, 5, 6],
            "右键、向上一格、向下两格、向左一格"
        );

        // 物理键：Shift + A、数字、回车
        let keys = |codes: &[(&str, bool)]| {
            codes
                .iter()
                .map(|(c, d)| InputEvent::Key {
                    code: c.to_string(),
                    down: *d,
                })
                .collect::<Vec<_>>()
        };
        inj.handle(
            &keys(&[
                ("ShiftLeft", true),
                ("KeyA", true),
                ("KeyA", false),
                ("ShiftLeft", false),
            ]),
            &d,
        );
        inj.handle(
            &keys(&[
                ("KeyB", true),
                ("KeyB", false),
                ("Digit1", true),
                ("Digit1", false),
            ]),
            &d,
        );
        inj.handle(&keys(&[("Enter", true), ("Enter", false)]), &d);
        let (text, ..) = probe.collect(Duration::from_millis(300), 0);
        assert_eq!(text, "Ab1\n");

        // 按住不放：控制端发来的重复按下被忽略，由 X 服务器自动重复
        let a = (crate::host::input::evdev::lookup("KeyA").unwrap() + 8) as u8;
        inj.handle(&keys(&[("KeyA", true)]), &d);
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(100));
            inj.handle(&keys(&[("KeyA", true)]), &d);
        }
        // 会话结束：自动释放
        inj.release_all();
        let (_, presses, ..) = probe.collect(Duration::from_millis(300), a);
        println!("按住 1 秒: {presses} 次按下");
        assert!(presses >= 1, "没有收到按键");
        assert!(presses < 40, "出现了双重自动重复: {presses}");

        // 输入文字：布局里有的字符、需要 Shift 的字符、布局里没有的字符（临时绑定空闲键码）。
        // 接收端和真实程序一样边收边按当时的键盘映射翻译，所以放在另一个线程里
        let typed = |inj: &mut Injector, text: &str, wait: u64| {
            std::thread::scope(|s| {
                let rx = s.spawn(|| probe.collect(Duration::from_millis(wait), 0).0);
                std::thread::sleep(Duration::from_millis(50));
                let t = Instant::now();
                inj.handle(&[InputEvent::Text { text: text.into() }], &d);
                println!("输入 {} 个字符用时 {:?}", text.chars().count(), t.elapsed());
                rx.join().unwrap()
            })
        };
        let sample = "Hello, World! é ü 世界 ✓ 你好世界\n";
        assert_eq!(typed(&mut inj, sample, 800), sample);
        // Caps Lock 开着时输入的文字大小写不变
        inj.handle(&keys(&[("CapsLock", true), ("CapsLock", false)]), &d);
        assert_eq!(typed(&mut inj, "aB", 400), "aB");
        inj.handle(&keys(&[("CapsLock", true), ("CapsLock", false)]), &d);
        // 不同字符超过空闲键码数量：需要分段复用
        let many: String = (0x4e00u32..0x4e00 + 120)
            .filter_map(char::from_u32)
            .collect();
        assert_eq!(typed(&mut inj, &many, 3000), many);

        // 会话结束后临时绑定的键码应恢复原样
        drop(inj);
        let now = keymap(&probe.x);
        let diff: Vec<_> = now
            .iter()
            .zip(&original_keymap)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .collect();
        assert!(
            now.len() == original_keymap.len() && diff.is_empty(),
            "临时绑定没有恢复: {diff:?}"
        );
    }
}
