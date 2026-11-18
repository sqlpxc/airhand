//! Independent system recording client for e2e_mic. Only open the input created by a test build.
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use anyhow::{Context, Result};

pub(crate) struct Recorder {
    child: Child,
    audio: Receiver<[u8; 1920]>,
}

/// A fresh process also verifies the OS-visible objects after a crash or uninstall.
#[cfg(target_os = "macos")]
fn mac_reader() -> Command {
    let (uid, tap) = super::native::identifiers();
    let mut cmd = Command::new("/usr/bin/swift");
    cmd.arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../scripts/record-test-mic.swift"
    ))
    .args([uid.to_str().unwrap(), tap.to_str().unwrap()]);
    cmd
}

pub(super) fn published_objects() -> usize {
    #[cfg(target_os = "macos")]
    {
        let out = mac_reader().arg("--count").output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
    #[cfg(target_os = "linux")]
    {
        let out = Command::new("pactl")
            .args(["list", "short", "sources"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let (uid, _) = super::native::identifiers();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .filter(|s| s.split_whitespace().nth(1) == uid.to_str().ok())
            .count()
    }
}

impl Recorder {
    pub fn open() -> Result<Self> {
        #[cfg(target_os = "macos")]
        let mut cmd = mac_reader();
        #[cfg(target_os = "linux")]
        let mut cmd = {
            let mut cmd = Command::new("parec");
            cmd.args([
                "--raw",
                "--format=float32le",
                "--rate=48000",
                "--channels=1",
                "--latency-msec=20",
                "--device",
            ])
            .arg(super::native::identifiers().0.to_str().unwrap());
            cmd
        };
        let mut child = cmd
            .stdout(Stdio::piped())
            .spawn()
            .context("start microphone recorder")?;
        let mut stdout = child.stdout.take().unwrap();
        let (tx, audio) = channel();
        let recorder = Self { child, audio };
        std::thread::spawn(move || {
            let mut data = [0; 1920];
            while stdout.read_exact(&mut data).is_ok() && tx.send(data).is_ok() {}
        });
        recorder
            .audio
            .recv_timeout(Duration::from_secs(12))
            .context("read initial silence from the test microphone")?;
        Ok(recorder)
    }

    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<()> {
        for data in self.audio.try_iter() {
            out.extend(
                data.as_chunks::<4>()
                    .0
                    .iter()
                    .copied()
                    .map(f32::from_le_bytes),
            );
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
