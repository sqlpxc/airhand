//! 取的一方（见 [`super`]）：对方的公告交给平台（[`RemoteOffer`]），本机有程序要内容时列目录、按读的位置取文件
//! （[`RemoteFile`]），收对方开过来的文件流、解密、交给读的那一方。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::SessionLane;
use airhand_proto::files::{FileEntry, FileError, FileMsg, MAX_FETCHES, WINDOW, reset};
use bytes::Bytes;
use quinn::{ReadError, RecvStream, VarInt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::debug;

use super::Inner;
use crate::lanes::LaneStreams;
use crate::secure::Opener;
use crate::wire::{WireError, read_packet};

/// 等一页列表最多这么久
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// 等一条文件流开过来、读它开头的取用编号最多这么久
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// 取到一半连接断了（直连断了、换了路）：从断的地方再取，最多这么多次
const RETRIES: u8 = 3;
/// 最多记着这么多个对方的公告（最新的和之前的：本机程序可能还在从之前的那个读）
const KEEP_OFFERS: usize = 2;

/// 对方给的东西、本端在取的
pub(super) struct Remote {
    /// 对方的公告（编号），最新的在后面
    offers: VecDeque<u32>,
    /// 最新的公告已经被对方的文字取代了
    cleared: bool,
    next_req: u32,
    lists: HashMap<u32, oneshot::Sender<Page>>,
    next_fetch: u32,
    slots: HashMap<u32, Slot>,
    /// 同时最多取 [`MAX_FETCHES`] 个
    permits: Arc<Semaphore>,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            offers: VecDeque::new(),
            cleared: false,
            next_req: 0,
            lists: HashMap::new(),
            next_fetch: 0,
            slots: HashMap::new(),
            permits: Arc::new(Semaphore::new(MAX_FETCHES)),
        }
    }
}

/// 一页列表
type Page = Result<(Vec<FileEntry>, Option<u32>), FileError>;

/// 一次取用：文件流到了以后交给谁
struct Slot {
    /// 文件流到了时取走（只能有一条）
    opener: Option<Opener>,
    tx: mpsc::Sender<Delivery>,
    /// 这条流该有多少字节的内容
    expected: u64,
}

/// 文件流上来的
enum Delivery {
    Data(Bytes),
    /// 内容都到了
    End,
    Error(FileError),
}

/// 对方的公告：平台把它放进本机剪贴板，本机有程序粘贴时从这里列目录、取文件。复制出来的是同一个
#[derive(Clone)]
pub struct RemoteOffer {
    inner: Arc<Inner>,
    clip: u32,
    items: Arc<Vec<FileEntry>>,
}

impl std::fmt::Debug for RemoteOffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteOffer")
            .field("clip", &self.clip)
            .field("items", &self.items.len())
            .finish()
    }
}

/// 同一个会话的同一个公告
impl PartialEq for RemoteOffer {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner) && self.clip == other.clip
    }
}

pub(super) fn offered(inner: &Arc<Inner>, clip: u32, items: Vec<FileEntry>) -> RemoteOffer {
    // 名称不合规矩的、重名的不要（给的一方不该发这样的）
    let mut seen = std::collections::HashSet::new();
    let items: Vec<_> = items
        .into_iter()
        .filter(|i| {
            i.path.len() == 1
                && airhand_proto::files::valid_name(&i.path[0])
                && seen.insert(i.path[0].clone())
        })
        .collect();
    let mut r = inner.remote.lock().unwrap();
    r.offers.push_back(clip);
    while r.offers.len() > KEEP_OFFERS {
        r.offers.pop_front();
    }
    r.cleared = false;
    drop(r);
    debug!(
        clip,
        items = items.len(),
        "the peer offered clipboard files"
    );
    RemoteOffer {
        inner: inner.clone(),
        clip,
        items: Arc::new(items),
    }
}

pub(super) fn cleared(inner: &Inner) {
    inner.remote.lock().unwrap().cleared = true;
}

pub(super) fn listing(
    inner: &Inner,
    req: u32,
    entries: Vec<FileEntry>,
    next: Option<u32>,
    error: Option<FileError>,
) {
    if let Some(tx) = inner.remote.lock().unwrap().lists.remove(&req) {
        let _ = tx.send(match error {
            Some(e) => Err(e),
            None => Ok((entries, next)),
        });
    }
}

