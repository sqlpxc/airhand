//! 本机磁盘上的文件（桌面版剪贴板里复制的文件）：[`Provider`] 的实现。
//!
//! 公告的是用户选中的那几个路径（最上层那一项本身是链接时跟随它：用户要的是内容）。之后的每一段都在它下面，
//! 而且**不跟随链接**：路径中间的一段是链接（符号链接、Windows 的目录联接）就当作没有，列目录时链接列成链接
//! （带着它指向的路径）。打开文件时最后一段也不能是链接（Unix 上用 `O_NOFOLLOW` 打开，查和打开之间被换成链接也打不开）。
//! 名字不是 UTF-8 的（Unix 上才可能有）列不出来：协议里的名字是字符串。

use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use airhand_proto::files::{FileEntry, FileError, FileKind, valid_name};

use super::Provider;

/// 本机磁盘上的一组文件（见模块说明）
pub struct LocalFs {
    /// 公告里的每一项：名称、真实的路径、样子
    roots: Vec<(String, PathBuf, FileEntry)>,
}

impl LocalFs {
    /// 用户选中的这些路径（剪贴板里的）。不存在的、名字不是 UTF-8 的、重名的不要
    pub fn new(paths: &[PathBuf]) -> Self {
        let mut roots: Vec<(String, PathBuf, FileEntry)> = Vec::new();
        for p in paths {
            // 用户选中的是链接：跟随它（见模块说明）
            let Ok(real) = std::fs::canonicalize(p) else {
                continue;
            };
            let Ok(meta) = std::fs::metadata(&real) else {
                continue;
            };
            let Some(name) = p
                .file_name()
                .or_else(|| real.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if !valid_name(&name) || roots.iter().any(|(n, ..)| *n == name) {
                continue;
            }
            let kind = if meta.is_dir() {
                FileKind::Dir
            } else {
                FileKind::File
            };
            let entry = FileEntry {
                path: vec![name.clone()],
                kind,
                size: if kind == FileKind::File {
                    meta.len()
                } else {
                    0
                },
                mtime: mtime(&meta),
                target: None,
            };
            roots.push((name, real, entry));
        }
        Self { roots }
    }

    /// 一个都没有（剪贴板里的路径都不在了）
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// 公告里的那几项的真实路径（平台判断剪贴板里的是不是自己放的，见各平台的剪贴板）
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.roots.iter().map(|(_, p, _)| p.as_path())
    }

    /// `path` 在本机的哪里：第一段是公告里的一项，之后的每一段都不能是链接（最后一段由调用的一方检查）
    fn resolve(&self, path: &[String]) -> Result<PathBuf, FileError> {
        let (first, rest) = path.split_first().ok_or(FileError::Invalid)?;
        let (_, root, _) = self
            .roots
            .iter()
            .find(|(n, ..)| n == first)
            .ok_or(FileError::NotFound)?;
        let mut full = root.clone();
        for (i, part) in rest.iter().enumerate() {
            if !valid_name(part) {
                return Err(FileError::Invalid);
            }
            full.push(part);
            // 中间的一段：必须是真的目录，不是链接
            if i + 1 < rest.len() {
                let meta = std::fs::symlink_metadata(&full).map_err(io_error)?;
                if meta.file_type().is_symlink() || !meta.is_dir() {
                    return Err(FileError::NotFound);
                }
            }
        }
        Ok(full)
    }
}

impl Provider for LocalFs {
    fn items(&self) -> Vec<FileEntry> {
        self.roots.iter().map(|(_, _, e)| e.clone()).collect()
    }

    fn read_dir(&self, path: &[String]) -> Result<Vec<FileEntry>, FileError> {
        // 最上层那一项本身可以是链接（跟随过了，resolve 给的是它的真实路径）；下面的不行
        let dir = self.resolve(path)?;
        let meta = std::fs::symlink_metadata(&dir).map_err(io_error)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(FileError::NotFound);
        }
        let mut out = Vec::new();
        for e in std::fs::read_dir(&dir).map_err(io_error)? {
            let Ok(e) = e else { continue };
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !valid_name(&name) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(e.path()) else {
                continue;
            };
            out.push(entry(name, &meta, &e.path()));
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    fn open(
        &self,
        path: &[String],
        offset: u64,
    ) -> Result<(Box<dyn Read + Send>, FileEntry), FileError> {
        let full = self.resolve(path)?;
        let meta = std::fs::symlink_metadata(&full).map_err(io_error)?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(FileError::Invalid);
        }
        let mut file = open_no_follow(&full).map_err(io_error)?;
        let meta = file.metadata().map_err(io_error)?;
        if !meta.is_file() {
            return Err(FileError::Invalid);
        }
        let name = path.last().cloned().unwrap_or_default();
        let e = FileEntry {
            path: vec![name],
            kind: FileKind::File,
            size: meta.len(),
            mtime: mtime(&meta),
            target: None,
        };
        file.seek(SeekFrom::Start(offset)).map_err(io_error)?;
        Ok((Box::new(file), e))
    }
}

