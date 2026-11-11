//! broker 的测试：大多是真正的 QUIC 连接（进程内的服务器 + airhand-link 的客户端链路）。
//! 调试构建里每次加锁都检查先后（见 [`super::lock`]），所以这些测试也守着锁的顺序。

use airhand_link::Problem;
use airhand_link::controller::{
    ConnectOptions, ControllerService, Frontend, SessionEvent, SessionUi, UiClosed, establish,
};
use airhand_link::host::{Access, Gate, HostStreams, Incoming, Request, Router};
use airhand_link::lanes::open_lane;
use airhand_link::net::{LinkConfig, Signal, connect_signal};
use airhand_link::secure::{self, Identity, Initiator, InitiatorParams};
use airhand_link::wire::{read_msg, write_msg};
use airhand_proto::handshake::PublicKey;
use airhand_proto::session::{ByeReason, QualitySettings};
use airhand_proto::signal::{
    ClientMsg, ConnectError, Reclaim, ResumeToken, SessionGrant, connect_outcome,
};
use airhand_proto::{CODE_MAX, CODE_MIN, SessionLane, datagram};
use quinn::VarInt;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::time::Instant;
use tokio::sync::watch;

use super::limits::{FAIL_LIMIT, limiter_key, registration_key};
use super::registry::MAX_WAITING;
use super::tokens::{ct_eq, hmac_sha256};
use super::*;

/// 谁都让进来的被控端（测试用）
struct OpenGate(Identity);

impl Gate for OpenGate {
    fn identity(&self) -> Identity {
        self.0.clone()
    }
    fn access(&self) -> Access {
        Access::Open
    }
    fn is_known(&self, _: &PublicKey) -> bool {
        false
    }
}

/// 被控端收到的会话请求（没有直连；本机用户断开所有控制端的通知永远不会来）
fn incoming(
    host: &Signal,
    router: &Arc<Router>,
    code: u32,
    msg: ServerMsg,
) -> (Incoming, watch::Sender<ByeReason>) {
    let ServerMsg::Incoming {
        session_id,
        peer,
        init,
        ..
    } = msg
    else {
        panic!("expected Incoming, got {msg:?}");
    };
    let (kick_tx, kick) = watch::channel(ByeReason::Kicked);
    let inc = Incoming {
        conn: host.conn.clone(),
        router: router.clone(),
        session_id,
        init,
        code,
        peer,
        kick,
        approvals: Default::default(),
        direct: None,
    };
    (inc, kick_tx)
}

/// 控制端握手的第一条消息（第一次连这个控制码，不配对）
fn initiate(code: u32) -> (Initiator, Vec<u8>) {
    let me = Identity::generate();
    let intro = secure::controller_intro("ctl", "test", QualitySettings::default(), false);
    secure::initiate(InitiatorParams {
        code,
        me: &me,
        host_key: None,
        pairing: None,
        intro: &intro,
    })
    .unwrap()
}

/// 回应这个被控端收到的会话请求：端到端握手、打开会话。会话一直开着，直到任务被中止
fn answer(sig: Signal, code: u32) -> tokio::task::JoinHandle<()> {
    let gate = OpenGate(Identity::generate());
    tokio::spawn(async move {
        let mut sessions = Vec::new();
        let mut buf = Vec::new();
        // 整个拿进来（只用到其中几个字段的话，别的字段在函数返回时就被丢掉了：信令流随之关闭）
        let mut sig = sig;
        let (router, _router) = Router::start(&sig.conn);
        while let Ok(Some(m)) =
            read_msg::<_, ServerMsg>(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf).await
        {
            if let ServerMsg::Incoming { .. } = m {
                let (inc, kick) = incoming(&sig, &router, code, m);
                if let Ok(s) = HostStreams::open(&inc, 2, 16, &gate).await {
                    sessions.push((s, kick));
                }
            }
        }
    })
}

#[test]
fn limiter_key_groups_addresses() {
    let key = |s: &str| limiter_key(s.parse().unwrap()).to_string();
    assert_eq!(key("203.0.113.9"), "203.0.113.9");
    // 同一个 /64 里换地址也算同一个来源
    assert_eq!(key("2001:db8:1:2:aaaa::1"), "2001:db8:1:2::");
    assert_eq!(key("2001:db8:1:2:bbbb::9"), "2001:db8:1:2::");
    // IPv4 映射地址按 IPv4 计
    assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
    // 新分配控制码的限量：IPv6 按 /48
    let reg = |s: &str| registration_key(s.parse().unwrap()).to_string();
    assert_eq!(reg("2001:db8:1:2:aaaa::1"), "2001:db8:1::");
    assert_eq!(reg("2001:db8:1:ff00::1"), "2001:db8:1::");
    assert_eq!(reg("::ffff:203.0.113.9"), "203.0.113.9");
}

#[test]
fn resume_token_binds_code_and_secret() {
    let broker = Broker::new(test_config(None)).unwrap();
    let t = broker.resume_token(123_456, &[1; 16]);
    assert!(ct_eq(&t, &broker.resume_token(123_456, &[1; 16])));
    assert!(!ct_eq(&t, &broker.resume_token(123_457, &[1; 16])));
    assert!(!ct_eq(&t, &broker.resume_token(123_456, &[2; 16])));
    // 前一半只和控制码、服务器有关：换了密钥也认得是自己发的，别的控制码、别的服务器不认
    assert!(broker.vouches(123_456, &broker.resume_token(123_456, &[2; 16])));
    assert!(!broker.vouches(123_457, &t));
    assert!(!Broker::new(test_config(None)).unwrap().vouches(123_456, &t));
    assert!(!broker.vouches(123_456, &[0; 16]));
    assert!(!ct_eq(&t, &t[..15]));
}

/// RFC 4231 测试用例 2（密钥 "Jefe" 补零到 32 字节，按 HMAC 的规则和原来一样）
#[test]
fn hmac_matches_rfc4231() {
    let mut key = [0u8; 32];
    key[..4].copy_from_slice(b"Jefe");
    let mac = hmac_sha256(&key, &[b"what do ya ", b"want for nothing?"]);
    let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
}

#[test]
fn secrets_are_signed_for_one_code() {
    let broker = Broker::new(test_config(None)).unwrap();
    let s = broker.issue_secret(123_456);
    assert!(broker.issued_here(123_456, &s));
    assert!(!broker.issued_here(123_457, &s));
    let mut forged = s;
    forged[15] ^= 1;
    assert!(!broker.issued_here(123_456, &forged));
    // 别的服务器（不同的密钥）签发的不认
    assert!(
        !Broker::new(test_config(None))
            .unwrap()
            .issued_here(123_456, &s)
    );
}

fn test_config(db: Option<std::path::PathBuf>) -> Config {
    Config {
        max_sessions_per_host: 8,
        db,
        max_conns_per_ip: Config::DEFAULT_MAX_CONNS_PER_IP,
        new_codes_per_ip: 1000,
        connects_per_minute: 1000,
    }
}

/// 进程内的服务器（只监听 127.0.0.1），固定控制码只记在内存里
async fn start() -> (Arc<Broker>, LinkConfig) {
    start_with(None).await
}

async fn start_with(db: Option<std::path::PathBuf>) -> (Arc<Broker>, LinkConfig) {
    start_cfg(test_config(db)).await
}

async fn start_cfg(cfg: Config) -> (Arc<Broker>, LinkConfig) {
    let ck = rcgen::generate_simple_self_signed(vec!["airhand".into()]).unwrap();
    let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()).into();
    let endpoint = quinn::Endpoint::server(
        crate::server_config(chain, key).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let server = endpoint.local_addr().unwrap().to_string();
    let broker = Broker::new(cfg).unwrap();
    let b = broker.clone();
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            tokio::spawn(b.clone().handle_incoming(incoming));
        }
    });
    let cfg = LinkConfig::new(server, "", "test", "test").unwrap();
    (broker, cfg)
}

