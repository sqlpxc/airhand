//! 两个 [`Files`]（给的一方、取的一方）经真的 QUIC 连接说话：消息直接交给对方的 [`Files::handle`]，文件流走连接
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use airhand_proto::files::{FileEntry, FileError, FileKind, FileMsg, WINDOW};
use tokio::sync::mpsc;

use super::*;
use crate::lanes::Router;
use crate::tasks::OwnedTask;

struct ChanPeer(mpsc::UnboundedSender<FileMsg>);

impl Peer for ChanPeer {
    fn send(&self, msg: FileMsg) -> bool {
        self.0.send(msg).is_ok()
    }
}

/// 一对接好的 [`Files`]：`giver` 给、`taker` 取（反过来也行）。`limit`：中转的上限（这里没有直连，都算中转）
struct Pair {
    giver: Files,
    taker: Files,
    /// 取的一方收到的公告
    offers: mpsc::UnboundedReceiver<RemoteOffer>,
    /// 给的一方的文件消息（测试里偷看）
    _tasks: Vec<OwnedTask<()>>,
    _conns: (quinn::Connection, quinn::Connection),
}

async fn pair(limit: Option<u64>) -> Pair {
    const SID: u64 = 42;
    let (a, b) = crate::tests::quic_pair().await;
    let (ra, ta) = Router::start(&a);
    let (rb, tb) = Router::start(&b);
    let (inbox_a, reg_a) = ra.register(SID);
    let (inbox_b, reg_b) = rb.register(SID);
    let (keys_a, keys_b) = crate::secure::FileKeys::test_pair();
    let (to_b, mut from_a) = mpsc::unbounded_channel();
    let (to_a, mut from_b) = mpsc::unbounded_channel();
    let (giver, ga) = Files::start(
        SID,
        Paths::new(a.clone()),
        keys_a,
        limit,
        ChanPeer(to_b),
        inbox_a.files,
    );
    let (taker, gb) = Files::start(
        SID,
        Paths::new(b.clone()),
        keys_b,
        limit,
        ChanPeer(to_a),
        inbox_b.files,
    );
    let (offers_tx, offers) = mpsc::unbounded_channel();
    let t = taker.clone();
    let pump_b = tokio::spawn(async move {
        while let Some(m) = from_a.recv().await {
            if let Some(o) = t.handle(m) {
                let _ = offers_tx.send(o);
            }
        }
    });
    let g = giver.clone();
    let pump_a = tokio::spawn(async move {
        while let Some(m) = from_b.recv().await {
            g.handle(m);
        }
    });
    let keep = tokio::spawn(async move {
        let _keep = (reg_a, reg_b, ta, tb);
        std::future::pending::<()>().await
    });
    Pair {
        giver,
        taker,
        offers,
        _tasks: vec![
            ga,
            gb,
            OwnedTask(pump_a),
            OwnedTask(pump_b),
            OwnedTask(keep),
        ],
        _conns: (a, b),
    }
}

/// 临时目录（测试结束时删掉）
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "airhand-files-e2e-{name}-{}-{}",
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

fn p(s: &[&str]) -> Vec<String> {
    s.iter().map(|s| s.to_string()).collect()
}

/// 有点规律、又不是全一样的内容（错位了看得出来）
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32).wrapping_mul(2_654_435_761).to_le_bytes()[1] ^ seed)
        .collect()
}

async fn read_all(f: &mut RemoteFile) -> Result<Vec<u8>, FileError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 100_000];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), f.read(&mut buf))
            .await
            .expect("reading timed out")?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

async fn next_offer(pair: &mut Pair) -> RemoteOffer {
    tokio::time::timeout(Duration::from_secs(5), pair.offers.recv())
        .await
        .expect("no offer")
        .unwrap()
}

