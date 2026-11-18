//! 给的一方（见 [`super`]）：公告本机剪贴板里的文件，回答列表，按取的一方给的额度把文件写进文件通道。
//!
//! 一次取用一个任务：打开文件（核对大小、修改时间），挑一条路（直连通着就走直连；只能走中转时，第一次要经中转发
//! 这个公告里的东西之前先算整个公告有多大，超过服务器的上限就回答 [`FileError::TooLarge`]），开一条文件通道上的帧流，
//! 一帧一帧加密写进去。取的一方取消了、读不下去了，就 reset 这条流（[`reset::CANCELLED`] / [`reset::SOURCE`]）。

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use airhand_proto::SessionLane;
use airhand_proto::files::{
    CHUNK, FileEntry, FileError, FileKind, FileMsg, MAX_OFFER, MAX_SERVING, MAX_TREE, PAGE, reset,
    valid_path,
};
use airhand_proto::framing::PREFIX;
use airhand_proto::handshake::SEAL_OVERHEAD;
use bytes::Bytes;
use quinn::{SendStream, VarInt, WriteError};
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tracing::debug;

use super::{Inner, Provider};
use crate::lanes::open_lane;

/// 最多留着这么多个本端的公告（最新的和之前的：对方可能还在从之前的那个复制）
const KEEP_OFFERS: usize = 2;
/// 一个公告最多记着这么多棵列过的树（分页用）
const KEEP_TREES: usize = 8;
/// 一页列表最多这么多字节（估计的，远小于大块通道的上限）
const PAGE_BYTES: usize = 1 << 20;
/// 写一帧最多等这么久（额度之内还写不进去，就是连接出问题了）
const WRITE_TIMEOUT: Duration = Duration::from_secs(60);

/// 本端给的东西
#[derive(Default)]
pub(super) struct Local {
    next_clip: u32,
    offers: VecDeque<Arc<Offer>>,
    /// 最近一次取用的编号（必须递增，见 `airhand_proto::files`）
    last_fetch: Option<u32>,
    serving: HashMap<u32, Serving>,
}

/// 本端的一个公告
pub(super) struct Offer {
    clip: u32,
    provider: Arc<dyn Provider>,
    items: Vec<FileEntry>,
    /// 整个公告不超过中转的上限（第一次要经中转发时才算，见模块说明）
    relay_ok: OnceLock<bool>,
    /// 列过的树（分页时接着用）
    trees: Mutex<VecDeque<Tree>>,
}

/// 列过的一棵树：路径、是不是整棵树、列表
type Tree = (Vec<String>, bool, Arc<Vec<FileEntry>>);

/// 正在给的一次取用
struct Serving {
    credit: watch::Sender<u64>,
    cancel: watch::Sender<bool>,
    task: AbortHandle,
}

/// 取的一方要取什么（[`FileMsg::Fetch`]）
pub(super) struct Request {
    pub fetch: u32,
    pub clip: u32,
    pub path: Vec<String>,
    pub offset: u64,
    pub size: u64,
    pub mtime: Option<u64>,
    pub until: u64,
}

pub(super) fn offer(inner: &Arc<Inner>, provider: Arc<dyn Provider>) -> Option<u32> {
    let mut items = provider.items();
    items.truncate(MAX_OFFER);
    // 公告是一条大块通道上的消息：太长的（成千上万个很长的名字）去掉后面的
    let budget = airhand_proto::MAX_BULK_MSG / 2;
    while !items.is_empty() && encoded_len(&items) > budget {
        items.truncate(items.len() * 3 / 4);
    }
    if items.is_empty() {
        return None;
    }
    let clip = {
        let mut l = inner.local.lock().unwrap();
        l.next_clip = l.next_clip.wrapping_add(1).max(1);
        let clip = l.next_clip;
        l.offers.push_back(Arc::new(Offer {
            clip,
            provider,
            items: items.clone(),
            relay_ok: OnceLock::new(),
            trees: Mutex::default(),
        }));
        while l.offers.len() > KEEP_OFFERS {
            l.offers.pop_front();
        }
        clip
    };
    debug!(clip, items = items.len(), "offering clipboard files");
    inner.pending.send_replace(Some(clip));
    inner.send(FileMsg::Offer { clip, items });
    Some(clip)
}

fn encoded_len(items: &[FileEntry]) -> usize {
    items
        .iter()
        .map(|i| 24 + i.path.iter().map(String::len).sum::<usize>())
        .sum()
}

pub(super) fn withdraw(inner: &Arc<Inner>) {
    inner.pending.send_replace(None);
}

