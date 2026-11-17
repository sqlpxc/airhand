//! 剪贴板里的文件（见 `airhand_link::files`、[`crate::clipfiles`]）在 Windows 服务里：被控端是 SYSTEM，交给托盘做。
//!
//! * 控制端复制的文件要放进剪贴板，成为资源管理器（登录的用户）粘贴得了的虚拟文件：SYSTEM 进程里的数据对象
//!   资源管理器调不了，所以由托盘放（[`place`]），托盘粘贴时经管道向被控端列目录、读文件（被控端再向控制端取）；
//! * 本机复制的文件要以登录的用户的身份读（不然控制端能借 SYSTEM 读到这个用户读不了的文件）：被控端公告给控制端的
//!   是 [`TrayFs`]，控制端来取时经管道请托盘读（[`provider`]）。
//!
//! 管道 `\\.\pipe\airhand-clip-<会话>`，安全设置和 [`super::ipc`] 的一样（只有交互登录的用户能连、不能冒充被控端，
//! 托盘核对管道的所有者）。被控端是服务端，同一时刻只认最近连上的那个托盘。每条消息是
//! `u32 JSON 长度 | u32 数据长度 | JSON | 数据`（数据是文件的内容）。请求带编号（`req`），回答带同一个编号。
//! 没有托盘（没人登录、托盘退出了）时：控制端复制的文件不放，本机复制的文件不公告。

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use airhand_link::files::{LocalFs, Provider, RemoteFile, RemoteOffer};
use airhand_proto::files::{FileEntry, FileError};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::clipfiles::{self, Source, SourceFile};

/// 一条消息的 JSON、数据最多这么长
const MAX_JSON: usize = 16 << 20;
const MAX_DATA: usize = 2 << 20;
/// 一次最多读这么多
const READ_CHUNK: usize = 1 << 20;
/// 等列目录的回答最多这么久（对方的目录可能很大、网络可能很慢）
const LIST_WAIT: Duration = Duration::from_secs(120);
/// 等一块数据最多这么久
const READ_WAIT: Duration = Duration::from_secs(120);
/// 最多记着这么多个放进剪贴板的公告
const KEEP_OFFERS: usize = 4;

fn pipe_name(session: u32) -> String {
    format!(r"\\.\pipe\airhand-clip-{session}")
}

/// 管道上的消息（见模块说明）
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase", rename_all_fields = "camelCase")]
enum Msg {
    // ── 被控端 → 托盘 ──
    /// 控制端复制的文件：放进剪贴板（`key` 认它）
    Place {
        key: u64,
        items: Vec<FileEntry>,
    },
    /// 剪贴板里还是它的话拿掉
    Withdraw {
        key: u64,
    },
    /// 本机复制的这些路径：托盘建一个 `LocalFs`（编号 `id`），回答 `Items`
    Local {
        req: u64,
        id: u64,
        paths: Vec<PathBuf>,
    },
    LocalList {
        req: u64,
        id: u64,
        path: Vec<String>,
    },
    /// 打开一个文件（编号 `file`，从 `offset` 开始读），回答 `Opened`
    LocalOpen {
        req: u64,
        id: u64,
        file: u64,
        path: Vec<String>,
        offset: u64,
    },
    /// 从打开的文件接着读最多 `len` 字节，回答 `Data`
    LocalRead {
        req: u64,
        file: u64,
        len: u32,
    },
    LocalClose {
        file: u64,
    },
    LocalDrop {
        id: u64,
    },

    // ── 托盘 → 被控端 ──
    /// 放进剪贴板了（放不了也说）
    Ready {
        key: u64,
    },
    Items {
        req: u64,
        items: Vec<FileEntry>,
    },
    Opened {
        req: u64,
        entry: Option<FileEntry>,
        error: Option<FileError>,
    },
    RemoteList {
        req: u64,
        key: u64,
        path: Vec<String>,
        recursive: bool,
    },
    /// 从 `offset` 开始读对方的文件最多 `len` 字节（`file`：托盘选的编号，被控端开着它、接着读），回答 `Data`
    RemoteRead {
        req: u64,
        key: u64,
        file: u64,
        path: Vec<String>,
        size: u64,
        mtime: Option<u64>,
        offset: u64,
        len: u32,
    },
    RemoteClose {
        file: u64,
    },

