//! 双显卡电脑（核显 + 独显的笔记本、插了两块显卡的台式机）上让桌面复制找对显卡。
//!
//! # 问题
//!
//! Windows 给每个进程定一个"显卡偏好"（设置 → 图形里按程序选的、电源设置、显卡驱动的切换配置），DXGI 第一次用到时
//! 经 `win32u!NtGdiDdDDIGetCachedHybridQueryValue` 问内核要（`D3DKMT_GPU_PREFERENCE_QUERY_STATE`）。有偏好时 DXGI
//! 把偏好的显卡排在第一个，偏好的显卡下面没有显示器时（独显只负责渲染，屏幕接在核显上）还把显示器"搬"过去：在它下面
//! 放一个虚拟输出，真正接着这块显示器的显卡下面就看不到它了（`dxgi!ReparentOutputs`）。普通程序的画面 DXGI 会转交给
//! 真正的显卡，桌面复制却不会：对虚拟输出 `DuplicateOutput` 一定是 `DXGI_ERROR_UNSUPPORTED`（0x887A0004），换成真正那块
//! 显卡上的设备也不行（见微软的 KB3019314）。
//!
//! 只选一种偏好也解决不了：显示器分别接在两块显卡上时（笔记本屏幕在核显上、HDMI 接的外接显示器在独显上），选哪块显卡
//! 另一块上的显示器都会被搬走。被控端还以 SYSTEM 身份运行（见 `winsvc`），用户在设置里按程序选的偏好管不到它。
//!
//! # 做法
//!
//! 和 Sunshine 一样（LizardByte/Sunshine#3530）：把本进程里的 `NtGdiDdDDIGetCachedHybridQueryValue` 换成一个总是回答
//! "没有偏好"（`D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED`）的函数。DXGI 于是不按注册表、电源设置、驱动去挑显卡，
//! 也不搬显示器：每块显示器留在真正接着它的显卡下面，在那块显卡上建的设备（[`super::create_device`]）就能复制它。
//!
//! * 要在本进程第一次用 DXGI 之前换掉（被控端一启动就换，见 [`ignore_gpu_preference`] 的调用处）；
//! * 只改本进程里的这一份代码（系统 DLL 的代码页写时复制），别的进程不受影响；
//! * 本进程不再听用户的显卡偏好：被控端只用 DXGI 采集和编码，本来就该用接着显示器的那块显卡；
//! * 这是没有公开的系统接口，以后的 Windows 可能改掉。函数不在、开头不是预期的系统调用代码（别的软件已经改过、
//!   不是 x64）时什么也不做，和以前一样。

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

/// `D3DKMT_GPU_PREFERENCE_STATE_UNSPECIFIED`（d3dkmthk.h 的 `D3DKMT_GPU_PREFERENCE_QUERY_STATE`）
const UNSPECIFIED: i32 = 3;

const STATUS_SUCCESS: i32 = 0;
const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;

/// 替换 `NtGdiDdDDIGetCachedHybridQueryValue(D3DKMT_GPU_PREFERENCE_QUERY_STATE*)`：总是"没有偏好"。
/// 参数为空时和真的系统调用一样返回 `STATUS_INVALID_PARAMETER`
unsafe extern "system" fn no_preference(state: *mut i32) -> i32 {
    if state.is_null() {
        return STATUS_INVALID_PARAMETER;
    }
    unsafe { state.write(UNSPECIFIED) };
    STATUS_SUCCESS
}

/// 让本进程里的 DXGI 不管显卡偏好（见模块说明）。只在第一次调用时生效，之后什么也不做
pub fn ignore_gpu_preference() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| match install() {
        Ok(()) => info!("DXGI ignores the GPU preference of this process"),
        Err(e) => warn!(
            "can't make DXGI ignore the GPU preference, desktop duplication may fail on hybrid GPUs: {e:#}"
        ),
    });
}

