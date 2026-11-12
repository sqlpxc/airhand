//! Windows DXGI Desktop Duplication 采集。
//!
//! * 只在桌面有变化时才产生新帧；
//! * 显卡支持视频处理器时（几乎都支持），画面复制进自己的纹理，留在显卡上（[`GpuFrame`]）：硬件编码器在同一个
//!   设备上缩放、转换颜色、编码（见 `encoder/mf.rs`），需要像素时才读回内存（[`GpuFrame::read_pixels`]）。
//!   不支持时每帧复制到 staging 纹理、读回内存；
//! * 优先使用 `DuplicateOutput1`（支持 HDR 桌面自动转换为 BGRA8）；
//! * 打开前先切到当前的输入桌面（见 [`crate::host::desktop`]）：UAC 确认框、锁屏出现时桌面复制会中断
//!   （`DXGI_ERROR_ACCESS_LOST`），重新打开时就跟着切过去了；
//! * 旋转了的显示器（竖屏）：桌面复制给出的是没有旋转的画面，复制时转成用户看到的方向（见 [`Rotation`]），
//!   画面的宽高、方向与 [`DisplayInfo`]（桌面坐标）一致，控制端的鼠标坐标才能对上。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::session::{CursorShape, DisplayInfo};
use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1,
    D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Multithread, ID3D11Texture2D, ID3D11VideoDevice,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, DXGI_OUTPUT_DESC, IDXGIAdapter, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GetCursorInfo,
};
use windows::core::Interface;

use super::{CaptureUpdate, Capturer, CursorVisibility, Frame, FramePool};

// ───────────────────────────── GPU 上的画面 ─────────────────────────────

/// 一次打开桌面复制所用的 D3D11 设备。GPU 上的画面属于它，编码器在同一个设备上处理这些画面。
///
/// 采集线程、各个会话的编码线程（视频处理器、读回）和硬件编码器都用这个设备的立即上下文，所以打开了多线程保护。
pub struct D3dSource {
    /// 见 [`super::next_source`]
    id: u64,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// 显卡的 LUID（硬件编码器要在同一块显卡上）
    luid: u64,
    /// 读回用的 staging 纹理
    staging: Mutex<Option<(ID3D11Texture2D, u32, u32)>>,
}

// D3D11 设备本身是线程安全的；立即上下文打开了多线程保护（见 `D3dSource::new`）
unsafe impl Send for D3dSource {}
unsafe impl Sync for D3dSource {}

impl D3dSource {
    /// 设备要带 `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` 创建（视频处理器要用）
    pub(crate) fn new(device: ID3D11Device, ctx: ID3D11DeviceContext) -> Result<Self> {
        unsafe {
            device
                .cast::<ID3D11VideoDevice>()
                .context("the device has no video support")?;
            let mt: ID3D11Multithread = ctx.cast().context("ID3D11Multithread")?;
            let _ = mt.SetMultithreadProtected(true);
            let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?;
            Ok(Self {
                id: super::next_source(),
                luid: adapter_luid(&adapter)?,
                device,
                ctx,
                staging: Mutex::new(None),
            })
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.ctx
    }

    pub fn luid(&self) -> u64 {
        self.luid
    }
}

/// 显卡的 LUID，按 `MFT_ENUM_ADAPTER_LUID` 的方式拼成 64 位（低 32 位是 LowPart）
pub(crate) fn adapter_luid(adapter: &IDXGIAdapter) -> Result<u64> {
    let desc = unsafe { adapter.GetDesc() }.context("IDXGIAdapter::GetDesc")?;
    let l = desc.AdapterLuid;
    Ok(((l.HighPart as u32 as u64) << 32) | l.LowPart as u64)
}

/// GPU 上的一帧画面：没有旋转过的 BGRA 纹理（桌面复制给出的方向），转成用户看到的方向由用的一方做
/// （视频处理器转，或者读回时转，见 [`Rotation`]）。
#[derive(Clone)]
pub struct GpuFrame {
    source: Arc<D3dSource>,
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
    rotation: Rotation,
}

// 纹理只在打开了多线程保护的设备上使用（见 D3dSource）
unsafe impl Send for GpuFrame {}
unsafe impl Sync for GpuFrame {}

impl GpuFrame {
    /// 在 `source` 的设备上新建一个 `w`×`h` 的纹理（还没有画面）
    pub(crate) fn new(source: Arc<D3dSource>, w: u32, h: u32, rotation: Rotation) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // 和桌面复制给出的纹理一样；视频处理器的输入视图要求其一
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { source.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D")?;
        Ok(Self {
            texture: tex.ok_or_else(|| anyhow!("the texture is null"))?,
            source,
            width: w,
            height: h,
            rotation,
        })
    }

