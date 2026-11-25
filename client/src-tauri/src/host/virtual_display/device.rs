//! 虚拟显示器的设备：MttVDD（Virtual Display Driver，<https://github.com/VirtualDrivers/Virtual-Display-Driver>，MIT 许可）。
//!
//! 驱动随安装包带着（`windows/vdd/`，见那里的 README），为所有用户安装时安装程序问用户要不要装，
//! 装的时候运行 `airhand.exe --install-vdd`，卸载时可以选择一起删除（`--uninstall-vdd`，见 `crate::winsvc`）。
//!
//! * 设备是 Airhand 自己建的一个根设备 [`INSTANCE`]，硬件 ID 是驱动的 INF 认的 [`HARDWARE_ID`]。平时**停用**着：
//!   MttVDD 启用着就至少有一个虚拟显示器（个数写 0 也当成 1），会多出一块看不见的扩展屏，鼠标、窗口都会跑过去。
//!   要用的时候启用、用完停用（[`set_enabled`]，要管理员权限：被控端以 SYSTEM 身份运行）。停用是持久的：
//!   重启之后还停用着；
//! * 驱动启用时读设置文件 `vdd_settings.xml`，所在的目录在注册表 [`DRIVER_KEY`] 的 `VDDPATH` 里（不设置时是
//!   `C:\VirtualDisplayDriver`）。安装时设成 [`settings_dir`]，被控端每次启用之前重写它（[`write_settings`]）：
//!   一个显示器，想要的分辨率排第一（驱动把第一个当作首选，第一次见到它的 Windows 就用这个）；
//! * 安装时在 [`RECORD`] 记下这是 Airhand 装的设备。这台电脑上已经有别的 MttVDD 设备（例如 Sunshine 用的）时不装、
//!   也不用它：同一个驱动的两个设备会抢同一个设置文件和命名管道，启用、停用也会打断别人的虚拟显示器；
//! * 驱动的签名是 SignPath Foundation 的代码签名证书（不是微软的）：安装时 Windows 会问用户要不要安装这个发布者的
//!   设备软件（所以由安装程序在前台装，不由服务在后台悄悄装）。Windows 11 的 ARM64 版只认微软签名的驱动，装不上。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use tracing::{info, warn};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, ERROR_DEVINST_ALREADY_EXISTS, ERROR_NO_MORE_ITEMS, HANDLE,
    HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_QUERY,
    TokenElevation,
};
use windows::Win32::Storage::FileSystem::CreateDirectoryW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR};
use winreg::RegKey;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE};

use super::Mode;

/// 驱动的 INF 认的硬件 ID
pub const HARDWARE_ID: &str = r"Root\MttVDD";
/// Airhand 建的设备（固定的实例 ID：重装、升级都是同一个设备，Windows 记着的显示设置也接着用）
pub const INSTANCE: &str = r"ROOT\AIRHANDVIRTUALDISPLAY\0000";
/// Airhand 记下的安装信息：`Instance`（是 Airhand 装的）、`Isolated`（防窥撤下了本机的显示器，还没恢复）。
/// 安装程序还在这里记 `Asked`（问过用户要不要装了，见 `windows/hooks.nsh`）
pub const RECORD: &str = r"SOFTWARE\Airhand\VirtualDisplay";
/// 驱动读的注册表：`VDDPATH` 是设置文件所在的目录
const DRIVER_KEY: &str = r"SOFTWARE\MikeTheTech\VirtualDisplayDriver";
/// 设置文件所在的目录：SYSTEM 和管理员能改，别人（包括运行驱动的 LOCAL SERVICE）只能读
const SETTINGS_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;WD)";

/// 设置文件所在的目录：`%ProgramData%\Airhand\VirtualDisplay`
pub fn settings_dir() -> PathBuf {
    let data = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(data).join("Airhand").join("VirtualDisplay")
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// 本进程有管理员权限（被控端是 SYSTEM；仅为当前用户安装时的托盘进程没有，用不了虚拟显示器）
pub(super) fn elevated() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut e = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut e as *mut _ as *mut core::ffi::c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && e.TokenIsElevated != 0
    }
}