async fn next(sig: &mut Signal) -> ServerMsg {
    let mut buf = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        read_msg(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf),
    )
    .await
    .expect("timed out waiting for a server message")
    .unwrap()
    .expect("the server closed the control stream")
}

async fn register(cfg: &LinkConfig, reclaim: Option<Reclaim>) -> (Signal, Reclaim) {
    register_with(cfg, reclaim, false).await
}

async fn register_with(
    cfg: &LinkConfig,
    reclaim: Option<Reclaim>,
    keep: bool,
) -> (Signal, Reclaim) {
    register_full(cfg, reclaim, keep, false).await
}

async fn register_full(
    cfg: &LinkConfig,
    reclaim: Option<Reclaim>,
    keep: bool,
    rekey: bool,
) -> (Signal, Reclaim) {
    // 注册紧跟着版本握手发（和真正的被控端一样）
    let register = ClientMsg::Register {
        reclaim,
        keep,
        rekey,
    };
    let mut sig = connect_signal(cfg, Role::Host, Some(&register))
        .await
        .unwrap();
    match next(&mut sig).await {
        ServerMsg::Registered { code, secret } => (sig, Reclaim { code, secret }),
        other => panic!("{other:?}"),
    }
}

/// 发一个连接请求（不带握手：只看服务器怎么回答），下一条消息必须是它的回答
async fn ask(
    ctrl: &mut Signal,
    req_id: u32,
    code: u32,
    resume: Option<ResumeToken>,
) -> Result<SessionGrant, ConnectError> {
    ask_with(ctrl, req_id, code, resume, Vec::new()).await
}

/// 发一个带着握手的第一条消息的连接请求，下一条消息必须是它的回答
async fn ask_with(
    ctrl: &mut Signal,
    req_id: u32,
    code: u32,
    resume: Option<ResumeToken>,
    init: Vec<u8>,
) -> Result<SessionGrant, ConnectError> {
    write_msg(
        &mut ctrl.send,
        &ClientMsg::Connect {
            req_id,
            code,
            resume,
            init,
        },
    )
    .await
    .unwrap();
    match next(ctrl).await {
        ServerMsg::ConnectResult {
            req_id: r,
            grant,
            error,
        } if r == req_id => connect_outcome(grant, error),
        other => panic!("{other:?}"),
    }
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn waiter_count(broker: &Broker) -> usize {
    broker.waiters.lock().values().map(Vec::len).sum()
}

/// 画面通道堵住了（控制端一直不读）：控制通道、数据报照样到。真正的会话里，控制端在 select 里取消接收不会弄坏帧边界，
/// 画面通道正常结束也盖不住被控端有意发的最后一条控制消息
#[tokio::test]
async fn blocked_video_lane_does_not_block_control_or_audio() {
    use airhand_link::lanes::first_stream;
    use airhand_link::transport::FromHost;
    use airhand_proto::session::{HostMsg, Notice};
    let (_broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let (ctrl_router, _ctrl_router) = Router::start(&ctrl.conn);
    let sid = ask(&mut ctrl, 1, id.code, None).await.unwrap().session_id;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
    let (mut inbox, _reg) = ctrl_router.register(sid);
    let mut host_control = open_lane(&host.conn, sid, SessionLane::Control)
        .await
        .unwrap();
    let mut video_tx = open_lane(&host.conn, sid, SessionLane::Video)
        .await
        .unwrap();
    // 控制端从来不读画面通道
    let stalled = tokio::spawn(async move {
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..128 {
            video_tx.write_all(&chunk).await.unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !stalled.is_finished(),
        "video must be blocked by the unread lane"
    );
    write_msg(
        &mut host_control,
        &HostMsg::Notice(Some(Notice::NoDisplays)),
    )
    .await
    .unwrap();
    let mut ctrl_control = first_stream(&mut inbox.control, Duration::from_secs(2))
        .await
        .unwrap();
    let mut buf = Vec::new();
    let msg: HostMsg = timeout(
        Duration::from_secs(2),
        read_msg(&mut ctrl_control, airhand_proto::MAX_CONTROL_MSG, &mut buf),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(matches!(msg, HostMsg::Notice(Some(Notice::NoDisplays))));
    let d = [&sid.to_le_bytes()[..], &[datagram::AUDIO], b"opus"].concat();
    host.conn.send_datagram(d.into()).unwrap();
    let got = timeout(Duration::from_secs(2), inbox.audio.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got[datagram::HEADER..], b"opus");
    stalled.abort();
    let _ = stalled.await;

    // 这一次是真正的会话：经服务器转发的通道上做端到端握手，之后都是密文
    let (host_router, _host_router) = Router::start(&host.conn);
    let (initiator, init) = initiate(id.code);
    let grant = ask_with(&mut ctrl, 2, id.code, None, init).await.unwrap();
    let msg = next(&mut host).await;
    let (inc, _kick) = incoming(&host, &host_router, id.code, msg);
    let gate = OpenGate(Identity::generate());
    let (io, link) = tokio::join!(
        HostStreams::open(&inc, 2, 16, &gate),
        establish(&ctrl_router, &ctrl.conn, grant.session_id, initiator)
    );
    let ((io, _), mut link) = (io.unwrap(), link.unwrap());
    assert!(
        timeout(Duration::from_millis(10), link.receiver.recv())
            .await
            .is_err()
    );
    io.out.send(&HostMsg::Bye(ByeReason::Kicked)).unwrap();
    io.close(inc.session_id, Ok(())).await.unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(2), link.receiver.recv())
            .await
            .unwrap()
            .unwrap(),
        FromHost::Control(HostMsg::Bye(ByeReason::Kicked))
    ));
}

/// 带凭证的请求：不是本服务器为这个控制码发的凭证什么也不透露；被控端掉线期间一直重连不算猜码、
/// 不受限速影响，被控端回来时马上收到通知
#[tokio::test]
async fn resume_and_wait_for_the_host() {
    let (broker, cfg) = start().await;
    let ip = limiter_key("127.0.0.1".parse().unwrap());
    let (host, id) = register(&cfg, None).await;
    let code = id.code;
    let other = if code == CODE_MAX { CODE_MIN } else { code + 1 };
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let token = ask(&mut ctrl, 1, code, None).await.unwrap().resume;

    // 不是为这个控制码发的凭证：不管控制码在不在线、存不存在都是同一个回答，不算猜错，也不等
    let mut stranger = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    for (req, c, t) in [(1, code, [0; 16]), (2, other, [0; 16]), (3, other, token)] {
        assert_eq!(
            ask(&mut stranger, req, c, Some(t)).await.unwrap_err(),
            ConnectError::Revoked
        );
    }
    assert_eq!(broker.failure_count(ip), 0);
    assert_eq!(waiter_count(&broker), 0);

    // 被控端掉线
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    drop(host);

    // 这个 IP 已经被限速了，带凭证的重连照样不受影响，也不算猜错
    for _ in 0..FAIL_LIMIT {
        broker.record_failure(ip);
    }
    assert_eq!(
        ask(&mut ctrl, 10, other, None).await.unwrap_err(),
        ConnectError::RateLimited
    );
    for i in 0..5 {
        assert_eq!(
            ask(&mut ctrl, 11 + i, code, Some(token)).await.unwrap_err(),
            ConnectError::Offline
        );
    }
    assert_eq!(broker.failure_count(ip), FAIL_LIMIT);

    // 被控端取回控制码：在等的控制端收到通知（只一次），没在等的收不到
    let (_host, again) = register(&cfg, Some(id)).await;
    assert_eq!(again.code, code);
    assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code: c } if c == code));
    assert!(ask(&mut ctrl, 20, code, Some(token)).await.is_ok());
    // 下一条消息就是回答，说明 stranger 没有收到通知
    assert_eq!(
        ask(&mut stranger, 4, other, Some(token)).await.unwrap_err(),
        ConnectError::Revoked
    );

    // 每条连接最多等 MAX_WAITING 个被控端；连接断开后全部忘掉
    for i in 0..MAX_WAITING as u32 + 8 {
        let c = CODE_MIN + i;
        if c != code {
            let t = broker.resume_token(c, &[0; 16]);
            assert_eq!(
                ask(&mut stranger, 100 + i, c, Some(t)).await.unwrap_err(),
                ConnectError::Offline
            );
        }
    }
    assert_eq!(waiter_count(&broker), MAX_WAITING);
    stranger.conn.close(VarInt::from_u32(0), b"");
    wait_until("the waiters are forgotten", || waiter_count(&broker) == 0).await;
}