pub(super) fn failed(inner: &Inner, fetch: u32, error: FileError) {
    let slot = inner.remote.lock().unwrap().slots.remove(&fetch);
    if let Some(s) = slot {
        let _ = s.tx.try_send(Delivery::Error(error));
    }
}

pub(super) fn close(inner: &Inner) {
    let mut r = inner.remote.lock().unwrap();
    r.lists.clear();
    for (_, s) in r.slots.drain() {
        let _ = s.tx.try_send(Delivery::Error(FileError::Closed));
    }
    r.permits.close();
}

impl RemoteOffer {
    /// 公告的编号
    pub fn clip(&self) -> u32 {
        self.clip
    }

    /// 这个进程里不重复的编号（会话和公告的编号）：平台认出剪贴板里放的是不是它
    pub fn key(&self) -> u64 {
        (self.inner.id << 32) | u64::from(self.clip)
    }

    /// 公告里的那几项（用户在对方那里选中的）：路径只有一段（名称），名称各不相同
    pub fn items(&self) -> &[FileEntry] {
        &self.items
    }

    /// 还是对方剪贴板里最新的东西（对方之后又复制了别的就不是了；正在读的照样读得完）
    pub fn is_current(&self) -> bool {
        let r = self.inner.remote.lock().unwrap();
        !r.cleared && r.offers.back() == Some(&self.clip) && !self.inner.is_closed()
    }

    /// 会话的运行时（平台在自己的线程上用 [`RemoteOffer::list_blocking`]、[`RemoteFile::read_blocking`]）
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.inner.rt
    }

    /// 告诉对方：放进本机剪贴板了（放不了也要说，对方在等，见 `airhand_proto::files`）
    pub fn ready(&self) {
        self.inner.send(FileMsg::Ready { clip: self.clip });
    }

    /// 列出目录 `path`（第一段是公告里的一项）：`recursive` 时是整棵树（路径相对 `path`，先浅后深），不然只有一层
    pub async fn list(
        &self,
        path: &[String],
        recursive: bool,
    ) -> Result<Vec<FileEntry>, FileError> {
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let (entries, next) = self.page(path, recursive, cursor).await?;
            out.extend(entries);
            match next {
                Some(n) if n > cursor && out.len() <= airhand_proto::files::MAX_TREE => cursor = n,
                Some(_) => return Err(FileError::TooMany),
                None => return Ok(out),
            }
        }
    }

    /// 同 [`RemoteOffer::list`]，阻塞的（不能在异步任务里调用）
    pub fn list_blocking(
        &self,
        path: &[String],
        recursive: bool,
    ) -> Result<Vec<FileEntry>, FileError> {
        self.inner.rt.block_on(self.list(path, recursive))
    }

    async fn page(&self, path: &[String], recursive: bool, cursor: u32) -> Page {
        if self.inner.is_closed() {
            return Err(FileError::Closed);
        }
        let (tx, rx) = oneshot::channel();
        let req = {
            let mut r = self.inner.remote.lock().unwrap();
            r.next_req = r.next_req.wrapping_add(1);
            let req = r.next_req;
            r.lists.insert(req, tx);
            req
        };
        self.inner.send(FileMsg::List {
            req,
            clip: self.clip,
            path: path.to_vec(),
            recursive,
            cursor,
        });
        let got = tokio::time::timeout(LIST_TIMEOUT, rx).await;
        self.inner.remote.lock().unwrap().lists.remove(&req);
        match got {
            Ok(Ok(page)) => page,
            Ok(Err(_)) => Err(FileError::Closed),
            Err(_) => Err(FileError::Closed),
        }
    }

    /// 打开文件 `path`（列表里的大小、修改时间带上：给的一方据此核对复制之后变没变）。读的时候才去取
    pub fn open(&self, path: Vec<String>, size: u64, mtime: Option<u64>) -> RemoteFile {
        RemoteFile {
            inner: self.inner.clone(),
            clip: self.clip,
            path,
            size,
            mtime,
            pos: 0,
            active: None,
            retries: 0,
        }
    }
}