    // ── 两个方向的回答 ──
    Listed {
        req: u64,
        entries: Vec<FileEntry>,
        error: Option<FileError>,
    },
    /// 数据在后面；`eof`：到末尾了
    Data {
        req: u64,
        error: Option<FileError>,
        eof: bool,
    },
}

impl Msg {
    /// 回答的是哪个请求
    fn reply_to(&self) -> Option<u64> {
        match self {
            Msg::Items { req, .. }
            | Msg::Opened { req, .. }
            | Msg::Listed { req, .. }
            | Msg::Data { req, .. } => Some(*req),
            _ => None,
        }
    }
}

struct Frame {
    msg: Msg,
    data: Vec<u8>,
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) -> Result<()> {
    let json = serde_json::to_vec(&f.msg)?;
    let mut head = [0u8; 8];
    head[..4].copy_from_slice(&(json.len() as u32).to_le_bytes());
    head[4..].copy_from_slice(&(f.data.len() as u32).to_le_bytes());
    w.write_all(&head).await?;
    w.write_all(&json).await?;
    w.write_all(&f.data).await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
    let mut head = [0u8; 8];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let json_len = u32::from_le_bytes(head[..4].try_into().unwrap()) as usize;
    let data_len = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
    if json_len > MAX_JSON || data_len > MAX_DATA {
        bail!("a clipboard pipe message is too large");
    }
    let mut json = vec![0u8; json_len];
    r.read_exact(&mut json).await?;
    let mut data = vec![0u8; data_len];
    r.read_exact(&mut data).await?;
    Ok(Some(Frame {
        msg: serde_json::from_slice(&json).context("bad clipboard pipe message")?,
        data,
    }))
}

/// 一条管道连接：发消息，发请求、等回答（阻塞地等：在平台、`spawn_blocking` 的线程上）
struct Conn {
    tx: mpsc::UnboundedSender<Frame>,
    next: AtomicU64,
    replies: Mutex<HashMap<u64, std::sync::mpsc::Sender<Frame>>>,
}

impl Conn {
    fn new(tx: mpsc::UnboundedSender<Frame>) -> Arc<Self> {
        Arc::new(Self {
            tx,
            next: AtomicU64::new(1),
            replies: Mutex::default(),
        })
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, msg: Msg, data: Vec<u8>) -> bool {
        self.tx.send(Frame { msg, data }).is_ok()
    }

    /// 发请求 `make(编号)`，等回答（最多 `wait`）。连接断了、等不到是 `None`
    fn call(&self, make: impl FnOnce(u64) -> Msg, wait: Duration) -> Option<Frame> {
        let req = self.id();
        let (tx, rx) = std::sync::mpsc::channel();
        self.replies.lock().unwrap().insert(req, tx);
        if !self.send(make(req), Vec::new()) {
            self.replies.lock().unwrap().remove(&req);
            return None;
        }
        let got = rx.recv_timeout(wait).ok();
        self.replies.lock().unwrap().remove(&req);
        got
    }

    /// 一个回答到了：交给在等它的
    fn reply(&self, f: Frame) {
        let Some(req) = f.msg.reply_to() else { return };
        if let Some(tx) = self.replies.lock().unwrap().remove(&req) {
            let _ = tx.send(f);
        }
    }

    /// 连接断了：在等的都不用等了
    fn close(&self) {
        self.replies.lock().unwrap().clear();
    }
}

