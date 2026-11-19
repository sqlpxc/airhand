//! DXGI 桌面纹理 → 采集管线共用的 BGRA8。只在确实收到 FP16 帧时创建着色器。

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::Display::*;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{PCSTR, s};

pub(super) struct DesktopPixels {
    display: [u16; 32],
    hdr: Option<HdrToSdr>,
}

impl DesktopPixels {
    pub(super) fn new(display: [u16; 32]) -> Self {
        Self { display, hdr: None }
    }

    /// `desc` 是这一帧的纹理描述。`dst` 是帧缓冲池的 BGRA8 纹理或 CPU 读回用的 staging 纹理。
    pub(super) fn copy(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        dst: &ID3D11Texture2D,
        readback: bool,
    ) -> Result<()> {
        match desc.Format {
            DXGI_FORMAT_B8G8R8A8_UNORM => {
                unsafe { ctx.CopyResource(dst, src) };
                Ok(())
            }
            DXGI_FORMAT_R16G16B16A16_FLOAT => {
                let hdr = match &mut self.hdr {
                    Some(hdr) => hdr,
                    slot @ None => {
                        let white = sdr_white(&self.display).unwrap_or(1.0);
                        tracing::info!(
                            sdr_white_nits = white * 80.0,
                            "converting FP16 desktop to SDR on the GPU"
                        );
                        slot.insert(HdrToSdr::new(device, white)?)
                    }
                };
                hdr.draw(device, ctx, src, desc, dst, readback)
            }
            format => bail!("unsupported desktop pixel format: {format:?}"),
        }
    }
}

/// Windows HDR 桌面用线性 scRGB，1.0 代表 80 nit。普通窗口的白色跟着这块屏的“SDR 内容亮度”走。
fn sdr_white(display: &[u16; 32]) -> Option<f32> {
    unsafe {
        let (mut npaths, mut nmodes) = (0, 0);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut npaths, &mut nmodes)
            .ok()
            .ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); npaths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nmodes as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut npaths,
            paths.as_mut_ptr(),
            &mut nmodes,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;
        for path in &paths[..npaths as usize] {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0
                || source.viewGdiDeviceName.split(|&c| c == 0).next()
                    != display.split(|&c| c == 0).next()
            {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                SDRWhiteLevel: 0,
            };
            if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                return Some(white.SDRWhiteLevel as f32 / 1000.0);
            }
        }
    }
    None
}

struct HdrToSdr {
    // 编码器也用同一个立即上下文。用独立上下文录好一次 draw，再整批提交并恢复立即上下文状态。
    commands: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    white: ID3D11Buffer,
    // 有的采集纹理不能直接建 SRV，staging 纹理也不能当渲染目标。这两种情况才需要中间纹理。
    sampleable: Option<(u32, u32, ID3D11Texture2D)>,
    renderable: Option<(u32, u32, ID3D11Texture2D)>,
}

fn compile(entry: PCSTR, profile: PCSTR) -> Result<Vec<u8>> {
    let source = include_bytes!("hdr.hlsl");
    let mut code = None;
    unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("hdr.hlsl"),
            None,
            None,
            entry,
            profile,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            None,
        )
        .context("compile HDR desktop shader")?;
        let code = code.context("empty shader bytecode")?;
        Ok(
            std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize())
                .to_vec(),
        )
    }
}

