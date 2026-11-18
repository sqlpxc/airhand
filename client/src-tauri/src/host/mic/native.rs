//! 会话期间发布的 Airhand Microphone。原生回调只从单生产者、单消费者的环形缓冲区取样本。
//! 设备和回调的生命周期由播放线程持有，所有阻塞的系统调用都在打开或关闭设备时执行。

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr::NonNull;

use airhand_proto::session::MicState;
use anyhow::{Result, anyhow, bail};

use super::{AHEAD_MS, Format, Output, Sample, Status};

unsafe extern "C" {
    #[cfg(target_os = "macos")]
    fn ah_mic_cleanup(
        uid: *const c_char,
        tap: *const c_char,
        error: *mut c_char,
        len: usize,
    ) -> c_int;
    #[cfg(target_os = "linux")]
    fn ah_mic_probe(error: *mut c_char, len: usize) -> c_int;
    fn ah_mic_open(
        uid: *const c_char,
        tap: *const c_char,
        name: *const c_char,
        error: *mut c_char,
        len: usize,
    ) -> *mut c_void;
    fn ah_mic_queued(handle: *mut c_void, error: *mut c_char, len: usize) -> c_int;
    fn ah_mic_write(handle: *mut c_void, bytes: *const u8, frames: u32);
    fn ah_mic_close(handle: *mut c_void, error: *mut c_char, len: usize) -> c_int;
}

pub(super) fn supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        objc2::available!(macos = 14.2)
    }
    #[cfg(target_os = "linux")]
    true
}

pub(super) fn probe() -> Status {
    if !supported() {
        return (MicState::Unsupported, None);
    }
    #[cfg(target_os = "linux")]
    {
        let mut error = [0; 256];
        if unsafe { ah_mic_probe(error.as_mut_ptr(), error.len()) } < 0 {
            return (MicState::Failed, Some(message(&error)));
        }
    }
    (MicState::Ready, None)
}

// Core Audio publishes these system-wide. Fast user switching and development tests must not
// remove another user's device. Keep the identifiers stable across upgrades and crashes.
pub(super) fn identifiers() -> (CString, CString) {
    let user = unsafe { libc::getuid() };
    #[cfg(test)]
    let scope = std::env::var("AIRHAND_TEST_MIC_GROUP")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_else(std::process::id);
    #[cfg(not(test))]
    let scope = 0u32;
    let domain = if cfg!(test) {
        format!("app.airhand.test.{scope}")
    } else {
        "app.airhand.client".to_owned()
    };
    (
        CString::new(format!("{domain}.microphone.{user}")).unwrap(),
        CString::new(format!(
            "{:08X}-E1B1-43A0-BBC7-0000{user:08X}",
            0x0bee_d0b4 ^ scope
        ))
        .unwrap(),
    )
}

fn message(buf: &[c_char]) -> String {
    // All error paths write with snprintf, and callers initialize the entire buffer to zero.
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[cfg(target_os = "macos")]
pub(crate) fn cleanup() -> Result<()> {
    if !supported() {
        return Ok(());
    }
    let (uid, tap) = identifiers();
    let mut error = [0; 256];
    if unsafe { ah_mic_cleanup(uid.as_ptr(), tap.as_ptr(), error.as_mut_ptr(), error.len()) } < 0 {
        bail!("{}", message(&error));
    }
    Ok(())
}

struct Device {
    handle: NonNull<c_void>,
    scratch: Vec<u8>,
}

pub(super) fn open() -> Result<Option<Box<dyn Output>>> {
    let (uid, tap) = identifiers();
    let mut error = [0; 256];
    let name = if cfg!(test) {
        c"Airhand Test Microphone"
    } else {
        c"Airhand Microphone"
    };
    let ptr = unsafe {
        ah_mic_open(
            uid.as_ptr(),
            tap.as_ptr(),
            name.as_ptr(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    let handle = NonNull::new(ptr).ok_or_else(|| anyhow!(message(&error)))?;
    Ok(Some(Box::new(Device {
        handle,
        scratch: Vec::new(),
    })))
}

impl Output for Device {
    fn format(&self) -> Format {
        Format {
            sample: Sample::F32,
            channels: 1,
            rate: 48_000,
            block_align: 4,
        }
    }

    fn buffered(&mut self) -> Result<(usize, usize)> {
        let mut error = [0; 256];
        let queued =
            unsafe { ah_mic_queued(self.handle.as_ptr(), error.as_mut_ptr(), error.len()) };
        if queued < 0 {
            bail!("{}", message(&error));
        }
        // The native ring holds 4096 frames. Expose only the desired 40 ms of headroom.
        Ok((
            queued as usize,
            self.format().rate as usize * AHEAD_MS / 1000,
        ))
    }

    fn write(&mut self, frames: usize, fill: &mut dyn FnMut(&mut [u8])) -> Result<()> {
        self.scratch.resize(frames * self.format().block_align, 0);
        fill(&mut self.scratch);
        unsafe { ah_mic_write(self.handle.as_ptr(), self.scratch.as_ptr(), frames as u32) };
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let mut error = [0; 256];
        if unsafe { ah_mic_close(self.handle.as_ptr(), error.as_mut_ptr(), error.len()) } < 0 {
            tracing::warn!("closing the virtual microphone: {}", message(&error));
        }
    }
}