/// 对方的一个文件（[`RemoteOffer::open`]）：按读的位置去取（见模块说明），读走了多少给多少额度。
/// 丢掉时取消还在取的
pub struct RemoteFile {
    inner: Arc<Inner>,
    clip: u32,
    path: Vec<String>,
    size: u64,
    mtime: Option<u64>,
    /// 下一次读从哪里开始
    pos: u64,
    active: Option<Active>,
    retries: u8,
}

/// 正在取的一次
struct Active {
    fetch: u32,
    rx: mpsc::Receiver<Delivery>,
    /// `buf` 第一个字节在文件里的位置
    at: u64,
    buf: Bytes,
    /// 额度给到了哪里
    granted: u64,
    _permit: OwnedSemaphorePermit,
}

impl RemoteFile {
    pub fn size(&self) -> u64 {
        self.size
    }

    /// 文件名（路径的最后一段）
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    /// 下一次从 `pos` 开始读（跳着读：之后的读换一条流从那里取）
    pub fn seek(&mut self, pos: u64) {
        self.pos = pos.min(self.size);
    }

    /// 读到 `out` 里，返回读了多少（0：到末尾了）。要等对方的数据
    pub async fn read(&mut self, out: &mut [u8]) -> Result<usize, FileError> {
        if out.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        loop {
            if self.active.as_ref().is_some_and(|a| a.at != self.pos) {
                self.cancel();
            }
            if self.active.is_none() {
                self.start().await?;
            }
            let a = self.active.as_mut().expect("started above");
            if !a.buf.is_empty() {
                let n = out.len().min(a.buf.len());
                out[..n].copy_from_slice(&a.buf[..n]);
                let _ = a.buf.split_to(n);
                a.at += n as u64;
                self.pos += n as u64;
                self.retries = 0;
                if self.pos >= self.size {
                    // 读完了：让出同时取的名额（本机程序可能还开着这个文件）
                    let done = self.active.take().expect("reading from it");
                    self.inner.remote.lock().unwrap().slots.remove(&done.fetch);
                    return Ok(n);
                }
                let a = self.active.as_mut().expect("reading from it");
                // 读走了一半的额度：再给一个窗口
                if a.granted < self.size && a.granted - a.at < WINDOW / 2 {
                    a.granted = (a.at + WINDOW).min(self.size);
                    self.inner.send(FileMsg::Credit {
                        fetch: a.fetch,
                        until: a.granted,
                    });
                }
                return Ok(n);
            }
            let error = match a.rx.recv().await {
                Some(Delivery::Data(b)) => {
                    a.buf = b;
                    continue;
                }
                // 流结束了却还没到末尾（不会：收的一方核对过长度），或者取用被收掉了
                Some(Delivery::End) | None => FileError::Closed,
                Some(Delivery::Error(e)) => e,
            };
            self.cancel();
            // 连接出了问题（直连断了）、对方忙：从这里再取
            let retry = matches!(error, FileError::Closed | FileError::Busy)
                && self.retries < RETRIES
                && !self.inner.is_closed();
            if !retry {
                debug!(
                    name = self.name(),
                    ?error,
                    "reading a clipboard file failed"
                );
                self.inner.notice(self.name(), error);
                return Err(error);
            }
            self.retries += 1;
            tokio::time::sleep(Duration::from_millis(300 * u64::from(self.retries))).await;
        }
    }

    /// 同 [`RemoteFile::read`]，阻塞的（平台的回调线程上用；不能在异步任务里调用）
    pub fn read_blocking(&mut self, out: &mut [u8]) -> Result<usize, FileError> {
        let rt = self.inner.rt.clone();
        rt.block_on(self.read(out))
    }

    /// 从 `pos` 开始取（同时取的满了就等）
    async fn start(&mut self) -> Result<(), FileError> {
        let permits = self.inner.remote.lock().unwrap().permits.clone();
        let permit = permits
            .acquire_owned()
            .await
            .map_err(|_| FileError::Closed)?;
        if self.inner.is_closed() {
            return Err(FileError::Closed);
        }
        let (tx, rx) = mpsc::channel((WINDOW as usize).div_ceil(airhand_proto::files::CHUNK) + 4);
        let until = (self.pos + WINDOW).min(self.size);
        let fetch = {
            let mut r = self.inner.remote.lock().unwrap();
            r.next_fetch = r.next_fetch.wrapping_add(1).max(1);
            let fetch = r.next_fetch;
            r.slots.insert(
                fetch,
                Slot {
                    opener: Some(self.inner.keys.opener(fetch)),
                    tx,
                    expected: self.size - self.pos,
                },
            );
            fetch
        };
        self.inner.send(FileMsg::Fetch {
            fetch,
            clip: self.clip,
            path: self.path.clone(),
            offset: self.pos,
            size: self.size,
            mtime: self.mtime,
            until,
        });
        self.active = Some(Active {
            fetch,
            rx,
            at: self.pos,
            buf: Bytes::new(),
            granted: until,
            _permit: permit,
        });
        Ok(())
    }

