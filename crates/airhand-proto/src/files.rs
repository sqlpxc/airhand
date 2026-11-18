//! 剪贴板里的文件：**复制时只发清单，粘贴时才传内容**。
//!
//! 两个方向一模一样（被控端复制、控制端粘贴，或者反过来），消息都是 [`FileMsg`]（被控端发的包在
//! [`crate::session::HostMsg::Files`] 里，控制端发的包在 [`crate::session::ControllerMsg::Files`] 里）。
//! 下面说"给的一方"是剪贴板里有文件的那一端，"取的一方"是要粘贴的那一端。
//!
//! # 公告
//!
//! 给的一方的剪贴板里有了文件（本机复制了文件），它发 [`FileMsg::Offer`]：只有用户选中的那几项（名称、是文件还是目录、
//! 大小、修改时间），文件夹不展开（复制一个很大的文件夹时不用先把它走一遍）。公告取代对方剪贴板里之前的东西；
//! 之后来的文字（`Clipboard`）也取代公告。取的一方把公告放进本机剪贴板（各平台怎么放见 `docs/architecture.md`），
//! 放好了回答 [`FileMsg::Ready`]：控制端在会话里按下粘贴键时，等公告到了、放好了再把这个键发过去
//! （公告走大块通道、按键走控制通道，两条通道之间没有先后）。
//!
//! # 粘贴时
//!
//! 取的一方在本机真的有程序要内容时才去取：
//!
//! * [`FileMsg::List`]：列出公告里的一个目录，`recursive` 时是整棵树（Windows 粘贴时要一次拿到）。分页，
//!   回答 [`FileMsg::Listing`]（走大块通道）；
//! * [`FileMsg::Fetch`]：从 `offset` 开始取一个文件。给的一方在文件通道（[`crate::SessionLane::File`]）上开一条帧流
//!   开头是明文的取用编号（`u32 LE`），之后是一帧一帧的密文
//!   （`u32 LE 长度 | 密封单元`，每帧最多 [`CHUNK`] 字节的内容），一直写到文件末尾，结束这条流：
//!   * 每次取用一把密钥：从会话密钥按方向和取用编号派生，计数从 0 开始、必须一个接一个。中转没法把别的取用的帧搬进来、
//!     调换、删掉；截短了取的一方核对长度（取多少是它自己说的）就知道；
//!   * **额度**：给的一方最多发到 `until` 这个位置（`Fetch` 里给初始值，之后 [`FileMsg::Credit`] 往后推）。取的一方
//!     按本机程序读走了多少给额度（[`WINDOW`]），所以中转上为一次取用最多积压这么多，占不满同一条连接上别的流的窗口；
//!   * 取的一方不要了：[`FileMsg::Cancel`]（同时叫停这条流，[`reset::CANCELLED`]）；
//!   * 给的一方给不了（文件没了、变了、没权限、经中转太大）：[`FileMsg::Failed`]。
//! * 取用编号由取的一方分配，一个会话里严格递增；给的一方拒绝不递增的（同一把密钥不能加密两份不一样的内容）。
//!
//! 给的一方只给公告里的东西：路径的第一段是公告里的一项，之后的每一段都在它下面（[`valid_path`]），不能有 `..`。
//!
//! # 走哪条路、经中转多大
//!
//! 文件的流直连通着就走直连，不占中转。只能走中转时，**一次粘贴（一个公告）经中转的总量**不能超过服务器的上限
//! （[`crate::signal::ServerHello::file_limit`]）：给的一方第一次要经中转发一个公告里的东西时，算一下整个公告有多大
//! （超过上限就停下，不用走完），超过了就回答 [`FileError::TooLarge`]。服务器看不到内容，按单条流兜底：
//! 一条文件流转发的字节超过 `wire_size(上限)` 就两头都 reset（[`reset::RELAY_LIMIT`]）。
//!
//! # 链接
//!
//! 文件夹里的链接（符号链接、Windows 的目录联接）不跟随，列成 [`FileKind::Link`]，带着它指向的路径（`target`，分隔符一律是 `/`）。
//! 最上层那一项本身是链接时，公告的是它指向的东西（用户选中它，要的是内容）。取的一方只重建 `target` 是相对路径、
//! 不出这个文件夹的链接（[`link_stays_inside`]），别的跳过。

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::SessionLane;
use crate::cbor::open_enum;
use crate::framing::PREFIX;
use crate::handshake::SEAL_OVERHEAD;