/// 读写一条管道：`handle` 处理收到的每一条（回答已经交给了在等的）
async fn pump<P, H>(pipe: P, conn: Arc<Conn>, mut rx: mpsc::UnboundedReceiver<Frame>, handle: H)
where
    P: AsyncRead + AsyncWrite + Unpin,
    H: Fn(Frame),
{
    let (mut r, mut w) = tokio::io::split(pipe);
    let writer = async {
        while let Some(f) = rx.recv().await {
            if let Err(e) = write_frame(&mut w, &f).await {
                debug!("clipboard pipe write failed: {e:#}");
                break;
            }
        }
    };
    let reader = async {
        loop {
            match read_frame(&mut r).await {
                Ok(Some(f)) if f.msg.reply_to().is_some() => conn.reply(f),
                Ok(Some(f)) => handle(f),
                Ok(None) => break,
                Err(e) => {
                    debug!("clipboard pipe read failed: {e:#}");
                    break;
                }
            }
        }
    };
    tokio::select! {
        () = writer => {}
        () = reader => {}
    }
    conn.close();
}

// ───────────────────────────── 被控端一侧 ─────────────────────────────

#[derive(Default)]
struct AgentSide {
    conn: Mutex<Option<Arc<Conn>>>,
    /// 放进剪贴板的公告（托盘按 key 来取）
    offers: Mutex<Vec<(u64, RemoteOffer)>>,
    /// 托盘在读的对方的文件
    files: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<RemoteFile>>>>,
}

fn agent() -> &'static Arc<AgentSide> {
    static A: OnceLock<Arc<AgentSide>> = OnceLock::new();
    A.get_or_init(Arc::default)
}

fn tray() -> Option<Arc<Conn>> {
    agent().conn.lock().unwrap().clone()
}

/// 控制端复制的文件：交给托盘放进剪贴板（见模块说明）。没有托盘时不放（告诉控制端放好了，别让它等）
pub fn place(offer: RemoteOffer) {
    let key = offer.key();
    let Some(conn) = tray() else {
        debug!("no tray to put the controller's files on the clipboard");
        return offer.ready();
    };
    let items = offer.items().to_vec();
    {
        let mut offers = agent().offers.lock().unwrap();
        offers.retain(|(k, _)| *k != key);
        offers.push((key, offer.clone()));
        while offers.len() > KEEP_OFFERS {
            offers.remove(0);
        }
    }
    if !conn.send(Msg::Place { key, items }, Vec::new()) {
        offer.ready();
    }
}

/// 剪贴板里还是公告 `key` 的话拿掉
pub fn withdraw(key: u64) {
    if let Some(conn) = tray() {
        conn.send(Msg::Withdraw { key }, Vec::new());
    }
    agent().offers.lock().unwrap().retain(|(k, _)| *k != key);
}

/// 本机复制的这些路径：交给托盘读（见模块说明）。没有托盘、一个都不在了时是 `None`（不公告）
pub async fn provider(paths: Vec<PathBuf>) -> Option<Arc<dyn Provider>> {
    let conn = tray()?;
    tokio::task::spawn_blocking(move || {
        let id = conn.id();
        let f = conn.call(|req| Msg::Local { req, id, paths }, LIST_WAIT)?;
        let Msg::Items { items, .. } = f.msg else {
            return None;
        };
        if items.is_empty() {
            conn.send(Msg::LocalDrop { id }, Vec::new());
            return None;
        }
        Some(Arc::new(TrayFs { conn, id, items }) as Arc<dyn Provider>)
    })
    .await
    .ok()
    .flatten()
}

/// 托盘读的本机文件（见模块说明）
struct TrayFs {
    conn: Arc<Conn>,
    id: u64,
    items: Vec<FileEntry>,
}

impl Provider for TrayFs {
    fn items(&self) -> Vec<FileEntry> {
        self.items.clone()
    }

    fn read_dir(&self, path: &[String]) -> std::result::Result<Vec<FileEntry>, FileError> {
        let id = self.id;
        let path = path.to_vec();
        let f = self
            .conn
            .call(|req| Msg::LocalList { req, id, path }, LIST_WAIT)
            .ok_or(FileError::Io)?;
        match f.msg {
            Msg::Listed { error: Some(e), .. } => Err(e),
            Msg::Listed { entries, .. } => Ok(entries),
            _ => Err(FileError::Io),
        }
    }

