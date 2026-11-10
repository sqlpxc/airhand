//! Linux X11 `XTEST` 注入。
//!
//! * 按键按**物理键码**注入（evdev + 8），远端的键盘布局/输入法照常工作；
//! * 按住不放时由 X 服务器自己产生自动重复，所以控制端发来的重复按下会被忽略，避免双重重复；
//! * 输入文字（"以键盘输入方式发送"）：当前布局里有的字符直接按对应的键（必要时加 Shift），
//!   没有的字符（中文、特殊符号等）临时绑定到一个空闲键码上再按，会话结束时恢复。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use airhand_proto::session::{DisplayInfo, InputEvent, MouseButton};
use anyhow::{Context, Result, bail};
use tracing::warn;
use x11rb::connection::Connection;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, ModMask};
use x11rb::protocol::xtest::{self, ConnectionExt as _};

use super::evdev;
use crate::host::x11::X11;

/// 滚轮：120 = 一格（与 Windows 相同的单位）
const WHEEL_STEP: i32 = 120;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_CAPS_LOCK: u32 = 0xffe5;

#[derive(Default)]
pub struct X11Injector {
    conn: Option<Conn>,
    retry_at: Option<Instant>,
    /// 按下的键（X11 键码）
    keys: HashSet<u8>,
    buttons: HashSet<MouseButton>,
    /// 还不够一格的滚动量 (水平, 垂直)
    wheel: (i32, i32),
}

impl X11Injector {
    pub fn handle(&mut self, events: &[InputEvent], display: &DisplayInfo) {
        if !self.ensure() {
            return;
        }
        if let Err(e) = self.apply(events, display) {
            warn!("input injection failed, will reconnect to X11: {e:#}");
            self.conn = None;
        }
    }

    pub fn release_all(&mut self) {
        let keys: Vec<u8> = self.keys.drain().collect();
        let buttons: Vec<MouseButton> = self.buttons.drain().collect();
        self.wheel = (0, 0);
        let Some(c) = &self.conn else { return };
        let r = (|| -> Result<()> {
            for kc in keys {
                c.key(kc, false)?;
            }
            for b in buttons {
                c.button(button_number(b), false)?;
            }
            c.x.conn.flush()?;
            Ok(())
        })();
        if let Err(e) = r {
            warn!("releasing keys failed: {e:#}");
        }
    }

    fn ensure(&mut self) -> bool {
        if self.conn.is_some() {
            return true;
        }
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        match Conn::open() {
            Ok(c) => {
                self.conn = Some(c);
                self.retry_at = None;
                true
            }
            Err(e) => {
                warn!("can't inject input: {e:#}");
                self.retry_at = Some(Instant::now() + Duration::from_secs(3));
                false
            }
        }
    }

