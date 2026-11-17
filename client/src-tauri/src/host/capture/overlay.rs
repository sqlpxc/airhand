//! 把光标画进画面（控制端的游戏模式要的，见 `airhand_proto::session` 的"鼠标"和 `QualitySettings::draw_cursor`）。
//!
//! 采集到的画面里没有硬件光标（Windows 的桌面复制、X11 的 GetImage 都不带）：按系统自己画光标的方式把它叠上去，
//! 包括反色的部分（文本输入的 I 形光标、黑白光标）——这里有画面的像素，可以真的和它异或，
//! 不用像控制端的本地光标那样画成黑色加白边。只处理光标那一小块，和平台无关：Windows 只把那一块从显卡上读回来
//! （见 `dxgi.rs`），X11 直接画在内存里的画面上。

/// 一个光标图像。画的时候给的位置是它的左上角（热点已经算进位置里了）
#[derive(Clone, Debug, PartialEq)]
pub enum CursorBitmap {
    /// 带透明度的彩色：RGBA，非预乘，按透明度混合（Windows 的彩色光标、X11 的光标）
    Rgba {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Windows 的带掩码彩色：每像素 B、G、R、掩码。掩码 0：直接用这个颜色；0xFF：和画面异或（颜色是 0 就是透明）
    MaskedColor {
        width: u32,
        height: u32,
        bgrm: Vec<u8>,
    },
    /// Windows 的黑白光标：上半是 AND 掩码、下半是 XOR 掩码，每像素 1 位（高位在前），每行 `pitch` 字节。
    /// `height` 是光标的高（掩码的一半）。画面 = (画面 AND 掩码) XOR 掩码：黑、白、透明、反色四种
    Monochrome {
        width: u32,
        height: u32,
        pitch: u32,
        bits: Vec<u8>,
    },
}

impl CursorBitmap {
    /// 光标的宽、高
    pub fn size(&self) -> (u32, u32) {
        match *self {
            Self::Rgba { width, height, .. }
            | Self::MaskedColor { width, height, .. }
            | Self::Monochrome { width, height, .. } => (width, height),
        }
    }

    /// 数据够不够这么大（采集到的不对就不画，不越界）
    pub fn valid(&self) -> bool {
        let (w, h) = self.size();
        let (w, h) = (w as usize, h as usize);
        w > 0
            && h > 0
            && match self {
                Self::Rgba { rgba, .. } => rgba.len() >= w * h * 4,
                Self::MaskedColor { bgrm, .. } => bgrm.len() >= w * h * 4,
                Self::Monochrome { pitch, bits, .. } => {
                    (*pitch as usize) * 8 >= w && bits.len() >= *pitch as usize * h * 2
                }
            }
    }