    fn open(
        &self,
        path: &[String],
        offset: u64,
    ) -> std::result::Result<(Box<dyn Read + Send>, FileEntry), FileError> {
        let (id, file) = (self.id, self.conn.id());
        let path = path.to_vec();
        let f = self
            .conn
            .call(
                |req| Msg::LocalOpen {
                    req,
                    id,
                    file,
                    path,
                    offset,
                },
                READ_WAIT,
            )
            .ok_or(FileError::Io)?;
        match f.msg {
            Msg::Opened {
                entry: Some(entry), ..
            } => Ok((
                Box::new(TrayReader {
                    conn: self.conn.clone(),
                    file,
                    eof: false,
                }),
                entry,
            )),
            Msg::Opened { error, .. } => Err(error.unwrap_or(FileError::Io)),
            _ => Err(FileError::Io),
        }
    }
}

impl Drop for TrayFs {
    fn drop(&mut self) {
        self.conn.send(Msg::LocalDrop { id: self.id }, Vec::new());
    }
}

/// 托盘打开的一个本机文件
struct TrayReader {
    conn: Arc<Conn>,
    file: u64,
    eof: bool,
}

impl Read for TrayReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.eof || buf.is_empty() {
            return Ok(0);
        }
        let (file, len) = (self.file, buf.len().min(READ_CHUNK) as u32);
        let f = self
            .conn
            .call(|req| Msg::LocalRead { req, file, len }, READ_WAIT)
            .ok_or_else(|| std::io::Error::other("the tray is gone"))?;
        match f.msg {
            Msg::Data {
                error: None, eof, ..
            } => {
                let n = f.data.len().min(buf.len());
                buf[..n].copy_from_slice(&f.data[..n]);
                self.eof = eof;
                Ok(n)
            }
            Msg::Data {
                error: Some(FileError::Denied),
                ..
            } => Err(std::io::ErrorKind::PermissionDenied.into()),
            _ => Err(std::io::Error::other("reading through the tray failed")),
        }
    }
}

impl Drop for TrayReader {
    fn drop(&mut self) {
        self.conn
            .send(Msg::LocalClose { file: self.file }, Vec::new());
    }
}