/// 控制端服务（airhand-link）：重连时自动带上凭证；被控端不在线时回答 HostOffline，
/// 被控端回来时调用 host_online
#[tokio::test]
async fn controller_service_waits_for_the_host() {
    struct NoUi;
    impl SessionUi for NoUi {
        fn packet(&self, _: Vec<u8>) -> Result<(), UiClosed> {
            Ok(())
        }
        fn event(&self, _: SessionEvent) -> Result<(), UiClosed> {
            Ok(())
        }
    }

    let (broker, cfg) = start().await;
    let ip = limiter_key("127.0.0.1".parse().unwrap());
    let (host, id) = register(&cfg, None).await;
    let host_conn = host.conn.clone();
    let host = answer(host, id.code);
    let (online_tx, mut online_rx) = mpsc::unbounded_channel();
    let svc = ControllerService::start(
        Arc::new(cfg.clone()),
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(move |code| {
            let _ = online_tx.send(code);
        }),
        Identity::generate(),
    );
    let code = id.code.to_string();
    let connect = |resume| {
        svc.connect(
            &code,
            ConnectOptions {
                resume,
                ..Default::default()
            },
            Arc::new(NoUi),
            Frontend::Browser,
        )
    };
    let first = loop {
        match connect(None).await {
            // 控制端服务还没连上服务器
            Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(20)).await,
            r => break r,
        }
    };
    let token = Some(first.unwrap().resume);

    host_conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    host.abort();
    for _ in 0..FAIL_LIMIT {
        broker.record_failure(ip);
    }
    assert_eq!(connect(token).await.unwrap_err(), Problem::HostOffline);
    // 不带凭证：普通的连接请求，照常限速
    assert_eq!(connect(None).await.unwrap_err(), Problem::RateLimited);

    let (host, _) = register(&cfg, Some(id)).await;
    let _host = answer(host, id.code);
    let code = tokio::time::timeout(Duration::from_secs(5), online_rx.recv())
        .await
        .expect("host_online wasn't called")
        .unwrap();
    assert_eq!(code, id.code);
    assert!(connect(token).await.is_ok());
}

/// 未知视频编码仍经过真实控制端会话转发给界面并确认；后面的已知帧继续接收。
#[tokio::test]
async fn unknown_video_codec_does_not_end_the_controller_session() {
    use airhand_link::host::Acks;
    use airhand_proto::media::{self, VideoHeader};
    use airhand_proto::session::Codec;
    struct Ui(mpsc::UnboundedSender<Vec<u8>>);
    /// 确认在读任务里就交给平台（Ping 也在那里回了）
    struct AckTx(mpsc::UnboundedSender<(u32, u64)>);
    impl Acks for AckTx {
        fn ack(&self, seq: u32, arrived: u64) {
            let _ = self.0.send((seq, arrived));
        }
    }
    impl SessionUi for Ui {
        fn packet(&self, p: Vec<u8>) -> Result<(), UiClosed> {
            self.0.send(p).map_err(|_| UiClosed)
        }
        fn event(&self, _: SessionEvent) -> Result<(), UiClosed> {
            Ok(())
        }
    }
    let (_broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let host_task = tokio::spawn(async move {
        let (router, _router) = Router::start(&host.conn);
        let msg = next(&mut host).await;
        let (inc, _kick) = incoming(&host, &router, id.code, msg);
        let session_id = inc.session_id;
        let gate = OpenGate(Identity::generate());
        let (io, _) = HostStreams::open(&inc, 2, 16, &gate).await.unwrap();
        let (ack_tx, mut acks) = mpsc::unbounded_channel();
        io.incoming.set_acks(Arc::new(AckTx(ack_tx)));
        for seq in [1, 2] {
            let mut header = VideoHeader {
                seq,
                key: true,
                codec: Codec::H264,
                width: 8,
                height: 8,
                capture_us: 0,
            }
            .encode();
            if seq == 1 {
                header[2] = 99;
            }
            io.video
                .send(media::frame(&header, &[&[1]]).into())
                .await
                .unwrap();
        }
        loop {
            let (seq, arrived) = timeout(Duration::from_secs(5), acks.recv())
                .await
                .unwrap()
                .unwrap();
            // 确认带着画面开始到达控制端的时刻
            assert!(arrived > 0);
            if seq == 2 {
                break;
            }
        }
        io.close(session_id, Ok(())).await.unwrap();
    });
    let (packets, mut received) = mpsc::unbounded_channel();
    let svc = ControllerService::start(
        Arc::new(cfg),
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        Identity::generate(),
    );
    let ui = Arc::new(Ui(packets));
    let session = timeout(Duration::from_secs(5), async {
        loop {
            match svc
                .connect(
                    &id.code.to_string(),
                    ConnectOptions::default(),
                    ui.clone(),
                    Frontend::Browser,
                )
                .await
            {
                Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(10)).await,
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    for seq in [1, 2] {
        let packet = timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        let h = VideoHeader::decode_any(&packet).unwrap();
        assert_eq!(h.seq, seq);
        assert_eq!(
            h.codec,
            if seq == 1 {
                Codec::Unknown(99)
            } else {
                Codec::H264
            }
        );
        svc.ack(session.sid, seq).unwrap();
    }
    host_task.await.unwrap();
}

/// 测试用的数据库文件。放在 target/ 里：服务器的后台任务在测试结束前一直开着数据库，Windows 上删不掉
fn temp_db() -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/test-tmp")
        .join(format!("broker-{:016x}", rand::random::<u64>()));
    let file = dir.join("airhand.db");
    (dir, file)
}

/// 服务器重启之后（只剩数据库，内存里的东西都不记得了），控制端可能比被控端先回来：凭证照样可以用；
/// 不保存控制码的被控端也能取回原来的控制码（密钥是这台服务器签发的）
#[tokio::test]
async fn resume_survives_a_server_restart() {
    let (dir, file) = temp_db();
    let (old, cfg) = start_with(Some(file.clone())).await;
    let (host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
    old.flush_bindings().await;
    drop((host, ctrl, old));

    let (_new, cfg) = start_with(Some(file)).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    assert_eq!(
        ask(&mut ctrl, 1, id.code, Some(token)).await.unwrap_err(),
        ConnectError::Offline
    );
    let (_host, again) = register(&cfg, Some(id)).await;
    assert_eq!(again, id);
    assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code } if code == id.code));
    assert!(ask(&mut ctrl, 2, id.code, Some(token)).await.is_ok());
    let _ = std::fs::remove_dir_all(dir);
}

/// 注册不能用来探测控制码有没有人用：拿不出服务器为这个控制码签发的密钥，不管它有没有人用都分配一个新的
#[tokio::test]
async fn registration_does_not_reveal_whether_a_code_is_taken() {
    let (broker, cfg) = start().await;
    let (_victim, v) = register(&cfg, None).await;
    let free = if v.code == CODE_MAX {
        CODE_MIN
    } else {
        v.code + 1
    };
    assert!(!broker.hosts.lock().contains_key(&free));
    for code in [v.code, free] {
        let (_s, got) = register(
            &cfg,
            Some(Reclaim {
                code,
                secret: [0xAB; 16],
            }),
        )
        .await;
        assert_ne!(got.code, code);
    }
    // 服务器为它签发的密钥：控制码空闲（例如保留期过了）时还给它
    let (host, own) = register(&cfg, None).await;
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || {
        !broker.hosts.lock().contains_key(&own.code)
    })
    .await;
    broker.reserved.lock().clear();
    let (_host, again) = register(&cfg, Some(own)).await;
    assert_eq!(again, own);
}