    fn apply(&mut self, events: &[InputEvent], d: &DisplayInfo) -> Result<()> {
        let c = self.conn.as_mut().expect("connected");
        // 不读的话 MappingNotify 之类的事件会一直堆在连接里
        while c.x.conn.poll_for_event()?.is_some() {}
        for ev in events {
            match ev {
                InputEvent::MouseMove { x, y } => {
                    let (px, py) = to_root(d, *x, *y);
                    c.fake(xproto::MOTION_NOTIFY_EVENT, 0, px, py)?;
                }
                // 相对移动：XTEST 的移动事件 detail = 1 表示相对（同 XTestFakeRelativeMotionEvent）
                InputEvent::MouseDelta { dx, dy } => {
                    let clamp = |v: i32| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                    c.fake(xproto::MOTION_NOTIFY_EVENT, 1, clamp(*dx), clamp(*dy))?;
                }
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
                    c.button(button_number(*button), *down)?;
                }
                InputEvent::Wheel { dx, dy } => {
                    // X11 只有"一格"的滚轮按键（4/5 垂直、6/7 水平），触控板的小幅滚动先累积
                    self.wheel.1 = accumulate(self.wheel.1, *dy as i32);
                    while self.wheel.1.abs() >= WHEEL_STEP {
                        let up = self.wheel.1 > 0;
                        c.click(if up { 4 } else { 5 })?;
                        self.wheel.1 -= WHEEL_STEP * self.wheel.1.signum();
                    }
                    self.wheel.0 = accumulate(self.wheel.0, *dx as i32);
                    while self.wheel.0.abs() >= WHEEL_STEP {
                        let right = self.wheel.0 > 0;
                        c.click(if right { 7 } else { 6 })?;
                        self.wheel.0 -= WHEEL_STEP * self.wheel.0.signum();
                    }
                }
                InputEvent::Key { code, down } => {
                    let Some(kc) = evdev::lookup(code).and_then(|k| u8::try_from(k + 8).ok())
                    else {
                        continue;
                    };
                    if *down {
                        if !self.keys.insert(kc) {
                            // 已经按着：自动重复由 X 服务器负责
                            continue;
                        }
                    } else if !self.keys.remove(&kc) {
                        // 没按下过的键不发送抬起，避免干扰本地用户
                        continue;
                    }
                    c.key(kc, *down)?;
                }
                InputEvent::Text { text } => c.type_text(text)?,
                InputEvent::ReleaseAll => {
                    for kc in self.keys.drain() {
                        c.key(kc, false)?;
                    }
                    for b in self.buttons.drain() {
                        c.button(button_number(b), false)?;
                    }
                    self.wheel = (0, 0);
                }
                // 桌面被控端不声明触摸和系统操作（HostHello），控制端不会发
                InputEvent::Touch { .. } | InputEvent::System { .. } => {}
            }
        }
        c.x.conn.flush()?;
        Ok(())
    }
}

/// 换方向时丢弃反方向的余量
fn accumulate(acc: i32, d: i32) -> i32 {
    if acc.signum() * d.signum() < 0 {
        d
    } else {
        acc.saturating_add(d)
    }
}

fn button_number(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
        // 不会用到：不认识的按钮不注入（见 apply）
        MouseButton::Unknown(_) => 0,
    }
}

/// 显示器内的归一化坐标 → 根窗口坐标。
fn to_root(d: &DisplayInfo, x: f32, y: f32) -> (i16, i16) {
    let px = d.x as f64 + x.clamp(0.0, 1.0) as f64 * (d.width.max(1) - 1) as f64;
    let py = d.y as f64 + y.clamp(0.0, 1.0) as f64 * (d.height.max(1) - 1) as f64;
    (
        px.round().clamp(0.0, i16::MAX as f64) as i16,
        py.round().clamp(0.0, i16::MAX as f64) as i16,
    )
}

/// 字符 → X11 keysym（Latin-1 直接对应，其余用 Unicode keysym）。
fn keysym(c: char) -> Option<u32> {
    Some(match c {
        '\r' | '\n' => 0xff0d, // Return
        '\t' => 0xff09,        // Tab
        '\u{8}' => 0xff08,     // BackSpace
        c if c.is_control() => return None,
        c if matches!(c as u32, 0x20..=0x7e | 0xa0..=0xff) => c as u32,
        c => 0x0100_0000 | c as u32,
    })
}

struct Conn {
    x: X11,
    xkb: bool,
    /// 为了输入文字临时绑定到空闲键码上的字符：键码 → (keysym, 最近一次使用)
    bound: HashMap<u8, (u32, u64)>,
    tick: u64,
}