    /// 采集的编号（见 [`super::next_source`]）
    pub fn source(&self) -> u64 {
        self.source.id
    }

    pub fn d3d(&self) -> &Arc<D3dSource> {
        &self.source
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// 纹理的尺寸（没有旋转的）
    pub fn texture_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// 要顺时针转多少度才是用户看到的方向（0、90、180、270）
    pub fn rotation_degrees(&self) -> u32 {
        match self.rotation {
            Rotation::None => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }

    /// 这个纹理能不能装 `source` 下一帧 `w`×`h` 的画面
    fn reusable(&self, source: &D3dSource, w: u32, h: u32) -> bool {
        self.source.id == source.id && (self.width, self.height) == (w, h)
    }

    /// 读回内存：转成用户看到的方向、紧密排列的 BGRA（`out` 的大小是转过之后的宽 × 高 × 4）
    pub(super) fn read_pixels(&self, out: &mut [u8]) -> Result<()> {
        let src = &*self.source;
        let (w, h) = (self.width, self.height);
        let mut staging = src.staging.lock().unwrap();
        let tex = match &*staging {
            Some((t, sw, sh)) if (*sw, *sh) == (w, h) => t.clone(),
            _ => {
                let t = staging_texture(&src.device, w, h)?;
                *staging = Some((t.clone(), w, h));
                t
            }
        };
        unsafe {
            src.ctx.CopyResource(&tex, &self.texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            src.ctx
                .Map(&tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map")?;
            copy_mapped(&mapped, w, h, self.rotation, out);
            src.ctx.Unmap(&tex, 0);
        }
        Ok(())
    }
}

fn staging_texture(device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
    tex.ok_or_else(|| anyhow!("staging texture is null"))
}

/// 把映射出来的 `w`×`h` 原始画面转成用户看到的方向，紧密排列地写进 `dst`
///
/// # Safety
/// `mapped` 是一个 `w`×`h` 的 BGRA 纹理映射出来的、还没有 Unmap
unsafe fn copy_mapped(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    w: u32,
    h: u32,
    rotation: Rotation,
    dst: &mut [u8],
) {
    let row = w as usize * 4;
    let pitch = mapped.RowPitch as usize;
    let src = mapped.pData as *const u8;
    assert!(dst.len() >= row * h as usize);
    unsafe {
        if rotation != Rotation::None {
            let len = pitch * (h as usize - 1) + row;
            let src = std::slice::from_raw_parts(src, len);
            copy_rotated(src, w as usize, h as usize, pitch, rotation, dst);
        } else if pitch == row {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), row * h as usize);
        } else {
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(y * pitch),
                    dst.as_mut_ptr().add(y * row),
                    row,
                );
            }
        }
    }
}

/// 在 `adapter` 上创建 D3D11 设备：先要视频支持（视频处理器、画面留在显卡上），不行再不要
pub(crate) fn create_device(
    adapter: &IDXGIAdapter1,
) -> Result<(ID3D11Device, ID3D11DeviceContext, bool)> {
    let levels: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];
    let create = |flags: D3D11_CREATE_DEVICE_FLAG| unsafe {
        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .context("D3D11CreateDevice")?;
        let device = device.ok_or_else(|| anyhow!("D3D11 device is null"))?;
        let ctx = ctx.ok_or_else(|| anyhow!("D3D11 context is null"))?;
        anyhow::Ok((device, ctx))
    };
    match create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
        Ok((d, c)) => Ok((d, c, true)),
        Err(e) => {
            tracing::debug!("no D3D11 video support: {e:#}");
            let (d, c) = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
            Ok((d, c, false))
        }
    }
}

