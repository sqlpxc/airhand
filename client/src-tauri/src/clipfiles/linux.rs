//! Linux：对方复制的文件放在一个 FUSE 文件系统里，剪贴板里放它们的路径（见 [`super`]）。**读到哪里才向对方取到哪里**。
//!
//! * 文件系统挂在 `$XDG_RUNTIME_DIR/airhand-clip/<进程号>`（只读，只有本用户能看），每个公告一个子目录
//!   （按 [`Source::key`]），下面是对方选中的那几项。列目录时才向对方列（一层），读文件时才向对方取，
//!   文件管理器自己显示进度、可以取消。文件夹里指向文件夹外面的链接不呈现（见 `airhand_proto::files` 的"链接"）；
//! * 剪贴板（X11 的 CLIPBOARD，Wayland 的程序经 XWayland 也看得到）由一个专门的线程拥有，提供
//!   `text/uri-list`（KDE、GTK4）、`x-special/gnome-copied-files`（GNOME、Xfce、MATE 的文件管理器）和路径文本；
//! * 本程序上次没退干净（崩溃）留下的挂载点，启动后第一次挂载时卸掉。
//!
//! 没有 FUSE（`/dev/fuse` 或 `fusermount3` 没有，例如在容器里）时不接收文件，只同步文本（对方照样等得到"放好了"）。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};

use airhand_proto::files::{FileEntry, FileError, FileKind, link_stays_inside};
use fuser::{
    BackgroundSession, FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::{Source, SourceFile};

/// 属性、目录项的缓存时间
const TTL: Duration = Duration::from_secs(1);
/// 最多留着这么多个公告（文件管理器可能还在从之前的那个复制）
const KEEP: usize = 4;
const ROOT: u64 = 1;

/// 一个节点：根目录、一个公告的目录、公告里的东西
struct Node {
    /// 哪个公告（根目录是 0）
    key: u64,
    /// 对方那里的路径（公告的目录是空的）
    path: Vec<String>,
    kind: FileKind,
    size: u64,
    mtime: Option<u64>,
    target: Option<String>,
    /// 下面的（列过才有）：名字 → 节点
    children: Option<Vec<(String, u64)>>,
}

#[derive(Default)]
struct State {
    sources: Vec<(u64, Arc<dyn Source>)>,
    /// 第 i 项是节点 i + 1
    nodes: Vec<Node>,
    handles: HashMap<u64, Arc<Mutex<Box<dyn SourceFile>>>>,
    next_fh: u64,
}

impl State {
    fn node(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(ino.checked_sub(1)? as usize)
    }

    fn add(&mut self, n: Node) -> u64 {
        self.nodes.push(n);
        self.nodes.len() as u64
    }

    fn source(&self, key: u64) -> Option<Arc<dyn Source>> {
        self.sources
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, s)| s.clone())
    }
}