impl Conn {
    fn open() -> Result<Self> {
        let x = X11::connect()?;
        if !x.has_extension(xtest::X11_EXTENSION_NAME) {
            bail!("the X server doesn't support the XTEST extension");
        }
        x.conn.xtest_get_version(2, 2)?.reply().context("XTEST")?;
        let xkb = x.has_extension(xkb::X11_EXTENSION_NAME)
            && x.conn
                .xkb_use_extension(1, 0)?
                .reply()
                .is_ok_and(|r| r.supported);
        Ok(Self {
            x,
            xkb,
            bound: HashMap::new(),
            tick: 0,
        })
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.x
            .conn
            .xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.x.root(), x, y, 0)?;
        Ok(())
    }

    fn key(&self, kc: u8, down: bool) -> Result<()> {
        self.fake(
            if down {
                xproto::KEY_PRESS_EVENT
            } else {
                xproto::KEY_RELEASE_EVENT
            },
            kc,
            0,
            0,
        )
    }

    fn button(&self, b: u8, down: bool) -> Result<()> {
        self.fake(
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            b,
            0,
            0,
        )
    }

    fn click(&self, b: u8) -> Result<()> {
        self.button(b, true)?;
        self.button(b, false)
    }

    fn tap(&self, kc: u8, shift: Option<u8>) -> Result<()> {
        if let Some(s) = shift {
            self.key(s, true)?;
        }
        self.key(kc, true)?;
        self.key(kc, false)?;
        if let Some(s) = shift {
            self.key(s, false)?;
        }
        Ok(())
    }

    fn type_text(&mut self, text: &str) -> Result<()> {
        let (min, max) = (
            self.x.conn.setup().min_keycode,
            self.x.conn.setup().max_keycode,
        );
        let map = self
            .x
            .conn
            .get_keyboard_mapping(min, max - min + 1)?
            .reply()?;
        let per = map.keysyms_per_keycode as usize;
        let sym = |kc: u8, col: usize| {
            if col < per {
                map.keysyms[(kc - min) as usize * per + col]
            } else {
                0
            }
        };
        // 之前的绑定如果被别人改掉了就作废
        self.bound.retain(|&kc, (ks, _)| sym(kc, 0) == *ks);
        let spare: Vec<u8> = (min..=max)
            .filter(|&kc| self.bound.contains_key(&kc) || (0..per).all(|c| sym(kc, c) == 0))
            .collect();

        let (group, locked) = if self.xkb {
            let s = self
                .x
                .conn
                .xkb_get_state(xkb::ID::USE_CORE_KBD.into())?
                .reply()?;
            (u8::from(s.group), s.locked_mods)
        } else {
            (0, ModMask::from(0u16))
        };
        // 当前布局里能直接按出来的字符：(键码, 是否需要 Shift)。不在第一组布局时只用临时绑定。
        // 我们自己的临时绑定不算（随时可能被换成别的字符，由 slot() 管理）
        let mut existing: HashMap<u32, (u8, bool)> = HashMap::new();
        if group == 0 {
            for col in [0, 1] {
                for kc in (min..=max).filter(|kc| !self.bound.contains_key(kc)) {
                    let ks = sym(kc, col);
                    if ks != 0 {
                        existing.entry(ks).or_insert((kc, col == 1));
                    }
                }
            }
        }
        let shift = existing.get(&XK_SHIFT_L).filter(|e| !e.1).map(|e| e.0);
        // Caps Lock 开着时会把字母变成大写：先关掉，输完再打开
        let caps = existing
            .get(&XK_CAPS_LOCK)
            .filter(|e| !e.1)
            .map(|e| e.0)
            .filter(|_| {
                locked.contains(ModMask::LOCK)
                    && text.chars().any(|c| c.is_lowercase() || c.is_uppercase())
            });
        if let Some(kc) = caps {
            self.tap(kc, None)?;
        }

        let mut binds: Vec<(u8, u32)> = Vec::new();
        let mut keys: Vec<(u8, bool)> = Vec::new();
        let mut segment = self.tick + 1;
        for ch in text.chars() {
            let Some(ks) = keysym(ch) else { continue };
            self.tick += 1;
            if let Some(&(kc, need_shift)) = existing.get(&ks)
                && (!need_shift || shift.is_some())
            {
                keys.push((kc, need_shift));
                continue;
            }
            let slot = match self.slot(ks, &spare, segment) {
                Some(s) => Some(s),
                None if !keys.is_empty() => {
                    // 空闲键码都被这一段文字占用了：先把这一段输出去，等对方程序处理完再复用
                    self.flush(&mut binds, &mut keys, shift)?;
                    self.x.sync()?;
                    std::thread::sleep(Duration::from_millis(20));
                    segment = self.tick;
                    self.slot(ks, &spare, segment)
                }
                None => None,
            };
            match slot {
                Some((kc, fresh)) => {
                    if fresh {
                        binds.push((kc, ks));
                    }
                    keys.push((kc, false));
                }
                None => tracing::debug!("no free keycode, can't type {ch:?}"),
            }
        }
        self.flush(&mut binds, &mut keys, shift)?;
        if let Some(kc) = caps {
            self.tap(kc, None)?;
        }
        Ok(())
    }

    /// 给 keysym 找一个键码：已经绑定过的直接用；否则用空闲键码，都占满了就挤掉最久没用的（本段文字用到的除外）。
    /// 返回 (键码, 是否需要新绑定)。
    fn slot(&mut self, ks: u32, spare: &[u8], segment: u64) -> Option<(u8, bool)> {
        let tick = self.tick;
        if let Some((&kc, e)) = self.bound.iter_mut().find(|(_, e)| e.0 == ks) {
            e.1 = tick;
            return Some((kc, false));
        }
        let kc = match spare
            .iter()
            .copied()
            .find(|kc| !self.bound.contains_key(kc))
        {
            Some(kc) => kc,
            None => {
                *self
                    .bound
                    .iter()
                    .filter(|(_, e)| e.1 < segment)
                    .min_by_key(|(_, e)| e.1)?
                    .0
            }
        };
        self.bound.insert(kc, (ks, tick));
        Some((kc, true))
    }

    fn flush(
        &mut self,
        binds: &mut Vec<(u8, u32)>,
        keys: &mut Vec<(u8, bool)>,
        shift: Option<u8>,
    ) -> Result<()> {
        if !binds.is_empty() {
            for (kc, ks) in binds.drain(..) {
                self.x.conn.change_keyboard_mapping(1, kc, 2, &[ks, ks])?;
            }
            // 让各个程序先处理 MappingNotify（刷新键盘映射），再收到按键
            self.x.sync()?;
            std::thread::sleep(Duration::from_millis(10));
        }
        for (kc, need_shift) in keys.drain(..) {
            self.tap(kc, if need_shift { shift } else { None })?;
        }
        self.x.conn.flush()?;
        Ok(())
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // 恢复临时绑定的键码
        for &kc in self.bound.keys() {
            let _ = self.x.conn.change_keyboard_mapping(1, kc, 2, &[0, 0]);
        }
        // 等服务器处理完再断开（只 flush 的话，别的程序可能在恢复之前就读到了键盘映射）
        let _ = self.x.sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_accumulation() {
        assert_eq!(accumulate(0, 60), 60);
        assert_eq!(accumulate(60, 60), 120);
        assert_eq!(accumulate(60, -30), -30, "换方向时丢弃余量");
        assert_eq!(accumulate(-60, -30), -90);
    }

    #[test]
    fn keysyms() {
        assert_eq!(keysym('a'), Some(0x61));
        assert_eq!(keysym('é'), Some(0xe9));
        assert_eq!(keysym('世'), Some(0x0100_4e16));
        assert_eq!(keysym('\r'), Some(0xff0d));
        assert_eq!(keysym('\u{1}'), None);
    }

    #[test]
    fn root_coordinates() {
        let d = DisplayInfo {
            index: 1,
            name: String::new(),
            x: 1920,
            y: 0,
            width: 1280,
            height: 1024,
            primary: false,
        };
        assert_eq!(to_root(&d, 0.0, 0.0), (1920, 0));
        assert_eq!(to_root(&d, 1.0, 1.0), (3199, 1023));
        assert_eq!(to_root(&d, 2.0, -1.0), (3199, 0));
    }
}