// ───────────────────────────── 桌面复制 ─────────────────────────────

struct Output {
    adapter: IDXGIAdapter1,
    output: IDXGIOutput1,
    desc: DXGI_OUTPUT_DESC,
}

fn outputs() -> Result<Vec<Output>> {
    let mut list = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            ai += 1;
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(desc) = output.GetDesc() else { continue };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let Ok(output) = output.cast::<IDXGIOutput1>() else {
                    continue;
                };
                list.push(Output {
                    adapter: adapter.clone(),
                    output,
                    desc,
                });
            }
        }
    }
    // 主显示器排第一，其余按桌面坐标从左到右
    list.sort_by_key(|o| {
        let r = o.desc.DesktopCoordinates;
        (!is_primary(&o.desc), r.left, r.top)
    });
    Ok(list)
}

fn is_primary(desc: &DXGI_OUTPUT_DESC) -> bool {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        if GetMonitorInfoW(desc.Monitor, &mut mi).as_bool() {
            return mi.dwFlags & 1 != 0; // MONITORINFOF_PRIMARY
        }
    }
    let r = desc.DesktopCoordinates;
    r.left == 0 && r.top == 0
}

pub fn displays() -> Result<Vec<DisplayInfo>> {
    Ok(outputs()?
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let r = o.desc.DesktopCoordinates;
            let raw = String::from_utf16_lossy(&o.desc.DeviceName);
            let name = raw
                .trim_end_matches('\0')
                .trim_start_matches(r"\\.\")
                .to_string();
            DisplayInfo {
                index: i as u8,
                name,
                x: r.left,
                y: r.top,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                primary: is_primary(&o.desc),
            }
        })
        .collect())
}

pub struct DxgiCapturer {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    dup: IDXGIOutputDuplication,
    /// 画面留在显卡上（显卡支持视频处理器时）；`None` 时每帧读回内存
    gpu: Option<Arc<D3dSource>>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    first: bool,
    shape_buf: Vec<u8>,
    /// 桌面复制上一次报告的硬件光标是否可见（不可见时每一帧都用 [`software_cursor`] 再看一次）
    hw_cursor: bool,
    /// 这块显示器在桌面上的范围（物理像素）
    desktop: RECT,
    /// 显示器的旋转方向（旋转方向变了桌面复制会中断，重新打开时更新）
    rotation: Rotation,
}