/// 公告 `clip` 里的东西（最新的和之前的）；路径不合规矩、第一段不是公告里的一项的不行
fn find(inner: &Inner, clip: u32, path: &[String]) -> Result<Arc<Offer>, FileError> {
    if !valid_path(path) {
        return Err(FileError::Invalid);
    }
    let l = inner.local.lock().unwrap();
    let offer = l
        .offers
        .iter()
        .find(|o| o.clip == clip)
        .ok_or(FileError::Expired)?;
    if !offer.items.iter().any(|i| i.path.first() == path.first()) {
        return Err(FileError::NotFound);
    }
    Ok(offer.clone())
}

pub(super) fn list(
    inner: &Arc<Inner>,
    req: u32,
    clip: u32,
    path: Vec<String>,
    recursive: bool,
    cursor: u32,
) {
    let offer = match find(inner, clip, &path) {
        Ok(o) => o,
        Err(e) => return inner.send(listing_error(req, e)),
    };
    let inner = inner.clone();
    inner.rt.clone().spawn(async move {
        let tree = {
            let offer = offer.clone();
            tokio::task::spawn_blocking(move || offer.tree(&path, recursive))
                .await
                .unwrap_or(Err(FileError::Io))
        };
        let msg = match tree {
            Ok(all) => page(req, &all, cursor as usize),
            Err(e) => listing_error(req, e),
        };
        inner.send(msg);
    });
}

fn listing_error(req: u32, error: FileError) -> FileMsg {
    FileMsg::Listing {
        req,
        entries: Vec::new(),
        next: None,
        error: Some(error),
    }
}

/// 从第 `cursor` 项开始的一页（[`PAGE`] 项、大约 [`PAGE_BYTES`] 字节以内）
fn page(req: u32, all: &[FileEntry], cursor: usize) -> FileMsg {
    let rest = all.get(cursor..).unwrap_or_default();
    let mut bytes = 0;
    let n = rest
        .iter()
        .take(PAGE)
        .take_while(|e| {
            bytes += 32
                + e.path.iter().map(|p| p.len() + 2).sum::<usize>()
                + e.target.as_ref().map_or(0, String::len);
            bytes <= PAGE_BYTES
        })
        .count()
        .max(rest.len().min(1));
    let end = cursor + n;
    FileMsg::Listing {
        req,
        entries: rest[..n].to_vec(),
        next: (end < all.len()).then_some(end as u32),
        error: None,
    }
}

impl Offer {
    /// 目录 `path` 下面的一层，或者整棵树（路径相对 `path`），列过的接着用。阻塞的
    fn tree(&self, path: &[String], recursive: bool) -> Result<Arc<Vec<FileEntry>>, FileError> {
        let cached = self
            .trees
            .lock()
            .unwrap()
            .iter()
            .find(|(p, r, _)| p == path && *r == recursive)
            .map(|(_, _, t)| t.clone());
        if let Some(t) = cached {
            return Ok(t);
        }
        let all = if recursive {
            walk(&*self.provider, path, None)?.0
        } else {
            self.provider.read_dir(path)?
        };
        let all = Arc::new(all);
        let mut trees = self.trees.lock().unwrap();
        trees.push_back((path.to_vec(), recursive, all.clone()));
        while trees.len() > KEEP_TREES {
            trees.pop_front();
        }
        Ok(all)
    }

    /// 整个公告不超过 `limit` 字节（超过了就停下，不用走完）。阻塞的
    fn within(&self, limit: u64) -> bool {
        *self.relay_ok.get_or_init(|| {
            let mut total = 0u64;
            for item in &self.items {
                total += item.size;
                if total > limit {
                    return false;
                }
                if item.kind == FileKind::Dir {
                    match walk(&*self.provider, &item.path, Some(limit - total)) {
                        Ok((_, size)) => total += size,
                        Err(_) => return false,
                    }
                    if total > limit {
                        return false;
                    }
                }
            }
            true
        })
    }
}

/// 整棵树（路径相对 `root`，先浅后深，每个目录里按 [`Provider::read_dir`] 的先后）和其中文件的总大小。
/// 链接不跟随。超过 [`MAX_TREE`] 项是 [`FileError::TooMany`]；给了 `budget` 时总大小超过它就停下（返回到那时为止的）
fn walk(
    provider: &dyn Provider,
    root: &[String],
    budget: Option<u64>,
) -> Result<(Vec<FileEntry>, u64), FileError> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut dirs: VecDeque<Vec<String>> = VecDeque::from([Vec::new()]);
    while let Some(rel) = dirs.pop_front() {
        let mut full = root.to_vec();
        full.extend(rel.iter().cloned());
        for e in provider.read_dir(&full)? {
            let mut path = rel.clone();
            path.extend(e.path);
            if e.kind == FileKind::Dir {
                if path.len() + root.len() < airhand_proto::files::MAX_DEPTH {
                    dirs.push_back(path.clone());
                } else {
                    continue;
                }
            }
            total += e.size;
            out.push(FileEntry { path, ..e });
            if out.len() > MAX_TREE {
                return Err(FileError::TooMany);
            }
            if budget.is_some_and(|b| total > b) {
                return Ok((out, total));
            }
        }
    }
    Ok((out, total))
}