fn uid_gid() -> (u32, u32) {
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn attr(ino: u64, n: &Node) -> FileAttr {
    let (uid, gid) = uid_gid();
    let mtime = n
        .mtime
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms))
        .unwrap_or(UNIX_EPOCH);
    let (kind, perm, size) = match n.kind {
        FileKind::Dir => (FileType::Directory, 0o555, 0),
        FileKind::Link => (
            FileType::Symlink,
            0o777,
            n.target.as_ref().map_or(0, |t| t.len() as u64),
        ),
        _ => (FileType::RegularFile, 0o444, n.size),
    };
    FileAttr {
        ino,
        size,
        blocks: size.div_ceil(512),
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind,
        perm,
        nlink: 1,
        uid,
        gid,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

/// 公告的目录名
fn key_name(key: u64) -> String {
    format!("{key:x}")
}

struct Fs(Arc<Mutex<State>>);

impl Fs {
    /// 节点 `ino` 下面的（第一次时向对方列）。阻塞的
    fn children(&self, ino: u64) -> Result<Vec<(String, u64)>, i32> {
        let (key, path, kind) = {
            let st = self.0.lock().unwrap();
            let n = st.node(ino).ok_or(libc::ENOENT)?;
            if let Some(c) = &n.children {
                return Ok(c.clone());
            }
            (n.key, n.path.clone(), n.kind)
        };
        if kind != FileKind::Dir {
            return Err(libc::ENOTDIR);
        }
        let entries: Vec<FileEntry> = if ino == ROOT {
            Vec::new()
        } else {
            let source = self.0.lock().unwrap().source(key).ok_or(libc::ENOENT)?;
            if path.is_empty() {
                source.items()
            } else {
                source.list(&path, false).map_err(errno)?
            }
        };
        let mut st = self.0.lock().unwrap();
        if ino == ROOT {
            let keys: Vec<u64> = st.sources.iter().map(|(k, _)| *k).collect();
            let mut out = Vec::new();
            for key in keys {
                let existing = st
                    .nodes
                    .iter()
                    .position(|n| n.key == key && n.path.is_empty())
                    .map(|i| i as u64 + 1);
                let ino = existing.unwrap_or_else(|| {
                    st.add(Node {
                        key,
                        path: Vec::new(),
                        kind: FileKind::Dir,
                        size: 0,
                        mtime: None,
                        target: None,
                        children: None,
                    })
                });
                out.push((key_name(key), ino));
            }
            // 根目录每次都重新看有哪些公告
            return Ok(out);
        }
        let mut out = Vec::new();
        for e in entries {
            let name = e.name().to_string();
            // 本机表示不了的链接（指到这个文件夹外面的）不呈现
            if e.kind == FileKind::Link
                && !e
                    .target
                    .as_deref()
                    .is_some_and(|t| !path.is_empty() && link_stays_inside(&path, t))
            {
                continue;
            }
            if !matches!(e.kind, FileKind::File | FileKind::Dir | FileKind::Link) {
                continue;
            }
            let mut full = path.clone();
            full.push(name.clone());
            let ino = st.add(Node {
                key,
                path: full,
                kind: e.kind,
                size: e.size,
                mtime: e.mtime,
                target: e.target,
                children: None,
            });
            out.push((name, ino));
        }
        st.nodes[ino as usize - 1].children = Some(out.clone());
        Ok(out)
    }
}

fn errno(e: FileError) -> i32 {
    match e {
        FileError::NotFound | FileError::Expired => libc::ENOENT,
        FileError::Denied => libc::EACCES,
        FileError::TooLarge => libc::EFBIG,
        FileError::Busy => libc::EBUSY,
        _ => libc::EIO,
    }
}

impl Filesystem for Fs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            return reply.error(libc::ENOENT);
        };
        match self.children(parent) {
            Ok(c) => match c.iter().find(|(n, _)| n == name) {
                Some((_, ino)) => {
                    let st = self.0.lock().unwrap();
                    let n = st.node(*ino).expect("listed");
                    reply.entry(&TTL, &attr(*ino, n), 0);
                }
                None => reply.error(libc::ENOENT),
            },
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let st = self.0.lock().unwrap();
        match st.node(ino) {
            Some(n) => reply.attr(&TTL, &attr(ino, n)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn readlink(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyData) {
        let st = self.0.lock().unwrap();
        match st.node(ino).and_then(|n| n.target.clone()) {
            Some(t) => reply.data(t.as_bytes()),
            None => reply.error(libc::EINVAL),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let children = match self.children(ino) {
            Ok(c) => c,
            Err(e) => return reply.error(e),
        };
        let st = self.0.lock().unwrap();
        let mut all = vec![(ino, FileType::Directory, ".".to_string())];
        all.push((ROOT, FileType::Directory, "..".to_string()));
        for (name, child) in children {
            let kind = match st.node(child).map(|n| n.kind) {
                Some(FileKind::Dir) => FileType::Directory,
                Some(FileKind::Link) => FileType::Symlink,
                _ => FileType::RegularFile,
            };
            all.push((child, kind, name));
        }
        for (i, (child, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            if reply.add(child, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(libc::EROFS);
        }
        let mut st = self.0.lock().unwrap();
        let Some(n) = st.node(ino) else {
            return reply.error(libc::ENOENT);
        };
        if n.kind != FileKind::File {
            return reply.error(libc::EISDIR);
        }
        let (key, path, size, mtime) = (n.key, n.path.clone(), n.size, n.mtime);
        let Some(source) = st.source(key) else {
            return reply.error(libc::ENOENT);
        };
        let file = source.open(path, size, mtime);
        st.next_fh += 1;
        let fh = st.next_fh;
        st.handles.insert(fh, Arc::new(Mutex::new(file)));
        reply.opened(fh, 0);
    }

    /// 读的时候要等对方的数据：在别的线程上等，不挡住这个文件系统别的操作
    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(file) = self.0.lock().unwrap().handles.get(&fh).cloned() else {
            return reply.error(libc::EBADF);
        };
        std::thread::spawn(move || {
            let mut f = file.lock().unwrap();
            f.seek(offset.max(0) as u64);
            let mut buf = vec![0u8; size as usize];
            let mut got = 0;
            while got < buf.len() {
                match f.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) => return reply.error(errno(e)),
                }
            }
            reply.data(&buf[..got]);
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.0.lock().unwrap().handles.remove(&fh);
        reply.ok();
    }
}

/// 挂好的文件系统
struct Mounted {
    dir: PathBuf,
    state: Arc<Mutex<State>>,
    _session: BackgroundSession,
}

/// 挂载点的上一层（本用户的运行时目录下）
fn base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("airhand-clip")
}

/// 本程序上次没退干净留下的挂载点：进程已经不在了的卸掉、删掉
fn clean_stale(base: &Path) {
    let Ok(dirs) = std::fs::read_dir(base) else {
        return;
    };
    for d in dirs.flatten() {
        let alive = d
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
            .is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
        if alive {
            continue;
        }
        let _ = std::process::Command::new("fusermount3")
            .arg("-uz")
            .arg(d.path())
            .status();
        let _ = std::fs::remove_dir(d.path());
    }
}

fn mount() -> Option<&'static Mounted> {
    static M: OnceLock<Option<Mounted>> = OnceLock::new();
    M.get_or_init(|| {
        let base = base();
        clean_stale(&base);
        let dir = base.join(std::process::id().to_string());
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("can't create {}: {e}", dir.display());
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
        let state = Arc::new(Mutex::new(State::default()));
        state.lock().unwrap().add(Node {
            key: 0,
            path: Vec::new(),
            kind: FileKind::Dir,
            size: 0,
            mtime: None,
            target: None,
            children: None,
        });
        let options = [
            MountOption::RO,
            MountOption::FSName("airhand".into()),
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
        ];
        match fuser::spawn_mount2(Fs(state.clone()), &dir, &options) {
            Ok(session) => Some(Mounted {
                dir,
                state,
                _session: session,
            }),
            Err(e) => {
                tracing::info!("no FUSE, the peer's files can't be pasted here: {e}");
                let _ = std::fs::remove_dir(&dir);
                None
            }
        }
    })
    .as_ref()
}

/// 剪贴板里的文件在哪里（本程序挂的文件系统里的路径：自己放的，不是本机复制的）
pub fn is_our_path(p: &Path) -> bool {
    p.starts_with(base().join(std::process::id().to_string()))
}

pub fn apply(source: Arc<dyn Source>) {
    std::thread::spawn(move || {
        if let Some(m) = mount() {
            let key = source.key();
            let items = source.items();
            {
                let mut st = m.state.lock().unwrap();
                st.sources.retain(|(k, _)| *k != key);
                st.sources.push((key, source.clone()));
                while st.sources.len() > KEEP {
                    st.sources.remove(0);
                }
                // 根目录重新列
                st.nodes[0].children = None;
            }
            let dir = m.dir.join(key_name(key));
            let paths: Vec<PathBuf> = items
                .iter()
                .filter(|i| matches!(i.kind, FileKind::File | FileKind::Dir))
                .map(|i| dir.join(i.name()))
                .collect();
            if let Err(e) = owner().set(key, paths) {
                tracing::warn!("can't put the peer's files on the clipboard: {e:#}");
            }
        }
        source.ready();
    });
}

pub fn withdraw(key: u64) {
    if owner().owns(key) {
        owner().clear();
    }
    if let Some(m) = mount() {
        let mut st = m.state.lock().unwrap();
        st.sources.retain(|(k, _)| *k != key);
        st.nodes[0].children = None;
    }
}

pub fn is_ours() -> bool {
    owner().owns_any()
}

// ───────────────────────────── 剪贴板 ─────────────────────────────

/// 拥有 X11 的 CLIPBOARD、回答别的程序要的格式（见模块说明）
struct Owner {
    /// 现在放着的：公告、路径
    current: Mutex<Option<(u64, Vec<PathBuf>)>>,
    tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

fn owner() -> &'static Owner {
    static O: OnceLock<Owner> = OnceLock::new();
    O.get_or_init(|| Owner {
        current: Mutex::new(None),
        tx: Mutex::new(None),
    })
}

/// 路径的 `file://` URI（不是字母数字和 `-._~/` 的字节都转义）
fn uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

impl Owner {
    fn owns(&self, key: u64) -> bool {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(k, _)| *k == key)
    }

    fn owns_any(&self) -> bool {
        self.current.lock().unwrap().is_some()
    }

    fn clear(&self) {
        *self.current.lock().unwrap() = None;
        if let Some(tx) = self.tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }

    /// 放上这些路径（另开一个线程拥有剪贴板，直到别的程序拿走或者 [`Owner::clear`]）
    fn set(&'static self, key: u64, paths: Vec<PathBuf>) -> anyhow::Result<()> {
        self.clear();
        let (conn, screen) = x11rb::connect(None)?;
        let root = conn.setup().roots[screen].root;
        let win = conn.generate_id()?;
        conn.create_window(
            0,
            win,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let atom = |name: &str| -> anyhow::Result<u32> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let atoms = Atoms {
            clipboard: atom("CLIPBOARD")?,
            targets: atom("TARGETS")?,
            timestamp: atom("TIMESTAMP")?,
            uri_list: atom("text/uri-list")?,
            gnome: atom("x-special/gnome-copied-files")?,
            utf8: atom("UTF8_STRING")?,
            text_plain: atom("text/plain;charset=utf-8")?,
            marker: atom("AIRHAND_REMOTE_FILES")?,
        };
        conn.set_selection_owner(win, atoms.clipboard, x11rb::CURRENT_TIME)?;
        let got = conn.get_selection_owner(atoms.clipboard)?.reply()?.owner;
        anyhow::ensure!(got == win, "another program kept the clipboard");
        conn.flush()?;
        let (tx, rx) = std::sync::mpsc::channel();
        *self.current.lock().unwrap() = Some((key, paths.clone()));
        *self.tx.lock().unwrap() = Some(tx);
        std::thread::Builder::new()
            .name("airhand-clipfiles".into())
            .spawn(move || {
                serve(&conn, &atoms, &paths, &rx);
                let _ = conn.destroy_window(win);
                let _ = conn.flush();
                let mut current = self.current.lock().unwrap();
                if current.as_ref().is_some_and(|(k, _)| *k == key) {
                    *current = None;
                }
            })?;
        Ok(())
    }
}

struct Atoms {
    clipboard: u32,
    targets: u32,
    timestamp: u32,
    uri_list: u32,
    gnome: u32,
    utf8: u32,
    text_plain: u32,
    marker: u32,
}

/// 回答别的程序要的格式，直到别的程序拿走剪贴板（SelectionClear）或者要我们放开
fn serve(
    conn: &impl Connection,
    a: &Atoms,
    paths: &[PathBuf],
    stop: &std::sync::mpsc::Receiver<()>,
) {
    let uris: Vec<String> = paths.iter().map(|p| uri(p)).collect();
    let uri_list = uris.iter().map(|u| format!("{u}\r\n")).collect::<String>();
    let gnome = std::iter::once("copy".to_string())
        .chain(uris.iter().cloned())
        .collect::<Vec<_>>()
        .join("\n");
    let text = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    loop {
        if stop.try_recv().is_ok() {
            return;
        }
        let event = match conn.poll_for_event() {
            Ok(Some(e)) => e,
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(_) => return,
        };
        match event {
            Event::SelectionClear(e) if e.selection == a.clipboard => return,
            Event::SelectionRequest(req) => {
                let property = if req.property == u32::from(AtomEnum::NONE) {
                    req.target
                } else {
                    req.property
                };
                let t = req.target;
                let ok = if t == a.targets {
                    let list = [
                        a.targets,
                        a.timestamp,
                        a.uri_list,
                        a.gnome,
                        a.utf8,
                        a.text_plain,
                        u32::from(AtomEnum::STRING),
                        a.marker,
                    ];
                    conn.change_property32(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok()
                } else if t == a.timestamp {
                    conn.change_property32(
                        PropMode::REPLACE,
                        req.requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[0],
                    )
                    .is_ok()
                } else {
                    let data: Option<&str> = if t == a.uri_list {
                        Some(&uri_list)
                    } else if t == a.gnome {
                        Some(&gnome)
                    } else if t == a.utf8 || t == a.text_plain || t == u32::from(AtomEnum::STRING) {
                        Some(&text)
                    } else if t == a.marker {
                        Some("1")
                    } else {
                        None
                    };
                    data.is_some_and(|d| {
                        conn.change_property8(
                            PropMode::REPLACE,
                            req.requestor,
                            property,
                            t,
                            d.as_bytes(),
                        )
                        .is_ok()
                    })
                };
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property: if ok {
                        property
                    } else {
                        u32::from(AtomEnum::NONE)
                    },
                };
                let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内存里的一组文件（不经过网络）
    struct Fake {
        key: u64,
        ready: Mutex<bool>,
    }

    fn e(path: &[&str], kind: FileKind, size: u64, target: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            size,
            mtime: Some(1_700_000_000_000),
            target: target.map(str::to_string),
        }
    }

    fn big() -> Vec<u8> {
        (0..3_000_000u32).map(|i| (i % 251) as u8).collect()
    }

    struct FakeFile {
        data: Vec<u8>,
        pos: u64,
    }

    impl SourceFile for FakeFile {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, FileError> {
            let rest = &self.data[(self.pos as usize).min(self.data.len())..];
            let n = rest.len().min(buf.len()).min(10_000);
            buf[..n].copy_from_slice(&rest[..n]);
            self.pos += n as u64;
            Ok(n)
        }
        fn seek(&mut self, pos: u64) {
            self.pos = pos;
        }
        fn position(&self) -> u64 {
            self.pos
        }
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn name(&self) -> String {
            String::new()
        }
    }

    impl Source for Fake {
        fn items(&self) -> Vec<FileEntry> {
            vec![
                e(&["proj"], FileKind::Dir, 0, None),
                e(&["note.txt"], FileKind::File, 5, None),
            ]
        }
        fn list(&self, path: &[String], _: bool) -> Result<Vec<FileEntry>, FileError> {
            match path.join("/").as_str() {
                "proj" => Ok(vec![
                    e(&["a:b.txt"], FileKind::File, 5, None),
                    e(&["sub"], FileKind::Dir, 0, None),
                    e(&["inside"], FileKind::Link, 0, Some("sub")),
                    e(&["outside"], FileKind::Link, 0, Some("/etc/passwd")),
                ]),
                "proj/sub" => Ok(vec![e(&["big.bin"], FileKind::File, 3_000_000, None)]),
                _ => Err(FileError::NotFound),
            }
        }
        fn open(&self, path: Vec<String>, _: u64, _: Option<u64>) -> Box<dyn SourceFile> {
            let data = match path.join("/").as_str() {
                "note.txt" => b"hello".to_vec(),
                "proj/a:b.txt" => b"colon".to_vec(),
                "proj/sub/big.bin" => big(),
                _ => Vec::new(),
            };
            Box::new(FakeFile { data, pos: 0 })
        }
        fn ready(&self) {
            *self.ready.lock().unwrap() = true;
        }
        fn key(&self) -> u64 {
            self.key
        }
    }

    /// 真的 FUSE 和 X11 剪贴板（要 /dev/fuse、fusermount3、DISPLAY）：对方的文件出现在挂载点里，读到的和对方的一样，
    /// 指到文件夹外面的链接不在；剪贴板里是它们的路径（文件管理器粘贴的就是这些）。
    /// 手动运行：`cargo test --lib clipfiles::linux -- --ignored`
    #[test]
    #[ignore]
    fn remote_files_appear_in_fuse_and_on_the_clipboard() {
        let fake = Arc::new(Fake {
            key: 0x5a,
            ready: Mutex::new(false),
        });
        apply(fake.clone());
        for _ in 0..100 {
            if *fake.ready.lock().unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(*fake.ready.lock().unwrap());
        let dir = mount().expect("FUSE").dir.join("5a");
        let mut names: Vec<_> = std::fs::read_dir(dir.join("proj"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["a:b.txt", "inside", "sub"]);
        assert_eq!(
            std::fs::read_link(dir.join("proj/inside")).unwrap(),
            Path::new("sub")
        );
        assert_eq!(std::fs::read(dir.join("note.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dir.join("proj/sub/big.bin")).unwrap(), big());
        // 跨过链接也读得到（在这个文件夹里）
        assert_eq!(
            std::fs::metadata(dir.join("proj/inside/big.bin"))
                .unwrap()
                .len(),
            3_000_000
        );
        assert!(is_ours());
        let files = arboard::Clipboard::new()
            .unwrap()
            .get()
            .file_list()
            .unwrap();
        assert_eq!(files, [dir.join("proj"), dir.join("note.txt")]);
        assert!(files.iter().all(|f| is_our_path(f)));
        withdraw(0x5a);
        assert!(!is_ours());
    }

    #[test]
    fn uris_escape_what_they_must() {
        assert_eq!(
            uri(Path::new("/run/a b/中.txt")),
            "file:///run/a%20b/%E4%B8%AD.txt"
        );
        assert_eq!(uri(Path::new("/x/y-z_1.~")), "file:///x/y-z_1.~");
    }
}