/// 文件流上一帧最多这么多字节的内容
pub const CHUNK: usize = 256 * 1024;

/// 取的一方一次给多少额度：给的一方没被读走的最多这么多（中转上为一次取用最多积压这么多）
pub const WINDOW: u64 = 2 * 1024 * 1024;

/// 取的一方同时最多取这么多个文件（本机几个程序同时在读时），多的排队
pub const MAX_FETCHES: usize = 4;

/// 给的一方同时最多给这么多个（多出来的回答 [`FileError::Busy`]）
pub const MAX_SERVING: usize = 8;

/// 一页 [`FileMsg::Listing`] 最多这么多项
pub const PAGE: usize = 1000;

/// 一棵树（`recursive` 的 [`FileMsg::List`]）最多这么多项：再多的回答 [`FileError::TooMany`]
pub const MAX_TREE: usize = 100_000;

/// 一个公告最多这么多项（用户选中的）
pub const MAX_OFFER: usize = 10_000;

/// 路径最多这么多段
pub const MAX_DEPTH: usize = 64;

/// 路径的一段最多这么多字节（UTF-8）
pub const MAX_NAME: usize = 1024;

/// QUIC 的应用错误码：文件流为什么被 reset / 叫停。服务器 reset 时用 `RELAY_*`，两端都看得到
pub mod reset {
    /// 取的一方不要了
    pub const CANCELLED: u32 = 0x10;
    /// 给的一方读不下去了（原因在 [`super::FileMsg::Failed`] 里）
    pub const SOURCE: u32 = 0x11;
    /// 服务器：这条流超过了中转的上限（[`crate::signal::ServerHello::file_limit`]）
    pub const RELAY_LIMIT: u32 = 0x20;
    /// 服务器：超过了配额（预留：现在的服务器不限配额）
    pub const RELAY_QUOTA: u32 = 0x21;
    /// 服务器：不转文件流（别的原因，预留）
    pub const RELAY_REFUSED: u32 = 0x22;
}

/// 一条文件流运送 `len` 字节的内容时整条流有多少字节（会话头之后：取用编号 + 每帧的长度前缀和加密开销 + 内容）。
/// 服务器按它兜底（[`reset::RELAY_LIMIT`]）
pub const fn wire_size(len: u64) -> u64 {
    let chunks = len.div_ceil(CHUNK as u64);
    4 + chunks * (PREFIX + SEAL_OVERHEAD) as u64 + len
}

open_enum! {
    /// 公告、列表里的一项是什么
    pub enum FileKind {
        File = 0 => "file",
        Dir = 1 => "dir",
        /// 链接（不跟随，见模块说明的"链接"）
        Link = 2 => "link",
    }
}

open_enum! {
    /// 给不了的原因（取的一方按自己的界面语言显示）
    pub enum FileError {
        /// 没有这个文件（被删了、改名了），或者公告里没有这一项
        NotFound = 0 => "notFound",
        /// 没有权限读
        Denied = 1 => "denied",
        /// 复制之后文件变了（大小、修改时间对不上）
        Changed = 2 => "changed",
        /// 读的时候出错了
        Io = 3 => "io",
        /// 只能走中转，这次粘贴又超过了服务器允许的大小（见模块说明的"走哪条路"）
        TooLarge = 4 => "tooLarge",
        /// 这个公告已经不在了（对方的剪贴板又变了）
        Expired = 5 => "expired",
        /// 请求不对（路径不合规矩、取用编号不递增）
        Invalid = 6 => "invalid",
        /// 同时取的太多了，等一下再取
        Busy = 7 => "busy",
        /// 文件夹里的东西太多了（[`MAX_TREE`]）
        TooMany = 8 => "tooMany",
        /// 会话结束了、连接断了
        Closed = 9 => "closed",
    }
}

/// 公告、列表里的一项。JSON（交给界面的）是 camelCase
#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// 路径：公告里是一段（名称）；列表里是相对被列的那个目录的几段
    #[n(0)]
    pub path: Vec<String>,
    #[n(1)]
    pub kind: FileKind,
    /// 文件的大小（目录、链接是 0）
    #[n(2)]
    pub size: u64,
    /// 修改时间（Unix 毫秒）：不知道时没有
    #[n(3)]
    #[serde(default)]
    pub mtime: Option<u64>,
    /// 链接指向哪里（分隔符是 `/`）
    #[n(4)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl FileEntry {
    /// 名称（路径的最后一段）
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }
}