// D3D11 设备只在采集线程内使用
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn open(index: u8) -> Result<Self> {
        // 没有权限打开安全桌面时（不是以 SYSTEM 运行）留在原来的桌面上，DuplicateOutput 会报 E_ACCESSDENIED
        crate::host::desktop::follow();
        let outs = outputs()?;
        let o = outs
            .get(index as usize)
            .ok_or_else(|| anyhow!("display {index} doesn't exist"))?;
        let (device, ctx, video) = create_device(&o.adapter)?;
        let gpu = if video {
            match D3dSource::new(device.clone(), ctx.clone()) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::info!("frames will be read back from the GPU: {e:#}");
                    None
                }
            }
        } else {
            tracing::info!("no D3D11 video support, frames will be read back from the GPU");
            None
        };
        unsafe {
            let dup = match o.output.cast::<IDXGIOutput5>() {
                Ok(o5) => match o5.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::debug!(
                            "DuplicateOutput1 failed ({}), trying DuplicateOutput",
                            e.code()
                        );
                        o.output
                            .DuplicateOutput(&device)
                            .context("DuplicateOutput")?
                    }
                },
                Err(_) => o
                    .output
                    .DuplicateOutput(&device)
                    .context("DuplicateOutput")?,
            };
            let desc = dup.GetDesc();
            if desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                let format = desc.ModeDesc.Format;
                bail!("unsupported desktop pixel format: {format:?}");
            }
            let rotation = Rotation::from_dxgi(desc.Rotation);
            if rotation != Rotation::None {
                tracing::info!(index, ?rotation, "the display is rotated");
            }
            Ok(Self {
                device,
                ctx,
                dup,
                gpu,
                staging: None,
                first: true,
                shape_buf: Vec::new(),
                hw_cursor: true,
                desktop: o.desc.DesktopCoordinates,
                rotation,
            })
        }
    }

    fn staging(&mut self, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if let Some((t, sw, sh)) = &self.staging
            && *sw == w
            && *sh == h
        {
            return Ok(t.clone());
        }
        let tex = staging_texture(&self.device, w, h)?;
        self.staging = Some((tex.clone(), w, h));
        Ok(tex)
    }

    /// 把桌面复制给出的纹理复制一份：显卡上的复制进帧缓冲池里的纹理，否则复制进 staging 纹理（之后读回）
    fn copy(&mut self, tex: &ID3D11Texture2D, pool: &mut FramePool) -> Result<Copied> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let (w, h) = (desc.Width, desc.Height);
        let Some(source) = &self.gpu else {
            let staging = self.staging(w, h)?;
            unsafe { self.ctx.CopyResource(&staging, tex) };
            return Ok(Copied::Staging(staging, w, h));
        };
        let (fw, fh) = self.rotation.rotated_size(w, h);
        let mut frame = pool.checkout_with(fw, fh, |g| g.reusable(source, w, h));
        let f = Arc::get_mut(&mut frame).expect("unique frame");
        if f.gpu.is_none() {
            f.gpu = Some(GpuFrame::new(source.clone(), w, h, self.rotation)?);
        }
        let g = f.gpu.as_mut().expect("just set");
        g.rotation = self.rotation;
        unsafe {
            self.ctx.CopyResource(&g.texture, tex);
            // 马上交给显卡去复制，不等用的一方提交
            self.ctx.Flush();
        }
        Ok(Copied::Gpu(frame))
    }
}

/// 这一帧复制到哪里了
enum Copied {
    Gpu(Arc<Frame>),
    Staging(ID3D11Texture2D, u32, u32),
}

