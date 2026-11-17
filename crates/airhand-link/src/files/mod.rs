//! 剪贴板里的文件（协议见 `airhand_proto::files`）：复制时只发清单，粘贴时才传内容。
//!
//! 一个会话一个 [`Files`]，被控端和控制端用的是同一个：两端都既能**给**（本机剪贴板里有文件，[`Files::offer`]）
//! 又能**取**（对方的公告放进本机剪贴板，本机有程序粘贴时 [`RemoteOffer::list`] / [`RemoteOffer::open`]）。
//!
//! * 给的一方：本机的文件由平台的 [`Provider`] 读（桌面是 [`LocalFs`]，Android 是 content URI，网页控制台是浏览器里的
//!   File 对象）。只给公告里的东西；文件夹里的链接不跟随；取用编号必须递增；同时最多给 [`MAX_SERVING`] 个。
//!   直连通着就从直连发，不然经中转，这时一次粘贴（一个公告）的总量不能超过服务器的上限（见 `serve`）；
//! * 取的一方：[`RemoteFile`] 按本机程序读的位置去取（跳着读就换一条流），读走了多少就给多少额度（[`WINDOW`]）。
//!   平台的回调大多是阻塞的（Windows 的 `IStream::Read`、FUSE 的 `read`、Android 的代理文件描述符），所以它也有阻塞的读法
//!   （[`RemoteFile::read_blocking`]）。
//!
//! 公告、列表走大块通道，取用、额度、取消、失败走控制通道（发给对方的消息经 [`Peer`]：被控端是它的 `Outbox`，
//! 控制端是命令队列和大块通道）；文件的内容在文件通道上，每次取用一条帧流（见 `airhand_proto::files`）。
//!
//! 会话结束时调用 [`Files::close`]：给到一半的、取到一半的都停下（之后的读都是 [`FileError::Closed`]）。

mod fetch;
mod local;
mod serve;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_proto::files::{FileEntry, FileError, FileMsg};
use tokio::sync::{mpsc, watch};
use tracing::debug;

use crate::lanes::LaneStreams;
use crate::paths::Paths;
use crate::secure::FileKeys;
use crate::tasks::OwnedTask;

pub use airhand_proto::files::{MAX_FETCHES, MAX_SERVING, WINDOW};
pub use fetch::{RemoteFile, RemoteOffer};
pub use local::LocalFs;

/// 把文件消息发给对方。被控端经它的 `Outbox`，控制端经命令队列（控制通道）和大块通道
pub trait Peer: Send + Sync + 'static {
    /// 排进发送队列（走哪条通道看 [`FileMsg::lane`]）。排满了、会话结束了是 `false`
    fn send(&self, msg: FileMsg) -> bool;

    /// 会话结束了（[`Files::close`]）：放开发送队列，之后不再发
    fn close(&self) {}
}

/// 给的一方读本机的文件（见模块说明）。方法都是阻塞的：在 `spawn_blocking` 的线程上调用
pub trait Provider: Send + Sync + 'static {
    /// 公告里的那几项（用户选中的，按选中的顺序）：路径只有一段（名称），名称各不相同
    fn items(&self) -> Vec<FileEntry>;

    /// 列出目录 `path`（第一段是公告里的一项）下面的一层：每项的路径只有一段（名称）。链接不跟随，列成链接
    fn read_dir(&self, path: &[String]) -> Result<Vec<FileEntry>, FileError>;

    /// 打开文件 `path`，从 `offset` 开始读：读取端，和它现在的样子（大小、修改时间，和公告、列表里的对一下）
    fn open(
        &self,
        path: &[String],
        offset: u64,
    ) -> Result<(Box<dyn std::io::Read + Send>, FileEntry), FileError>;
}

/// 值得告诉用户的事（控制端交给界面显示）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNotice {
    /// 哪个文件（名称）
    pub name: String,
    pub error: FileError,
}

/// 一个会话的剪贴板文件（见模块说明）。复制出来的是同一个
#[derive(Clone)]
pub struct Files(Arc<Inner>);

pub(crate) struct Inner {
    /// 这个进程里的第几个（[`RemoteOffer::key`] 用）
    id: u64,
    session_id: u64,
    paths: Paths,
    keys: FileKeys,
    /// 一次粘贴经中转最多多少（服务器在版本握手里说的；`None` 不限）
    limit: Option<u64>,
    peer: Box<dyn Peer>,
    rt: tokio::runtime::Handle,
    local: Mutex<serve::Local>,
    remote: Mutex<fetch::Remote>,
    /// 本端最新的公告对方还没放好：它的编号（见 [`Files::await_ready`]）
    pending: watch::Sender<Option<u32>>,
    notices: Mutex<Option<mpsc::UnboundedSender<FileNotice>>>,
    closed: watch::Sender<bool>,
}

