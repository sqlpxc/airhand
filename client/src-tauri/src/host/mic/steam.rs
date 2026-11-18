//! 从本机 Steam 安装虚拟麦克风。Steam Client Service 通常已把驱动解包到 Common Files，
//! 刚装好的 Steam 也可能只有 bin/drivers.exe（带签名的自解压 ZIP）。这里只读取其中的麦克风驱动包。

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{Duration, Instant};

use airhand_proto::session::MicState;
use anyhow::{Context, Result, bail, ensure};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{ERROR_DEVINST_ALREADY_EXISTS, ERROR_NO_MORE_ITEMS};
use windows::Win32::System::SystemInformation::{
    IMAGE_FILE_MACHINE_AMD64, IMAGE_FILE_MACHINE_ARM64, IMAGE_FILE_MACHINE_I386,
};
use windows::Win32::System::Threading::{GetCurrentProcess, IsWow64Process2};
use windows::core::{BOOL, HSTRING, Owned, PCWSTR, w};
use winreg::{RegKey, enums::*};
use zip::ZipArchive;

use super::Status;
use crate::host::capture::wire_detail;

const INF: &str = "SteamStreamingMicrophone.inf";
const FILES: [&str; 3] = [
    INF,
    "steamstreamingmicrophone.cat",
    "SteamStreamingMicrophone.sys",
];
const HARDWARE_ID: PCWSTR = w!(r"ROOT\SteamStreamingMicrophone");
const INSTANCE: PCWSTR = w!(r"ROOT\SteamStreamingMicrophone\0000");
const ENDPOINT_WAIT: Duration = Duration::from_secs(15);

#[derive(Debug)]
enum Package {
    Directory(PathBuf),
    Archive(PathBuf, &'static str),
}

fn native_arch() -> Result<&'static str> {
    let mut native = Default::default();
    unsafe {
        IsWow64Process2(
            GetCurrentProcess(),
            &mut Default::default(),
            Some(&mut native),
        )?
    };
    match native {
        IMAGE_FILE_MACHINE_AMD64 => Ok("x64"),
        IMAGE_FILE_MACHINE_I386 => Ok("x86"),
        IMAGE_FILE_MACHINE_ARM64 => Ok("arm64"),
        _ => bail!("unsupported Windows driver architecture {:#x}", native.0),
    }
}

fn steam_roots() -> Vec<PathBuf> {
    const STEAM_KEY: &str = r"Software\Valve\Steam";
    let mut roots = Vec::new();
    let mut add = |hive, key: &str, view, name: &str| {
        if let Ok(path) = RegKey::predef(hive)
            .open_subkey_with_flags(key, KEY_READ | view)
            .and_then(|key| key.get_value::<OsString, _>(name))
        {
            roots.push(PathBuf::from(path));
        }
    };
    for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
        add(HKEY_LOCAL_MACHINE, STEAM_KEY, view, "InstallPath");
    }
    if crate::winsvc::is_agent() {
        // 服务的 HKCU 是 SYSTEM。登录用户的自定义 SteamPath 在其已加载的用户配置单元里。
        for sid in RegKey::predef(HKEY_USERS).enum_keys().flatten() {
            if sid.starts_with("S-1-") && !sid.ends_with("_Classes") {
                add(HKEY_USERS, &format!(r"{sid}\{STEAM_KEY}"), 0, "SteamPath");
            }
        }
    } else {
        add(HKEY_CURRENT_USER, STEAM_KEY, 0, "SteamPath");
    }
    roots.extend(steam_folders(["ProgramFiles(x86)", "ProgramFiles"]));
    roots
}

fn steam_folders(vars: [&str; 2]) -> impl Iterator<Item = PathBuf> {
    vars.into_iter()
        .filter_map(std::env::var_os)
        .map(|p| PathBuf::from(p).join("Steam"))
}

fn discover(
    services: impl IntoIterator<Item = PathBuf>,
    roots: Vec<PathBuf>,
) -> Result<Option<Package>> {
    let mut bases = Vec::new();
    for service in services {
        if service.join("SteamService.exe").try_exists()? {
            bases.push(service);
        }
    }
    for root in roots {
        // 注册表里的路径可以由用户写入。只查本机磁盘上的 Steam，避免服务访问网络路径。
        if root.is_absolute()
            && matches!(root.components().next(), Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::Disk(_)))
            && root.join("steam.exe").try_exists()?
        {
            bases.push(root.join("bin"));
            bases.push(root);
        }
    }
    if bases.is_empty() {
        return Ok(None);
    }
    find_package(&bases, native_arch()?).map(Some)
}

fn find_package(bases: &[PathBuf], arch: &'static str) -> Result<Package> {
    for base in bases {
        let dir = base.join("drivers").join("Windows10").join(arch);
        if FILES.iter().all(|name| dir.join(name).is_file()) {
            return Ok(Package::Directory(dir));
        }
    }
    for base in bases {
        let archive = base.join("drivers.exe");
        if archive.is_file() {
            return Ok(Package::Archive(archive, arch));
        }
    }
    bail!(
        "Steam's microphone driver package is missing for {arch}. Open or update Steam on the host"
    )
}

