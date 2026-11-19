//! 对方复制的文件放进本机剪贴板（剪贴板里的文件，见 `airhand_link::files`）：**粘贴时才传**。
//!
//! | 平台 | 怎么放 |
//! |---|---|
//! | Windows | "虚拟文件"（`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`，和远程桌面一样）：资源管理器粘贴时才要每个文件的内容，读多少取多少，进度和取消都是资源管理器自己的（`windows`） |
//! | Linux | 一个 FUSE 文件系统，剪贴板里放它里面的路径：文件管理器读到哪里才取到哪里（`linux`）；没有 FUSE 时不放 |
//! | macOS | 菜单栏保留最新的清单；选“接收文件到…”并选定文件夹后才传（`macos`） |
//!
//! 对方的名字在本机不一定合法（Linux 上可以有 `:`、`?`，Windows 不区分大小写……）：放进剪贴板之前按本机的规矩改
//! （[`local_names`]），改不了的（路径太长）跳过；链接本机表示不了的也跳过（见 `airhand_proto::files` 的"链接"）。

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

use airhand_link::files::RemoteOffer;
use airhand_proto::files::{FileEntry, FileKind};

#[cfg(any(windows, target_os = "linux"))]
pub use blocking::{Source, SourceFile, apply_source};

// Windows and FUSE supply files through blocking callbacks. The Mac menu owns an async RemoteOffer.
#[cfg(any(windows, target_os = "linux"))]
mod blocking {
    use super::*;
    use airhand_link::files::RemoteFile;
    use airhand_proto::files::FileError;
    use std::sync::Arc;

    /// 对方复制的文件从哪里取：一般就是会话里的 [`RemoteOffer`]；Windows 服务里的被控端（SYSTEM）把它交给托盘
    /// （登录的用户）放进剪贴板，托盘经管道向被控端取（见 `winsvc::clip`）。方法都是阻塞的
    pub trait Source: Send + Sync + 'static {
        /// 用户在对方那里选中的那几项
        fn items(&self) -> Vec<FileEntry>;
        /// 列出目录（见 [`RemoteOffer::list`]）
        fn list(&self, path: &[String], recursive: bool) -> Result<Vec<FileEntry>, FileError>;
        /// 打开文件（读的时候才去取）
        fn open(&self, path: Vec<String>, size: u64, mtime: Option<u64>) -> Box<dyn SourceFile>;
        /// 放进剪贴板了（放不了也要说）
        fn ready(&self);
        /// 同一个公告（拿掉时认它）
        fn key(&self) -> u64;
    }

    /// 对方的一个文件（见 [`RemoteFile`]）。方法都是阻塞的
    #[cfg_attr(not(windows), allow(dead_code))]
    pub trait SourceFile: Send {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, FileError>;
        fn seek(&mut self, pos: u64);
        fn position(&self) -> u64;
        fn size(&self) -> u64;
        fn name(&self) -> String;
    }

    /// 会话里的公告：直接向对方取
    pub(super) struct Offer(pub(super) RemoteOffer);

    impl Source for Offer {
        fn items(&self) -> Vec<FileEntry> {
            self.0.items().to_vec()
        }

        fn list(&self, path: &[String], recursive: bool) -> Result<Vec<FileEntry>, FileError> {
            self.0.list_blocking(path, recursive)
        }

        fn open(&self, path: Vec<String>, size: u64, mtime: Option<u64>) -> Box<dyn SourceFile> {
            Box::new(self.0.open(path, size, mtime))
        }

        fn ready(&self) {
            self.0.ready();
        }

        fn key(&self) -> u64 {
            offer_key(&self.0)
        }
    }

    impl SourceFile for RemoteFile {
        fn read(&mut self, buf: &mut [u8]) -> Result<usize, FileError> {
            self.read_blocking(buf)
        }

        fn seek(&mut self, pos: u64) {
            RemoteFile::seek(self, pos);
        }

        fn position(&self) -> u64 {
            RemoteFile::position(self)
        }

        fn size(&self) -> u64 {
            RemoteFile::size(self)
        }

        fn name(&self) -> String {
            RemoteFile::name(self).to_string()
        }
    }

    /// 同 [`apply`]，从任意的 [`Source`] 取
    pub fn apply_source(source: Arc<dyn Source>) {
        #[cfg(windows)]
        windows::apply(source);
        #[cfg(target_os = "linux")]
        linux::apply(source);
    }
}