impl HdrToSdr {
    fn new(device: &ID3D11Device, sdr_white: f32) -> Result<Self> {
        let (mut commands, mut vertex, mut pixel, mut white) = (None, None, None, None);
        let constants = [1.0 / sdr_white, 0.0, 0.0, 0.0];
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of_val(&constants) as u32,
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: constants.as_ptr().cast(),
            ..Default::default()
        };
        unsafe {
            device
                .CreateDeferredContext(0, Some(&mut commands))
                .context("HDR deferred context")?;
            device
                .CreateVertexShader(
                    &compile(s!("vertex"), s!("vs_4_0"))?,
                    None,
                    Some(&mut vertex),
                )
                .context("HDR vertex shader")?;
            device
                .CreatePixelShader(&compile(s!("pixel"), s!("ps_4_0"))?, None, Some(&mut pixel))
                .context("HDR pixel shader")?;
            device
                .CreateBuffer(&desc, Some(&data), Some(&mut white))
                .context("HDR SDR white level")?;
        }
        Ok(Self {
            commands: commands.context("null deferred context")?,
            vertex: vertex.context("null vertex shader")?,
            pixel: pixel.context("null pixel shader")?,
            white: white.context("null constant buffer")?,
            sampleable: None,
            renderable: None,
        })
    }

    fn draw(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        desc: &D3D11_TEXTURE2D_DESC,
        dst: &ID3D11Texture2D,
        readback: bool,
    ) -> Result<()> {
        let (w, h) = (desc.Width, desc.Height);
        let input = if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0 {
            Some(scratch(
                &mut self.sampleable,
                device,
                w,
                h,
                desc.Format,
                D3D11_BIND_SHADER_RESOURCE,
            )?)
        } else {
            None
        };
        let output = if readback {
            Some(scratch(
                &mut self.renderable,
                device,
                w,
                h,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_RENDER_TARGET,
            )?)
        } else {
            None
        };
        unsafe {
            let mut srv = None;
            device
                .CreateShaderResourceView(input.as_ref().unwrap_or(src), None, Some(&mut srv))
                .context("HDR desktop input view")?;
            let mut rtv = None;
            device
                .CreateRenderTargetView(output.as_ref().unwrap_or(dst), None, Some(&mut rtv))
                .context("SDR desktop output view")?;

            let cmd = &self.commands;
            if let Some(input) = &input {
                cmd.CopyResource(input, src);
            }
            cmd.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            cmd.RSSetViewports(Some(&[D3D11_VIEWPORT {
                Width: w as f32,
                Height: h as f32,
                MaxDepth: 1.0,
                ..Default::default()
            }]));
            cmd.VSSetShader(&self.vertex, None);
            cmd.PSSetShader(&self.pixel, None);
            cmd.PSSetConstantBuffers(0, Some(&[Some(self.white.clone())]));
            cmd.PSSetShaderResources(0, Some(&[srv]));
            cmd.OMSetRenderTargets(Some(&[rtv]), None);
            cmd.Draw(3, 0);
            if let Some(output) = &output {
                cmd.OMSetRenderTargets(None, None);
                cmd.CopyResource(dst, output);
            }
            let mut list = None;
            cmd.FinishCommandList(false, Some(&mut list))
                .context("record HDR desktop conversion")?;
            ctx.ExecuteCommandList(&list.context("null HDR command list")?, true);
        }
        Ok(())
    }
}

fn scratch(
    slot: &mut Option<(u32, u32, ID3D11Texture2D)>,
    device: &ID3D11Device,
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Texture2D> {
    if let Some((sw, sh, tex)) = slot
        && (*sw, *sh) == (w, h)
    {
        return Ok(tex.clone());
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        ..Default::default()
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
        .context("HDR scratch texture")?;
    let tex = tex.context("null HDR scratch texture")?;
    *slot = Some((w, h, tex.clone()));
    Ok(tex)
}

#[cfg(test)]
mod tests {
    use super::super::{Rotation, copy_mapped, staging_texture};
    use super::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;

    /// 用软件 D3D11 设备构造真的 FP16 桌面纹理，不依赖测试机的显示器是否打开 HDR。
    #[test]
    fn fp16_desktop_to_bgra() -> Result<()> {
        let (mut device, mut ctx) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )?;
        }
        let (device, ctx) = (device.unwrap(), ctx.unwrap());
        let (w, h) = (3, 2);
        // FP16 的 [0, 0.5, 1]、参考白 2 (= 160 nit)、强光 8、负值、黑、0.125 的红色。
        let pixels: [[u16; 4]; 6] = [
            [0, 0x3800, 0x3c00, 0],
            [0x4000; 4],
            [0x4800; 4],
            [0xbc00, 0, 0x3800, 0],
            [0; 4],
            [0x3000, 0, 0, 0],
        ];
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: w * 8,
            ..Default::default()
        };
        let staging = staging_texture(&device, w, h)?;
        let target = scratch(
            &mut None,
            &device,
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_RENDER_TARGET,
        )?;
        let mut desktop = DesktopPixels {
            display: [0; 32],
            hdr: Some(HdrToSdr::new(&device, 2.0)?),
        };
        for bind in [D3D11_BIND_SHADER_RESOURCE, D3D11_BIND_FLAG(0)] {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind.0 as u32,
                ..Default::default()
            };
            let mut src = None;
            unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut src))? };
            let src = src.unwrap();
            for readback in [false, true] {
                // 清空上次的输出，以防这一次没有真正渲染也误通过。
                let zeros = [0u8; 24];
                unsafe {
                    ctx.UpdateSubresource(&target, 0, None, zeros.as_ptr().cast(), w * 4, 0);
                    ctx.CopyResource(&staging, &target);
                }
                desktop.copy(
                    &device,
                    &ctx,
                    &src,
                    &desc,
                    if readback { &staging } else { &target },
                    readback,
                )?;
                let mut result = [0u8; 24];
                unsafe {
                    if !readback {
                        ctx.CopyResource(&staging, &target);
                    }
                    let mut map = D3D11_MAPPED_SUBRESOURCE::default();
                    ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
                    copy_mapped(&map, w, h, Rotation::None, &mut result);
                    ctx.Unmap(&staging, 0);
                }
                let expected: [u8; 24] = [
                    188, 137, 0, 255, 240, 240, 240, 255, 253, 253, 253, 255, 137, 0, 0, 255, 0, 0,
                    0, 255, 0, 0, 71, 255,
                ];
                assert!(
                    result.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 1),
                    "{bind:?}, readback={readback}: {result:?}"
                );
            }
        }
        Ok(())
    }
}