impl Package {
    /// 固定输入和输出文件名，解压时限制实际写入的大小。安装使用这份临时副本，避免验证后源文件被替换。
    fn stage(&self, dest: &Path) -> Result<PathBuf> {
        let copy = |name: &str, reader: &mut dyn Read| -> Result<()> {
            const MAX_FILE: u64 = 16 * 1024 * 1024;
            let written = io::copy(
                &mut reader.take(MAX_FILE + 1),
                &mut File::create(dest.join(name))?,
            )?;
            ensure!(
                written <= MAX_FILE,
                "Steam microphone driver file {name} is too large"
            );
            Ok(())
        };
        match self {
            Self::Directory(dir) => {
                for name in FILES {
                    copy(name, &mut File::open(dir.join(name))?)?;
                }
            }
            Self::Archive(file, arch) => {
                let mut archive = ZipArchive::new(File::open(file)?)?;
                for name in FILES {
                    let path = format!("drivers/Windows10/{arch}/{name}");
                    let mut entry = archive
                        .by_name(&path)
                        .with_context(|| format!("{path} is missing from Steam's drivers.exe"))?;
                    copy(name, &mut entry)?;
                }
            }
        }
        Ok(dest.join(INF))
    }

    fn install(self) -> Result<()> {
        let dir = tempfile::Builder::new().prefix("airhand-mic-").tempdir()?;
        let inf = self
            .stage(dir.path())
            .context("reading Steam's microphone driver package")?;
        let path = HSTRING::from(inf.as_path());
        verify_inf(&path)?;
        install_device(&path)
    }
}

fn verify_inf(path: &HSTRING) -> Result<()> {
    let mut signer = SP_INF_SIGNER_INFO_V2_W {
        cbSize: size_of::<SP_INF_SIGNER_INFO_V2_W>() as u32,
        ..Default::default()
    };
    unsafe { SetupVerifyInfFileW(path, None, &mut signer).ok() }
        .context("verifying Steam's microphone driver catalog")
}

/// 新机器上还没有 ROOT 设备。使用 Steam INF 声明的固定实例名，重试时复用它。Windows 根据硬件 ID 绑定签名驱动。
fn install_device(path: &HSTRING) -> Result<()> {
    unsafe {
        let devices = Owned::new(SetupDiCreateDeviceInfoList(
            Some(&GUID_DEVCLASS_MEDIA),
            None,
        )?);
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        let created = match SetupDiCreateDeviceInfoW(
            *devices,
            INSTANCE,
            &GUID_DEVCLASS_MEDIA,
            PCWSTR::null(),
            None,
            Default::default(),
            Some(&mut data),
        ) {
            Ok(()) => {
                // SPDRP_HARDWAREID 是两个 NUL 结尾的 REG_MULTI_SZ。
                let ids: Vec<u8> = HARDWARE_ID
                    .as_wide()
                    .iter()
                    .copied()
                    .chain([0, 0])
                    .flat_map(u16::to_le_bytes)
                    .collect();
                SetupDiSetDeviceRegistryPropertyW(
                    *devices,
                    &mut data,
                    SPDRP_HARDWAREID,
                    Some(&ids),
                )?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, *devices, Some(&data))
                    .context("registering the Steam microphone (administrator rights required)")?;
                true
            }
            Err(e) if e.code() == ERROR_DEVINST_ALREADY_EXISTS.to_hresult() => false,
            Err(e) => return Err(e).context("creating the Steam microphone device"),
        };
        let mut reboot = BOOL::default();
        if let Err(e) = UpdateDriverForPlugAndPlayDevicesW(
            None,
            HARDWARE_ID,
            path,
            INSTALLFLAG_NONINTERACTIVE,
            Some(&mut reboot),
        ) {
            // 注册根设备时 PnP 可能已经从驱动存储绑定了同版或更新的驱动。保留它，等 WASAPI 端点出现。
            if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() {
                return Ok(());
            }
            if created {
                let _ = SetupDiCallClassInstaller(DIF_REMOVE, *devices, Some(&data));
            }
            return Err(e)
                .context("installing the Steam microphone driver (administrator rights required)");
        }
        ensure!(
            !reboot.as_bool(),
            "Windows needs a restart to finish installing the Steam microphone"
        );
        Ok(())
    }
}

fn package() -> Result<Package, Status> {
    let services = steam_folders(["CommonProgramFiles(x86)", "CommonProgramFiles"]);
    match discover(services, steam_roots()) {
        Ok(Some(p)) => Ok(p),
        Ok(None) => Err((MicState::SteamMissing, None)),
        Err(e) => Err((MicState::SetupFailed, Some(wire_detail(&e)))),
    }
}

/// 被动探测只查位置。只有播放线程（用户打开了转发）才进入安装流程。
pub(super) fn probe_missing() -> Status {
    package().map_or_else(|status| status, |_| (MicState::NoDevice, None))
}