pub(super) fn fetch(inner: &Arc<Inner>, req: Request) {
    let fetch = req.fetch;
    let started = (|| {
        {
            let mut l = inner.local.lock().unwrap();
            // 同一把密钥不能加密两份内容：编号必须递增
            if l.last_fetch.is_some_and(|last| fetch <= last) {
                return Err(FileError::Invalid);
            }
            l.last_fetch = Some(fetch);
            if l.serving.len() >= MAX_SERVING {
                return Err(FileError::Busy);
            }
        }
        let offer = find(inner, req.clip, &req.path)?;
        if req.offset > req.size {
            return Err(FileError::Invalid);
        }
        let (credit, credit_rx) = watch::channel(req.until);
        let (cancel, cancel_rx) = watch::channel(false);
        // 拿着锁开任务、登记：任务结束时才删得掉自己（很快就结束的任务不会留下一个删不掉的登记）
        let mut l = inner.local.lock().unwrap();
        let task = inner
            .rt
            .spawn(serve_one(inner.clone(), offer, req, credit_rx, cancel_rx));
        l.serving.insert(
            fetch,
            Serving {
                credit,
                cancel,
                task: task.abort_handle(),
            },
        );
        Ok(())
    })();
    if let Err(error) = started {
        debug!(fetch, ?error, "refusing a file fetch");
        inner.send(FileMsg::Failed { fetch, error });
    }
}

/// 正在给的有几个（测试用）
#[cfg(test)]
pub(super) fn serving(inner: &Inner) -> usize {
    inner.local.lock().unwrap().serving.len()
}

pub(super) fn credit(inner: &Inner, fetch: u32, until: u64) {
    if let Some(s) = inner.local.lock().unwrap().serving.get(&fetch) {
        s.credit.send_if_modified(|u| {
            let more = until > *u;
            if more {
                *u = until;
            }
            more
        });
    }
}

pub(super) fn cancel(inner: &Inner, fetch: u32) {
    if let Some(s) = inner.local.lock().unwrap().serving.get(&fetch) {
        s.cancel.send_replace(true);
    }
}

pub(super) fn close(inner: &Inner) {
    let mut l = inner.local.lock().unwrap();
    for (_, s) in l.serving.drain() {
        s.cancel.send_replace(true);
        s.task.abort();
    }
    l.offers.clear();
}

/// 一次取用为什么停下
#[derive(Debug)]
enum Stop {
    /// 取的一方取消了
    Cancelled,
    /// 给不了（原因告诉取的一方）
    Failed(FileError),
    /// 对方（或者中转）叫停了这条流
    Stopped(VarInt),
    /// 连接出了问题
    Broken,
}

impl From<FileError> for Stop {
    fn from(e: FileError) -> Self {
        Stop::Failed(e)
    }
}

/// 一次取用（见模块说明）
async fn serve_one(
    inner: Arc<Inner>,
    offer: Arc<Offer>,
    req: Request,
    mut credit: watch::Receiver<u64>,
    mut cancel: watch::Receiver<bool>,
) {
    let fetch = req.fetch;
    let name = req.path.last().cloned().unwrap_or_default();
    let mut stream: Option<SendStream> = None;
    let r = tokio::select! {
        r = send_file(&inner, &offer, &req, &mut credit, &mut stream) => r,
        _ = cancel.wait_for(|c| *c) => Err(Stop::Cancelled),
    };
    let code = |c: u32| VarInt::from_u32(c);
    match r {
        Ok(()) => debug!(fetch, "a clipboard file was sent"),
        Err(Stop::Cancelled) => {
            if let Some(s) = &mut stream {
                let _ = s.reset(code(reset::CANCELLED));
            }
        }
        Err(Stop::Failed(error)) => {
            debug!(fetch, ?error, "can't send a clipboard file");
            // 原因先发（控制通道优先级高，多半比 reset 先到），取的一方就知道为什么
            inner.send(FileMsg::Failed { fetch, error });
            if let Some(s) = &mut stream {
                let _ = s.reset(code(reset::SOURCE));
            }
            inner.notice(&name, error);
        }
        // 中转说太大了（给的一方算过整个公告，一般到不了这里）
        Err(Stop::Stopped(c)) if c == code(reset::RELAY_LIMIT) => {
            inner.send(FileMsg::Failed {
                fetch,
                error: FileError::TooLarge,
            });
            inner.notice(&name, FileError::TooLarge);
        }
        Err(Stop::Stopped(_) | Stop::Broken) => {
            if let Some(s) = &mut stream {
                let _ = s.reset(code(0));
            }
        }
    }
    inner.local.lock().unwrap().serving.remove(&fetch);
}