/// 记下的是 Airhand 装的这个设备
fn recorded() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(RECORD, KEY_READ)
        .and_then(|k| k.get_value::<String, _>("Instance"))
        .is_ok_and(|i| i.eq_ignore_ascii_case(INSTANCE))
}

/// 设备节点（不在时为 `None`）
pub(super) fn devnode() -> Option<u32> {
    let id = HSTRING::from(INSTANCE);
    let mut node = 0u32;
    let r = unsafe { CM_Locate_DevNodeW(&mut node, &id, CM_LOCATE_DEVNODE_NORMAL) };
    (r == CR_SUCCESS).then_some(node)
}

/// 能用虚拟显示器：Airhand 装的设备在，本进程有权限启用、停用它
pub(super) fn usable() -> bool {
    recorded() && devnode().is_some() && (crate::winsvc::is_agent() || elevated())
}

/// 设备启用着（不管启动成功没有）
pub(super) fn enabled() -> Result<bool> {
    let node = devnode().context("the virtual display device is gone")?;
    let (mut status, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
    let r = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, node, 0) };
    ensure!(r == CR_SUCCESS, "CM_Get_DevNode_Status failed ({})", r.0);
    Ok(!(status.contains(DN_HAS_PROBLEM) && problem == CM_PROB_DISABLED))
}

/// 打开设备（`SetupDi*` 用的设备信息集合和它在里面的那一项）
fn open() -> Result<(Owned<HDEVINFO>, SP_DEVINFO_DATA)> {
    unsafe {
        let set = Owned::new(SetupDiCreateDeviceInfoList(
            Some(&GUID_DEVCLASS_DISPLAY),
            None,
        )?);
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        SetupDiOpenDeviceInfoW(*set, &HSTRING::from(INSTANCE), None, 0, Some(&mut data))
            .context("opening the virtual display device")?;
        Ok((set, data))
    }
}

/// 启用 / 停用设备（持久的：重启之后还这样）。启用之后驱动读设置文件、报告虚拟显示器，Windows 要一两秒才认出它
pub(super) fn set_enabled(on: bool) -> Result<()> {
    let (set, data) = open()?;
    let params = SP_PROPCHANGE_PARAMS {
        ClassInstallHeader: SP_CLASSINSTALL_HEADER {
            cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
            InstallFunction: DIF_PROPERTYCHANGE,
        },
        StateChange: if on { DICS_ENABLE } else { DICS_DISABLE },
        Scope: DICS_FLAG_GLOBAL,
        HwProfile: 0,
    };
    unsafe {
        SetupDiSetClassInstallParamsW(
            *set,
            Some(&data),
            Some(&params.ClassInstallHeader),
            size_of::<SP_PROPCHANGE_PARAMS>() as u32,
        )?;
        SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, *set, Some(&data)).with_context(|| {
            format!(
                "{} the virtual display",
                if on { "enabling" } else { "disabling" }
            )
        })?;
    }
    Ok(())
}

/// 防窥撤下了本机的显示器、还没恢复（进程意外退出时下次启动按它恢复，见 `super::recover`）
pub(super) fn was_isolated() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(RECORD, KEY_READ)
        .and_then(|k| k.get_value::<u32, _>("Isolated"))
        .is_ok_and(|v| v != 0)
}

pub(super) fn set_isolated(on: bool) {
    let r = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(RECORD, KEY_WRITE)
        .and_then(|k| {
            if on {
                k.set_value("Isolated", &1u32)
            } else {
                match k.delete_value("Isolated") {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    r => r,
                }
            }
        });
    if let Err(e) = r {
        warn!("can't record the privacy mode state: {e}");
    }
}

/// 常见的分辨率：想要的那个之外也给 Windows 几个可选的（控制端在显示设置里换分辨率）
const COMMON: [(u32, u32); 10] = [
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
    (1366, 768),
    (1280, 720),
    (1600, 900),
    (1920, 1200),
    (2560, 1600),
    (1280, 800),
    (3440, 1440),
];