/// 一个公告的编号（同一个进程里不重复）
pub fn offer_key(offer: &RemoteOffer) -> u64 {
    offer.key()
}

/// 对方复制了文件：放进本机剪贴板（之前放的随之作废）。放好了（放不了也一样）告诉对方。
/// Windows 服务里的被控端是 SYSTEM：资源管理器（登录的用户）调不了它的数据对象，交给托盘放（见 `winsvc::clip`）
pub fn apply(offer: RemoteOffer) {
    #[cfg(target_os = "macos")]
    macos::apply(offer);
    #[cfg(windows)]
    if crate::winsvc::is_agent() {
        return crate::winsvc::clip::place(offer);
    }
    #[cfg(any(windows, target_os = "linux"))]
    apply_source(std::sync::Arc::new(blocking::Offer(offer)));
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    offer.ready();
}

/// 会话结束了：本机剪贴板里还是它的文件的话拿掉（之后再粘贴也取不到了）
pub fn withdraw(offer: &RemoteOffer) {
    #[cfg(windows)]
    if crate::winsvc::is_agent() {
        return crate::winsvc::clip::withdraw(offer_key(offer));
    }
    withdraw_key(offer_key(offer));
}

/// 同 [`withdraw`]，按公告编号
pub fn withdraw_key(key: u64) {
    #[cfg(windows)]
    windows::withdraw(key);
    #[cfg(target_os = "linux")]
    linux::withdraw(key);
    #[cfg(target_os = "macos")]
    macos::withdraw(key);
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    let _ = key;
}

/// 剪贴板里的这个路径是不是自己放的（Linux：在本程序挂的文件系统里）：不当成本机复制的文件
pub fn is_our_path(p: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    return linux::is_our_path(p);
    #[cfg(not(target_os = "linux"))]
    {
        let _ = p;
        false
    }
}

/// 本机剪贴板里是不是对方的文件（自己放的）：是的话不当成本机复制了东西（不然又公告回去）
#[allow(dead_code)]
pub fn is_ours() -> bool {
    #[cfg(windows)]
    return windows::is_ours();
    #[cfg(target_os = "linux")]
    return linux::is_ours();
    #[cfg(not(any(windows, target_os = "linux")))]
    false
}

/// 放进本机剪贴板的一项：对方那里的路径，本机的名字（几段，按本机的规矩改过）
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    pub remote: Vec<String>,
    pub local: Vec<String>,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: Option<u64>,
}

/// 名字在本机的样子（Windows 的规矩：不能有 `<>:"/\|?*` 和控制字符，结尾不能是点和空格，不能是 CON、NUL、COM1 这些设备名）
#[cfg_attr(not(windows), allow(dead_code))]
pub fn local_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.ends_with(['.', ' ']) {
        while s.ends_with(['.', ' ']) {
            s.pop();
        }
        s.push('_');
    }
    if s.is_empty() {
        s.push('_');
    }
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    s
}