    /// 把光标的 (cx, cy) 这个像素叠到画面的一个像素（BGRA）上
    fn apply(&self, cx: usize, cy: usize, px: &mut [u8]) {
        match self {
            Self::Rgba { width, rgba, .. } => {
                let i = (cy * *width as usize + cx) * 4;
                let (r, g, b, a) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
                match a {
                    0 => {}
                    255 => px[..3].copy_from_slice(&[b, g, r]),
                    a => {
                        let a = a as u32;
                        let mix = |c: u8, d: u8| {
                            ((c as u32 * a + d as u32 * (255 - a) + 127) / 255) as u8
                        };
                        px[0] = mix(b, px[0]);
                        px[1] = mix(g, px[1]);
                        px[2] = mix(r, px[2]);
                    }
                }
            }
            Self::MaskedColor { width, bgrm, .. } => {
                let i = (cy * *width as usize + cx) * 4;
                let c = &bgrm[i..i + 3];
                if bgrm[i + 3] == 0 {
                    px[..3].copy_from_slice(c);
                } else {
                    px[0] ^= c[0];
                    px[1] ^= c[1];
                    px[2] ^= c[2];
                }
            }
            Self::Monochrome {
                height,
                pitch,
                bits,
                ..
            } => {
                let bit = |row: usize| (bits[row * *pitch as usize + cx / 8] >> (7 - cx % 8)) & 1;
                let and = if bit(cy) == 1 { 0xFF } else { 0 };
                let xor = if bit(cy + *height as usize) == 1 {
                    0xFF
                } else {
                    0
                };
                for c in &mut px[..3] {
                    *c = (*c & and) ^ xor;
                }
            }
        }
    }
}

/// 左上角在 `(x, y)` 的光标落在 `w`×`h` 的画面上的部分：`(x0, y0, x1, y1)`（不含 x1、y1）。完全在画面外时为 `None`
pub fn bounds(
    cursor: &CursorBitmap,
    (x, y): (i32, i32),
    (w, h): (u32, u32),
) -> Option<(u32, u32, u32, u32)> {
    let (cw, ch) = cursor.size();
    let x0 = x.max(0) as i64;
    let y0 = y.max(0) as i64;
    let x1 = (x as i64 + cw as i64).min(w as i64);
    let y1 = (y as i64 + ch as i64).min(h as i64);
    (x0 < x1 && y0 < y1).then_some((x0 as u32, y0 as u32, x1 as u32, y1 as u32))
}

/// 把左上角在 `(x, y)` 的光标画进 `w`×`h` 的画面（坐标可以是负的、超出画面，只画落在画面上的部分）。
///
/// 画面的像素在 `buf` 里：`pixel(x, y)` 是画面上 (x, y)（BGRA）在 `buf` 里的下标，只会问 [`bounds`] 范围里的；
/// 返回 `None` 的跳过。这样 `buf` 可以是整幅画面，也可以只是光标那一小块（Windows 从显卡上读回来的、
/// 显示器转过方向时还要换算坐标，见 `dxgi.rs`）
pub fn draw(
    cursor: &CursorBitmap,
    (x, y): (i32, i32),
    size: (u32, u32),
    buf: &mut [u8],
    pixel: impl Fn(u32, u32) -> Option<usize>,
) {
    if !cursor.valid() {
        return;
    }
    let Some((x0, y0, x1, y1)) = bounds(cursor, (x, y), size) else {
        return;
    };
    for fy in y0..y1 {
        for fx in x0..x1 {
            let Some(i) = pixel(fx, fy).filter(|&i| i + 4 <= buf.len()) else {
                continue;
            };
            let (cx, cy) = (
                (fx as i64 - x as i64) as usize,
                (fy as i64 - y as i64) as usize,
            );
            cursor.apply(cx, cy, &mut buf[i..i + 4]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `w`×`h` 的画面，每个像素都是 `bgra`
    fn frame(w: u32, h: u32, bgra: [u8; 4]) -> Vec<u8> {
        bgra.repeat((w * h) as usize)
    }

    fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        buf[i..i + 4].try_into().unwrap()
    }

    fn whole(w: u32) -> impl Fn(u32, u32) -> Option<usize> {
        move |x, y| Some(((y * w + x) * 4) as usize)
    }

    /// 彩色光标：不透明的盖上去、半透明的混合、全透明的不动；只画落在画面上的部分
    #[test]
    fn rgba_blends_and_clips() {
        let c = CursorBitmap::Rgba {
            width: 3,
            height: 1,
            // 不透明红、半透明白、全透明
            rgba: vec![255, 0, 0, 255, 255, 255, 255, 128, 9, 9, 9, 0],
        };
        let (w, h) = (4, 2);
        let mut buf = frame(w, h, [0, 0, 0, 255]);
        draw(&c, (1, 1), (w, h), &mut buf, whole(w));
        assert_eq!(px(&buf, w, 1, 1), [0, 0, 255, 255], "red, BGRA");
        assert_eq!(
            px(&buf, w, 2, 1),
            [128, 128, 128, 255],
            "half white on black"
        );
        assert_eq!(px(&buf, w, 3, 1), [0, 0, 0, 255], "transparent");
        assert_eq!(px(&buf, w, 0, 0), [0, 0, 0, 255], "outside the cursor");

        // 一半在画面左边外面、一半在上边外面
        let mut buf = frame(w, h, [7, 7, 7, 255]);
        draw(&c, (-2, 0), (w, h), &mut buf, whole(w));
        assert_eq!(
            px(&buf, w, 0, 0),
            [7, 7, 7, 255],
            "transparent pixel landed here"
        );
        draw(&c, (-1, 0), (w, h), &mut buf, whole(w));
        assert_eq!(px(&buf, w, 0, 0), [131, 131, 131, 255]);
        assert_eq!(bounds(&c, (4, 0), (w, h)), None);
        assert_eq!(bounds(&c, (0, -1), (w, h)), None);
        assert_eq!(bounds(&c, (-1, 1), (w, h)), Some((0, 1, 2, 2)));
    }

    /// 带掩码的彩色：掩码 0 用颜色，0xFF 和画面异或（颜色 0 就是透明）
    #[test]
    fn masked_color_xors() {
        let c = CursorBitmap::MaskedColor {
            width: 3,
            height: 1,
            bgrm: vec![1, 2, 3, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0xFF],
        };
        let mut buf = frame(3, 1, [0x10, 0x20, 0x30, 255]);
        draw(&c, (0, 0), (3, 1), &mut buf, whole(3));
        assert_eq!(px(&buf, 3, 0, 0), [1, 2, 3, 255]);
        assert_eq!(px(&buf, 3, 1, 0), [0xEF, 0xDF, 0xCF, 255], "inverted");
        assert_eq!(px(&buf, 3, 2, 0), [0x10, 0x20, 0x30, 255], "transparent");
    }

    /// 黑白光标：(AND, XOR) = (0,0) 黑、(0,1) 白、(1,0) 透明、(1,1) 反色
    #[test]
    fn monochrome_masks() {
        // 4×1，每行 1 字节：AND 0b0011_0000、XOR 0b0101_0000
        let c = CursorBitmap::Monochrome {
            width: 4,
            height: 1,
            pitch: 1,
            bits: vec![0b0011_0000, 0b0101_0000],
        };
        let mut buf = frame(4, 1, [0x10, 0x20, 0x30, 255]);
        draw(&c, (0, 0), (4, 1), &mut buf, whole(4));
        assert_eq!(px(&buf, 4, 0, 0), [0, 0, 0, 255], "black");
        assert_eq!(px(&buf, 4, 1, 0), [255, 255, 255, 255], "white");
        assert_eq!(px(&buf, 4, 2, 0), [0x10, 0x20, 0x30, 255], "transparent");
        assert_eq!(px(&buf, 4, 3, 0), [0xEF, 0xDF, 0xCF, 255], "inverted");
    }

    /// 只给了光标那一小块：按换算出来的下标画，换算不了的跳过；数据不够的光标不画
    #[test]
    fn draws_into_a_region() {
        let c = CursorBitmap::Rgba {
            width: 2,
            height: 2,
            rgba: [255, 255, 255, 255].repeat(4),
        };
        // 画面 10×10，光标在 (4, 5)；只读回了 (4, 5)..(6, 7) 这一块
        let mut region = vec![0u8; 2 * 2 * 4];
        draw(&c, (4, 5), (10, 10), &mut region, |x, y| {
            (x != 5 || y != 6).then(|| (((y - 5) * 2 + (x - 4)) * 4) as usize)
        });
        assert_eq!(&region[..12], &[255, 255, 255, 0].repeat(3)[..]);
        assert_eq!(&region[12..], &[0, 0, 0, 0], "skipped");

        let short = CursorBitmap::Rgba {
            width: 2,
            height: 2,
            rgba: vec![255; 8],
        };
        assert!(!short.valid());
        let mut buf = frame(2, 2, [0, 0, 0, 255]);
        draw(&short, (0, 0), (2, 2), &mut buf, whole(2));
        assert_eq!(buf, frame(2, 2, [0, 0, 0, 255]));
    }
}