/// 设置文件的内容：一个显示器，`mode` 排第一（首选），再加上几个常见的分辨率（都是 60 Hz）。
/// 不写全局的刷新率：驱动会把它们和每个分辨率组合一遍，和单独写的重复
pub(super) fn settings_xml(mode: Mode) -> String {
    let mut list: Vec<(u32, u32, Vec<u32>)> = Vec::new();
    let mut rates = vec![mode.hz.clamp(24, 240)];
    if rates[0] != 60 {
        rates.push(60);
    }
    list.push((mode.width, mode.height, rates));
    for (w, h) in COMMON {
        if !list.iter().any(|(lw, lh, _)| (*lw, *lh) == (w, h)) {
            list.push((w, h, vec![60]));
        }
    }
    let mut out = String::from(
        "<?xml version='1.0' encoding='utf-8'?>\n\
         <!-- Written by Airhand before it enables its virtual display: edits are overwritten. -->\n\
         <vdd_settings>\n    <monitors>\n        <count>1</count>\n    </monitors>\n\
         \x20   <gpu>\n        <friendlyname>default</friendlyname>\n    </gpu>\n    <resolutions>\n",
    );
    for (w, h, rates) in list {
        out.push_str(&format!(
            "        <resolution>\n            <width>{w}</width>\n            <height>{h}</height>\n"
        ));
        for r in rates {
            out.push_str(&format!("            <refresh_rate>{r}</refresh_rate>\n"));
        }
        out.push_str("        </resolution>\n");
    }
    out.push_str(
        "    </resolutions>\n    <options>\n        <CustomEdid>false</CustomEdid>\n\
         \x20       <PreventSpoof>false</PreventSpoof>\n        <EdidCeaOverride>false</EdidCeaOverride>\n\
         \x20       <HardwareCursor>true</HardwareCursor>\n        <SDR10bit>false</SDR10bit>\n\
         \x20       <HDRPlus>false</HDRPlus>\n        <logging>false</logging>\n\
         \x20       <debuglogging>false</debuglogging>\n    </options>\n</vdd_settings>\n",
    );
    out
}

/// 建好设置文件所在的目录（只有 SYSTEM 和管理员能改）
fn make_settings_dir() -> Result<PathBuf> {
    let dir = settings_dir();
    if dir.is_dir() {
        return Ok(dir);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(SETTINGS_SDDL),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: BOOL(0),
        };
        let r = CreateDirectoryW(&HSTRING::from(dir.as_path()), Some(&sa));
        LocalFree(Some(HLOCAL(sd.0)));
        if let Err(e) = r
            && !dir.is_dir()
        {
            return Err(e).with_context(|| format!("creating {}", dir.display()));
        }
    }
    Ok(dir)
}

/// 重写设置文件（启用之前，见模块说明）
pub(super) fn write_settings(mode: Mode) -> Result<()> {
    let dir = make_settings_dir()?;
    let path = dir.join("vdd_settings.xml");
    std::fs::write(&path, settings_xml(mode)).with_context(|| format!("writing {}", path.display()))
}

// ───────────────────────────── 安装 / 卸载 ─────────────────────────────

/// [`install`] 的结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    /// 装好了（或者已经是这个版本）
    Done,
    /// 用户在 Windows 的提示里选了不安装
    Declined,
    /// 这台电脑上已经有别的 MttVDD 设备（例如 Sunshine 用的）：没有装（见模块说明）
    Foreign,
}

/// 设备集合里第 `i` 项的一个字符串属性（硬件 ID 是 REG_MULTI_SZ：按 NUL 拆开）
fn string_property(
    set: HDEVINFO,
    data: &SP_DEVINFO_DATA,
    prop: SETUP_DI_REGISTRY_PROPERTY,
) -> Vec<String> {
    let mut buf = vec![0u8; 1024];
    let mut needed = 0u32;
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(set, data, prop, None, Some(&mut buf), Some(&mut needed))
    };
    if ok.is_err() {
        return Vec::new();
    }
    let units: Vec<u16> = buf[..(needed as usize).min(buf.len())]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    units
        .split(|&c| c == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> String {
    let mut buf = [0u16; 512];
    let mut needed = 0u32;
    if unsafe { SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buf), Some(&mut needed)) }.is_err()
    {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 这台电脑上有别的（不是 Airhand 建的）MttVDD 设备
fn foreign_device() -> Result<bool> {
    unsafe {
        let set = Owned::new(SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )?);
        let mut i = 0;
        loop {
            let mut data = SP_DEVINFO_DATA {
                cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
                ..Default::default()
            };
            if SetupDiEnumDeviceInfo(*set, i, &mut data).is_err() {
                return Ok(false);
            }
            i += 1;
            let ids = string_property(*set, &data, SPDRP_HARDWAREID);
            if ids.iter().any(|id| id.eq_ignore_ascii_case(HARDWARE_ID))
                && !instance_id(*set, &data).eq_ignore_ascii_case(INSTANCE)
            {
                return Ok(true);
            }
        }
    }
}