/// 打开文件读：最后一段是链接就打不开（Unix；查过之后被换成链接也一样）
fn open_no_follow(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    // Windows 上不用 FILE_FLAG_OPEN_REPARSE_POINT：它也让 OneDrive 的占位文件、重复数据删除过的文件不经过处理，
    // 读出来的不是内容。所以最后一段在查和打开之间被换成链接时挡不住（要本机有程序在传的时候改这个文件夹）
    let file = o.open(path)?;
    // 查和打开之间换成了目录、管道之类的
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

/// 目录里的一项（`meta` 是不跟随链接的样子）
fn entry(name: String, meta: &Metadata, full: &Path) -> FileEntry {
    let ft = meta.file_type();
    let (kind, size, target) = if ft.is_symlink() {
        let target = std::fs::read_link(full)
            .ok()
            .and_then(|t| t.to_str().map(|s| s.replace('\\', "/")));
        (FileKind::Link, 0, target)
    } else if ft.is_dir() {
        (FileKind::Dir, 0, None)
    } else {
        (FileKind::File, meta.len(), None)
    };
    FileEntry {
        path: vec![name],
        kind,
        size,
        mtime: mtime(meta),
        target,
    }
}

fn mtime(meta: &Metadata) -> Option<u64> {
    let t = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(t.as_millis()).ok()
}

fn io_error(e: std::io::Error) -> FileError {
    match e.kind() {
        std::io::ErrorKind::NotFound => FileError::NotFound,
        std::io::ErrorKind::PermissionDenied => FileError::Denied,
        _ => FileError::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    /// 临时目录（测试结束时删掉）
    struct Temp(PathBuf);

    impl Temp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "airhand-files-{name}-{}-{}",
                std::process::id(),
                rand::random::<u32>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn lists_and_opens_under_the_offered_items_only() {
        let t = Temp::new("local");
        let dir = t.0.join("photos");
        std::fs::create_dir_all(dir.join("2024")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("2024").join("b.bin"), vec![7u8; 1000]).unwrap();
        std::fs::write(t.0.join("secret.txt"), b"no").unwrap();
        std::fs::write(t.0.join("single.txt"), b"one").unwrap();
        let fs = LocalFs::new(&[dir.clone(), t.0.join("single.txt"), t.0.join("missing")]);
        let items = fs.items();
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].name(), items[0].kind), ("photos", FileKind::Dir));
        assert_eq!(
            (items[1].name(), items[1].kind, items[1].size),
            ("single.txt", FileKind::File, 3)
        );
        let list = fs.read_dir(&p(&["photos"])).unwrap();
        let names: Vec<_> = list.iter().map(|e| (e.name(), e.kind)).collect();
        assert_eq!(names, [("2024", FileKind::Dir), ("a.txt", FileKind::File)]);
        let (mut r, e) = fs.open(&p(&["photos", "2024", "b.bin"]), 990).unwrap();
        assert_eq!(e.size, 1000);
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, [7u8; 10]);
        // 公告外面的东西、不合规矩的路径
        assert_eq!(
            fs.open(&p(&["secret.txt"]), 0).err(),
            Some(FileError::NotFound)
        );
        assert_eq!(
            fs.open(&p(&["photos", "..", "secret.txt"]), 0).err(),
            Some(FileError::Invalid)
        );
        assert_eq!(
            fs.read_dir(&p(&["photos", "a.txt"])).err(),
            Some(FileError::NotFound)
        );
        assert_eq!(
            fs.open(&p(&["photos", "2024"]), 0).err(),
            Some(FileError::Invalid)
        );
    }

    /// 文件夹里的链接不跟随：列成链接（带着指向的路径），打不开、也不能从它下面走
    #[cfg(unix)]
    #[test]
    fn links_inside_are_not_followed() {
        let t = Temp::new("links");
        let dir = t.0.join("proj");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib").join("x"), b"x").unwrap();
        std::fs::write(t.0.join("key"), b"secret").unwrap();
        std::os::unix::fs::symlink("lib", dir.join("lib64")).unwrap();
        std::os::unix::fs::symlink(t.0.join("key"), dir.join("key")).unwrap();
        // 用户选中的是一个链接：跟随它
        std::os::unix::fs::symlink(&dir, t.0.join("shortcut")).unwrap();
        let fs = LocalFs::new(&[t.0.join("shortcut")]);
        assert_eq!(fs.items()[0].kind, FileKind::Dir);
        let list = fs.read_dir(&p(&["shortcut"])).unwrap();
        let find = |n: &str| list.iter().find(|e| e.name() == n).unwrap().clone();
        assert_eq!(
            (find("lib64").kind, find("lib64").target.as_deref()),
            (FileKind::Link, Some("lib"))
        );
        assert_eq!(find("key").kind, FileKind::Link);
        assert_eq!(
            fs.open(&p(&["shortcut", "key"]), 0).err(),
            Some(FileError::Invalid)
        );
        assert_eq!(
            fs.open(&p(&["shortcut", "lib64", "x"]), 0).err(),
            Some(FileError::NotFound)
        );
        assert_eq!(
            fs.read_dir(&p(&["shortcut", "lib64"])).err(),
            Some(FileError::NotFound)
        );
        assert!(fs.open(&p(&["shortcut", "lib", "x"]), 0).is_ok());
    }
}