    /// 不要正在取的了
    fn cancel(&mut self) {
        let Some(a) = self.active.take() else { return };
        let had = self.inner.remote.lock().unwrap().slots.remove(&a.fetch);
        // 流已经到了（登记没了）、还在路上：都告诉给的一方（流到了的那一边收的任务看到接收端没了也会叫停）
        if had.is_some() || a.at < self.size {
            self.inner.send(FileMsg::Cancel { fetch: a.fetch });
        }
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// 收对方开过来的文件流（中转、直连上的都在 `files` 里），一条一个任务
pub(super) async fn accept(inner: Arc<Inner>, mut files: LaneStreams) {
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            s = files.recv() => match s {
                Some(s) => {
                    tasks.spawn(receive(inner.clone(), s.recv));
                }
                None => break,
            },
            Some(_) = tasks.join_next() => {}
        }
    }
}

fn stop(mut recv: RecvStream, code: u32) {
    let _ = recv.stop(VarInt::from_u32(code));
}

/// 一条文件流：开头的取用编号找到它是哪次取用，一帧一帧解密、交给读的一方，核对长度
async fn receive(inner: Arc<Inner>, mut recv: RecvStream) {
    let mut id = [0u8; 4];
    if tokio::time::timeout(STREAM_TIMEOUT, recv.read_exact(&mut id))
        .await
        .map_or(true, |r| r.is_err())
    {
        return stop(recv, reset::CANCELLED);
    }
    let fetch = u32::from_le_bytes(id);
    let slot = {
        let mut r = inner.remote.lock().unwrap();
        r.slots.get_mut(&fetch).and_then(|s| {
            let opener = s.opener.take()?;
            Some((opener, s.tx.clone(), s.expected))
        })
    };
    // 不要了的（取消了、重复的流）
    let Some((mut opener, tx, expected)) = slot else {
        return stop(recv, reset::CANCELLED);
    };
    let max = SessionLane::File.max_frame_size();
    let mut got = 0u64;
    let outcome = loop {
        match read_packet(&mut recv, max).await {
            Ok(Some(mut frame)) => {
                if opener.open(&mut frame).is_err() {
                    debug!(fetch, "a clipboard file frame failed authentication");
                    break Delivery::Error(FileError::Io);
                }
                got += frame.len() as u64;
                if got > expected {
                    break Delivery::Error(FileError::Io);
                }
                if tx.send(Delivery::Data(Bytes::from(frame))).await.is_err() {
                    // 读的一方不要了
                    return stop(recv, reset::CANCELLED);
                }
            }
            Ok(None) if got == expected => break Delivery::End,
            // 截短了
            Ok(None) => break Delivery::Error(FileError::Io),
            Err(WireError::Io(e)) => break Delivery::Error(stream_error(e)),
            Err(_) => break Delivery::Error(FileError::Io),
        }
    };
    if !matches!(outcome, Delivery::End) {
        stop(recv, reset::CANCELLED);
    }
    inner.remote.lock().unwrap().slots.remove(&fetch);
    let _ = tx.send(outcome).await;
}

/// 读文件流出错了：中转说太大了是 [`FileError::TooLarge`]；给的一方读不下去了（原因随后在 `Failed` 里）是
/// [`FileError::Io`]；别的（连接断了、直连断了）是 [`FileError::Closed`]：可以从断的地方再取
fn stream_error(e: std::io::Error) -> FileError {
    let code = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ReadError>())
        .and_then(|r| match r {
            ReadError::Reset(c) => Some(c.into_inner()),
            _ => None,
        });
    match code {
        Some(c) if c == u64::from(reset::RELAY_LIMIT) => FileError::TooLarge,
        Some(c) if c == u64::from(reset::SOURCE) => FileError::Io,
        _ => FileError::Closed,
    }
}
