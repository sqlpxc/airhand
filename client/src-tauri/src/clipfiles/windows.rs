//! Windows：对方复制的文件以"虚拟文件"放进本机剪贴板（见 [`super`]）。
//!
//! 一个 STA 线程拥有 OLE 剪贴板上的数据对象（[`DataObject`]），它提供：
//!
//! * `FileGroupDescriptorW`：整棵树（目录在前，名字按本机的规矩改过，见 [`super::local_names`]）。资源管理器粘贴时才要，
//!   这时才向对方列目录；
//! * `FileContents`（第几项）：一个 [`RemoteStream`]，资源管理器读多少才向对方取多少（`IStream::Read` 在这个线程上
//!   阻塞地等对方的数据）；进度条和取消都是资源管理器自己的；
//! * `Preferred DropEffect` 是复制（不是剪切）；还有一个本程序私有的格式，看得出剪贴板里的是自己放的（[`is_ours`]）。
//!
//! 不提供 `CF_HDROP`：本机的剪贴板同步也就不会把它当成本机复制了文件、又公告回去。
//! 退出时不 `OleFlushClipboard`（那会把所有格式都要一遍）：之后再粘贴就是取不到。

use std::collections::HashSet;
use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};

use airhand_proto::files::{FileError, FileKind};
use std::sync::Arc;
use windows::Win32::Foundation::{
    DV_E_FORMATETC, DV_E_LINDEX, E_FAIL, E_INVALIDARG, E_NOTIMPL, FILETIME, HGLOBAL, LPARAM,
    OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_INVALIDFUNCTION, WPARAM,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject,
    IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl,
    LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{
    GetClipboardSequenceNumber, IsClipboardFormatAvailable, RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    DROPEFFECT_COPY, OleInitialize, OleSetClipboard, ReleaseStgMedium,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES,
    FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
    SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW,
    TranslateMessage, WM_APP, WM_USER,
};
use windows::core::{BOOL, HRESULT, PWSTR, Ref, Result, implement, w};

use super::{Local, Source, SourceFile, local_names};

/// 文件描述里的名字最多这么多个字符（`cFileName` 是 `WCHAR[260]`，留一个给结尾的 0）
const MAX_PATH_CHARS: usize = 259;

/// 剪贴板格式
struct Formats {
    descriptor: u16,
    contents: u16,
    effect: u16,
    /// 本程序私有：剪贴板里的是自己放的
    marker: u16,
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| unsafe {
        Formats {
            descriptor: RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16,
            contents: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
            effect: RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16,
            marker: RegisterClipboardFormatW(w!("Airhand Remote Files")) as u16,
        }
    })
}

enum Cmd {
    Apply(Arc<dyn Source>),
    /// 按 [Source::key]
    Withdraw(u64),
}

/// 拥有剪贴板上的数据对象的 STA 线程
struct Sta {
    tx: Mutex<mpsc::Sender<Cmd>>,
    thread: u32,
}

fn sta() -> &'static Sta {
    static STA: OnceLock<Sta> = OnceLock::new();
    STA.get_or_init(|| {
        let (tx, rx) = mpsc::channel();
        let (id_tx, id_rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("airhand-clipfiles".into())
            .spawn(move || run(rx, id_tx));
        let thread = match spawned {
            Ok(_) => id_rx.recv().unwrap_or(0),
            Err(e) => {
                tracing::warn!("can't start the clipboard files thread: {e}");
                0
            }
        };
        Sta {
            tx: Mutex::new(tx),
            thread,
        }
    })
}

fn send(cmd: Cmd) {
    let s = sta();
    let sent = s.tx.lock().unwrap().send(cmd);
    if let Err(mpsc::SendError(Cmd::Apply(source))) = sent {
        source.ready();
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(s.thread, WM_APP, WPARAM(0), LPARAM(0));
    }
}

pub fn apply(source: Arc<dyn Source>) {
    send(Cmd::Apply(source));
}

pub fn withdraw(key: u64) {
    send(Cmd::Withdraw(key));
}