#[derive(Default)]
pub(super) struct DriverSetup {
    /// 每次打开转发最多实际安装一次。设备出现后播放线程会直接打开它。
    attempt: Option<Result<Instant, String>>,
}

impl DriverSetup {
    pub(super) fn missing(&mut self, notify: &dyn Fn(Status)) -> Status {
        self.try_install(package, Package::install, notify)
    }

    fn try_install<P>(
        &mut self,
        find: impl FnOnce() -> Result<P, Status>,
        install: impl FnOnce(P) -> Result<()>,
        notify: &dyn Fn(Status),
    ) -> Status {
        if self.attempt.is_none() {
            let package = match find() {
                Ok(p) => p,
                Err(status) => return status,
            };
            notify((MicState::Preparing, None));
            tracing::info!("installing Steam Streaming Microphone");
            self.attempt = Some(
                install(package)
                    .map(|()| Instant::now() + ENDPOINT_WAIT)
                    .map_err(|e| {
                        tracing::warn!("Steam microphone setup failed: {e:#}");
                        wire_detail(&e)
                    }),
            );
        }
        match self.attempt.as_ref().unwrap() {
            Ok(deadline) if Instant::now() < *deadline => (MicState::Preparing, None),
            Ok(_) => (
                MicState::SetupFailed,
                Some("Check that both Steam Streaming Microphone devices are enabled in Windows sound settings".into()),
            ),
            Err(detail) => (MicState::SetupFailed, Some(detail.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use zip::{ZipWriter, write::SimpleFileOptions};

    #[test]
    fn discovers_custom_steam_and_stages_only_the_native_microphone() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("Steam 客户端");
        let locate = || discover([], vec![root.clone()]);
        assert!(locate()?.is_none()); // 注册表残留，但没有 Steam
        fs::create_dir_all(root.join("bin"))?;
        fs::write(root.join("steam.exe"), [])?;
        assert!(locate().is_err()); // Steam 在，只是包还没到

        let arch = native_arch()?;
        let mut archive = ZipWriter::new(File::create(root.join("bin/drivers.exe"))?);
        for name in FILES.into_iter().chain(["SteamStreamingSpeakers.inf"]) {
            archive.start_file(
                format!("drivers/Windows10/{arch}/{name}"),
                SimpleFileOptions::default(),
            )?;
            archive.write_all(name.as_bytes())?;
        }
        drop(archive.finish()?);
        let stage = tempfile::tempdir()?;
        let inf = locate()?.unwrap().stage(stage.path())?;
        assert_eq!(fs::read(&inf)?, INF.as_bytes());
        assert_eq!(fs::read_dir(stage.path())?.count(), FILES.len());
        assert!(verify_inf(&HSTRING::from(inf.as_path())).is_err());

        let service = temp.path().join("Common Files/Steam");
        let unpacked = service.join("drivers/Windows10").join(arch);
        fs::create_dir_all(&unpacked)?;
        fs::write(service.join("SteamService.exe"), [])?;
        for name in FILES {
            fs::write(unpacked.join(name), [])?;
        }
        assert!(
            matches!(discover([service], vec![root])?, Some(Package::Directory(dir)) if dir == unpacked)
        );
        Ok(())
    }

    #[test]
    fn opening_waits_for_steam_and_installs_once_per_activation() {
        let mut setup = DriverSetup::default();
        let missing = (MicState::SteamMissing, None);
        assert_eq!(
            setup.try_install(
                || Err::<(), _>(missing.clone()),
                |_| unreachable!(),
                &|_| unreachable!()
            ),
            missing
        );
        let pending = (MicState::Preparing, None);
        assert_eq!(
            setup.try_install(|| Ok(()), |_| Ok(()), &|s| assert_eq!(s, pending)),
            pending
        );
        let poll = |setup: &mut DriverSetup| {
            setup.try_install(
                || -> Result<(), Status> { unreachable!() },
                |_| unreachable!(),
                &|_| unreachable!(),
            )
        };
        assert_eq!(poll(&mut setup), pending);
        setup.attempt = Some(Ok(Instant::now()));
        assert_eq!(poll(&mut setup).0, MicState::SetupFailed);

        let mut retry = DriverSetup::default();
        let failed = retry.try_install(|| Ok(()), |_| bail!("access denied"), &|s| {
            assert_eq!(s, pending)
        });
        assert_eq!(
            failed,
            (MicState::SetupFailed, Some("access denied".into()))
        );
        assert_eq!(poll(&mut retry), failed);
    }

    /// 在装了 Steam 的 Windows 上读取实际的自解压包并验证微软签名。
    #[test]
    #[ignore]
    fn verifies_local_steam_microphone_archive() -> Result<()> {
        let archive = steam_roots()
            .into_iter()
            .map(|root| root.join("bin/drivers.exe"))
            .find(|path| path.is_file())
            .context("Steam's drivers.exe")?;
        let temp = tempfile::tempdir()?;
        let inf = Package::Archive(archive, native_arch()?).stage(temp.path())?;
        verify_inf(&HSTRING::from(inf.as_path()))
    }
}