/// 用真实 QUIC 和端到端握手检查通道预算：大剪贴板在大块通道上可以，控制通道上的大消息、发错通道的消息都结束会话
#[tokio::test]
async fn session_lane_limits_reject_oversized_control_and_wrong_lanes() {
    use airhand_proto::framing;
    use airhand_proto::session::{ControllerMsg, InputEvent};
    for wrong_lane in [false, true] {
        let (_broker, cfg) = start().await;
        let (mut host, id) = register(&cfg, None).await;
        let (host_router, _host_router) = Router::start(&host.conn);
        let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
        let (ctrl_router, _ctrl_router) = Router::start(&ctrl.conn);
        let (initiator, init) = initiate(id.code);
        let grant = ask_with(&mut ctrl, 1, id.code, None, init).await.unwrap();
        let msg = next(&mut host).await;
        let (inc, _kick) = incoming(&host, &host_router, id.code, msg);
        let gate = OpenGate(Identity::generate());
        let (host_io, link) = tokio::join!(
            HostStreams::open(&inc, 2, 16, &gate),
            establish(&ctrl_router, &ctrl.conn, grant.session_id, initiator)
        );
        let ((mut host_io, _), mut link) = (host_io.unwrap(), link.unwrap());
        let text = "x".repeat(100_000);
        // 大剪贴板在大块通道上有效。
        let mut frame = framing::encode(&ControllerMsg::Clipboard { text: text.clone() });
        link.bulk.1.seal_frame(&mut frame).unwrap();
        link.bulk.0.write_all(&frame).await.unwrap();
        let req = timeout(Duration::from_secs(5), host_io.incoming.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(req, Request::Clipboard(ref t) if *t == text));
        if wrong_lane {
            // 已知消息发到错误通道，即使密文认证成功也应拒绝。
            let keyframe = ControllerMsg::RequestKeyframe { after: 1 };
            assert_eq!(keyframe.lane(), SessionLane::Control);
            let mut frame = framing::encode(&keyframe);
            link.bulk.1.seal_frame(&mut frame).unwrap();
            link.bulk.0.write_all(&frame).await.unwrap();
        } else {
            // 同样大小的输入超出了控制通道的上限（控制端会分批发）
            let mut frame = framing::encode(&ControllerMsg::Input {
                t: 0,
                events: vec![InputEvent::Text { text }],
            });
            link.control.1.seal_frame(&mut frame).unwrap();
            link.control.0.write_all(&frame).await.unwrap();
        }
        assert!(
            timeout(Duration::from_secs(5), host_io.incoming.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert!(host_io.close(inc.session_id, Ok(())).await.is_err());
    }
}

/// 本机用户断开所有控制端时换密钥：之前的重连凭证作废，控制码不变。
/// 被控端用新密钥取回控制码；没来得及保存新密钥时，旧密钥也还认（同时再换一个），保存了之后旧的就不认了
#[tokio::test]
async fn rekey_voids_tokens_and_keeps_the_code() {
    let (dir, file) = temp_db();
    let (broker, cfg) = start_with(Some(file.clone())).await;
    let (mut host, id) = register_with(&cfg, None, true).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let old_token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));

    write_msg(&mut host.send, &ClientMsg::Rekey { secret: id.secret })
        .await
        .unwrap();
    let ServerMsg::Rekeyed { secret } = next(&mut host).await else {
        panic!("expected Rekeyed");
    };
    assert_ne!(secret, id.secret);
    // 旧凭证作废了
    assert_eq!(
        ask(&mut ctrl, 2, id.code, Some(old_token))
            .await
            .unwrap_err(),
        ConnectError::Revoked
    );
    // 控制码本身照样能连，拿到的是新凭证
    let fresh = ask(&mut ctrl, 3, id.code, None).await.unwrap().resume;
    assert_ne!(fresh, old_token);
    assert!(ask(&mut ctrl, 4, id.code, Some(fresh)).await.is_ok());

    // 服务器重启（新的绑定在回答 Rekeyed 之前就落盘了）
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    broker.flush_bindings().await;
    drop((host, ctrl, broker));
    let (broker, cfg) = start_with(Some(file)).await;
    // 被控端没来得及保存新密钥，拿着旧的来：控制码还给它，并且换一个新的
    let (h1, again) = register_with(&cfg, Some(id), true).await;
    assert_eq!(again.code, id.code);
    assert!(again.secret != id.secret && again.secret != secret);
    h1.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    // 带着新密钥回来：之后旧密钥不再认
    let (h2, back) = register_with(&cfg, Some(again), true).await;
    assert_eq!(back, again);
    h2.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    broker.reserved.lock().clear();
    let (_h3, stolen) = register_with(&cfg, Some(id), true).await;
    assert_ne!(stolen.code, id.code);
    let _ = std::fs::remove_dir_all(dir);
}