pub fn is_ours() -> bool {
    unsafe { IsClipboardFormatAvailable(formats().marker as u32).is_ok() }
}

/// 剪贴板上现在是哪个公告（放上去之后剪贴板的序号没变才还是它）
struct Current {
    key: u64,
    sequence: u32,
}

fn run(rx: mpsc::Receiver<Cmd>, id: mpsc::Sender<u32>) {
    unsafe {
        if let Err(e) = OleInitialize(None) {
            tracing::warn!("OleInitialize failed: {e}");
        }
        let mut msg = MSG::default();
        // 先建好消息队列，别的线程才能往这里 PostThreadMessage
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
        let _ = id.send(GetCurrentThreadId());
        let mut current: Option<Current> = None;
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_APP && msg.hwnd.is_invalid() {
                while let Ok(cmd) = rx.try_recv() {
                    handle(cmd, &mut current);
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn handle(cmd: Cmd, current: &mut Option<Current>) {
    match cmd {
        Cmd::Apply(source) => {
            let key = source.key();
            let obj: IDataObject = DataObject::new(source.clone()).into();
            // 别的程序正开着剪贴板：等一下再试
            let mut placed = false;
            for _ in 0..10 {
                if unsafe { OleSetClipboard(&obj) }.is_ok() {
                    placed = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            source.ready();
            if placed {
                tracing::debug!(key, "put the peer's files on the clipboard");
                *current = Some(Current {
                    key,
                    sequence: unsafe { GetClipboardSequenceNumber() },
                });
            } else {
                tracing::warn!("can't put the peer's files on the clipboard: it stayed busy");
            }
        }
        Cmd::Withdraw(key) => {
            let ours = current.as_ref().is_some_and(|c| {
                c.key == key && unsafe { GetClipboardSequenceNumber() } == c.sequence
            });
            if ours {
                unsafe {
                    let _ = OleSetClipboard(None);
                }
            }
            if current.as_ref().is_some_and(|c| c.key == key) {
                *current = None;
            }
        }
    }
}

/// 剪贴板上的数据对象（见模块说明）
#[implement(IDataObject)]
struct DataObject {
    source: Arc<dyn Source>,
    /// 整棵树：第一次要文件描述时才向对方列（之后的 `FileContents` 按序号找）
    tree: Mutex<Option<std::result::Result<Vec<Local>, FileError>>>,
}

impl DataObject {
    fn new(source: Arc<dyn Source>) -> Self {
        Self {
            source,
            tree: Mutex::new(None),
        }
    }

    /// 整棵树在本机的样子（阻塞：要等对方列目录）
    fn tree(&self) -> std::result::Result<Vec<Local>, FileError> {
        let mut t = self.tree.lock().unwrap();
        if let Some(r) = &*t {
            return r.clone();
        }
        let r = (|| {
            let mut out = Vec::new();
            let mut taken = HashSet::new();
            let mut skipped = 0;
            for item in self.source.items() {
                let tree = match item.kind {
                    FileKind::Dir => self.source.list(&item.path, true)?,
                    _ => Vec::new(),
                };
                skipped += local_names(&item, &tree, &mut taken, MAX_PATH_CHARS, &mut out);
            }
            if skipped > 0 {
                tracing::info!(
                    skipped,
                    "some of the peer's files can't be pasted here (links, paths too long)"
                );
            }
            Ok(out)
        })();
        *t = Some(r.clone());
        r
    }

    /// `FileGroupDescriptorW`
    fn descriptor(&self) -> Result<STGMEDIUM> {
        let tree = self.tree().map_err(|e| {
            tracing::debug!(?e, "can't list the peer's files");
            windows::core::Error::from(E_FAIL)
        })?;
        let header = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
        let size = header + tree.len().max(1) * size_of::<FILEDESCRIPTORW>();
        let flags = (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_WRITESTIME.0 | FD_PROGRESSUI.0) as u32
            | FD_UNICODE.0 as u32;
        let descriptors = tree.iter().map(|l| {
            // FILEDESCRIPTORW 是紧凑排列的（字段不对齐）：名字先放好，整个赋值
            let mut file_name = [0u16; 260];
            let name: Vec<u16> = l.local.join("\\").encode_utf16().collect();
            let n = name.len().min(file_name.len() - 1);
            file_name[..n].copy_from_slice(&name[..n]);
            FILEDESCRIPTORW {
                dwFlags: flags,
                dwFileAttributes: if l.kind == FileKind::Dir {
                    FILE_ATTRIBUTE_DIRECTORY.0
                } else {
                    FILE_ATTRIBUTE_NORMAL.0
                },
                nFileSizeHigh: (l.size >> 32) as u32,
                nFileSizeLow: l.size as u32,
                ftLastWriteTime: filetime(l.mtime),
                cFileName: file_name,
                ..Default::default()
            }
        });
        let mut bytes = vec![0u8; size];
        bytes[..4].copy_from_slice(&(tree.len() as u32).to_le_bytes());
        for (i, d) in descriptors.enumerate() {
            let at = header + i * size_of::<FILEDESCRIPTORW>();
            // SAFETY: FILEDESCRIPTORW 是普通的 C 结构体，按字节拷贝
            let raw = unsafe {
                std::slice::from_raw_parts(
                    (&d as *const FILEDESCRIPTORW).cast::<u8>(),
                    size_of::<FILEDESCRIPTORW>(),
                )
            };
            bytes[at..at + raw.len()].copy_from_slice(raw);
        }
        hglobal(&bytes)
    }

    /// `FileContents` 的第 `index` 项
    fn contents(&self, index: i32) -> Result<STGMEDIUM> {
        let tree = self
            .tree()
            .map_err(|_| windows::core::Error::from(E_FAIL))?;
        let l = usize::try_from(index)
            .ok()
            .and_then(|i| tree.get(i))
            .filter(|l| l.kind == FileKind::File)
            .ok_or(windows::core::Error::from(DV_E_LINDEX))?;
        let file = self.source.open(l.remote.clone(), l.size, l.mtime);
        let stream: IStream = RemoteStream {
            file: Mutex::new(file),
            mtime: l.mtime,
        }
        .into();
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn supports(f: &FORMATETC) -> bool {
        let cf = formats();
        let hglobal = f.tymed & TYMED_HGLOBAL.0 as u32 != 0;
        (f.cfFormat == cf.descriptor && hglobal)
            || (f.cfFormat == cf.contents && f.tymed & TYMED_ISTREAM.0 as u32 != 0)
            || (f.cfFormat == cf.effect && hglobal)
            || (f.cfFormat == cf.marker && hglobal)
    }
}

/// 一块全局内存（`STGMEDIUM`，`TYMED_HGLOBAL`），装着 `bytes`
fn hglobal(bytes: &[u8]) -> Result<STGMEDIUM> {
    unsafe {
        let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))?;
        let p = GlobalLock(h);
        if p.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

/// Unix 毫秒 → FILETIME（1601 年起的 100 纳秒）；不知道时是 0
fn filetime(mtime: Option<u64>) -> FILETIME {
    let t = mtime.map_or(0, |ms| (ms + 11_644_473_600_000) * 10_000);
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    }
}

fn formatetc(cf: u16, tymed: u32) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed,
    }
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, f: *const FORMATETC) -> Result<STGMEDIUM> {
        let f = unsafe { f.as_ref() }.ok_or(windows::core::Error::from(E_INVALIDARG))?;
        let cf = formats();
        if !DataObject::supports(f) {
            return Err(DV_E_FORMATETC.into());
        }
        if f.cfFormat == cf.descriptor {
            self.descriptor()
        } else if f.cfFormat == cf.contents {
            self.contents(f.lindex)
        } else if f.cfFormat == cf.effect {
            hglobal(&DROPEFFECT_COPY.0.to_le_bytes())
        } else {
            hglobal(&std::process::id().to_le_bytes())
        }
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        match unsafe { f.as_ref() } {
            Some(f) if DataObject::supports(f) => S_OK,
            _ => DV_E_FORMATETC,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { out.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        // DATA_S_SAMEFORMATETC
        HRESULT(0x0004_0130)
    }

    /// 资源管理器粘贴完会告诉数据对象做了什么（"Performed DropEffect"、"Paste Succeeded"）：收下就是了
    fn SetData(&self, _: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> Result<()> {
        if release.as_bool() && !medium.is_null() {
            unsafe { ReleaseStgMedium(medium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let cf = formats();
        let all = [
            formatetc(cf.descriptor, TYMED_HGLOBAL.0 as u32),
            formatetc(cf.contents, TYMED_ISTREAM.0 as u32),
            formatetc(cf.effect, TYMED_HGLOBAL.0 as u32),
            formatetc(cf.marker, TYMED_HGLOBAL.0 as u32),
        ];
        unsafe { SHCreateStdEnumFmtEtc(&all) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// 对方的一个文件（`FileContents`）：资源管理器读多少向对方取多少
#[implement(IStream)]
struct RemoteStream {
    file: Mutex<Box<dyn SourceFile>>,
    mtime: Option<u64>,
}

impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, read: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_INVALIDARG;
        }
        // SAFETY: 调用的一方给的缓冲区有 cb 个字节
        let buf = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
        let mut file = self.file.lock().unwrap();
        // 读满（或者读到末尾）：有的程序把读得比要的少当成到末尾了
        let mut got = 0;
        let result = loop {
            if got == buf.len() {
                break S_OK;
            }
            match file.read(&mut buf[got..]) {
                Ok(0) => break S_FALSE,
                Ok(n) => got += n,
                Err(e) => {
                    tracing::debug!(name = file.name(), ?e, "reading a pasted file failed");
                    break E_FAIL;
                }
            }
        };
        if let Some(read) = unsafe { read.as_mut() } {
            *read = got as u32;
        }
        result
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_INVALIDFUNCTION
    }
}

impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, distance: i64, origin: STREAM_SEEK, new: *mut u64) -> Result<()> {
        let mut file = self.file.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => file.position() as i64,
            STREAM_SEEK_END => file.size() as i64,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let pos = base
            .checked_add(distance)
            .filter(|p| *p >= 0)
            .ok_or(windows::core::Error::from(STG_E_INVALIDFUNCTION))?;
        file.seek(pos as u64);
        if let Some(new) = unsafe { new.as_mut() } {
            *new = file.position();
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn CopyTo(&self, _: Ref<IStream>, _: u64, _: *mut u64, _: *mut u64) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> Result<()> {
        Ok(())
    }

    fn Revert(&self) -> Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, out: *mut STATSTG, flag: &STATFLAG) -> Result<()> {
        let out = unsafe { out.as_mut() }.ok_or(windows::core::Error::from(E_INVALIDARG))?;
        let file = self.file.lock().unwrap();
        let name = if *flag == STATFLAG_NONAME {
            PWSTR::null()
        } else {
            let wide: Vec<u16> = file.name().encode_utf16().chain([0]).collect();
            unsafe {
                let p = CoTaskMemAlloc(wide.len() * 2).cast::<u16>();
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                }
                PWSTR(p)
            }
        };
        *out = STATSTG {
            pwcsName: name,
            r#type: STGTY_STREAM.0 as u32,
            cbSize: file.size(),
            mtime: filetime(self.mtime),
            ..Default::default()
        };
        Ok(())
    }

    fn Clone(&self) -> Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// 这些测试动的是本机真的剪贴板（会把它清掉），也会真的往磁盘上粘贴：手动运行
/// （`cargo test --lib clipfiles::windows -- --ignored --test-threads=1`）
#[cfg(test)]
mod tests {
    use super::*;
    use airhand_proto::files::FileEntry;
    use std::collections::HashMap;
    use windows::Win32::System::Ole::OleGetClipboard;

    /// 内存里的一组文件（不经过网络）
    struct Fake {
        items: Vec<FileEntry>,
        trees: HashMap<Vec<String>, Vec<FileEntry>>,
        files: HashMap<Vec<String>, Vec<u8>>,
        ready: Mutex<bool>,
        key: u64,
    }

    fn entry(path: &[&str], kind: FileKind, size: u64) -> FileEntry {
        FileEntry {
            path: path.iter().map(|s| s.to_string()).collect(),
            kind,
            size,
            mtime: Some(1_700_000_000_000),
            target: None,
        }
    }

    fn fake(key: u64) -> Arc<Fake> {
        let big: Vec<u8> = (0..1_500_000u32).map(|i| (i % 251) as u8).collect();
        let mut files = HashMap::new();
        files.insert(vec!["proj".into(), "a:b.txt".into()], b"colon".to_vec());
        files.insert(
            vec!["proj".into(), "sub".into(), "big.bin".into()],
            big.clone(),
        );
        files.insert(vec!["note.txt".into()], b"hello from afar".to_vec());
        let mut trees = HashMap::new();
        trees.insert(
            vec!["proj".into()],
            vec![
                entry(&["a:b.txt"], FileKind::File, 5),
                entry(&["lib64"], FileKind::Link, 0),
                entry(&["sub"], FileKind::Dir, 0),
                entry(&["sub", "big.bin"], FileKind::File, big.len() as u64),
                entry(&["sub", "empty"], FileKind::Dir, 0),
            ],
        );
        Arc::new(Fake {
            items: vec![
                entry(&["proj"], FileKind::Dir, 0),
                entry(&["note.txt"], FileKind::File, 15),
            ],
            trees,
            files,
            ready: Mutex::new(false),
            key,
        })
    }

    struct FakeFile {
        data: Vec<u8>,
        pos: u64,
        name: String,
    }

    impl SourceFile for FakeFile {
        fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, FileError> {
            let rest = &self.data[(self.pos as usize).min(self.data.len())..];
            // 一次最多给 7000 字节：读的一方要自己读满
            let n = rest.len().min(buf.len()).min(7000);
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
            self.name.clone()
        }
    }

    impl Source for Fake {
        fn items(&self) -> Vec<FileEntry> {
            self.items.clone()
        }
        fn list(
            &self,
            path: &[String],
            _recursive: bool,
        ) -> std::result::Result<Vec<FileEntry>, FileError> {
            self.trees.get(path).cloned().ok_or(FileError::NotFound)
        }
        fn open(&self, path: Vec<String>, _: u64, _: Option<u64>) -> Box<dyn SourceFile> {
            Box::new(FakeFile {
                data: self.files.get(&path).cloned().unwrap_or_default(),
                pos: 0,
                name: path.last().cloned().unwrap_or_default(),
            })
        }
        fn ready(&self) {
            *self.ready.lock().unwrap() = true;
        }
        fn key(&self) -> u64 {
            self.key
        }
    }

    fn wait_ready(f: &Fake) {
        for _ in 0..100 {
            if *f.ready.lock().unwrap() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("never ready");
    }

    /// 别的程序（这里是另一个 STA 线程）从剪贴板要文件描述、每个文件的内容：名字按本机的规矩改过、
    /// 链接跳过、目录在前；内容读得满
    #[test]
    #[ignore]
    fn the_clipboard_offers_virtual_files() {
        let f = fake(0xa1);
        apply(f.clone());
        wait_ready(&f);
        assert!(is_ours());
        unsafe {
            OleInitialize(None).unwrap();
            let obj = OleGetClipboard().unwrap();
            let cf = formats();
            let mut fmt = formatetc(cf.descriptor, TYMED_HGLOBAL.0 as u32);
            let medium = obj.GetData(&fmt).unwrap();
            let h = medium.u.hGlobal;
            let p = GlobalLock(h).cast::<u8>();
            let n = u32::from_le_bytes(*p.cast::<[u8; 4]>()) as usize;
            let header = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
            let mut names = Vec::new();
            for i in 0..n {
                let d = std::ptr::read_unaligned(
                    p.add(header + i * size_of::<FILEDESCRIPTORW>())
                        .cast::<FILEDESCRIPTORW>(),
                );
                let raw = d.cFileName;
                let len = raw.iter().position(|c| *c == 0).unwrap_or(raw.len());
                let attrs = d.dwFileAttributes;
                names.push((
                    String::from_utf16_lossy(&raw[..len]),
                    attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
                ));
            }
            let _ = GlobalUnlock(h);
            assert_eq!(
                names,
                [
                    ("proj".to_string(), true),
                    ("proj\\a_b.txt".to_string(), false),
                    ("proj\\sub".to_string(), true),
                    ("proj\\sub\\big.bin".to_string(), false),
                    ("proj\\sub\\empty".to_string(), true),
                    ("note.txt".to_string(), false),
                ]
            );
            // 第 3 项：big.bin
            fmt = FORMATETC {
                lindex: 3,
                ..formatetc(cf.contents, TYMED_ISTREAM.0 as u32)
            };
            let medium = obj.GetData(&fmt).unwrap();
            let stream = (*medium.u.pstm).clone().unwrap();
            let mut out = Vec::new();
            let mut buf = vec![0u8; 100_000];
            loop {
                let mut read = 0u32;
                let hr = stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read));
                out.extend_from_slice(&buf[..read as usize]);
                if hr != S_OK || read == 0 {
                    break;
                }
            }
            assert_eq!(
                out,
                f.files[&vec!["proj".to_string(), "sub".into(), "big.bin".into()]]
            );
            // 目录没有内容
            fmt.lindex = 2;
            assert!(obj.GetData(&fmt).is_err());
        }
        withdraw(0xa1);
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!is_ours());
    }

    /// 真的让资源管理器（外壳的"粘贴"）把剪贴板里的虚拟文件粘贴到一个文件夹里
    #[test]
    #[ignore]
    fn explorer_pastes_the_virtual_files() {
        use windows::Win32::UI::Shell::Common::ITEMIDLIST;
        use windows::Win32::UI::Shell::{
            CMINVOKECOMMANDINFO, IContextMenu, IShellFolder, SHBindToParent, SHParseDisplayName,
        };
        let dir = std::env::temp_dir().join(format!("airhand-paste-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = fake(0xa2);
        apply(f.clone());
        wait_ready(&f);
        unsafe {
            OleInitialize(None).unwrap();
            let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
            SHParseDisplayName(
                &windows::core::HSTRING::from(dir.as_os_str()),
                None,
                &mut pidl,
                0,
                None,
            )
            .unwrap();
            let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
            let parent: IShellFolder = SHBindToParent(pidl, Some(&mut child)).unwrap();
            let menu: IContextMenu = parent
                .GetUIObjectOf(Default::default(), &[child.cast_const()], None)
                .unwrap();
            let info = CMINVOKECOMMANDINFO {
                cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
                lpVerb: windows::core::PCSTR(c"paste".as_ptr().cast()),
                nShow: 1,
                ..Default::default()
            };
            menu.InvokeCommand(&info).unwrap();
        }
        // 外壳在自己的线程上复制
        let big = dir.join("proj").join("sub").join("big.bin");
        for _ in 0..300 {
            if std::fs::metadata(&big).is_ok_and(|m| m.len() == 1_500_000) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(
            std::fs::read(dir.join("note.txt")).unwrap(),
            b"hello from afar"
        );
        assert_eq!(
            std::fs::read(dir.join("proj").join("a_b.txt")).unwrap(),
            b"colon"
        );
        assert_eq!(
            std::fs::read(&big).unwrap(),
            f.files[&vec!["proj".to_string(), "sub".into(), "big.bin".into()]]
        );
        assert!(dir.join("proj").join("sub").join("empty").is_dir());
        assert!(!dir.join("proj").join("lib64").exists());
        withdraw(0xa2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