impl Capturer for DxgiCapturer {
    fn next(&mut self, timeout: Duration, pool: &mut FramePool) -> Result<CaptureUpdate> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut res: Option<IDXGIResource> = None;
        if let Err(e) = unsafe {
            self.dup
                .AcquireNextFrame(timeout.as_millis() as u32, &mut info, &mut res)
        } {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(CaptureUpdate::default());
            }
            return Err(anyhow!("AcquireNextFrame: {e}"));
        }
        let mut update = CaptureUpdate::default();

        // 光标：位置由控制端本地光标负责，这里只同步可见性与形状。
        // 硬件光标不可见时不一定是隐藏了，可能是改成了画进画面里的软件光标（见 software_cursor）：
        // 这期间每一帧都再看一次，软件光标期间程序隐藏了光标（画面会变）也能发现
        if info.LastMouseUpdateTime != 0 {
            self.hw_cursor = info.PointerPosition.Visible.as_bool();
        }
        if info.LastMouseUpdateTime != 0 || !self.hw_cursor {
            update.cursor_visibility = Some(if self.hw_cursor {
                CursorVisibility::Shown
            } else {
                software_cursor(&self.desktop)
            });
        }
        if info.PointerShapeBufferSize > 0 {
            update.cursor_shape = self.pointer_shape(info.PointerShapeBufferSize);
        }

        // 仅鼠标移动（画面未变）时 LastPresentTime 为 0
        let copied = if info.LastPresentTime == 0 && !self.first {
            Ok(None)
        } else {
            res.take()
                .ok_or_else(|| anyhow!("desktop resource is null"))
                .and_then(|r| Ok(r.cast::<ID3D11Texture2D>()?))
                .and_then(|tex| self.copy(&tex, pool))
                .map(Some)
        };
        drop(res);
        // 尽早释放，让 DWM 继续合成
        let _ = unsafe { self.dup.ReleaseFrame() };
        let Some(copied) = copied? else {
            return Ok(update);
        };
        self.first = false;

        let mut frame = match copied {
            Copied::Gpu(frame) => frame,
            Copied::Staging(staging, w, h) => {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    self.ctx
                        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                }
                .context("Map")?;
                let (fw, fh) = self.rotation.rotated_size(w, h);
                let mut frame = pool.checkout(fw, fh);
                let f = Arc::get_mut(&mut frame).expect("unique frame");
                unsafe {
                    copy_mapped(&mapped, w, h, self.rotation, f.pixels_mut());
                    self.ctx.Unmap(&staging, 0);
                }
                frame
            }
        };
        // 采集时间用画面真正呈现的时刻（LastPresentTime），比"复制完成的时刻"更准，
        // 声音包也用同一个时钟（QPC），音画同步就靠这两个时间戳对齐
        if let Some(t) = crate::host::clock::from_qpc(info.LastPresentTime) {
            Arc::get_mut(&mut frame).expect("unique frame").captured = t;
        }
        pool.keep(&frame);
        update.frame = Some(frame);
        Ok(update)
    }
}

/// 显示器的旋转方向（`DXGI_OUTDUPL_DESC::Rotation`）。
///
/// 显示器旋转后，桌面复制给出的画面仍然是没有旋转的方向（例如 1080×1920 的竖屏给出 1920×1080 的画面），
/// 需要自己转过来。对应关系与微软的 DXGIDesktopDuplication 示例（DisplayManager::SetDirtyVert）相同。
/// 光标的形状和位置已经是用户看到的方向，不用转。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rotation {
    None,
    R90,
    R180,
    R270,
}

impl Rotation {
    fn from_dxgi(r: DXGI_MODE_ROTATION) -> Self {
        match r {
            DXGI_MODE_ROTATION_ROTATE90 => Self::R90,
            DXGI_MODE_ROTATION_ROTATE180 => Self::R180,
            DXGI_MODE_ROTATION_ROTATE270 => Self::R270,
            _ => Self::None,
        }
    }

    /// `w`×`h` 的原始画面转过来之后的尺寸
    fn rotated_size(self, w: u32, h: u32) -> (u32, u32) {
        match self {
            Self::R90 | Self::R270 => (h, w),
            Self::None | Self::R180 => (w, h),
        }
    }
}

/// 把 `sw`×`sh` 的原始 BGRA 画面（每行 `pitch` 字节）转成用户看到的方向，紧密排列地写进 `dst`。
///
/// 按 32×32 的小块处理：转 90° 时读源图是按列跳着读的，分块后读到的缓存行能在块内用完。
fn copy_rotated(src: &[u8], sw: usize, sh: usize, pitch: usize, rot: Rotation, dst: &mut [u8]) {
    let (dw, dh) = match rot {
        Rotation::R90 | Rotation::R270 => (sh, sw),
        Rotation::None | Rotation::R180 => (sw, sh),
    };
    assert!(sw > 0 && sh > 0 && pitch >= sw * 4);
    assert!(src.len() >= pitch * (sh - 1) + sw * 4 && dst.len() >= dw * dh * 4);
    // 目标像素 (dx, dy) 来自源图的哪个像素（字节偏移）
    let source = |dx: usize, dy: usize| -> usize {
        let (sx, sy) = match rot {
            Rotation::None => (dx, dy),
            Rotation::R90 => (dy, dw - 1 - dx),
            Rotation::R180 => (sw - 1 - dx, sh - 1 - dy),
            Rotation::R270 => (dh - 1 - dy, dx),
        };
        sy * pitch + sx * 4
    };
    // 目标向右走一个像素时，源图的字节偏移怎么变
    let step: isize = match rot {
        Rotation::None => 4,
        Rotation::R90 => -(pitch as isize),
        Rotation::R180 => -4,
        Rotation::R270 => pitch as isize,
    };
    const TILE: usize = 32;
    for ty in (0..dh).step_by(TILE) {
        for tx in (0..dw).step_by(TILE) {
            let tw = TILE.min(dw - tx);
            for dy in ty..(ty + TILE).min(dh) {
                let mut s = source(tx, dy) as isize;
                let row = &mut dst[(dy * dw + tx) * 4..(dy * dw + tx + tw) * 4];
                for px in row.as_chunks_mut::<4>().0 {
                    let i = s as usize;
                    px.copy_from_slice(&src[i..i + 4]);
                    s += step;
                }
            }
        }
    }
}