/// 建设备节点（已经有了的话什么也不做）。返回是不是新建的
fn create_device() -> Result<bool> {
    unsafe {
        let set = Owned::new(SetupDiCreateDeviceInfoList(
            Some(&GUID_DEVCLASS_DISPLAY),
            None,
        )?);
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        match SetupDiCreateDeviceInfoW(
            *set,
            &HSTRING::from(INSTANCE),
            &GUID_DEVCLASS_DISPLAY,
            PCWSTR::null(),
            None,
            Default::default(),
            Some(&mut data),
        ) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_DEVINST_ALREADY_EXISTS.to_hresult() => return Ok(false),
            Err(e) => return Err(e).context("creating the virtual display device"),
        }
        // SPDRP_HARDWAREID 是两个 NUL 结尾的 REG_MULTI_SZ
        let ids: Vec<u8> = wide(HARDWARE_ID)
            .into_iter()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        SetupDiSetDeviceRegistryPropertyW(*set, &mut data, SPDRP_HARDWAREID, Some(&ids))?;
        SetupDiCallClassInstaller(DIF_REGISTERDEVICE, *set, Some(&data))
            .context("registering the virtual display device")?;
        Ok(true)
    }
}

fn remove_device() -> Result<()> {
    let (set, data) = open()?;
    unsafe { SetupDiCallClassInstaller(DIF_REMOVE, *set, Some(&data)) }
        .context("removing the virtual display device")
}

/// 驱动在驱动库里的名字（`oem12.inf`）：设备的驱动键里的 `InfPath`
fn driver_inf() -> Option<String> {
    let (set, data) = open().ok()?;
    let key = string_property(*set, &data, SPDRP_DRIVER)
        .into_iter()
        .next()?;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            format!(r"SYSTEM\CurrentControlSet\Control\Class\{key}"),
            KEY_READ,
        )
        .and_then(|k| k.get_value::<String, _>("InfPath"))
        .ok()
}

/// 用户在 Windows 的"要安装这个设备软件吗"里选了不安装（或者关掉了它）
fn declined(e: &windows::core::Error) -> bool {
    // ERROR_AUTHENTICODE_TRUST_NOT_ESTABLISHED、ERROR_AUTHENTICODE_PUBLISHER_NOT_TRUSTED（SetupAPI 的错误码）
    const NOT_TRUSTED: [u32; 2] = [0xE000_0242, 0xE000_0243];
    e.code() == ERROR_CANCELLED.to_hresult() || NOT_TRUSTED.contains(&(e.code().0 as u32))
}

