//! 剪贴板里的文件（见 `airhand_link::files`），给 Kotlin 用（`core/ClipFiles.kt`）：
//!
//! * 对方复制的文件：被控端（手机在被控）收到公告时发事件 `{"type":"clipboardFiles","key","items"}`；控制端收到
//!   会话事件 `clipboardFiles` 之后用 `remoteFiles` 命令换一个 `key`。Kotlin 用 `key` 把它们放进手机的剪贴板
//!   （content URI，由 `ClipFilesProvider` 提供），别的应用读的时候才经这里向对方取（[`list`]、[`open`]、[`read`]）；
//! * 手机这边的文件（分享给 Airhand 的）：Kotlin 打开每个文件、把文件描述符交过来（[`FdFiles`]），公告给对方，
//!   对方粘贴时从这些文件描述符读。只有文件，没有文件夹（分享菜单给不了文件夹）。

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
#[cfg(not(unix))]
use std::io::{Seek, SeekFrom};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use airhand_link::files::{Provider, RemoteFile, RemoteOffer};
use airhand_proto::files::{FileEntry, FileError, FileKind, valid_name};
use serde::Deserialize;
use serde_json::json;

/// 最多记着这么多个对方的公告
const KEEP: usize = 8;

#[derive(Default)]
struct Registry {
    offers: Vec<(i64, RemoteOffer)>,
    files: HashMap<i64, Arc<Mutex<RemoteFile>>>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(Mutex::default)
}

static NEXT_FILE: AtomicI64 = AtomicI64::new(1);

/// 记下对方的一个公告，返回给 Kotlin 的 `key`（Kotlin 的 long：公告编号的低 63 位）
pub fn register(offer: RemoteOffer) -> i64 {
    let key = (offer.key() & i64::MAX as u64) as i64;
    let mut r = registry().lock().unwrap();
    r.offers.retain(|(k, _)| *k != key);
    r.offers.push((key, offer));
    while r.offers.len() > KEEP {
        r.offers.remove(0);
    }
    key
}

fn offer(key: i64) -> Option<RemoteOffer> {
    registry()
        .lock()
        .unwrap()
        .offers
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, o)| o.clone())
}

/// 公告给 Kotlin 的样子：`{"type":"clipboardFiles","key","items"}`
pub fn event(key: i64, offer: &RemoteOffer) -> String {
    json!({ "type": "clipboardFiles", "key": key, "items": offer.items() }).to_string()
}

/// 放进剪贴板了（放不了也要说）
pub fn ready(key: i64) {
    if let Some(o) = offer(key) {
        o.ready();
    }
}

/// 列目录：`{"entries":[...]}` 或 `{"error":"notFound"}`（阻塞的）
pub fn list(key: i64, path: &str, recursive: bool) -> String {
    let r = (|| {
        let path: Vec<String> = serde_json::from_str(path).map_err(|_| FileError::Invalid)?;
        offer(key)
            .ok_or(FileError::Expired)?
            .list_blocking(&path, recursive)
    })();
    match r {
        Ok(entries) => json!({ "entries": entries }).to_string(),
        Err(e) => json!({ "error": e }).to_string(),
    }
}

/// 打开对方的一个文件（读的时候才取），返回文件的编号；公告不在了是 0
pub fn open(key: i64, path: &str, size: i64, mtime: i64) -> i64 {
    let Ok(path) = serde_json::from_str::<Vec<String>>(path) else {
        return 0;
    };
    let Some(o) = offer(key) else { return 0 };
    let mtime = (mtime >= 0).then_some(mtime as u64);
    let f = o.open(path, size.max(0) as u64, mtime);
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    registry()
        .lock()
        .unwrap()
        .files
        .insert(id, Arc::new(Mutex::new(f)));
    id
}

/// 从 `offset` 开始读满 `buf`（到末尾为止），返回读了多少；出错是负数（`-1 - FileError 的编号`）。阻塞的
pub fn read(file: i64, offset: u64, buf: &mut [u8]) -> i64 {
    let Some(f) = registry().lock().unwrap().files.get(&file).cloned() else {
        return -1 - FileError::Closed.code() as i64;
    };
    let mut f = f.lock().unwrap();
    f.seek(offset);
    let mut got = 0;
    while got < buf.len() {
        match f.read_blocking(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) => return -1 - e.code() as i64,
        }
    }
    got as i64
}

pub fn close(file: i64) {
    registry().lock().unwrap().files.remove(&file);
}

/// Kotlin 交过来的手机上的一个文件
#[derive(Debug, Deserialize)]
pub struct FdFile {
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub mtime: Option<u64>,
    /// 打开着的文件描述符（`ParcelFileDescriptor.detachFd()`）：交给这里之后由这里关
    pub fd: i32,
}

/// 手机上分享给 Airhand 的文件（见模块说明）
pub struct FdFiles {
    files: Vec<(FileEntry, Arc<File>)>,
}

impl FdFiles {
    /// 名字不合规矩的、重名的不要（它们的文件描述符随之关掉）
    pub fn new(list: Vec<FdFile>) -> Self {
        let mut files: Vec<(FileEntry, Arc<File>)> = Vec::new();
        for f in list {
            // SAFETY: Kotlin 把文件描述符的所有权交过来了（detachFd）
            let file = unsafe { from_fd(f.fd) };
            if !valid_name(&f.name) || files.iter().any(|(e, _)| e.name() == f.name) {
                continue;
            }
            let entry = FileEntry {
                path: vec![f.name],
                kind: FileKind::File,
                size: f.size,
                mtime: f.mtime,
                target: None,
            };
            files.push((entry, Arc::new(file)));
        }
        Self { files }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[cfg(unix)]
unsafe fn from_fd(fd: i32) -> File {
    use std::os::fd::FromRawFd;
    unsafe { File::from_raw_fd(fd) }
}

#[cfg(not(unix))]
unsafe fn from_fd(_fd: i32) -> File {
    unreachable!("only on Android")
}

impl Provider for FdFiles {
    fn items(&self) -> Vec<FileEntry> {
        self.files.iter().map(|(e, _)| e.clone()).collect()
    }

    fn read_dir(&self, _path: &[String]) -> Result<Vec<FileEntry>, FileError> {
        Err(FileError::NotFound)
    }

    fn open(
        &self,
        path: &[String],
        offset: u64,
    ) -> Result<(Box<dyn Read + Send>, FileEntry), FileError> {
        let [name] = path else {
            return Err(FileError::NotFound);
        };
        let (entry, file) = self
            .files
            .iter()
            .find(|(e, _)| e.name() == name)
            .ok_or(FileError::NotFound)?;
        Ok((
            Box::new(Reader {
                file: file.clone(),
                pos: offset,
            }),
            entry.clone(),
        ))
    }
}

/// 同一个文件可能同时被取几次：各读各的位置（pread），不共用文件描述符的读写位置
struct Reader {
    file: Arc<File>,
    pos: u64,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(&*self.file, buf, self.pos)?;
        #[cfg(not(unix))]
        let n = {
            let mut f = &*self.file;
            f.seek(SeekFrom::Start(self.pos))?;
            f.read(buf)?
        };
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_answers_errors_as_json() {
        assert_eq!(list(12345, "[\"x\"]", false), r#"{"error":"expired"}"#);
        assert_eq!(list(12345, "not json", false), r#"{"error":"invalid"}"#);
        assert_eq!(open(12345, "[\"x\"]", 1, -1), 0);
        assert_eq!(
            read(999, 0, &mut [0u8; 4]),
            -1 - FileError::Closed.code() as i64
        );
    }
}