/// 换掉 `win32u!NtGdiDdDDIGetCachedHybridQueryValue`
#[cfg(target_arch = "x86_64")]
fn install() -> Result<()> {
    use windows::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::System::Memory::{
        PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;
    use windows::core::{s, w};

    unsafe {
        // 不释放：改过的代码要一直留在进程里
        let module = LoadLibraryW(w!("win32u.dll")).context("LoadLibrary(win32u.dll)")?;
        let Some(original) = GetProcAddress(module, s!("NtGdiDdDDIGetCachedHybridQueryValue"))
        else {
            // Windows 10 1803 之前没有显卡偏好
            bail!("NtGdiDdDDIGetCachedHybridQueryValue doesn't exist");
        };
        let target = original as usize as *mut u8;
        // x64 的系统调用入口：mov r10, rcx; mov eax, <编号>; ……; syscall; ret。不是这样的（被别的软件改过、
        // ARM64 上模拟运行的 x64 进程看到的转接代码）不动它
        let head = std::slice::from_raw_parts(target, 4);
        if head != [0x4C, 0x8B, 0xD1, 0xB8] {
            bail!("NtGdiDdDDIGetCachedHybridQueryValue starts with {head:02X?}, not a system call");
        }
        // mov rax, <no_preference>; jmp rax（12 字节，系统调用入口有 24 字节）。不用再调用原来的函数，不需要跳板
        let hook: unsafe extern "system" fn(*mut i32) -> i32 = no_preference;
        let mut patch = [0u8; 12];
        patch[..2].copy_from_slice(&[0x48, 0xB8]);
        patch[2..10].copy_from_slice(&(hook as usize as u64).to_le_bytes());
        patch[10..].copy_from_slice(&[0xFF, 0xE0]);

        let mut old = PAGE_PROTECTION_FLAGS::default();
        VirtualProtect(target.cast(), patch.len(), PAGE_EXECUTE_READWRITE, &mut old)
            .context("VirtualProtect")?;
        std::ptr::copy_nonoverlapping(patch.as_ptr(), target, patch.len());
        let mut unused = PAGE_PROTECTION_FLAGS::default();
        let _ = VirtualProtect(target.cast(), patch.len(), old, &mut unused);
        let _ = FlushInstructionCache(GetCurrentProcess(), Some(target.cast()), patch.len());
        Ok(())
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn install() -> Result<()> {
    bail!("only x64 is supported")
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::E_ACCESSDENIED;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1};
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{Interface, s, w};

    /// 换掉之后，直接调用和经 gdi32 的 `D3DKMTGetCachedHybridQueryValue`（转到同一段代码）都回答"没有偏好"
    #[test]
    fn answers_unspecified() {
        super::ignore_gpu_preference();
        type QueryFn = unsafe extern "system" fn(*mut i32) -> i32;
        unsafe {
            let win32u = LoadLibraryW(w!("win32u.dll")).unwrap();
            let Some(f) = GetProcAddress(win32u, s!("NtGdiDdDDIGetCachedHybridQueryValue")) else {
                println!("no NtGdiDdDDIGetCachedHybridQueryValue on this system");
                return;
            };
            let f: QueryFn = std::mem::transmute(f);
            let mut state = 0;
            assert_eq!(f(&mut state), super::STATUS_SUCCESS);
            assert_eq!(state, super::UNSPECIFIED);
            assert_eq!(f(std::ptr::null_mut()), super::STATUS_INVALID_PARAMETER);

            let gdi32 = LoadLibraryW(w!("gdi32.dll")).unwrap();
            if let Some(f) = GetProcAddress(gdi32, s!("D3DKMTGetCachedHybridQueryValue")) {
                let f: QueryFn = std::mem::transmute(f);
                let mut state = 0;
                assert_eq!(f(&mut state), super::STATUS_SUCCESS);
                assert_eq!(state, super::UNSPECIFIED);
            }
        }
    }

    /// 每块显卡下面的每个显示器都能桌面复制。双显卡电脑上先在"设置 → 系统 → 屏幕 → 显示卡"里给测试程序
    /// （`target\debug\deps\airhand_lib-*.exe`）选高性能或者节能再跑；设了 `AIRHAND_KEEP_GPU_PREFERENCE` 时不换掉
    /// 显卡偏好，对照着看。`cargo test --lib duplicates_every_output -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn duplicates_every_output() {
        if std::env::var_os("AIRHAND_KEEP_GPU_PREFERENCE").is_none() {
            super::ignore_gpu_preference();
        }
        let name = |chars: &[u16]| {
            String::from_utf16_lossy(chars)
                .trim_end_matches('\0')
                .to_string()
        };
        let mut failed = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().unwrap();
            let mut ai = 0;
            while let Ok(adapter) = factory.EnumAdapters1(ai) {
                ai += 1;
                let desc = adapter.GetDesc1().unwrap();
                println!("adapter {}: {}", ai - 1, name(&desc.Description));
                let mut oi = 0;
                while let Ok(output) = adapter.EnumOutputs(oi) {
                    oi += 1;
                    let d = output.GetDesc().unwrap();
                    let display = name(&d.DeviceName);
                    if !d.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let (device, ..) = super::super::create_device(&adapter).unwrap();
                    match output
                        .cast::<IDXGIOutput1>()
                        .unwrap()
                        .DuplicateOutput(&device)
                    {
                        Ok(_) => println!("  {display}: ok"),
                        // 安全桌面（UAC、锁屏）上不是 SYSTEM 就没有权限，和显卡无关
                        Err(e) if e.code() == E_ACCESSDENIED => {
                            println!("  {display}: access denied")
                        }
                        Err(e) => {
                            println!("  {display}: {}", e.code());
                            failed.push(display);
                        }
                    }
                }
            }
        }
        assert!(failed.is_empty(), "can't duplicate {failed:?}");
    }
}