/// 同一把已保存密钥的重复换钥是幂等的；断线、服务器重启都不能丢失归属。
#[tokio::test]
async fn repeated_rekey_preserves_the_last_confirmed_secret() {
    let (dir, file) = temp_db();
    let (broker, cfg) = start_with(Some(file.clone())).await;
    let (mut host, saved) = register_with(&cfg, None, true).await;
    let request = ClientMsg::Rekey {
        secret: saved.secret,
    };
    write_msg(&mut host.send, &request).await.unwrap();
    write_msg(&mut host.send, &request).await.unwrap();
    let ServerMsg::Rekeyed { secret: first } = next(&mut host).await else {
        panic!()
    };
    let ServerMsg::Rekeyed { secret: duplicate } = next(&mut host).await else {
        panic!()
    };
    assert_eq!(first, duplicate);
    assert_eq!(
        broker.bindings.lock().check(saved.code, &saved.secret),
        crate::codes::Owner::Previous
    );
    host.conn.close(VarInt::from_u32(0), b"replies not saved");
    wait_until("the host disconnected", || broker.stats().0 == 0).await;
    broker.flush_bindings().await;
    let (restarted, cfg) = start_with(Some(file)).await;
    let (mut host, recovered) = register_full(&cfg, Some(saved), true, true).await;
    assert_eq!(recovered.code, saved.code);
    assert_ne!(recovered.secret, saved.secret);
    // 新请求证明已经保存 recovered：只在这时淘汰原来的 saved。
    write_msg(
        &mut host.send,
        &ClientMsg::Rekey {
            secret: recovered.secret,
        },
    )
    .await
    .unwrap();
    let ServerMsg::Rekeyed { secret: next } = next(&mut host).await else {
        panic!()
    };
    assert_ne!(next, recovered.secret);
    assert_eq!(
        restarted
            .bindings
            .lock()
            .check(saved.code, &recovered.secret),
        crate::codes::Owner::Previous
    );
    assert_eq!(
        restarted.bindings.lock().check(saved.code, &saved.secret),
        crate::codes::Owner::Other
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// 被控端服务（airhand-link）：本机用户断开所有控制端时请服务器换密钥，并保存新密钥；被断开的控制端拿着旧凭证连不回来
#[tokio::test]
async fn host_service_rekeys_on_kick_all() {
    use airhand_link::host::{HostCmd, HostEvents};
    struct Events(mpsc::UnboundedSender<(Reclaim, bool)>);
    impl HostEvents for Events {
        fn server(&self, _: airhand_link::ServerState) {}
        async fn registered(&self, code: Reclaim, changed: bool) {
            let _ = self.0.send((code, changed));
        }
        async fn released(&self) {}
        fn ended(&self, _: u64) {}
        fn offline(&self) {}
    }
    let (_broker, cfg) = start().await;
    let (tx, mut saved) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(airhand_link::host::run(
        Arc::new(cfg.clone()),
        true,
        None,
        Arc::new(Events(tx)),
        |_inc| async {},
        cmd_rx,
    ));
    let next_saved = async |saved: &mut mpsc::UnboundedReceiver<(Reclaim, bool)>| {
        timeout(Duration::from_secs(5), saved.recv())
            .await
            .expect("the host didn't save its code")
            .unwrap()
    };
    let (first, changed) = next_saved(&mut saved).await;
    assert!(changed);
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let token = ask(&mut ctrl, 1, first.code, None).await.unwrap().resume;

    cmd_tx.send(HostCmd::KickAll).unwrap();
    let (second, changed) = next_saved(&mut saved).await;
    assert!(changed);
    assert_eq!(second.code, first.code);
    assert_ne!(second.secret, first.secret);
    assert_eq!(
        ask(&mut ctrl, 2, first.code, Some(token))
            .await
            .unwrap_err(),
        ConnectError::Revoked
    );
    task.abort();
}

/// 被控端发了 Rekey 却没等到回答就断线了（服务器可能没收到、或者收到了还没记下就重启了）：
/// 重新注册时带上 rekey，直到换成了为止（airhand-link 的被控端服务）
#[tokio::test]
async fn host_service_retries_an_unanswered_rekey() {
    use airhand_link::host::{HostCmd, HostEvents};
    struct Events;
    impl HostEvents for Events {
        fn server(&self, _: airhand_link::ServerState) {}
        async fn registered(&self, _: Reclaim, _: bool) {}
        async fn released(&self) {}
        fn ended(&self, _: u64) {}
        fn offline(&self) {}
    }
    // 只会说信令的假服务器
    let ck = rcgen::generate_simple_self_signed(vec!["airhand".into()]).unwrap();
    let chain = vec![CertificateDer::from(ck.cert.der().to_vec())];
    let key = PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()).into();
    let endpoint = quinn::Endpoint::server(
        crate::server_config(chain, key).unwrap(),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let cfg = LinkConfig::new(endpoint.local_addr().unwrap().to_string(), "", "t", "t").unwrap();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    // 不保存控制码的被控端
    let task = tokio::spawn(airhand_link::host::run(
        Arc::new(cfg),
        false,
        None,
        Arc::new(Events),
        |_inc| async {},
        cmd_rx,
    ));
    let mut buf = Vec::new();
    // 接受被控端的一条连接，握手，返回它的注册请求
    let mut accept = async || {
        let incoming = timeout(Duration::from_secs(10), endpoint.accept())
            .await
            .expect("the host didn't (re)connect")
            .unwrap();
        let conn = incoming.await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        assert!(
            read_frame(&mut recv, MAX_SIGNAL_MSG, &mut buf)
                .await
                .unwrap()
        );
        let hello: ClientHello = airhand_proto::framing::decode_known(&buf).unwrap();
        assert_eq!(hello.role, Role::Host);
        let answer = ServerHello::answer(hello.versions(), airhand_proto::Features::NONE);
        assert!(answer.version.is_some());
        write_frame(&mut send, &airhand_proto::framing::to_vec(&answer))
            .await
            .unwrap();
        let reg: ClientMsg = read_msg(&mut recv, MAX_SIGNAL_MSG, &mut buf)
            .await
            .unwrap()
            .unwrap();
        (conn, send, recv, reg)
    };
    let (conn, mut send, mut recv, reg) = accept().await;
    assert!(
        matches!(
            reg,
            ClientMsg::Register {
                reclaim: None,
                rekey: false,
                ..
            }
        ),
        "{reg:?}"
    );
    let id = Reclaim {
        code: 123_456,
        secret: [7; 16],
    };
    write_msg(
        &mut send,
        &ServerMsg::Registered {
            code: id.code,
            secret: id.secret,
        },
    )
    .await
    .unwrap();
    cmd_tx.send(HostCmd::KickAll).unwrap();
    cmd_tx.send(HostCmd::KickAll).unwrap();
    let (requests, mut received) = mpsc::channel(4);
    let reader = tokio::spawn(async move {
        let mut b = Vec::new();
        while let Ok(Some(m)) = read_msg::<_, ClientMsg>(&mut recv, MAX_SIGNAL_MSG, &mut b).await {
            if requests.send(m).await.is_err() {
                break;
            }
        }
    });
    let rekey = received.recv().await.unwrap();
    assert!(matches!(rekey, ClientMsg::Rekey { secret: s } if s == id.secret));
    assert!(
        timeout(Duration::from_millis(100), received.recv())
            .await
            .is_err(),
        "only one rekey may be outstanding"
    );
    write_msg(&mut send, &ServerMsg::Rekeyed { secret: [8; 16] })
        .await
        .unwrap();
    let rekey = timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(rekey, ClientMsg::Rekey { secret: s } if s == [8; 16]));
    // 第二次换钥的回答还没收到就断了：重连使用已经保存的第一把新密钥。
    conn.close(VarInt::from_u32(0), b"gone");
    let (_conn, _send, _recv, reg) = accept().await;
    assert!(
        matches!(reg, ClientMsg::Register { reclaim: Some(r), keep: false, rekey: true } if r == (Reclaim { code: id.code, secret: [8; 16] })),
        "{reg:?}"
    );
    reader.abort();
    task.abort();
}

/// 不保存控制码的被控端发了 Rekey 却没等到回答（断线了），服务器又恰好重启（什么都没记下）：
/// 被控端注册时带上 rekey，服务器还回控制码的同时换密钥，旧密钥不会"重新生效"、旧凭证也就恢复不了
#[tokio::test]
async fn rekey_on_register_survives_a_lost_answer_and_a_restart() {
    let (dir, file) = temp_db();
    let (old, cfg) = start_with(Some(file.clone())).await;
    let (host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let token = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
    old.flush_bindings().await;
    drop((host, ctrl, old));

    let (broker, cfg) = start_with(Some(file)).await;
    let (h1, again) = register_full(&cfg, Some(id), false, true).await;
    assert_eq!(again.code, id.code);
    assert_ne!(again.secret, id.secret);
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    assert_eq!(
        ask(&mut ctrl, 1, id.code, Some(token)).await.unwrap_err(),
        ConnectError::Revoked
    );
    // 新密钥又没收到、拿着旧的回来：控制码还给它，但再换一个，旧凭证一直作废
    h1.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    let (_h2, third) = register(&cfg, Some(id)).await;
    assert_eq!(third.code, id.code);
    assert!(third.secret != id.secret && third.secret != again.secret);
    assert_eq!(
        ask(&mut ctrl, 2, id.code, Some(token)).await.unwrap_err(),
        ConnectError::Revoked
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// 我的设备里保存的凭证可能作废了（对方断开过所有控制端）：对方在线时回答 Revoked（控制端改用控制码直接连），
/// 不在线时照样等它、对方上线时也通知，都不算猜错——从我的设备打开一台不在线的设备，不用再拿控制码试一次
#[tokio::test]
async fn stale_tokens_are_revoked_and_never_count_as_guesses() {
    let (broker, cfg) = start().await;
    let ip = limiter_key("127.0.0.1".parse().unwrap());
    let (mut host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let stale = ask(&mut ctrl, 1, id.code, None).await.unwrap().resume;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
    write_msg(&mut host.send, &ClientMsg::Rekey { secret: id.secret })
        .await
        .unwrap();
    let ServerMsg::Rekeyed { secret } = next(&mut host).await else {
        panic!("expected Rekeyed");
    };
    assert_eq!(
        ask(&mut ctrl, 2, id.code, Some(stale)).await.unwrap_err(),
        ConnectError::Revoked
    );
    // 对方不在线：等它
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 0).await;
    drop(host);
    assert_eq!(
        ask(&mut ctrl, 3, id.code, Some(stale)).await.unwrap_err(),
        ConnectError::Offline
    );
    assert_eq!(broker.failure_count(ip), 0);
    // 对方回来了：照样通知；重连时才知道凭证作废了，改用控制码直接连
    let (_host, _) = register(
        &cfg,
        Some(Reclaim {
            code: id.code,
            secret,
        }),
    )
    .await;
    assert!(matches!(next(&mut ctrl).await, ServerMsg::HostOnline { code } if code == id.code));
    assert_eq!(
        ask(&mut ctrl, 4, id.code, Some(stale)).await.unwrap_err(),
        ConnectError::Revoked
    );
    assert!(ask(&mut ctrl, 5, id.code, None).await.is_ok());
    assert_eq!(broker.failure_count(ip), 0);
}

/// 控制码存在、但被控端还没准备好（正在注册、正在断开）：回答和控制码不存在时一模一样——
/// 一样算猜错、一样晚回答，不能靠回答的快慢看出控制码存在
#[tokio::test]
async fn a_host_that_isnt_ready_is_answered_like_an_unknown_code() {
    let (broker, cfg) = start().await;
    let ip = limiter_key("127.0.0.1".parse().unwrap());
    let (_host, id) = register(&cfg, None).await;
    broker.hosts.lock().get_mut(&id.code).unwrap().ready = false;
    let unknown = (CODE_MIN..=CODE_MAX)
        .find(|c| !broker.hosts.lock().contains_key(c))
        .unwrap();
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    for (req, code) in [(1, id.code), (2, unknown)] {
        let t = Instant::now();
        assert_eq!(
            ask(&mut ctrl, req, code, None).await.unwrap_err(),
            ConnectError::NotFound
        );
        assert!(t.elapsed() >= Duration::from_millis(400), "{code}");
    }
    assert_eq!(broker.failure_count(ip), 2);
}

/// 服务器不管会话有几条通道、各是什么：以后新加的通道（这里是 5 号）照样转给另一端；
/// 同一端把同一条通道开两次的拒绝；任何一端的控制通道结束，整个会话结束，其它通道随之断开
#[tokio::test]
async fn lanes_the_server_has_never_heard_of_are_relayed() {
    use airhand_proto::SessionHeader;
    let (broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let sid = ask(&mut ctrl, 1, id.code, None).await.unwrap().session_id;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
    let lane = async |conn: &quinn::Connection, lane: u8| {
        let mut send = conn.open_uni().await.unwrap();
        let h = SessionHeader {
            session_id: sid,
            lane,
            priority: 3,
        };
        send.write_all(&h.encode()).await.unwrap();
        send
    };
    // 另一端收到的：同样的会话头，然后是数据
    let accept = async |conn: &quinn::Connection| {
        let mut recv = timeout(Duration::from_secs(2), conn.accept_uni())
            .await
            .unwrap()
            .unwrap();
        let mut hdr = [0u8; SessionHeader::LEN];
        recv.read_exact(&mut hdr).await.unwrap();
        (SessionHeader::decode(&hdr).unwrap(), recv)
    };
    // 新的通道：不用等控制通道，另一端马上收到
    let mut h5 = lane(&host.conn, 5).await;
    h5.write_all(b"future lane").await.unwrap();
    let (h, mut c5) = accept(&ctrl.conn).await;
    assert_eq!((h.session_id, h.lane, h.priority), (sid, 5, 3));
    let mut buf = [0u8; 11];
    timeout(Duration::from_secs(2), c5.read_exact(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf, b"future lane");
    let hc = lane(&host.conn, SessionLane::Control as u8).await;
    let mut cc = lane(&ctrl.conn, SessionLane::Control as u8).await;
    cc.write_all(b"ping").await.unwrap();
    let (h, mut hcr) = accept(&host.conn).await;
    assert_eq!(h.lane, SessionLane::Control as u8);
    let mut b4 = [0u8; 4];
    timeout(Duration::from_secs(2), hcr.read_exact(&mut b4))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&b4, b"ping");
    let (_, _ccr) = accept(&ctrl.conn).await;
    // 同一端再开一次 5 号通道：拒绝
    let dup = lane(&host.conn, 5).await;
    assert!(
        timeout(Duration::from_secs(2), dup.stopped())
            .await
            .unwrap()
            .is_ok()
    );
    assert!(broker.sessions.lock().contains_key(&sid));
    // 一端的控制通道结束：整个会话结束，别的通道随之断开
    drop(hc);
    wait_until("the session is gone", || {
        !broker.sessions.lock().contains_key(&sid)
    })
    .await;
    assert!(
        timeout(Duration::from_secs(2), c5.read(&mut buf))
            .await
            .unwrap()
            .is_err()
    );
    drop(cc);
}

/// 数据报按开头的会话 ID 在会话的两端之间转发（服务器批准了就转，不用等通道）；不是这个会话的连接发的、
/// 会话 ID 不对的、会话结束之后的都不转
#[tokio::test]
async fn datagrams_are_relayed_between_the_two_ends_of_a_session() {
    let (broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let sid = ask(&mut ctrl, 1, id.code, None).await.unwrap().session_id;
    assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
    let dgram = |sid: u64, body: &[u8]| {
        [&sid.to_le_bytes()[..], &[datagram::AUDIO], body]
            .concat()
            .into()
    };
    let recv = async |conn: &quinn::Connection| {
        let d = timeout(Duration::from_secs(2), conn.read_datagram())
            .await
            .expect("no datagram")
            .unwrap();
        assert_eq!(datagram::session(&d), Some(sid));
        d[datagram::HEADER..].to_vec()
    };
    host.conn.send_datagram(dgram(sid, b"hello")).unwrap();
    assert_eq!(recv(&ctrl.conn).await, b"hello");
    ctrl.conn.send_datagram(dgram(sid, b"back")).unwrap();
    assert_eq!(recv(&host.conn).await, b"back");
    // 别的连接冒用这个会话、会话 ID 不对：都不转（接着收到的是后面那个）
    let other = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    other.conn.send_datagram(dgram(sid, b"intruder")).unwrap();
    host.conn.send_datagram(dgram(sid ^ 1, b"lost")).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    ctrl.conn.send_datagram(dgram(sid, b"back again")).unwrap();
    host.conn.send_datagram(dgram(sid, b"again")).unwrap();
    assert_eq!(recv(&host.conn).await, b"back again");
    assert_eq!(recv(&ctrl.conn).await, b"again");
    // 会话结束（一端的控制通道结束了）：不再转
    let control = open_lane(&host.conn, sid, SessionLane::Control)
        .await
        .unwrap();
    drop(control);
    wait_until("the session is gone", || {
        !broker.sessions.lock().contains_key(&sid)
    })
    .await;
    // 两端的转发任务都记着这个会话的另一端（见 relay_datagrams）：会话结束之后照样不转
    host.conn.send_datagram(dgram(sid, b"late")).unwrap();
    ctrl.conn.send_datagram(dgram(sid, b"late")).unwrap();
    for conn in [&host.conn, &ctrl.conn] {
        let late = timeout(Duration::from_millis(300), conn.read_datagram()).await;
        assert!(late.is_err(), "relayed a datagram after the session ended");
    }
}

/// 被控端（airhand-link 的 HostStreams）的声音包走数据报，经服务器转发，控制端服务交给界面
#[tokio::test]
async fn controller_service_gets_audio_over_datagrams() {
    use airhand_proto::media::{self, AudioHeader, Kind};
    struct Ui(mpsc::UnboundedSender<Vec<u8>>);
    impl SessionUi for Ui {
        fn packet(&self, p: Vec<u8>) -> Result<(), UiClosed> {
            if Kind::of(&p) == Some(Kind::Audio) {
                let _ = self.0.send(p);
            }
            Ok(())
        }
        fn event(&self, _: SessionEvent) -> Result<(), UiClosed> {
            Ok(())
        }
    }
    let (broker, cfg) = start().await;
    let (mut host, id) = register(&cfg, None).await;
    let svc = ControllerService::start(
        Arc::new(cfg.clone()),
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        Identity::generate(),
    );
    let (tx, mut audio) = mpsc::unbounded_channel();
    let ui = Arc::new(Ui(tx));
    let code = id.code.to_string();
    let connecting = async {
        loop {
            let r = svc
                .connect(
                    &code,
                    ConnectOptions {
                        settings: QualitySettings {
                            fps: 50,
                            ..QualitySettings::default()
                        },
                        audio: true,
                        ..ConnectOptions::default()
                    },
                    ui.clone(),
                    Frontend::Browser,
                )
                .await;
            match r {
                Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(20)).await,
                r => break r,
            }
        }
    };
    let gate = OpenGate(Identity::generate());
    let (router, _router) = Router::start(&host.conn);
    let accepting = async {
        let msg = next(&mut host).await;
        let (inc, kick) = incoming(&host, &router, id.code, msg);
        let (s, admitted) = HostStreams::open(&inc, 2, 16, &gate).await.unwrap();
        (inc.session_id, s, admitted, kick)
    };
    let (info, (sid, session, admitted, _kick)) = tokio::join!(connecting, accepting);
    let info = info.unwrap();
    // 两端的安全码一样；控制端记下的是被控端真正的公钥；开始时的画质和声音在握手里就到了
    assert_eq!(info.safety, admitted.safety);
    assert_eq!((admitted.settings.fps, admitted.audio), (50, true));
    assert_eq!(info.host_key, airhand_link::secure::hex(gate.0.public()));
    assert!(broker.sessions.lock().contains_key(&sid));
    for seq in 1..=5u32 {
        let h = AudioHeader {
            seq,
            start: seq == 1,
            capture_us: seq as u64,
        };
        let packet = media::frame(&h.encode(), &[&[seq as u8; 200]]);
        session.audio.send(packet.into()).await.unwrap();
    }
    let mut got = Vec::new();
    while got.len() < 5 {
        let p = timeout(Duration::from_secs(5), audio.recv())
            .await
            .expect("no audio")
            .unwrap();
        assert_eq!(p.len(), media::HEADER_LEN + 200);
        got.push(AudioHeader::decode(&p).unwrap().seq);
    }
    assert_eq!(got, [1, 2, 3, 4, 5]);
    assert!(host.conn.stats().frame_tx.datagram >= 5);
    drop(session);
}

/// 新分配的控制码按来源限量；取回自己原来的控制码不算
#[tokio::test]
async fn new_codes_are_limited_per_network() {
    let (_broker, cfg) = start_cfg(Config {
        new_codes_per_ip: 2,
        ..test_config(None)
    })
    .await;
    let (_a, a) = register(&cfg, None).await;
    let (_b, _) = register(&cfg, None).await;
    let mut sig = connect_signal(&cfg, Role::Host, None).await.unwrap();
    write_msg(
        &mut sig.send,
        &ClientMsg::Register {
            reclaim: None,
            keep: false,
            rekey: false,
        },
    )
    .await
    .unwrap();
    let reason = timeout(Duration::from_secs(5), sig.conn.closed())
        .await
        .unwrap();
    assert!(
        matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
            if c.error_code == VarInt::from_u32(close::REGISTER_REFUSED)),
        "{reason:?}"
    );
    let (_again, back) = register(&cfg, Some(a)).await;
    assert_eq!(back, a);
}

/// 控制码对了的连接请求也限速（按来源和控制码），不然知道控制码的人反复发起连接，就能一直占满被控端的会话槽位、
/// 让它不停地问本机用户。带凭证的也算，换一条连接也一样；被拒绝的请求到不了被控端、不占槽位、不算猜错；
/// 别的被控端不受影响
#[tokio::test]
async fn sessions_are_limited_per_source_and_code() {
    let (broker, cfg) = start_cfg(Config {
        connects_per_minute: 3,
        ..test_config(None)
    })
    .await;
    let ip = limiter_key("127.0.0.1".parse().unwrap());
    let (mut host, id) = register(&cfg, None).await;
    let (mut other_host, other) = register(&cfg, None).await;
    let mut ctrl = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let mut token = None;
    for req in 0..3 {
        let grant = ask(&mut ctrl, req, id.code, token).await.unwrap();
        token = Some(grant.resume);
        assert!(matches!(next(&mut host).await, ServerMsg::Incoming { .. }));
    }
    assert_eq!(broker.stats().1, 3);

    for (req, resume) in [(10, None), (11, token)] {
        assert_eq!(
            ask(&mut ctrl, req, id.code, resume).await.unwrap_err(),
            ConnectError::RateLimited
        );
    }
    let mut second = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    assert_eq!(
        ask(&mut second, 1, id.code, None).await.unwrap_err(),
        ConnectError::RateLimited
    );
    assert_eq!(broker.stats().1, 3);
    assert_eq!(broker.failure_count(ip), 0);
    // 被控端没有再收到请求：下一条消息是它自己换密钥的回答
    write_msg(&mut host.send, &ClientMsg::Rekey { secret: id.secret })
        .await
        .unwrap();
    assert!(matches!(next(&mut host).await, ServerMsg::Rekeyed { .. }));

    assert!(ask(&mut second, 2, other.code, None).await.is_ok());
    assert!(matches!(
        next(&mut other_host).await,
        ServerMsg::Incoming { .. }
    ));
}

/// 限速只看窗口之内的：过了窗口就不算了；不同的来源、不同的控制码各算各的
#[test]
fn limiter_counts_per_key_within_its_window() {
    let limiter = Limiter::new(Rank::Connects, Duration::from_millis(100));
    let ip: std::net::IpAddr = "203.0.113.9".parse().unwrap();
    limiter.hit((ip, 123_456));
    limiter.hit((ip, 123_456));
    assert_eq!(limiter.count((ip, 123_456)), 2);
    assert_eq!(limiter.count((ip, 123_457)), 0);
    assert_eq!(limiter.count(("203.0.113.10".parse().unwrap(), 123_456)), 0);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(limiter.count((ip, 123_456)), 0);
}

#[tokio::test]
async fn connections_are_limited_per_network() {
    let (broker, cfg) = start_cfg(Config {
        max_conns_per_ip: 2,
        ..test_config(None)
    })
    .await;
    let a = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    let _b = connect_signal(&cfg, Role::Controller, None).await.unwrap();
    assert!(connect_signal(&cfg, Role::Controller, None).await.is_err());
    a.conn.close(VarInt::from_u32(0), b"");
    drop(a);
    wait_until("a connection slot is free", || {
        broker.conns.lock().values().sum::<usize>() < 2
    })
    .await;
    assert!(connect_signal(&cfg, Role::Controller, None).await.is_ok());
}

/// 固定的控制码：服务器重启（只剩数据库里的绑定）之后，只有原来的被控端能取回，别人拿不走；
/// 被控端放弃（Release）后解除绑定，也不再保留
#[tokio::test]
async fn fixed_codes_are_never_handed_out() {
    let (dir, file) = temp_db();
    let (old, cfg) = start_with(Some(file.clone())).await;
    let (host, id) = register_with(&cfg, None, true).await;
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || old.stats().0 == 0).await;
    old.flush_bindings().await;
    drop((host, old));

    let (broker, cfg) = start_with(Some(file.clone())).await;
    assert_eq!(broker.stats().2, 1);
    // 冒领：知道控制码、不知道密钥
    let thief = Reclaim {
        code: id.code,
        secret: [0xEE; 16],
    };
    let (_thief, got) = register(&cfg, Some(thief)).await;
    assert_ne!(got.code, id.code);
    // 原来的被控端
    let (mut host, again) = register_with(&cfg, Some(id), true).await;
    assert_eq!(again, id);

    // 放弃控制码：解除绑定，断开后不保留
    write_msg(&mut host.send, &ClientMsg::Release)
        .await
        .unwrap();
    wait_until("the binding is removed", || broker.stats().2 == 0).await;
    host.conn.close(VarInt::from_u32(0), b"");
    wait_until("the host is gone", || broker.stats().0 == 1).await;
    assert!(!broker.reserved.lock().contains_key(&id.code));
    // Persistence is asynchronous; await the disk barrier outside broker locks.
    broker.flush_bindings().await;
    assert!(!Bindings::open(Some(&file)).unwrap().contains(id.code));
    let _ = std::fs::remove_dir_all(dir);
}

/// 直连：两端在服务器之外打通一条路，画面换到直连上（控制端的统计里 direct 为 true），控制消息照常；
/// 直连突然断了（被控端的 socket 只往服务器发）：画面马上改回中转、要一个关键帧，控制端不再等断在直连上的帧，
/// 画面接着来；直连恢复之后再换回去。整个过程中画面的序号只增不减
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_takes_the_direct_path_and_falls_back_to_the_relay() {
    use airhand_link::host::{Acks, DirectHub, HostLink};
    use airhand_proto::media::{self, VideoHeader};
    use airhand_proto::session::{Codec, HostMsg, Notice};
    use std::sync::atomic::{AtomicBool, AtomicU32};

    struct Ui {
        packets: mpsc::UnboundedSender<Vec<u8>>,
        direct: watch::Sender<Option<bool>>,
        notices: mpsc::UnboundedSender<String>,
    }
    impl SessionUi for Ui {
        fn packet(&self, p: Vec<u8>) -> Result<(), UiClosed> {
            self.packets.send(p).map_err(|_| UiClosed)
        }
        fn event(&self, e: SessionEvent) -> Result<(), UiClosed> {
            match e {
                SessionEvent::Stats { direct, frames, .. } if frames > 0 => {
                    self.direct.send_replace(Some(direct));
                }
                SessionEvent::Notice { notice } => {
                    let _ = self.notices.send(format!("{notice:?}"));
                }
                _ => {}
            }
            Ok(())
        }
    }
    /// 平台的码率控制：记下换路
    #[derive(Default)]
    struct Paths {
        lost: AtomicU32,
        changed: AtomicU32,
    }
    impl Acks for Paths {
        fn ack(&self, _: u32, _: u64) {}
        fn path_changed(&self, lost: bool) {
            self.changed.fetch_add(1, Ordering::Relaxed);
            if lost {
                self.lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let (_broker, cfg) = start().await;
    let relay: SocketAddr = cfg.server.parse().unwrap();
    let (mut host, id) = register(&cfg, None).await;
    let (hub, _hub_task) = DirectHub::start(&host.endpoint, &host.punch, &host.conn).unwrap();
    let (router, _router) = Router::start(&host.conn);
    let svc = ControllerService::start(
        Arc::new(cfg.clone()),
        tokio::runtime::Handle::current(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        Identity::generate(),
    );
    let (packets, mut received) = mpsc::unbounded_channel();
    let (direct_tx, mut direct) = watch::channel(None);
    let (notices, mut notice_rx) = mpsc::unbounded_channel();
    let ui = Arc::new(Ui {
        packets,
        direct: direct_tx,
        notices,
    });
    let code = id.code.to_string();
    let connecting = async {
        loop {
            match svc
                .connect(
                    &code,
                    ConnectOptions::default(),
                    ui.clone(),
                    Frontend::Browser,
                )
                .await
            {
                Err(Problem::NotConnected) => tokio::time::sleep(Duration::from_millis(20)).await,
                r => break r.unwrap(),
            }
        }
    };
    let gate = OpenGate(Identity::generate());
    let host_endpoint = host.endpoint.local_addr().unwrap();
    let accepting = async {
        let msg = next(&mut host).await;
        let ServerMsg::Incoming { observed, .. } = msg else {
            panic!("expected Incoming");
        };
        // 服务器告诉被控端它自己的公网地址（在这里就是 127.0.0.1 上连服务器的那个端口）
        assert_eq!(observed.port(), host_endpoint.port());
        let (mut inc, kick) = incoming(&host, &router, id.code, msg);
        inc.direct = Some(HostLink {
            hub: hub.clone(),
            observed,
        });
        let (io, _) = HostStreams::open(&inc, 4, 16, &gate).await.unwrap();
        (inc.session_id, io, kick)
    };
    let (info, (session_id, mut io, _kick)) = tokio::join!(connecting, accepting);
    let paths = Arc::new(Paths::default());
    io.incoming.set_acks(paths.clone());
    // 会话开始了（发了 Hello）：开始建直连
    io.started().notify_one();

    // 被控端：每 20 ms 一帧，控制端要关键帧时下一帧是关键帧；每 200 ms 发一条提示（控制消息）
    let keyframes = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let (keyframes, stop) = (keyframes.clone(), stop.clone());
        tokio::spawn(async move {
            let mut seq = 0u32;
            let mut key = true;
            let mut tick = tokio::time::interval(Duration::from_millis(20));
            while !stop.load(Ordering::Relaxed) {
                tokio::select! {
                    m = io.incoming.recv() => match m {
                        Some(Request::Keyframe(_)) => {
                            keyframes.fetch_add(1, Ordering::Relaxed);
                            key = true;
                        }
                        Some(_) => {}
                        None => break,
                    },
                    _ = tick.tick() => {
                        seq += 1;
                        let header = VideoHeader {
                            seq,
                            key: std::mem::take(&mut key),
                            codec: Codec::H264,
                            width: 8,
                            height: 8,
                            capture_us: 0,
                        }
                        .encode();
                        io.video
                            .send(media::frame(&header, &[&[0; 600]]).into())
                            .await
                            .unwrap();
                        if seq.is_multiple_of(10) {
                            io.out
                                .send(&HostMsg::Notice(Some(Notice::CaptureFailed {
                                    detail: seq.to_string(),
                                })))
                                .unwrap();
                        }
                    }
                }
            }
            io.close(session_id, Ok(())).await
        })
    };
    // 控制端：每一帧都确认，序号只增不减
    let svc = Arc::new(svc);
    let frames = Arc::new(AtomicU32::new(0));
    let checker = {
        let (svc, frames, sid) = (svc.clone(), frames.clone(), info.sid);
        tokio::spawn(async move {
            let mut last = 0u32;
            while let Some(p) = received.recv().await {
                let Some(h) = VideoHeader::decode(&p) else {
                    continue;
                };
                assert!(h.seq > last, "video went backwards: {} after {last}", h.seq);
                last = h.seq;
                frames.fetch_add(1, Ordering::Relaxed);
                let _ = svc.ack(sid, h.seq);
            }
        })
    };
    let wait_for = async |direct: &mut watch::Receiver<Option<bool>>, want: bool, secs: u64| {
        timeout(
            Duration::from_secs(secs),
            direct.wait_for(|d| *d == Some(want)),
        )
        .await
        .unwrap_or_else(|_| panic!("video didn't switch to direct={want} in {secs} s"))
        .unwrap();
    };

    // 本机上直连比中转快：画面换到直连上
    wait_for(&mut direct, true, 10).await;
    assert_eq!(paths.lost.load(Ordering::Relaxed), 0);
    assert!(paths.changed.load(Ordering::Relaxed) >= 1);
    // 控制消息（经两条路合并）照常到
    let before = notice_rx.len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(notice_rx.len() > before);

    // 直连断了：被控端只往服务器发包
    host.punch.only_to(Some(relay));
    wait_for(&mut direct, false, 6).await;
    assert_eq!(paths.lost.load(Ordering::Relaxed), 1);
    assert!(
        keyframes.load(Ordering::Relaxed) >= 1,
        "no keyframe after losing the direct path"
    );
    // 画面在中转上接着来
    let at = keyframes.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_millis(500)).await;
    while notice_rx.try_recv().is_ok() {}
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(notice_rx.try_recv().is_ok(), "control messages stopped");
    assert_eq!(*direct.borrow(), Some(false));

    // 直连恢复了：过一会儿换回去
    host.punch.only_to(None);
    wait_for(&mut direct, true, 15).await;
    assert!(keyframes.load(Ordering::Relaxed) >= at);

    stop.store(true, Ordering::Relaxed);
    pump.await.unwrap().unwrap();
    // 检查的任务只在出错（序号倒退）时才会结束
    if checker.is_finished() {
        checker.await.unwrap();
    } else {
        checker.abort();
    }
    let frames = frames.load(Ordering::Relaxed);
    assert!(frames > 100, "{frames}");
    drop(svc);
}