impl DxgiCapturer {
    fn pointer_shape(&mut self, size: u32) -> Option<CursorShape> {
        self.shape_buf.resize(size as usize, 0);
        let mut required = 0u32;
        let mut si = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        unsafe {
            self.dup
                .GetFramePointerShape(
                    size,
                    self.shape_buf.as_mut_ptr().cast(),
                    &mut required,
                    &mut si,
                )
                .ok()?;
        }
        convert_pointer(&self.shape_buf, &si)
    }
}

/// 硬件光标不可见时，光标到底怎么样了（`desktop`：这块显示器在桌面上的范围）。
///
/// 桌面复制的 `PointerPosition.Visible` 说的是**硬件光标**：拖动窗口、改变窗口大小时（还有放大镜、
/// 某些显卡驱动和虚拟显示器）系统改用软件光标，直接画进桌面画面里，这时它也是 FALSE，光标却没有隐藏。
/// 把它当成隐藏，控制端就会改发相对移动、锁定指针，每拖一次窗口锁一次（浏览器每次都提示"按 Esc 显示光标"）。
/// 用 `GetCursorInfo` 区分：
///
/// * 游戏等真的隐藏了光标（`ShowCursor(FALSE)`、`SetCursor(NULL)`），它也说不显示：隐藏；
/// * 显示着、在这块显示器上：画在画面里了，控制端不再显示本地光标（不然有两个）；
/// * 显示着、在别的显示器上（这块显示器的硬件光标也是不可见）：控制端照常显示本地光标、发绝对坐标，
///   一动就回到这块屏幕上。
///
/// 采集线程跟着输入桌面走（见 [`crate::host::desktop`]），看到的是用户正在用的那个桌面的光标；
/// 线程按每显示器 DPI 感知运行，坐标和 `DesktopCoordinates` 一样是物理像素
fn software_cursor(desktop: &RECT) -> CursorVisibility {
    let mut ci = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut ci) }.is_err()
        || ci.flags.0 & CURSOR_SHOWING.0 == 0
        || ci.flags.0 & CURSOR_SUPPRESSED.0 != 0
        || ci.hCursor.is_invalid()
    {
        return CursorVisibility::Hidden;
    }
    let p = ci.ptScreenPos;
    if (desktop.left..desktop.right).contains(&p.x) && (desktop.top..desktop.bottom).contains(&p.y)
    {
        CursorVisibility::InFrame
    } else {
        CursorVisibility::Shown
    }
}