/// 公告里的一项（和它下面的整棵树，`tree` 是对方列出来的，路径相对这一项）在本机的样子：先目录后里面的东西，
/// 名字按本机的规矩改过，同一个目录里只差大小写的改名（`a (2).txt`）。链接、路径太长（`max_path` 个字符以上）的跳过，
/// 返回跳过了几项
#[cfg_attr(not(windows), allow(dead_code))]
pub fn local_names(
    item: &FileEntry,
    tree: &[FileEntry],
    taken: &mut std::collections::HashSet<String>,
    max_path: usize,
    out: &mut Vec<Local>,
) -> usize {
    let mut skipped = 0;
    // 对方路径（相对这一项）→ 本机路径
    let mut dirs: std::collections::HashMap<Vec<String>, Vec<String>> = Default::default();
    let mut push =
        |remote_rel: &[String], parent: Vec<String>, e: &FileEntry, out: &mut Vec<Local>| {
            let name = unique(&parent, &local_name(e.name()), taken);
            let mut local = parent;
            local.push(name);
            if local.iter().map(|s| s.chars().count() + 1).sum::<usize>() > max_path {
                return None;
            }
            let mut remote = item.path.clone();
            remote.extend(remote_rel.iter().cloned());
            out.push(Local {
                remote,
                local: local.clone(),
                kind: e.kind,
                size: e.size,
                mtime: e.mtime,
            });
            Some(local)
        };
    match item.kind {
        FileKind::File | FileKind::Dir => {
            let Some(root) = push(&[], Vec::new(), item, out) else {
                return 1 + tree.len();
            };
            if item.kind == FileKind::Dir {
                dirs.insert(Vec::new(), root);
                for e in tree {
                    let (parent_rel, _) = e.path.split_at(e.path.len().saturating_sub(1));
                    let Some(parent) = dirs.get(parent_rel).cloned() else {
                        // 上一层跳过了
                        skipped += 1;
                        continue;
                    };
                    match e.kind {
                        FileKind::Dir | FileKind::File => match push(&e.path, parent, e, out) {
                            Some(local) if e.kind == FileKind::Dir => {
                                dirs.insert(e.path.clone(), local);
                            }
                            Some(_) => {}
                            None => skipped += 1,
                        },
                        // 本机表示不了的链接、不认识的
                        _ => skipped += 1,
                    }
                }
            }
        }
        _ => skipped += 1,
    }
    skipped
}

/// 同一个目录里不区分大小写地不重名：重了就加 ` (2)`、` (3)`……（加在扩展名前面）
#[cfg_attr(not(windows), allow(dead_code))]
fn unique(parent: &[String], name: &str, taken: &mut std::collections::HashSet<String>) -> String {
    let key = |n: &str| {
        let mut k = parent.join("\\");
        k.push('\\');
        k.push_str(&n.to_lowercase());
        k
    };
    if taken.insert(key(name)) {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2.. {
        let candidate = format!("{stem} ({n}){ext}");
        if taken.insert(key(&candidate)) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_local_rules() {
        assert_eq!(local_name("a:b?.txt"), "a_b_.txt");
        assert_eq!(local_name("notes. "), "notes_");
        assert_eq!(local_name("dots..."), "dots_");
        assert_eq!(local_name("con.txt"), "_con.txt");
        assert_eq!(local_name("COM3"), "_COM3");
        assert_eq!(local_name("COMMON"), "COMMON");
        assert_eq!(local_name("tab\there"), "tab_here");
        assert_eq!(local_name("中文 名字.pdf"), "中文 名字.pdf");
    }

    fn e(path: &[&str], kind: FileKind) -> FileEntry {
        FileEntry {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            size: 1,
            mtime: None,
            target: None,
        }
    }

    #[test]
    fn trees_become_local_paths() {
        let item = e(&["proj"], FileKind::Dir);
        let tree = [
            e(&["A.txt"], FileKind::File),
            e(&["a.txt"], FileKind::File),
            e(&["lib64"], FileKind::Link),
            e(&["src"], FileKind::Dir),
            e(&["src", "x:y.rs"], FileKind::File),
            e(&["deep"], FileKind::Dir),
            e(&["deep", &"n".repeat(300)], FileKind::Dir),
            e(&["deep", &"n".repeat(300), "f"], FileKind::File),
        ];
        let mut out = Vec::new();
        let skipped = local_names(&item, &tree, &mut Default::default(), 259, &mut out);
        let got: Vec<_> = out.iter().map(|l| l.local.join("\\")).collect();
        assert_eq!(
            got,
            [
                "proj",
                "proj\\A.txt",
                "proj\\a (2).txt",
                "proj\\src",
                "proj\\src\\x_y.rs",
                "proj\\deep"
            ]
        );
        // 链接、太长的目录和它下面的
        assert_eq!(skipped, 3);
        assert_eq!(out[4].remote, ["proj", "src", "x:y.rs"]);
    }
}