/// 被控端：在本会话的管道上等托盘（同一时刻只认最近连上的那个）
pub async fn serve() {
    let session = super::ipc::my_session();
    let name = pipe_name(session);
    let mut server = match super::ipc::create(&name, true) {
        Ok(s) => s,
        Err(e) => {
            warn!("clipboard files won't reach the tray: {e:#}");
            return;
        }
    };
    loop {
        if let Err(e) = server.connect().await {
            warn!("clipboard pipe connect failed: {e}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        }
        let next = loop {
            match super::ipc::create(&name, false) {
                Ok(s) => break s,
                Err(e) => {
                    warn!("{e:#}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        let client = std::mem::replace(&mut server, next);
        let mut client_session = u32::MAX;
        unsafe {
            use std::os::windows::io::AsRawHandle;
            let _ = windows::Win32::System::Pipes::GetNamedPipeClientSessionId(
                windows::Win32::Foundation::HANDLE(client.as_raw_handle()),
                &mut client_session,
            );
        }
        if client_session != session {
            warn!(
                client_session,
                "rejected a clipboard pipe from another session"
            );
            continue;
        }
        info!("the tray is handling clipboard files");
        let (tx, rx) = mpsc::unbounded_channel();
        let conn = Conn::new(tx);
        *agent().conn.lock().unwrap() = Some(conn.clone());
        tokio::spawn(async move {
            let c = conn.clone();
            pump(client, conn.clone(), rx, move |f| from_tray(&c, f)).await;
            let mut current = agent().conn.lock().unwrap();
            if current.as_ref().is_some_and(|x| Arc::ptr_eq(x, &conn)) {
                *current = None;
            }
            agent().files.lock().unwrap().clear();
            debug!("the tray stopped handling clipboard files");
        });
    }
}

/// 托盘发来的（回答之外的）
fn from_tray(conn: &Arc<Conn>, f: Frame) {
    let offer = |key: u64| {
        agent()
            .offers
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, o)| o.clone())
    };
    match f.msg {
        Msg::Ready { key } => {
            if let Some(o) = offer(key) {
                o.ready();
            }
        }
        Msg::RemoteList {
            req,
            key,
            path,
            recursive,
        } => {
            let conn = conn.clone();
            let offer = offer(key);
            tokio::spawn(async move {
                let r = match offer {
                    Some(o) => o.list(&path, recursive).await,
                    None => Err(FileError::Expired),
                };
                let (entries, error) = match r {
                    Ok(e) => (e, None),
                    Err(e) => (Vec::new(), Some(e)),
                };
                conn.send(
                    Msg::Listed {
                        req,
                        entries,
                        error,
                    },
                    Vec::new(),
                );
            });
        }
        Msg::RemoteRead {
            req,
            key,
            file,
            path,
            size,
            mtime,
            offset,
            len,
        } => {
            let conn = conn.clone();
            let existing = agent().files.lock().unwrap().get(&file).cloned();
            let opened = existing.or_else(|| {
                let f = Arc::new(tokio::sync::Mutex::new(offer(key)?.open(path, size, mtime)));
                agent().files.lock().unwrap().insert(file, f.clone());
                Some(f)
            });
            tokio::spawn(async move {
                let Some(f) = opened else {
                    conn.send(
                        Msg::Data {
                            req,
                            error: Some(FileError::Expired),
                            eof: true,
                        },
                        Vec::new(),
                    );
                    return;
                };
                let mut f = f.lock().await;
                f.seek(offset);
                let mut buf = vec![0u8; (len as usize).min(READ_CHUNK)];
                let mut got = 0;
                let mut error = None;
                while got < buf.len() {
                    match f.read(&mut buf[got..]).await {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
                buf.truncate(got);
                let eof = f.position() >= f.size();
                conn.send(Msg::Data { req, error, eof }, buf);
            });
        }
        Msg::RemoteClose { file } => {
            agent().files.lock().unwrap().remove(&file);
        }
        other => debug!(?other, "unexpected clipboard pipe message from the tray"),
    }
}

// ───────────────────────────── 托盘一侧 ─────────────────────────────

#[derive(Default)]
struct TraySide {
    /// 被控端要公告的本机文件
    locals: Mutex<HashMap<u64, Arc<LocalFs>>>,
    /// 打开着的本机文件
    readers: Mutex<HashMap<u64, SharedReader>>,
}

type SharedReader = Arc<Mutex<Box<dyn Read + Send>>>;

/// 托盘：一直连着本会话里服务的被控端（断了自动重连），替它放剪贴板里的文件、读本机的文件。一直跑到进程退出（丢掉返回的句柄不会停）
pub fn start_tray() -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async {
        let name = pipe_name(super::ipc::my_session());
        loop {
            if let Ok(pipe) = super::ipc::open_pipe(&name, true) {
                debug!("handling clipboard files for the host");
                let (tx, rx) = mpsc::unbounded_channel();
                let conn = Conn::new(tx);
                let side = Arc::new(TraySide::default());
                let c = conn.clone();
                pump(pipe, conn, rx, move |f| from_agent(&c, &side, f)).await;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

/// 被控端发来的（回答之外的）
fn from_agent(conn: &Arc<Conn>, side: &Arc<TraySide>, f: Frame) {
    match f.msg {
        Msg::Place { key, items } => clipfiles::apply_source(Arc::new(ProxySource {
            conn: conn.clone(),
            key,
            items,
        })),
        Msg::Withdraw { key } => clipfiles::withdraw_key(key),
        Msg::Local { req, id, paths } => {
            let (conn, side) = (conn.clone(), side.clone());
            tokio::task::spawn_blocking(move || {
                let fs = Arc::new(LocalFs::new(&paths));
                let items = fs.items();
                side.locals.lock().unwrap().insert(id, fs);
                conn.send(Msg::Items { req, items }, Vec::new());
            });
        }
        Msg::LocalList { req, id, path } => {
            let (conn, side) = (conn.clone(), side.clone());
            tokio::task::spawn_blocking(move || {
                let fs = side.locals.lock().unwrap().get(&id).cloned();
                let r = fs.map_or(Err(FileError::Expired), |fs| fs.read_dir(&path));
                let (entries, error) = match r {
                    Ok(e) => (e, None),
                    Err(e) => (Vec::new(), Some(e)),
                };
                conn.send(
                    Msg::Listed {
                        req,
                        entries,
                        error,
                    },
                    Vec::new(),
                );
            });
        }
        Msg::LocalOpen {
            req,
            id,
            file,
            path,
            offset,
        } => {
            let (conn, side) = (conn.clone(), side.clone());
            tokio::task::spawn_blocking(move || {
                let fs = side.locals.lock().unwrap().get(&id).cloned();
                let r = fs.map_or(Err(FileError::Expired), |fs| fs.open(&path, offset));
                let (entry, error) = match r {
                    Ok((reader, entry)) => {
                        side.readers
                            .lock()
                            .unwrap()
                            .insert(file, Arc::new(Mutex::new(reader)));
                        (Some(entry), None)
                    }
                    Err(e) => (None, Some(e)),
                };
                conn.send(Msg::Opened { req, entry, error }, Vec::new());
            });
        }
        Msg::LocalRead { req, file, len } => {
            let (conn, side) = (conn.clone(), side.clone());
            tokio::task::spawn_blocking(move || {
                let reader = side.readers.lock().unwrap().get(&file).cloned();
                let Some(reader) = reader else {
                    return conn.send(
                        Msg::Data {
                            req,
                            error: Some(FileError::Expired),
                            eof: true,
                        },
                        Vec::new(),
                    );
                };
                let mut r = reader.lock().unwrap();
                let mut buf = vec![0u8; (len as usize).min(READ_CHUNK)];
                let mut got = 0;
                let mut error = None;
                let mut eof = false;
                while got < buf.len() {
                    match r.read(&mut buf[got..]) {
                        Ok(0) => {
                            eof = true;
                            break;
                        }
                        Ok(n) => got += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            error = Some(if e.kind() == std::io::ErrorKind::PermissionDenied {
                                FileError::Denied
                            } else {
                                FileError::Io
                            });
                            break;
                        }
                    }
                }
                buf.truncate(got);
                conn.send(Msg::Data { req, error, eof }, buf)
            });
        }
        Msg::LocalClose { file } => {
            side.readers.lock().unwrap().remove(&file);
        }
        Msg::LocalDrop { id } => {
            side.locals.lock().unwrap().remove(&id);
        }
        other => debug!(?other, "unexpected clipboard pipe message from the host"),
    }
}

/// 被控端放进剪贴板的公告（托盘这边）：粘贴时经管道向被控端取
struct ProxySource {
    conn: Arc<Conn>,
    key: u64,
    items: Vec<FileEntry>,
}

impl Source for ProxySource {
    fn items(&self) -> Vec<FileEntry> {
        self.items.clone()
    }

    fn list(
        &self,
        path: &[String],
        recursive: bool,
    ) -> std::result::Result<Vec<FileEntry>, FileError> {
        let key = self.key;
        let path = path.to_vec();
        let f = self
            .conn
            .call(
                |req| Msg::RemoteList {
                    req,
                    key,
                    path,
                    recursive,
                },
                LIST_WAIT,
            )
            .ok_or(FileError::Closed)?;
        match f.msg {
            Msg::Listed { error: Some(e), .. } => Err(e),
            Msg::Listed { entries, .. } => Ok(entries),
            _ => Err(FileError::Closed),
        }
    }

    fn open(&self, path: Vec<String>, size: u64, mtime: Option<u64>) -> Box<dyn SourceFile> {
        Box::new(ProxyFile {
            conn: self.conn.clone(),
            key: self.key,
            file: self.conn.id(),
            path,
            size,
            mtime,
            pos: 0,
        })
    }

    fn ready(&self) {
        self.conn.send(Msg::Ready { key: self.key }, Vec::new());
    }

    fn key(&self) -> u64 {
        self.key
    }
}

/// 被控端那边的一个对方的文件（托盘这边）
struct ProxyFile {
    conn: Arc<Conn>,
    key: u64,
    file: u64,
    path: Vec<String>,
    size: u64,
    mtime: Option<u64>,
    pos: u64,
}

impl SourceFile for ProxyFile {
    fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, FileError> {
        if buf.is_empty() || self.pos >= self.size {
            return Ok(0);
        }
        let f = self
            .conn
            .call(
                |req| Msg::RemoteRead {
                    req,
                    key: self.key,
                    file: self.file,
                    path: self.path.clone(),
                    size: self.size,
                    mtime: self.mtime,
                    offset: self.pos,
                    len: buf.len().min(READ_CHUNK) as u32,
                },
                READ_WAIT,
            )
            .ok_or(FileError::Closed)?;
        match f.msg {
            Msg::Data { error: Some(e), .. } => Err(e),
            Msg::Data { .. } => {
                let n = f.data.len().min(buf.len());
                buf[..n].copy_from_slice(&f.data[..n]);
                self.pos += n as u64;
                Ok(n)
            }
            _ => Err(FileError::Closed),
        }
    }

    fn seek(&mut self, pos: u64) {
        self.pos = pos.min(self.size);
    }

    fn position(&self) -> u64 {
        self.pos
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn name(&self) -> String {
        self.path.last().cloned().unwrap_or_default()
    }
}

impl Drop for ProxyFile {
    fn drop(&mut self) {
        self.conn
            .send(Msg::RemoteClose { file: self.file }, Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 消息经管道的格式往返（数据原样）
    #[tokio::test]
    async fn frames_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        let f = Frame {
            msg: Msg::Data {
                req: 7,
                error: Some(FileError::Denied),
                eof: true,
            },
            data: vec![1, 2, 3],
        };
        write_frame(&mut a, &f).await.unwrap();
        let back = read_frame(&mut b).await.unwrap().unwrap();
        assert!(matches!(
            back.msg,
            Msg::Data {
                req: 7,
                error: Some(FileError::Denied),
                eof: true
            }
        ));
        assert_eq!(back.data, [1, 2, 3]);
        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    /// 托盘读本机的文件、被控端经它公告：经过管道（这里是内存里的一对）读出来的和文件一样
    #[tokio::test]
    async fn the_tray_reads_local_files_for_the_host() {
        let dir = std::env::temp_dir().join(format!("airhand-clip-pipe-{}", rand::random::<u32>()));
        std::fs::create_dir_all(dir.join("d")).unwrap();
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 253) as u8).collect();
        std::fs::write(dir.join("d").join("big"), &data).unwrap();
        let (host_end, tray_end) = tokio::io::duplex(1 << 20);
        let (htx, hrx) = mpsc::unbounded_channel();
        let host = Conn::new(htx);
        let (ttx, trx) = mpsc::unbounded_channel();
        let tray = Conn::new(ttx);
        let side = Arc::new(TraySide::default());
        let t = tray.clone();
        tokio::spawn(pump(tray_end, tray, trx, move |f| from_agent(&t, &side, f)));
        let h = host.clone();
        tokio::spawn(pump(host_end, h, hrx, |_| {}));
        let got = tokio::task::spawn_blocking(move || {
            let id = host.id();
            let f = host
                .call(
                    |req| Msg::Local {
                        req,
                        id,
                        paths: vec![dir.join("d")],
                    },
                    LIST_WAIT,
                )
                .unwrap();
            let Msg::Items { items, .. } = f.msg else {
                panic!()
            };
            let fs = TrayFs {
                conn: host,
                id,
                items,
            };
            assert_eq!(fs.items()[0].name(), "d");
            let list = fs.read_dir(&["d".to_string()]).unwrap();
            assert_eq!(list[0].name(), "big");
            let (mut r, e) = fs.open(&["d".into(), "big".into()], 10).unwrap();
            assert_eq!(e.size, 3_000_000);
            let mut out = Vec::new();
            r.read_to_end(&mut out).unwrap();
            let _ = std::fs::remove_dir_all(&dir);
            out
        })
        .await
        .unwrap();
        assert!(got == data[10..]);
    }
}