/// DXGI 光标形状 → RGBA。
fn convert_pointer(buf: &[u8], si: &DXGI_OUTDUPL_POINTER_SHAPE_INFO) -> Option<CursorShape> {
    let (w, pitch) = (si.Width as usize, si.Pitch as usize);
    let ty = si.Type as i32;
    let h = if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        si.Height as usize / 2
    } else {
        si.Height as usize
    };
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    if ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
        // AND 掩码在上半部分，XOR 掩码在下半部分，各 1bpp
        if buf.len() < pitch * h * 2 {
            return None;
        }
        let bit = |row: usize, x: usize| (buf[row * pitch + x / 8] >> (7 - (x % 8))) & 1;
        let mut invert = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let (and, xor) = (bit(y, x), bit(y + h, x));
                let o = (y * w + x) * 4;
                match (and, xor) {
                    (0, 0) => rgba[o..o + 4].copy_from_slice(&[0, 0, 0, 255]),
                    (0, _) => rgba[o..o + 4].copy_from_slice(&[255, 255, 255, 255]),
                    (_, 0) => {}
                    _ => invert[y * w + x] = true,
                }
            }
        }
        outline_inverted(&mut rgba, &invert, w, h);
    } else {
        if buf.len() < pitch * h {
            return None;
        }
        let masked = ty == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0;
        let mut invert = vec![false; w * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * pitch + x * 4;
                let (b, g, r, a) = (buf[i], buf[i + 1], buf[i + 2], buf[i + 3]);
                let o = (y * w + x) * 4;
                if masked {
                    // alpha=0：直接显示颜色；alpha=0xFF：与屏幕异或（颜色为 0 则透明）
                    if a == 0 {
                        rgba[o..o + 4].copy_from_slice(&[r, g, b, 255]);
                    } else if r | g | b != 0 {
                        invert[y * w + x] = true;
                    }
                } else {
                    rgba[o..o + 4].copy_from_slice(&[r, g, b, a]);
                }
            }
        }
        if masked {
            outline_inverted(&mut rgba, &invert, w, h);
        }
    }
    Some(CursorShape {
        width: w as u16,
        height: h as u16,
        hot_x: si.HotSpot.x.clamp(0, w as i32 - 1) as u16,
        hot_y: si.HotSpot.y.clamp(0, h as i32 - 1) as u16,
        rgba,
    })
}