/// 复制一个文件夹：取的一方列出整棵树、一个个读出来，和原来的一样；大文件（好几个额度窗口）也一样；
/// 跳着读也对；对方放好了（Ready）给的一方就不再等
#[tokio::test]
async fn a_folder_travels_on_paste() {
    let t = Temp::new("folder");
    let dir = t.0.join("photos");
    std::fs::create_dir_all(dir.join("2024").join("empty")).unwrap();
    let big = content(3 * WINDOW as usize + 12_345, 1);
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    std::fs::write(dir.join("2024").join("a.txt"), b"hello").unwrap();
    std::fs::write(dir.join("2024").join("zero"), b"").unwrap();
    std::fs::write(t.0.join("note.txt"), content(1000, 2)).unwrap();
    let mut pair = pair(None).await;

    let provider = Arc::new(LocalFs::new(&[dir.clone(), t.0.join("note.txt")]));
    let clip = pair.giver.offer(provider).unwrap();
    assert!(pair.giver.awaiting_ready());
    let offer = next_offer(&mut pair).await;
    assert_eq!(offer.clip(), clip);
    assert!(offer.is_current());
    let names: Vec<_> = offer.items().iter().map(|i| (i.name(), i.kind)).collect();
    assert_eq!(
        names,
        [("photos", FileKind::Dir), ("note.txt", FileKind::File)]
    );
    offer.ready();
    assert!(pair.giver.await_ready(Duration::from_secs(5)).await);
    assert!(!pair.giver.awaiting_ready());

    let tree = offer.list(&p(&["photos"]), true).await.unwrap();
    let paths: Vec<_> = tree.iter().map(|e| (e.path.join("/"), e.kind)).collect();
    assert_eq!(
        paths,
        [
            ("2024".to_string(), FileKind::Dir),
            ("big.bin".to_string(), FileKind::File),
            ("2024/a.txt".to_string(), FileKind::File),
            ("2024/empty".to_string(), FileKind::Dir),
            ("2024/zero".to_string(), FileKind::File),
        ]
    );
    let one = offer.list(&p(&["photos", "2024"]), false).await.unwrap();
    assert_eq!(one.len(), 3);

    for e in tree.iter().filter(|e| e.kind == FileKind::File) {
        let mut path = p(&["photos"]);
        path.extend(e.path.iter().cloned());
        let mut f = offer.open(path.clone(), e.size, e.mtime);
        let got = read_all(&mut f).await.unwrap();
        let want = std::fs::read(dir.join(e.path.join("/"))).unwrap();
        assert_eq!(got.len(), want.len(), "{path:?}");
        assert!(got == want, "{path:?}");
    }
    // 跳着读：从中间开始，再跳回前面
    let size = big.len() as u64;
    let mut f = offer.open(p(&["photos", "big.bin"]), size, None);
    f.seek(size - 10);
    let mut buf = [0u8; 64];
    let n = f.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], &big[big.len() - 10..]);
    assert_eq!(f.read(&mut buf).await.unwrap(), 0);
    f.seek(5);
    let n = f.read(&mut buf).await.unwrap();
    assert!(n > 0);
    assert_eq!(&buf[..n], &big[5..5 + n]);
    drop(f);
    // 最上层的文件
    let note = &offer.items()[1];
    let mut f = offer.open(note.path.clone(), note.size, note.mtime);
    assert_eq!(read_all(&mut f).await.unwrap(), content(1000, 2));
}

/// 没有直连时一次粘贴（一个公告）经中转的总量不能超过上限：超过了给的一方回答太大，取的一方读不了，
/// 两边都告诉用户；没超过的照样传
#[tokio::test]
async fn too_much_for_the_relay_is_refused() {
    let t = Temp::new("limit");
    let dir = t.0.join("d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a"), content(600, 3)).unwrap();
    std::fs::write(dir.join("b"), content(600, 4)).unwrap();
    std::fs::write(t.0.join("small"), content(600, 5)).unwrap();
    let mut pair = pair(Some(1000)).await;
    let mut giver_notices = pair.giver.notices();
    let mut taker_notices = pair.taker.notices();

    // 文件夹一共 1200 字节：哪个文件都取不了（虽然每个都不到上限）
    pair.giver
        .offer(Arc::new(LocalFs::new(std::slice::from_ref(&dir))));
    let offer = next_offer(&mut pair).await;
    let tree = offer.list(&p(&["d"]), true).await.unwrap();
    let a = &tree[0];
    let mut f = offer.open(p(&["d", "a"]), a.size, a.mtime);
    assert_eq!(read_all(&mut f).await, Err(FileError::TooLarge));
    let n = giver_notices.recv().await.unwrap();
    assert_eq!((n.name.as_str(), n.error), ("a", FileError::TooLarge));
    let n = taker_notices.recv().await.unwrap();
    assert_eq!((n.name.as_str(), n.error), ("a", FileError::TooLarge));

    // 只有 600 字节的：照样传
    pair.giver
        .offer(Arc::new(LocalFs::new(&[t.0.join("small")])));
    let offer = next_offer(&mut pair).await;
    let small = &offer.items()[0];
    let mut f = offer.open(small.path.clone(), small.size, small.mtime);
    assert_eq!(read_all(&mut f).await.unwrap(), content(600, 5));
}