/// 剪贴板里的文件（见模块说明）。变体的编号是线上的类型，不能改、不能复用
#[derive(Encode, Decode, Debug, Clone, PartialEq, Eq)]
pub enum FileMsg {
    /// 给的一方：剪贴板里有了这些文件（用户选中的那几项）。`clip` 由给的一方分配，一个会话里递增
    #[n(0)]
    Offer {
        #[n(0)]
        clip: u32,
        #[n(1)]
        items: Vec<FileEntry>,
    },
    /// 取的一方：公告放进本机剪贴板了（放不了也回答，别让对方一直等）
    #[n(1)]
    Ready {
        #[n(0)]
        clip: u32,
    },
    /// 取的一方：列出公告 `clip` 里的目录 `path`（第一段是公告里的一项）。`recursive`：整棵树；
    /// `cursor`：从第几项开始（第一页是 0，之后照抄上一页的 `next`）
    #[n(2)]
    List {
        #[n(0)]
        req: u32,
        #[n(1)]
        clip: u32,
        #[n(2)]
        path: Vec<String>,
        #[n(3)]
        recursive: bool,
        #[n(4)]
        cursor: u32,
    },
    /// 给的一方：一页列表（`next`：还有，下一页从这里开始）。给不了时有 `error`、没有 `entries`
    #[n(3)]
    Listing {
        #[n(0)]
        req: u32,
        #[n(1)]
        entries: Vec<FileEntry>,
        #[n(2)]
        next: Option<u32>,
        #[n(3)]
        error: Option<FileError>,
    },
    /// 取的一方：从 `offset` 开始取公告 `clip` 里的文件 `path`，一直到末尾。`size`、`mtime`：它在公告、列表里的样子
    /// （对不上就是变了）；`until`：最多先发到这里（额度，见模块说明）
    #[n(4)]
    Fetch {
        #[n(0)]
        fetch: u32,
        #[n(1)]
        clip: u32,
        #[n(2)]
        path: Vec<String>,
        #[n(3)]
        offset: u64,
        #[n(4)]
        size: u64,
        #[n(5)]
        mtime: Option<u64>,
        #[n(6)]
        until: u64,
    },
    /// 取的一方：取用 `fetch` 可以发到 `until` 了（只往后推）
    #[n(5)]
    Credit {
        #[n(0)]
        fetch: u32,
        #[n(1)]
        until: u64,
    },
    /// 取的一方：不要了
    #[n(6)]
    Cancel {
        #[n(0)]
        fetch: u32,
    },
    /// 给的一方：取用 `fetch` 给不了（流开了的话随之 reset）
    #[n(7)]
    Failed {
        #[n(0)]
        fetch: u32,
        #[n(1)]
        error: FileError,
    },
}

impl FileMsg {
    /// 走哪条通道：公告、列表可能很大，走大块通道；别的都很小
    pub fn lane(&self) -> SessionLane {
        match self {
            FileMsg::Offer { .. } | FileMsg::Listing { .. } => SessionLane::Bulk,
            FileMsg::Ready { .. }
            | FileMsg::List { .. }
            | FileMsg::Fetch { .. }
            | FileMsg::Credit { .. }
            | FileMsg::Cancel { .. }
            | FileMsg::Failed { .. } => SessionLane::Control,
        }
    }
}

/// 路径的一段合不合规矩：不是空的、`.`、`..`，没有分隔符和 NUL，不太长
pub fn valid_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && s.len() <= MAX_NAME && !s.contains(['/', '\\', '\0'])
}

/// 一条路径合不合规矩（每一段都合规矩，不太深；空的不行）
pub fn valid_path(path: &[String]) -> bool {
    !path.is_empty() && path.len() <= MAX_DEPTH && path.iter().all(|s| valid_name(s))
}