/// "反色"像素无法用普通图片表示：画成黑色并加白色描边，在任何背景上都清晰可见。
fn outline_inverted(rgba: &mut [u8], invert: &[bool], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            if !invert[y * w + x] {
                continue;
            }
            let o = (y * w + x) * 4;
            rgba[o..o + 4].copy_from_slice(&[0, 0, 0, 255]);
            for (dx, dy) in [(-1i32, 0i32), (1, 0), (0, -1), (0, 1)] {
                let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let n = ny as usize * w + nx as usize;
                if !invert[n] && rgba[n * 4 + 3] == 0 {
                    rgba[n * 4..n * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `w`×`h` 的测试画面：每个像素记着自己的坐标，每行末尾有 `pad` 字节填充（模拟 RowPitch）
    fn image(w: usize, h: usize, pad: usize) -> (Vec<u8>, usize) {
        let pitch = w * 4 + pad;
        let mut v = vec![0xEE; pitch * h];
        for y in 0..h {
            for x in 0..w {
                v[y * pitch + x * 4..][..4].copy_from_slice(&[x as u8, y as u8, 0x5A, 0xFF]);
            }
        }
        (v, pitch)
    }

    fn rotate(
        src: &[u8],
        w: usize,
        h: usize,
        pitch: usize,
        rot: Rotation,
    ) -> (Vec<u8>, usize, usize) {
        let (dw, dh) = rot.rotated_size(w as u32, h as u32);
        let mut dst = vec![0; dw as usize * dh as usize * 4];
        copy_rotated(src, w, h, pitch, rot, &mut dst);
        (dst, dw as usize, dh as usize)
    }

    /// 目标图 (x, y) 处的像素来自原图的哪个坐标
    fn from(img: &[u8], w: usize, x: usize, y: usize) -> (u8, u8) {
        let p = &img[(y * w + x) * 4..][..4];
        (p[0], p[1])
    }

    /// 与微软示例的对应关系一致：转 90° 时画面顺时针转，原图左下角到左上角；转 270° 逆时针转
    #[test]
    fn corners() {
        let (w, h) = (3, 2);
        let (src, pitch) = image(w, h, 8);

        let (d, dw, dh) = rotate(&src, w, h, pitch, Rotation::R90);
        assert_eq!((dw, dh), (2, 3));
        assert_eq!(from(&d, dw, 0, 0), (0, 1));
        assert_eq!(from(&d, dw, 1, 0), (0, 0));
        assert_eq!(from(&d, dw, 0, 2), (2, 1));

        let (d, dw, dh) = rotate(&src, w, h, pitch, Rotation::R270);
        assert_eq!((dw, dh), (2, 3));
        assert_eq!(from(&d, dw, 0, 0), (2, 0));
        assert_eq!(from(&d, dw, 1, 2), (0, 1));

        let (d, dw, dh) = rotate(&src, w, h, pitch, Rotation::R180);
        assert_eq!((dw, dh), (3, 2));
        assert_eq!(from(&d, dw, 0, 0), (2, 1));
        assert_eq!(from(&d, dw, 2, 1), (0, 0));

        let (d, ..) = rotate(&src, w, h, pitch, Rotation::None);
        let (tight, _) = image(w, h, 0);
        assert_eq!(d, tight);
    }

    /// 跨多个分块、宽高不是分块大小的整数倍：90° 再 270° 回到原图，90° 两次等于 180°
    #[test]
    fn round_trip() {
        let (w, h) = (77, 45);
        let (src, pitch) = image(w, h, 12);
        let (tight, _) = image(w, h, 0);

        let (r90, w90, h90) = rotate(&src, w, h, pitch, Rotation::R90);
        let (back, ..) = rotate(&r90, w90, h90, w90 * 4, Rotation::R270);
        assert_eq!(back, tight);

        let (twice, ..) = rotate(&r90, w90, h90, w90 * 4, Rotation::R90);
        let (r180, ..) = rotate(&src, w, h, pitch, Rotation::R180);
        assert_eq!(twice, r180);
    }

    /// GPU 上的画面按需读回：和每帧读回一样转成用户看到的方向；只读回一次，帧缓冲池复用纹理和读回的缓冲
    #[test]
    fn gpu_frames_read_back_on_demand() {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.unwrap();
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(0) }) else {
            return;
        };
        let Ok((device, ctx, true)) = create_device(&adapter) else {
            println!("no D3D11 video support, skipping");
            return;
        };
        let source = Arc::new(D3dSource::new(device, ctx).unwrap());
        let (w, h) = (77usize, 45usize);
        let (img, _) = image(w, h, 0);
        let mut pool = FramePool::default();
        for rot in [
            Rotation::None,
            Rotation::R90,
            Rotation::R180,
            Rotation::R270,
        ] {
            let (fw, fh) = rot.rotated_size(w as u32, h as u32);
            let mut frame = pool.checkout_with(fw, fh, |g| g.reusable(&source, w as u32, h as u32));
            let f = Arc::get_mut(&mut frame).unwrap();
            if f.gpu.is_none() {
                f.gpu = Some(GpuFrame::new(source.clone(), w as u32, h as u32, rot).unwrap());
            }
            let g = f.gpu.as_mut().unwrap();
            g.rotation = rot;
            unsafe {
                source.ctx.UpdateSubresource(
                    &g.texture,
                    0,
                    None,
                    img.as_ptr().cast(),
                    (w * 4) as u32,
                    0,
                );
            }
            let first = frame.pixels().unwrap().as_ptr();
            let (want, ..) = rotate(&img, w, h, w * 4, rot);
            assert_eq!(frame.pixels().unwrap(), want, "{rot:?}");
            assert_eq!(frame.pixels().unwrap().as_ptr(), first, "read back twice");
            pool.keep(&frame);
        }
        // 池里只有一帧：同一个纹理一直在用
        assert_eq!(pool.frames.len(), 1);
    }
}
