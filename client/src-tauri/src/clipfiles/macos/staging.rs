//! A receive uses the selected volume, including external drives. Only a complete top-level
//! item is published. Before creating any partial content, leave a recovery link in the app's
//! private configuration directory. A forced quit therefore needs no scan of user folders.

use std::ffi::CString;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, symlink};
use std::path::{Path, PathBuf};

const PREFIX: &str = ".airhand-receive-";
const RECORD_PREFIX: &str = "pending-receive-";

pub(super) fn config_dir() -> io::Result<PathBuf> {
    crate::config::path()
        .and_then(|p| p.parent().map(Path::to_owned))
        .ok_or_else(|| io::Error::other("cannot locate the Airhand configuration directory"))
}

pub(super) struct Stage {
    record: PathBuf,
    dir: PathBuf,
    /// Random name inside the private directory. Only this item can be recursively removed.
    pub(super) item: PathBuf,
}

impl Stage {
    fn at(record: &Path, dir: PathBuf, token: &str) -> io::Result<Self> {
        if !dir.is_absolute()
            || dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_prefix(PREFIX))
                != Some(token)
        {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "invalid receive recovery link",
            ));
        };
        Ok(Self {
            item: dir.join(token),
            dir,
            record: record.to_owned(),
        })
    }

    pub(super) fn create(config: &Path, dest: &Path) -> io::Result<Self> {
        let dest = dest.canonicalize()?;
        // These two directories belong to the uninstaller. Completed items must live outside them.
        let config = config.canonicalize()?;
        if dest.starts_with(&config)
            || crate::login_item::bundle().is_some_and(|bundle| dest.starts_with(bundle))
        {
            return Err(io::Error::other(tr!(
                "请选择 Airhand 程序和配置目录以外的文件夹。",
                "Choose a folder outside Airhand’s application and configuration directories."
            )));
        }
        let token = format!("{:032x}", rand::random::<u128>());
        let record = config.join(format!("{RECORD_PREFIX}{token}"));
        let stage = Self::at(&record, dest.join(format!("{PREFIX}{token}")), &token)?;
        symlink(&stage.dir, &record)?;
        if let Err(e) = private_dir(&stage.dir) {
            // mkdir did not create anything. In particular an existing directory isn't ours.
            fs::remove_file(record)?;
            return Err(e);
        }
        Ok(stage)
    }

    pub(super) fn publish(&self, name: &str) -> io::Result<()> {
        let dest = self.dir.parent().expect("absolute staging directory");
        available_name(dest, name, |path| rename_exclusive(&self.item, path))
    }

    pub(super) fn clean(self) -> io::Result<()> {
        match fs::symlink_metadata(&self.dir) {
            Ok(meta) if meta.is_dir() => {
                match fs::symlink_metadata(&self.item) {
                    Ok(m) if m.is_dir() => fs::remove_dir_all(&self.item)?,
                    Ok(_) => fs::remove_file(&self.item)?,
                    Err(e) if e.kind() == ErrorKind::NotFound => (),
                    Err(e) => return Err(e),
                }
                // Preserve any unrelated files, even if someone put them in the hidden folder.
                fs::remove_dir(&self.dir)?;
            }
            Ok(_) => return Err(io::Error::other("receive staging directory was replaced")),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // A disconnected destination volume may still contain a partial. Keep the record.
                fs::metadata(self.dir.parent().expect("absolute staging directory"))?;
            }
            Err(e) => return Err(e),
        }
        fs::remove_file(self.record)
    }
}

pub(super) fn recover(config: &Path) -> io::Result<()> {
    let entries = match fs::read_dir(config) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut errors = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(token) = name
            .to_str()
            .and_then(|n| n.strip_prefix(RECORD_PREFIX))
            .filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        else {
            continue;
        };
        let record = entry.path();
        let result = fs::read_link(&record).and_then(|dir| {
            Stage::at(&record, dir.clone(), token)?
                .clean()
                .map_err(|e| io::Error::other(format!("{}: {e}", dir.display())))
        });
        if let Err(e) = result {
            errors.push(format!("{}: {e}", record.display()));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(errors.join("\n")))
    }
}

/// Runs on the receive worker. Cancelling drops any outstanding network read before cleanup.
pub(super) async fn receive<T>(
    config: &Path,
    dest: &Path,
    cancel: tokio::sync::oneshot::Receiver<()>,
    download: impl AsyncFnOnce(&Stage) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    let stage = Stage::create(config, dest)?;
    let result = tokio::select! {
        biased;
        _ = cancel => Ok(None),
        r = download(&stage) => r.map(Some),
    };
    stage.clean()?;
    result
}

pub(super) fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new().mode(0o700).create(path)
}

/// Let the destination filesystem arbitrate case, Unicode normalization, and concurrent copies.
/// Every operation passed here creates a new entry exclusively (including the final rename).
pub(super) fn available_name<T>(
    parent: &Path,
    name: &str,
    mut create: impl FnMut(&Path) -> io::Result<T>,
) -> io::Result<T> {
    if !airhand_proto::files::valid_name(name) {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "invalid remote filename",
        ));
    }
    let name = name.replace(':', "_");
    let (stem, ext) = name
        .rfind('.')
        .filter(|&i| i > 0)
        .map_or((name.as_str(), ""), |i| name.split_at(i));
    for n in 1u32.. {
        let path = if n == 1 {
            parent.join(&name)
        } else {
            parent.join(format!("{stem} ({n}){ext}"))
        };
        match create(&path) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            result => return result,
        }
    }
    Err(io::Error::other("too many filename collisions"))
}

fn rename_exclusive(from: &Path, to: &Path) -> io::Result<()> {
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