/// 位于 `dir`（相对公告的根，不含链接自己的名字）的链接指向 `target` 时，指的地方还在公告的这棵树里：
/// `target` 是相对路径，`..` 不跳出最上层那一项。取的一方只重建这样的链接
pub fn link_stays_inside(dir: &[String], target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains('\\') || target.contains(':')
    {
        return false;
    }
    // 最上层那一项（dir[0]）本身是根：不能跳到它外面去
    let mut depth = dir.len() as i64;
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 1 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_size_counts_every_frame() {
        assert_eq!(wire_size(0), 4);
        assert_eq!(wire_size(1), 4 + 28 + 1);
        assert_eq!(wire_size(CHUNK as u64), 4 + 28 + CHUNK as u64);
        assert_eq!(wire_size(CHUNK as u64 + 1), 4 + 2 * 28 + CHUNK as u64 + 1);
        let fifty = 50 << 20;
        assert_eq!(wire_size(fifty), 4 + 200 * 28 + fifty);
    }

    #[test]
    fn paths_and_names() {
        let p = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(valid_path(&p(&["a", "b c", "中文.txt"])));
        assert!(!valid_path(&p(&[])));
        for bad in ["", ".", "..", "a/b", "a\\b", "a\0"] {
            assert!(!valid_name(bad), "{bad:?}");
            assert!(!valid_path(&p(&["ok", bad])), "{bad:?}");
        }
        assert!(valid_name("..."));
        assert!(!valid_name(&"x".repeat(MAX_NAME + 1)));
        assert!(!valid_path(&vec!["d".to_string(); MAX_DEPTH + 1]));
    }

    #[test]
    fn links_must_stay_in_the_tree() {
        let dir = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // .app 的 Versions/Current -> A，Headers -> Versions/Current/Headers
        assert!(link_stays_inside(&dir(&["Foo.framework", "Versions"]), "A"));
        assert!(link_stays_inside(
            &dir(&["Foo.framework"]),
            "Versions/Current/Headers"
        ));
        // pnpm：node_modules/x -> .pnpm/x@1/node_modules/x
        assert!(link_stays_inside(
            &dir(&["proj", "node_modules"]),
            ".pnpm/x@1/node_modules/x"
        ));
        assert!(link_stays_inside(&dir(&["proj", "a", "b"]), "../../c"));
        // 跳到最上层那一项外面、绝对路径、Windows 的路径
        assert!(!link_stays_inside(&dir(&["proj", "a"]), "../../c"));
        assert!(!link_stays_inside(&dir(&["proj"]), ".."));
        assert!(!link_stays_inside(&dir(&["proj"]), "/usr/bin/python3"));
        assert!(!link_stays_inside(&dir(&["proj"]), "C:/Windows"));
        assert!(!link_stays_inside(&dir(&["proj"]), "a\\b"));
        assert!(!link_stays_inside(&dir(&["proj"]), ""));
    }

    #[test]
    fn messages_roundtrip_and_pick_lanes() {
        let entry = FileEntry {
            path: vec!["photos".into()],
            kind: FileKind::Dir,
            size: 0,
            mtime: Some(1_700_000_000_000),
            target: None,
        };
        let msgs = [
            FileMsg::Offer {
                clip: 3,
                items: vec![entry.clone()],
            },
            FileMsg::Ready { clip: 3 },
            FileMsg::List {
                req: 1,
                clip: 3,
                path: vec!["photos".into()],
                recursive: true,
                cursor: 0,
            },
            FileMsg::Listing {
                req: 1,
                entries: vec![entry],
                next: Some(1000),
                error: None,
            },
            FileMsg::Fetch {
                fetch: 9,
                clip: 3,
                path: vec!["photos".into(), "a.jpg".into()],
                offset: 5,
                size: 10,
                mtime: None,
                until: WINDOW,
            },
            FileMsg::Credit {
                fetch: 9,
                until: 2 * WINDOW,
            },
            FileMsg::Cancel { fetch: 9 },
            FileMsg::Failed {
                fetch: 9,
                error: FileError::TooLarge,
            },
        ];
        for m in msgs {
            let back: FileMsg = minicbor::decode(&minicbor::to_vec(&m).unwrap()).unwrap();
            assert_eq!(back, m);
            let lane = m.lane();
            match m {
                FileMsg::Offer { .. } | FileMsg::Listing { .. } => {
                    assert_eq!(lane, SessionLane::Bulk)
                }
                _ => assert_eq!(lane, SessionLane::Control),
            }
        }
    }

    #[test]
    fn entry_json() {
        use serde_json::json;
        let e = FileEntry {
            path: vec!["a".into(), "b".into()],
            kind: FileKind::Link,
            size: 0,
            mtime: None,
            target: Some("../c".into()),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({ "path": ["a", "b"], "kind": "link", "size": 0, "mtime": null, "target": "../c" })
        );
        assert_eq!(
            serde_json::to_value(FileError::TooLarge).unwrap(),
            json!("tooLarge")
        );
    }
}