/// 复制之后文件变了：给的一方不给（大小对不上）；公告外面的、过期的公告、取用编号不递增的都不给
#[tokio::test]
async fn changed_missing_and_replayed_requests_fail() {
    let t = Temp::new("changed");
    std::fs::write(t.0.join("f"), content(100, 6)).unwrap();
    std::fs::write(t.0.join("g"), content(100, 7)).unwrap();
    let mut pair = pair(None).await;
    pair.giver.offer(Arc::new(LocalFs::new(&[t.0.join("f")])));
    let offer = next_offer(&mut pair).await;
    let item = offer.items()[0].clone();
    std::fs::write(t.0.join("f"), content(150, 6)).unwrap();
    let mut f = offer.open(item.path.clone(), item.size, item.mtime);
    assert_eq!(read_all(&mut f).await, Err(FileError::Changed));
    let mut f = offer.open(p(&["g"]), 100, None);
    assert_eq!(read_all(&mut f).await, Err(FileError::NotFound));
    let mut f = offer.open(p(&["f", "..", "g"]), 100, None);
    assert_eq!(read_all(&mut f).await, Err(FileError::Invalid));

    // 之后又公告了两次：最早的那个过期了
    pair.giver.offer(Arc::new(LocalFs::new(&[t.0.join("g")])));
    let second = next_offer(&mut pair).await;
    assert!(!offer.is_current() && second.is_current());
    pair.giver.offer(Arc::new(LocalFs::new(&[t.0.join("g")])));
    let _third = next_offer(&mut pair).await;
    let mut f = offer.open(p(&["f"]), 150, None);
    assert_eq!(read_all(&mut f).await, Err(FileError::Expired));
    // 还留着上一个：正在从它复制的照样复制得完
    let mut f = second.open(p(&["g"]), 100, None);
    assert_eq!(read_all(&mut f).await.unwrap(), content(100, 7));
    // 对方的剪贴板换成了文字
    pair.taker.remote_cleared();
    assert!(!_third.is_current());

    // 编号不递增的取用（同一把密钥不能加密两份内容）
    let (tx, mut rx) = mpsc::unbounded_channel();
    let spy = Files::start(
        1,
        Paths::new(pair._conns.0.clone()),
        crate::secure::FileKeys::test_pair().0,
        None,
        ChanPeer(tx),
        mpsc::unbounded_channel().1,
    )
    .0;
    let clip = spy.offer(Arc::new(LocalFs::new(&[t.0.join("g")]))).unwrap();
    let _ = rx.recv().await;
    let fetch = |fetch| FileMsg::Fetch {
        fetch,
        clip,
        path: p(&["g"]),
        offset: 0,
        size: 100,
        mtime: None,
        until: WINDOW,
    };
    spy.handle(fetch(5));
    spy.handle(FileMsg::Cancel { fetch: 5 });
    spy.handle(fetch(5));
    let failed = loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
        {
            Some(FileMsg::Failed { fetch: 5, error }) => break error,
            Some(_) => continue,
            None => panic!(),
        }
    };
    assert_eq!(failed, FileError::Invalid);
}

/// 读到一半不要了（本机程序取消了粘贴）：给的一方停下，不再占着名额；会话结束了读的都是 Closed
#[tokio::test]
async fn cancelling_stops_the_giver() {
    let t = Temp::new("cancel");
    std::fs::write(t.0.join("big"), content(4 * WINDOW as usize, 8)).unwrap();
    let mut pair = pair(None).await;
    pair.giver.offer(Arc::new(LocalFs::new(&[t.0.join("big")])));
    let offer = next_offer(&mut pair).await;
    let item: FileEntry = offer.items()[0].clone();
    for _ in 0..(MAX_SERVING + 2) {
        let mut f = offer.open(item.path.clone(), item.size, item.mtime);
        let mut buf = [0u8; 1000];
        assert_eq!(f.read(&mut buf).await.unwrap(), 1000);
        drop(f);
    }
    // 取消了的都不再占着名额：再取一个照样取得到
    let mut f = offer.open(item.path.clone(), item.size, item.mtime);
    assert_eq!(read_all(&mut f).await.unwrap().len(), item.size as usize);
    let serving = || serve::serving(&pair.giver.0);
    for _ in 0..100 {
        if serving() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(serving(), 0);

    let mut f = offer.open(item.path.clone(), item.size, item.mtime);
    pair.taker.close();
    assert_eq!(read_all(&mut f).await, Err(FileError::Closed));
}

/// 平台的回调线程上用阻塞的读法（会话的运行时在测试的线程上照常跑）
#[tokio::test]
async fn blocking_reads_from_a_platform_thread() {
    let t = Temp::new("blocking");
    let data = content(WINDOW as usize + 77, 9);
    std::fs::write(t.0.join("x"), &data).unwrap();
    let mut pair = pair(None).await;
    pair.giver.offer(Arc::new(LocalFs::new(&[t.0.join("x")])));
    let offer = next_offer(&mut pair).await;
    let (done, got) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let tree = offer.list_blocking(&p(&["x"]), false);
        // 文件不能列
        assert_eq!(tree.err(), Some(FileError::NotFound));
        let item = offer.items()[0].clone();
        let mut f = offer.open(item.path, item.size, item.mtime);
        let mut out = Vec::new();
        let mut buf = vec![0u8; 4096];
        loop {
            let n = f.read_blocking(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let _ = done.send(out);
    });
    let got = tokio::time::timeout(Duration::from_secs(20), got)
        .await
        .unwrap()
        .unwrap();
    assert!(got == data);
}