/// `--install-vdd`：装上（升级）驱动，建好设备并停用它（见模块说明）。`inf`：安装目录里的 `vdd\MttVDD.inf`。
/// 要管理员权限；Windows 可能会弹出确认框，所以由安装程序在前台调用
pub fn install(inf: &Path) -> Result<Installed> {
    ensure!(elevated(), "administrator rights are needed");
    ensure!(inf.is_file(), "{} is missing", inf.display());
    if foreign_device()? {
        info!("another Virtual Display Driver device is installed, leaving it alone");
        return Ok(Installed::Foreign);
    }
    // 设置文件先写好：设备第一次启动时就读到它
    write_settings(Mode::default())?;
    let dir = settings_dir();
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(DRIVER_KEY)
        .and_then(|(k, _)| k.set_value("VDDPATH", &dir.as_os_str()))
        .context("writing the driver's settings path")?;
    let created = create_device()?;
    if created {
        // 还没有驱动的时候就停用：装好驱动之后不会先冒出一块显示器
        if let Err(e) = set_enabled(false) {
            info!("can't disable the virtual display before installing the driver: {e:#}");
        }
    }
    let mut reboot = BOOL::default();
    let r = unsafe {
        UpdateDriverForPlugAndPlayDevicesW(
            None,
            &HSTRING::from(HARDWARE_ID),
            &HSTRING::from(inf),
            UPDATEDRIVERFORPLUGANDPLAYDEVICES_FLAGS(0),
            Some(&mut reboot),
        )
    };
    match r {
        Ok(()) => {}
        // 驱动库里已经有同版或更新的驱动，设备用的就是它
        Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => {}
        Err(e) => {
            if created {
                let _ = remove_device();
            }
            if declined(&e) {
                info!("the user declined the virtual display driver");
                return Ok(Installed::Declined);
            }
            return Err(e).context("installing the virtual display driver");
        }
    }
    set_enabled(false)?;
    let (record, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey(RECORD)
        .context("recording the virtual display")?;
    record.set_value("Instance", &INSTANCE)?;
    if reboot.as_bool() {
        warn!("Windows needs a restart to finish installing the virtual display driver");
    }
    info!(inf = ?driver_inf(), "the virtual display driver is installed");
    Ok(Installed::Done)
}

/// `--uninstall-vdd`：删掉 Airhand 装的设备、驱动和设置（卸载时用户勾选了的话）。不是 Airhand 装的什么也不动
pub fn uninstall() -> Result<()> {
    ensure!(elevated(), "administrator rights are needed");
    if !recorded() {
        info!("Airhand didn't install a virtual display driver, nothing to remove");
        return Ok(());
    }
    super::recover();
    let inf = driver_inf();
    if devnode().is_some() {
        remove_device()?;
    }
    if let Some(inf) = inf {
        let ok =
            unsafe { SetupUninstallOEMInfW(&HSTRING::from(inf.as_str()), SUOI_FORCEDELETE, None) };
        if !ok.as_bool() {
            warn!(
                inf,
                "can't remove the virtual display driver from the driver store"
            );
        }
    }
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(k) = hklm.open_subkey_with_flags(DRIVER_KEY, KEY_READ | KEY_WRITE) {
        let _ = k.delete_value("VDDPATH");
        let empty = k.enum_values().next().is_none() && k.enum_keys().next().is_none();
        drop(k);
        if empty {
            let _ = hklm.delete_subkey(DRIVER_KEY);
            let _ = hklm.delete_subkey(r"SOFTWARE\MikeTheTech");
        }
    }
    let dir = settings_dir();
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        warn!("can't remove {}: {e}", dir.display());
    }
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    match hklm.delete_subkey_all(RECORD) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => bail!("can't delete {RECORD}: {e}"),
        _ => {}
    }
    info!("the virtual display driver is removed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 想要的分辨率排第一（驱动把第一个当作首选），常见的分辨率不重复，没有全局的刷新率
    #[test]
    fn settings_put_the_wanted_mode_first() {
        let xml = settings_xml(Mode {
            width: 2560,
            height: 1440,
            hz: 144,
        });
        let first = xml.find("<width>").unwrap();
        assert!(xml[first..].starts_with("<width>2560</width>\n            <height>1440</height>\n            <refresh_rate>144</refresh_rate>\n            <refresh_rate>60</refresh_rate>"));
        assert_eq!(
            xml.matches("<width>2560</width>\n            <height>1440</height>")
                .count(),
            1
        );
        assert_eq!(xml.matches("<width>").count(), COMMON.len());
        assert!(!xml.contains("g_refresh_rate"));
        assert!(xml.contains("<count>1</count>"));
        // XML 注释里不能有 --
        assert!(!xml.replace("<!--", "").replace("-->", "").contains("--"));
        let plain = settings_xml(Mode::default());
        assert_eq!(
            plain.matches("<refresh_rate>60</refresh_rate>").count(),
            COMMON.len()
        );
    }
}