async fn send_file(
    inner: &Arc<Inner>,
    offer: &Arc<Offer>,
    req: &Request,
    credit: &mut watch::Receiver<u64>,
    stream: &mut Option<SendStream>,
) -> Result<(), Stop> {
    let (mut reader, now) = {
        let provider = offer.provider.clone();
        let (path, offset) = (req.path.clone(), req.offset);
        tokio::task::spawn_blocking(move || provider.open(&path, offset))
            .await
            .map_err(|_| Stop::Failed(FileError::Io))??
    };
    if now.kind != FileKind::File {
        return Err(FileError::Invalid.into());
    }
    let mtime_changed = matches!((now.mtime, req.mtime), (Some(a), Some(b)) if a != b);
    if now.size != req.size || mtime_changed {
        return Err(FileError::Changed.into());
    }
    // 直连通着就走直连；只能走中转时整个公告不能超过服务器的上限
    let conn = match inner.paths.direct() {
        Some(direct) => direct,
        None => {
            if let Some(limit) = inner.limit {
                let offer = offer.clone();
                let ok = tokio::task::spawn_blocking(move || offer.within(limit))
                    .await
                    .unwrap_or(false);
                if !ok {
                    return Err(FileError::TooLarge.into());
                }
            }
            inner.paths.relay().clone()
        }
    };
    let s = stream.insert(
        open_lane(&conn, inner.session_id, SessionLane::File)
            .await
            .map_err(|_| Stop::Broken)?,
    );
    write(s, Bytes::copy_from_slice(&req.fetch.to_le_bytes())).await?;
    let mut sealer = inner.keys.sealer(req.fetch);
    let mut pos = req.offset;
    while pos < req.size {
        // 取的一方给了额度才发（见 airhand_proto::files 的"额度"）
        let until = *credit
            .wait_for(|u| *u > pos)
            .await
            .map_err(|_| Stop::Broken)?;
        let n = (until - pos).min(CHUNK as u64).min(req.size - pos) as usize;
        let (r, frame) = tokio::task::spawn_blocking(move || {
            let mut frame = Vec::with_capacity(PREFIX + n + SEAL_OVERHEAD);
            frame.resize(PREFIX + n, 0);
            let r = reader.read_exact(&mut frame[PREFIX..]);
            (r.map(|()| reader), frame)
        })
        .await
        .map_err(|_| Stop::Failed(FileError::Io))?;
        reader = r.map_err(|e| {
            Stop::Failed(match e.kind() {
                // 比复制时短了
                std::io::ErrorKind::UnexpectedEof => FileError::Changed,
                std::io::ErrorKind::PermissionDenied => FileError::Denied,
                _ => FileError::Io,
            })
        })?;
        let mut frame = frame;
        sealer
            .seal_frame(&mut frame)
            .map_err(|_| Stop::Failed(FileError::Io))?;
        write(s, Bytes::from(frame)).await?;
        pos += n as u64;
    }
    s.finish().map_err(|_| Stop::Broken)?;
    Ok(())
}

async fn write(s: &mut SendStream, b: Bytes) -> Result<(), Stop> {
    match tokio::time::timeout(WRITE_TIMEOUT, s.write_chunk(b)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(WriteError::Stopped(c))) => Err(Stop::Stopped(c)),
        _ => Err(Stop::Broken),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &[&str], kind: FileKind, size: u64) -> FileEntry {
        FileEntry {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            size,
            mtime: None,
            target: None,
        }
    }

    #[test]
    fn pages_fit_and_continue() {
        let all: Vec<_> = (0..2500)
            .map(|i| entry(&["d", &format!("f{i}")], FileKind::File, 1))
            .collect();
        let FileMsg::Listing { entries, next, .. } = page(1, &all, 0) else {
            panic!()
        };
        assert_eq!((entries.len(), next), (PAGE, Some(PAGE as u32)));
        let FileMsg::Listing { entries, next, .. } = page(1, &all, 2000) else {
            panic!()
        };
        assert_eq!((entries.len(), next), (500, None));
        // 名字很长的：按字节分页，至少一项
        let long: Vec<_> = (0..10)
            .map(|i| entry(&[&"x".repeat(600_000), &i.to_string()], FileKind::File, 1))
            .collect();
        let FileMsg::Listing { entries, next, .. } = page(1, &long, 0) else {
            panic!()
        };
        assert_eq!((entries.len(), next), (1, Some(1)));
        let FileMsg::Listing { entries, next, .. } = page(1, &[], 0) else {
            panic!()
        };
        assert!(entries.is_empty() && next.is_none());
    }
}