impl Files {
    /// 一个会话的剪贴板文件。`files`：对方开过来的文件流（中转、直连上的都在这里）；返回的任务收它们，丢掉时停止。
    /// 在 tokio 的运行时里调用
    pub fn start(
        session_id: u64,
        paths: Paths,
        keys: FileKeys,
        limit: Option<u64>,
        peer: impl Peer,
        files: LaneStreams,
    ) -> (Self, OwnedTask<()>) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let inner = Arc::new(Inner {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            session_id,
            paths,
            keys,
            limit,
            peer: Box::new(peer),
            rt: tokio::runtime::Handle::current(),
            local: Mutex::default(),
            remote: Mutex::default(),
            pending: watch::Sender::new(None),
            notices: Mutex::default(),
            closed: watch::Sender::new(false),
        });
        let task = OwnedTask(tokio::spawn(fetch::accept(inner.clone(), files)));
        (Self(inner), task)
    }

    /// 值得告诉用户的事（只能取一次；控制端交给界面）
    pub fn notices(&self) -> mpsc::UnboundedReceiver<FileNotice> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.notices.lock().unwrap() = Some(tx);
        rx
    }

    /// 本机剪贴板里有了这些文件：公告给对方（之前的公告作废，正在取的照样给完）。返回公告的编号；
    /// 一项都没有时不公告（`None`）
    pub fn offer(&self, provider: Arc<dyn Provider>) -> Option<u32> {
        serve::offer(&self.0, provider)
    }

    /// 本机剪贴板换成了别的（文字、清空了）：之前的公告不再是最新的（正在取的照样给完）
    pub fn withdraw(&self) {
        serve::withdraw(&self.0);
    }

    /// 本机最新的公告对方还没放好时等它放好（最多 `wait`）。返回 `true`：没有在等的，或者等到了
    pub async fn await_ready(&self, wait: Duration) -> bool {
        let mut rx = self.0.pending.subscribe();
        tokio::time::timeout(wait, rx.wait_for(Option::is_none))
            .await
            .is_ok()
    }

    /// 本机最新的公告对方还没放好
    pub fn awaiting_ready(&self) -> bool {
        self.0.pending.borrow().is_some()
    }

    /// 对方的剪贴板换成了文字：它之前的公告不再是最新的（[`RemoteOffer::is_current`]；正在读的照样读完）
    pub fn remote_cleared(&self) {
        fetch::cleared(&self.0);
    }

    /// 对方发来的文件消息。公告交给平台放进本机剪贴板（返回它）；别的在这里处理掉
    pub fn handle(&self, msg: FileMsg) -> Option<RemoteOffer> {
        if *self.0.closed.borrow() {
            return None;
        }
        match msg {
            FileMsg::Offer { clip, items } => return Some(fetch::offered(&self.0, clip, items)),
            FileMsg::Ready { clip } => {
                self.0.pending.send_if_modified(|p| {
                    let done = *p == Some(clip);
                    if done {
                        *p = None;
                    }
                    done
                });
            }
            FileMsg::List {
                req,
                clip,
                path,
                recursive,
                cursor,
            } => serve::list(&self.0, req, clip, path, recursive, cursor),
            FileMsg::Listing {
                req,
                entries,
                next,
                error,
            } => fetch::listing(&self.0, req, entries, next, error),
            FileMsg::Fetch {
                fetch,
                clip,
                path,
                offset,
                size,
                mtime,
                until,
            } => serve::fetch(
                &self.0,
                serve::Request {
                    fetch,
                    clip,
                    path,
                    offset,
                    size,
                    mtime,
                    until,
                },
            ),
            FileMsg::Credit { fetch, until } => serve::credit(&self.0, fetch, until),
            FileMsg::Cancel { fetch } => serve::cancel(&self.0, fetch),
            FileMsg::Failed { fetch, error } => fetch::failed(&self.0, fetch, error),
        }
        None
    }

    /// 会话结束了：停下给到一半的、取到一半的（见模块说明）
    pub fn close(&self) {
        self.0.closed.send_replace(true);
        serve::close(&self.0);
        fetch::close(&self.0);
        self.0.pending.send_replace(None);
        self.0.peer.close();
    }
}

impl Inner {
    /// 发给对方；排满了一会儿再试（最多 10 秒：会话卡住这么久，也就要结束了）
    fn send(self: &Arc<Self>, msg: FileMsg) {
        if self.peer.send(msg.clone()) {
            return;
        }
        let inner = Arc::downgrade(self);
        self.rt.spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let Some(inner) = inner.upgrade() else { return };
                if *inner.closed.borrow() {
                    return;
                }
                if inner.peer.send(msg.clone()) {
                    return;
                }
            }
            debug!("couldn't send a file message: the session's queue stayed full");
        });
    }

    /// 告诉用户（控制端的界面）
    fn notice(&self, name: &str, error: FileError) {
        if let Some(tx) = &*self.notices.lock().unwrap() {
            let _ = tx.send(FileNotice {
                name: name.to_string(),
                error,
            });
        }
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}

#[cfg(test)]
mod tests;
