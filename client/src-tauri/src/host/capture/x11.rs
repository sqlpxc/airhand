//! Linux X11 屏幕采集。
//!
//! * MIT-SHM：X 服务器把画面直接写进共享内存（memfd，不产生文件），省去经过套接字传输的几 MB 数据；
//!   服务器不支持时退回普通 `GetImage`；
//! * DAMAGE：只有显示器范围内的画面有变化才采集，静止的桌面不产生帧；没有 DAMAGE 时按帧率采集并比较内容；
//! * XFIXES：光标形状变化通知 + 光标图像（与 Windows 一样，画面里不含光标，由控制端本地绘制）；
//! * RandR：显示器布局或分辨率变化时报错，由采集线程重建采集器。

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airhand_proto::session::CursorShape;
use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder};

use super::{CaptureUpdate, Capturer, FramePool};
use crate::host::x11::{self, Monitor, X11};

/// 共享内存段（memfd + mmap），Drop 时解除映射。
struct Shm {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

impl Shm {
    fn new(x: &X11, len: usize) -> Result<Self> {
        let seg = x.conn.generate_id()?;
        unsafe {
            let fd = libc::memfd_create(c"airhand-capture".as_ptr(), libc::MFD_CLOEXEC);
            if fd < 0 {
                bail!("memfd_create: {}", std::io::Error::last_os_error());
            }
            let fd = <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd);
            if libc::ftruncate(std::os::fd::AsRawFd::as_raw_fd(&fd), len as libc::off_t) != 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(&fd),
                0,
            );
            if ptr == libc::MAP_FAILED {
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            // 先构造出来：后面出错时由 Drop 解除映射
            let me = Self {
                seg,
                ptr: ptr.cast(),
                len,
            };
            // fd 交给服务器后在本进程关闭，映射仍然有效
            x.conn
                .shm_attach_fd(me.seg, fd, false)?
                .check()
                .context("ShmAttachFd")?;
            Ok(me)
        }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

pub struct X11Capturer {
    x: X11,
    index: u8,
    mon: Monitor,
    shm: Option<Shm>,
    damage: Option<damage::Damage>,
    cursor: bool,
    /// 显示器范围内有未采集的变化
    dirty: bool,
    cursor_dirty: bool,
    first: bool,
    /// 没有 DAMAGE 时用内容哈希判断画面是否变化
    last_hash: u64,
    /// 上次检查显示器布局的时间（有些布局变化不会发 RandR 通知，例如 `xrandr --setmonitor`）
    layout_checked: Instant,
}

/// 定期检查显示器布局的间隔
const LAYOUT_CHECK: Duration = Duration::from_secs(2);

impl X11Capturer {
    pub fn open(index: u8) -> Result<Self> {
        let x = X11::connect()?;
        check_format(&x)?;
        let mon = x11::monitor(&x, index)?;
        let root = x.root();
        let len = mon.width as usize * mon.height as usize * 4;

        let shm = if x.has_extension(shm::X11_EXTENSION_NAME)
            && x.conn
                .shm_query_version()?
                .reply()
                .is_ok_and(|v| (v.major_version, v.minor_version) >= (1, 2))
        {
            match Shm::new(&x, len) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::info!("MIT-SHM unavailable, using GetImage: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        // DAMAGE 和光标通知都依赖 XFIXES
        let xfixes = x.has_extension(xfixes::X11_EXTENSION_NAME)
            && x.conn.xfixes_query_version(4, 0)?.reply().is_ok();
        let damage = if xfixes
            && x.has_extension(damage::X11_EXTENSION_NAME)
            && x.conn.damage_query_version(1, 1)?.reply().is_ok()
        {
            let id = x.conn.generate_id()?;
            x.conn
                .damage_create(id, root, damage::ReportLevel::DELTA_RECTANGLES)?;
            Some(id)
        } else {
            None
        };
        if xfixes {
            x.conn
                .xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
        }
        if x.has_extension(randr::X11_EXTENSION_NAME) {
            let mask = randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE;
            x.conn.randr_select_input(root, mask)?;
        }
        x.sync()?;
        tracing::info!(
            screen = index,
            monitor = %mon.name,
            w = mon.width,
            h = mon.height,
            shm = shm.is_some(),
            damage = damage.is_some(),
            "X11 capture opened"
        );
        Ok(Self {
            x,
            index,
            mon,
            shm,
            damage,
            cursor: xfixes,
            dirty: true,
            cursor_dirty: true,
            first: true,
            last_hash: 0,
            layout_checked: Instant::now(),
        })
    }

    /// 处理已到达的事件，不阻塞。
    fn drain_events(&mut self) -> Result<()> {
        let mut layout_changed = false;
        while let Some(ev) = self
            .x
            .conn
            .poll_for_event()
            .context("lost connection to X11")?
        {
            match ev {
                Event::DamageNotify(e) => {
                    if intersects(&self.mon, e.area.x, e.area.y, e.area.width, e.area.height) {
                        self.dirty = true;
                    }
                }
                Event::XfixesCursorNotify(_) => self.cursor_dirty = true,
                Event::RandrScreenChangeNotify(_) | Event::RandrNotify(_) => layout_changed = true,
                _ => {}
            }
        }
        if layout_changed || self.layout_checked.elapsed() > LAYOUT_CHECK {
            self.layout_checked = Instant::now();
            if x11::monitor(&self.x, self.index).ok().as_ref() != Some(&self.mon) {
                bail!("the display layout or resolution changed");
            }
        }
        Ok(())
    }

    fn grab(&mut self, pool: &mut FramePool) -> Result<Option<Arc<super::Frame>>> {
        let m = &self.mon;
        let (w, h) = (m.width as u32, m.height as u32);
        let len = w as usize * h as usize * 4;
        // 先清空累积的变化再采集：采集过程中发生的变化会产生新的通知，不会漏掉
        if let Some(d) = self.damage {
            self.x.conn.damage_subtract(d, x11rb::NONE, x11rb::NONE)?;
        }
        self.dirty = false;
        let captured = Instant::now();
        let root = self.x.root();
        let mut frame = pool.checkout(w, h);
        let f = Arc::get_mut(&mut frame).expect("unique frame");
        f.captured = captured;
        let data = f.pixels_mut();
        match &self.shm {
            Some(s) => {
                self.x
                    .conn
                    .shm_get_image(
                        root,
                        m.x,
                        m.y,
                        m.width,
                        m.height,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        s.seg,
                        0,
                    )?
                    .reply()
                    .context("ShmGetImage")?;
                let src = unsafe { std::slice::from_raw_parts(s.ptr, len) };
                data.copy_from_slice(src);
            }
            None => {
                let r = self
                    .x
                    .conn
                    .get_image(ImageFormat::Z_PIXMAP, root, m.x, m.y, m.width, m.height, !0)?
                    .reply()
                    .context("GetImage")?;
                if r.data.len() < len {
                    bail!("GetImage returned incomplete data");
                }
                data.copy_from_slice(&r.data[..len]);
            }
        }
        if self.damage.is_none() {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            data.hash(&mut hasher);
            let h = hasher.finish();
            if h == self.last_hash && !self.first {
                return Ok(None);
            }
            self.last_hash = h;
        }
        pool.keep(&frame);
        Ok(Some(frame))
    }

    fn cursor_shape(&self) -> Result<Option<CursorShape>> {
        let img = self.x.conn.xfixes_get_cursor_image()?.reply()?;
        Ok(convert_cursor(
            img.width,
            img.height,
            img.xhot,
            img.yhot,
            &img.cursor_image,
        ))
    }
}

impl Capturer for X11Capturer {
    fn next(&mut self, timeout: Duration, pool: &mut FramePool) -> Result<CaptureUpdate> {
        self.drain_events()?;
        if self.damage.is_some() && !self.dirty && !self.cursor_dirty {
            x11::wait_readable(&self.x, timeout)?;
            self.drain_events()?;
        }
        let mut update = CaptureUpdate::default();
        if self.cursor && self.cursor_dirty {
            self.cursor_dirty = false;
            if let Some(shape) = self.cursor_shape()? {
                // 全透明的光标就是隐藏了光标（例如播放视频时）
                update.cursor_visible =
                    Some(shape.rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 0));
                update.cursor_shape = Some(shape);
            }
        } else if self.first {
            update.cursor_visible = Some(true);
        }
        if self.dirty || self.damage.is_none() {
            update.frame = self.grab(pool)?;
            self.first = false;
        }
        Ok(update)
    }
}

fn intersects(m: &Monitor, x: i16, y: i16, w: u16, h: u16) -> bool {
    let (ax, ay, aw, ah) = (x as i32, y as i32, w as i32, h as i32);
    let (bx, by, bw, bh) = (m.x as i32, m.y as i32, m.width as i32, m.height as i32);
    ax < bx + bw && bx < ax + aw && ay < by + bh && by < ay + ah
}

/// 只支持最常见的 24/32 位 BGRX 小端格式（内存里就是 BGRA 排列，可以直接交给编码器）。
fn check_format(x: &X11) -> Result<()> {
    let setup = x.conn.setup();
    let s = x.screen();
    let bpp = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == s.root_depth)
        .map(|f| f.bits_per_pixel);
    let visual = s
        .allowed_depths
        .iter()
        .flat_map(|d| &d.visuals)
        .find(|v| v.visual_id == s.root_visual)
        .ok_or_else(|| anyhow!("can't find the root window's visual"))?;
    if !matches!(s.root_depth, 24 | 32)
        || bpp != Some(32)
        || setup.image_byte_order != ImageOrder::LSB_FIRST
        || (visual.red_mask, visual.green_mask, visual.blue_mask) != (0xff0000, 0xff00, 0xff)
    {
        let depth = s.root_depth;
        bail!("unsupported desktop pixel format (depth {depth}, {bpp:?} bits per pixel)");
    }
    Ok(())
}

/// XFIXES 光标图像（每像素一个 u32：预乘 alpha 的 ARGB）→ RGBA。
fn convert_cursor(w: u16, h: u16, hot_x: u16, hot_y: u16, argb: &[u32]) -> Option<CursorShape> {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || w > 256 || h > 256 || argb.len() < w * h {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for &p in &argb[..w * h] {
        let a = (p >> 24) as u8;
        let un = |c: u32| -> u8 {
            let c = (c & 0xff) as u8;
            match a {
                0 => 0,
                255 => c,
                _ => ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8,
            }
        };
        rgba.extend_from_slice(&[un(p >> 16), un(p >> 8), un(p), a]);
    }
    Some(CursorShape {
        width: w as u16,
        height: h as u16,
        hot_x: hot_x.min(w as u16 - 1),
        hot_y: hot_y.min(h as u16 - 1),
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_unpremultiply() {
        // 不透明红、半透明白（预乘后 0x80）、全透明
        let px = [0xffff0000u32, 0x80808080, 0x00000000, 0xff00ff00];
        let c = convert_cursor(2, 2, 5, 0, &px).unwrap();
        assert_eq!((c.hot_x, c.hot_y), (1, 0));
        assert_eq!(&c.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&c.rgba[4..8], &[255, 255, 255, 128]);
        assert_eq!(&c.rgba[8..12], &[0, 0, 0, 0]);
        assert_eq!(&c.rgba[12..16], &[0, 255, 0, 255]);
        assert!(convert_cursor(0, 2, 0, 0, &px).is_none());
        assert!(convert_cursor(4, 4, 0, 0, &px).is_none(), "数据不足");
    }

    #[test]
    fn rect_intersection() {
        let m = Monitor {
            name: String::new(),
            x: 1920,
            y: 0,
            width: 1280,
            height: 1024,
            primary: false,
        };
        assert!(intersects(&m, 1900, 10, 30, 30));
        assert!(!intersects(&m, 1900, 10, 20, 30));
        assert!(intersects(&m, 3000, 1000, 500, 500));
        assert!(!intersects(&m, 3200, 0, 10, 10));
    }

    /// 需要 X 服务器（例如 `Xvfb :99 &` 然后 `DISPLAY=:99`）。会在屏幕左上角画一个色块。
    /// `cargo test --lib x11_capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn x11_capture() {
        use x11rb::protocol::xproto::{CreateGCAux, CreateWindowAux, Rectangle, WindowClass};

        let mut cap = X11Capturer::open(0).expect("打开采集");
        let mut pool = FramePool::default();
        let first = cap.next(Duration::from_millis(100), &mut pool).unwrap();
        let f = first.frame.expect("第一次应当产生一帧");
        assert_eq!(
            (f.width, f.height),
            (cap.mon.width as u32, cap.mon.height as u32)
        );
        if cap.cursor {
            assert!(first.cursor_shape.is_some(), "应当带上光标形状");
        }
        drop(f);

        // 画面不变：等到超时也不产生帧（有 DAMAGE 时）
        if cap.damage.is_some() {
            // 先把其它程序（例如时钟）造成的变化消化掉
            for _ in 0..3 {
                let _ = cap.next(Duration::from_millis(20), &mut pool).unwrap();
            }
        }

        // 在显示器 (40,30) 处画一个 64x48 的纯色块，颜色 0x123456（R=12 G=34 B=56）
        let x = X11::connect().unwrap();
        let win = x.conn.generate_id().unwrap();
        let (mx, my) = (cap.mon.x + 40, cap.mon.y + 30);
        let aux = CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(0x123456);
        x.conn
            .create_window(
                0,
                win,
                x.root(),
                mx,
                my,
                64,
                48,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &aux,
            )
            .unwrap();
        x.conn.map_window(win).unwrap();
        let gc = x.conn.generate_id().unwrap();
        x.conn
            .create_gc(gc, win, &CreateGCAux::new().foreground(0xabcdef))
            .unwrap();
        x.conn
            .poly_fill_rectangle(
                win,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: 8,
                    height: 8,
                }],
            )
            .unwrap();
        x.sync().unwrap();

        let started = Instant::now();
        let frame = loop {
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "画面变化后没有产生新帧"
            );
            if let Some(f) = cap
                .next(Duration::from_millis(100), &mut pool)
                .unwrap()
                .frame
            {
                let d = f.pixels().unwrap();
                let px = |x: usize, y: usize| {
                    let i = (y * f.width as usize + x) * 4;
                    [d[i], d[i + 1], d[i + 2]]
                };
                if px(40 + 32, 30 + 24) == [0x56, 0x34, 0x12] {
                    break f;
                }
            }
        };
        let d = frame.pixels().unwrap();
        let px = |x: usize, y: usize| {
            let i = (y * frame.width as usize + x) * 4;
            [d[i], d[i + 1], d[i + 2]]
        };
        // 内存中是 BGRA 排列
        assert_eq!(px(40 + 32, 30 + 24), [0x56, 0x34, 0x12]);
        assert_eq!(px(40 + 2, 30 + 2), [0xef, 0xcd, 0xab]);
        println!(
            "采集 {}x{}，shm={} damage={}",
            frame.width,
            frame.height,
            cap.shm.is_some(),
            cap.damage.is_some()
        );
        x.conn.destroy_window(win).unwrap();
        x.sync().unwrap();
    }
}
