//! 端到端测试：真实服务器 + 真实被控端（屏幕采集 + 编码）+ 协议级控制端。
//!
//! 先在另一个终端启动服务器（不固定证书）：
//! ```text
//! AIRHAND_CERT=target/e2e/cert.pem AIRHAND_KEY=target/e2e/key.pem cargo run -p airhand-server
//! ```
//! 然后运行：`cargo test --lib e2e -- --ignored --nocapture`

use std::time::{Duration, Instant};

use airhand_link::secure::{self, FrameSealer, Identity, InitiatorParams};
use airhand_link::transport::{FromHost, HostReceiver, Openers, open_session, session_datagrams};
use airhand_link::wire::{read_msg, write_msg};
use airhand_proto::handshake::TAG_LEN;
use airhand_proto::media::{self, AudioHeader, VideoHeader};
use airhand_proto::session::{
    ByeReason, Codec, ControllerMsg, HostMsg, InputEvent, Notice, QualitySettings,
};
use airhand_proto::signal::{ClientMsg, ConnectError, ServerMsg, connect_outcome};
use airhand_proto::{MAX_SIGNAL_MSG, Role};

use airhand_link::ServerState;

use crate::host::HostService;
use crate::status;

/// 控制流（或者大块通道）的发送端：加密之后写进去
struct Ctl {
    send: quinn::SendStream,
    sealer: FrameSealer,
}

impl Ctl {
    async fn send(&mut self, msg: &ControllerMsg) {
        let mut f = airhand_proto::framing::encode_with_spare(msg, TAG_LEN);
        self.sealer.seal_frame(&mut f).unwrap();
        self.send.write_all(&f).await.unwrap();
    }
}

/// 被控端的控制码和配对码（测试里没有配置文件：默认要配对码）
async fn wait_code(hub: &status::StatusHub) -> u32 {
    wait_host(hub).await.0
}

async fn wait_host(hub: &status::StatusHub) -> (u32, Option<String>) {
    let mut rx = hub.subscribe();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        {
            let st = rx.borrow_and_update();
            if let (ServerState::Connected, Some(code)) = (&st.host.server, st.host.code) {
                return (code, st.host.pairing.clone());
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "被控端未能注册");
        let _ = tokio::time::timeout(left, rx.changed()).await;
    }
}

struct Report {
    hello: bool,
    encoders: Vec<(Codec, String, bool)>,
    frames: u32,
    keyframes: u32,
    bytes: usize,
    pongs: u32,
    cursor: u32,
    decoded: u32,
    /// 收到的声音包
    audio: u32,
    audio_starts: u32,
    /// 解码后的声音 RMS
    audio_rms: f64,
    /// 关闭声音之后仍然收到的包
    audio_late: u32,
    /// 收到时距采集时间多久（被控端时钟，毫秒）：最大值
    video_age_ms: f64,
    audio_age_ms: f64,
    /// 被控端报告缺少屏幕录制权限（macOS）：没有画面和声音
    no_screen_permission: bool,
    /// 收到的"采集失败"提示
    capture_failed: u32,
    /// 收到的"无法采集声音"提示
    audio_failed: u32,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e2e_roundtrip() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn")
        .try_init();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone());
    let (code, pairing) = wait_host(&hub).await;
    println!("被控端控制码: {code}，配对码: {pairing:?}");

    // 控制端：连接服务器
    let mut sig = airhand_link::net::connect_signal(&crate::config::link(), Role::Controller)
        .await
        .expect("控制端连接服务器");
    let mut buf = Vec::new();

    // 错误控制码
    let wrong = if code == 999_999 { 100_000 } else { code + 1 };
    write_msg(
        &mut sig.send,
        &ClientMsg::Connect {
            req_id: 1,
            code: wrong,
            resume: None,
        },
    )
    .await
    .unwrap();
    let r: ServerMsg = read_msg(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            r,
            ServerMsg::ConnectResult {
                req_id: 1,
                grant: None,
                error: Some(ConnectError::NotFound)
            }
        ),
        "{r:?}"
    );

    // 正确控制码
    write_msg(
        &mut sig.send,
        &ClientMsg::Connect {
            req_id: 2,
            code,
            resume: None,
        },
    )
    .await
    .unwrap();
    let r: ServerMsg = read_msg(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf)
        .await
        .unwrap()
        .unwrap();
    let ServerMsg::ConnectResult {
        req_id: 2,
        grant,
        error,
    } = r
    else {
        panic!("{r:?}")
    };
    let grant = connect_outcome(grant, error).expect("服务器批准了连接");
    println!("会话已授权: {} ({})", grant.host_name, grant.host_os);

    let streams = open_session(&sig.conn, grant.session_id, Role::Controller)
        .await
        .unwrap();
    let (send, mut recv) = streams.control;
    let mut send = send;
    // 端到端握手：第一次连，用配对码（测试里的被控端默认要配对码）
    let me = Identity::generate();
    // 和真正的控制端一样：开始时的画质和声音在握手里（被控端一开始就按它建编码器、采集声音）
    let settings = QualitySettings {
        codec: Codec::H264,
        fps: 30,
        ..Default::default()
    };
    let intro = secure::controller_intro("e2e", "test", settings, true);
    let init = secure::initiate(
        &mut recv,
        &mut send,
        InitiatorParams {
            session_id: grant.session_id,
            code,
            me: &me,
            host_key: None,
            pairing: pairing.as_deref(),
            intro: &intro,
        },
    )
    .await
    .expect("端到端握手");
    println!("端到端握手完成，安全码 {}", init.secured.safety);
    // 声音走数据报（放不进一个数据报的才走声音流）
    let datagrams = session_datagrams(&sig.conn, grant.session_id);
    // 控制流的发送端接着握手里的确认往下数
    let mut ctl = Ctl {
        send,
        sealer: init.sealer,
    };
    let (bulk_send, bulk_recv) = streams.bulk;
    // 剪贴板走大块通道
    let mut bulk = Ctl {
        send: bulk_send,
        sealer: init.secured.bulk.sealer,
    };
    let mut recv = HostReceiver::new(
        recv,
        bulk_recv,
        streams.video,
        streams.audio,
        Some(datagrams),
        Openers {
            bulk: init.secured.bulk.opener,
            video: init.secured.video.opener,
            audio: init.secured.audio.opener,
            datagrams: init.secured.datagram_opener,
            control: init.opener,
        },
    );
    ctl.send(&ControllerMsg::Input {
        t: 1,
        events: vec![InputEvent::ReleaseAll],
    })
    .await;
    // 声音（握手里已经打开了）：在本机以 -60 dBFS（几乎听不见）播放测试音，被控端环回采集后应能收到并解码
    let tone = match crate::host::audio::TonePlayer::start(440.0, 0.001) {
        Ok(t) => Some(t),
        Err(e) => {
            println!("无法播放测试音（没有声音输出设备？），跳过声音检查: {e:#}");
            None
        }
    };
    let mut audio_dec = opus::Decoder::new(48_000, opus::Channels::Stereo).unwrap();
    let mut pcm = vec![0f32; 5760 * 2];
    let (mut audio_energy, mut audio_samples) = (0f64, 0usize);
    let mut audio_off: Option<Instant> = None;

    let mut rep = Report {
        hello: false,
        encoders: vec![],
        frames: 0,
        keyframes: 0,
        bytes: 0,
        pongs: 0,
        cursor: 0,
        decoded: 0,
        audio: 0,
        audio_starts: 0,
        audio_rms: 0.0,
        audio_late: 0,
        video_age_ms: 0.0,
        audio_age_ms: 0.0,
        no_screen_permission: false,
        capture_failed: 0,
        audio_failed: 0,
    };
    // 被控端在同一进程里，可以直接读它的时钟：画面和声音的采集时间戳都应落在"刚刚"
    let age_ms = |capture_us: u64| {
        (crate::host::clock::us(Instant::now()) as f64 - capture_us as f64) / 1000.0
    };
    let mut decoder = openh264::decoder::Decoder::new().unwrap();
    let mut switched = false;
    // 剪贴板：控制端 → 被控端（写入本机剪贴板），然后本机修改 → 被控端应回传
    let original_clip = crate::clipboard::get_text().ok();
    let to_host = format!("airhand-e2e-to-host-{}", rand::random::<u32>());
    let from_host = format!("airhand-e2e-from-host-{}", rand::random::<u32>());
    bulk.send(&ControllerMsg::Clipboard {
        text: to_host.clone(),
    })
    .await;
    let mut clip_written = false;
    let mut clip_echo = 0;
    let mut clip_back = false;
    let started = Instant::now();
    let mut last_ping = Instant::now() - Duration::from_secs(1);

    while started.elapsed() < Duration::from_secs(8) {
        if last_ping.elapsed() > Duration::from_millis(500) {
            last_ping = Instant::now();
            ctl.send(&ControllerMsg::Ping { t: 1 }).await;
        }
        if !clip_written && started.elapsed() > Duration::from_secs(1) {
            let now = crate::clipboard::get_text().unwrap_or_default();
            assert_eq!(now, to_host, "被控端没有写入剪贴板");
            clip_written = true;
            crate::clipboard::set_text(&from_host).unwrap();
        }
        // 3 秒后切换到 AV1/HEVC（若被控端支持），验证运行时切换编码器
        if !switched && started.elapsed() > Duration::from_secs(3) {
            switched = true;
            let s = QualitySettings {
                codec: Codec::AV1,
                fps: 60,
                bitrate_kbps: 8000,
                ..settings
            };
            ctl.send(&ControllerMsg::Settings(s)).await;
        }
        // 6 秒后关闭声音：之后不应再收到声音包
        if audio_off.is_none() && started.elapsed() > Duration::from_secs(6) {
            audio_off = Some(Instant::now());
            ctl.send(&ControllerMsg::Audio { enabled: false }).await;
        }
        let msg = match tokio::time::timeout(Duration::from_secs(1), recv.recv()).await {
            Ok(m) => m.expect("读取会话消息"),
            Err(_) => continue, // 画面静止时不会有帧
        };
        let msg = match msg {
            FromHost::Video(p) => {
                let v = VideoHeader::decode(&p).expect("画面包的头");
                let data = media::payload(&p).expect("画面包的负载");
                let age = age_ms(v.capture_us);
                assert!(age >= 0.0, "画面时间戳在未来: {age} ms");
                rep.video_age_ms = rep.video_age_ms.max(age);
                rep.frames += 1;
                rep.keyframes += v.key as u32;
                rep.bytes += data.len();
                if rep.frames == 1 {
                    assert!(v.key, "第一帧必须是关键帧");
                }
                if v.codec == Codec::H264 && matches!(decoder.decode(data), Ok(Some(_))) {
                    rep.decoded += 1;
                }
                ctl.send(&ControllerMsg::Ack { seq: v.seq }).await;
                continue;
            }
            FromHost::Audio(p) => {
                let a = AudioHeader::decode(&p).expect("声音包的头");
                let age = age_ms(a.capture_us);
                assert!(age >= 0.0, "声音时间戳在未来: {age} ms");
                rep.audio_age_ms = rep.audio_age_ms.max(age);
                if audio_off.is_some_and(|t| t.elapsed() > Duration::from_millis(300)) {
                    rep.audio_late += 1;
                }
                rep.audio += 1;
                rep.audio_starts += a.start as u32;
                let n = audio_dec
                    .decode_float(media::payload(&p).unwrap(), &mut pcm, false)
                    .expect("Opus 解码");
                // 跳过开头几包（播放刚启动、编码器收敛）
                if rep.audio > 10 {
                    audio_energy += pcm[..n * 2]
                        .iter()
                        .map(|s| (*s as f64).powi(2))
                        .sum::<f64>();
                    audio_samples += n * 2;
                }
                continue;
            }
            FromHost::Control(m) => m,
        };
        match msg {
            HostMsg::Hello(h) => {
                rep.hello = true;
                // 桌面被控端：报告自己的平台，不接收触摸、没有手机那样的系统操作
                assert!(!h.touch && h.actions.is_empty(), "{h:?}");
                assert_ne!(h.platform, airhand_proto::session::Platform::Android);
                println!(
                    "Hello: {} / {} / 显示器 {} / 编码 {:?}",
                    h.hostname,
                    h.os,
                    h.displays.len(),
                    h.codecs
                );
            }
            HostMsg::Encoder(e) => {
                println!(
                    "编码器: {} {:?} {}x{} hw={}",
                    e.name, e.codec, e.width, e.height, e.hardware
                );
                rep.encoders.push((e.codec, e.name, e.hardware));
            }
            HostMsg::Pong { .. } => rep.pongs += 1,
            HostMsg::Cursor(_) | HostMsg::CursorUpdate { .. } => rep.cursor += 1,
            HostMsg::CursorImage { .. } => {}
            HostMsg::Clipboard { text } => {
                if text == to_host {
                    clip_echo += 1;
                } else if text == from_host {
                    clip_back = true;
                }
            }
            HostMsg::Bye(reason) => panic!("意外的 Bye: {reason:?}"),
            HostMsg::Displays { .. } => {}
            HostMsg::AwaitingApproval => panic!("桌面被控端不会请本机用户同意"),
            HostMsg::Notice(notice) => {
                println!("提示: {notice:?}");
                match notice.expect("同一个版本的提示都认识") {
                    Notice::MissingPermissions { screen, .. } => rep.no_screen_permission = screen,
                    Notice::CaptureFailed { .. } => rep.capture_failed += 1,
                    Notice::AudioFailed { .. } => rep.audio_failed += 1,
                    _ => {}
                }
            }
        }
    }
    let tone_ok = tone.is_some();
    drop(tone);
    rep.audio_rms = (audio_energy / audio_samples.max(1) as f64).sqrt();
    if let Some(t) = original_clip {
        let _ = crate::clipboard::set_text(&t);
    }
    assert!(clip_back, "本机剪贴板变化没有同步给控制端");
    assert_eq!(clip_echo, 0, "剪贴板出现回声");

    // 托盘"断开所有控制端"：控制端应收到 Bye
    _host.send(crate::host::HostCmd::KickAll);
    let bye = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match recv.recv().await {
                Ok(FromHost::Control(HostMsg::Bye(reason))) => break Some(reason),
                Ok(_) => continue,
                _ => break None,
            }
        }
    })
    .await
    .ok()
    .flatten();
    assert_eq!(
        bye,
        Some(ByeReason::Kicked),
        "没有收到\"断开所有控制端\"的 Bye"
    );
    let _ = ctl.send.finish();
    println!(
        "帧 {} (关键帧 {}) · {:.1} KB · H.264 解码成功 {} · pong {} · 光标 {} · 声音包 {} (start {}, RMS {:.6}, 关闭后 {})",
        rep.frames,
        rep.keyframes,
        rep.bytes as f64 / 1024.0,
        rep.decoded,
        rep.pongs,
        rep.cursor,
        rep.audio,
        rep.audio_starts,
        rep.audio_rms,
        rep.audio_late,
    );
    // macOS 上运行测试的终端没有"屏幕与系统录音"权限：被控端应当用专门的提示说明原因
    //（而不是报告为采集失败），画面和声音的检查跳过
    let capture = !rep.no_screen_permission;
    if !capture {
        println!("被控端没有屏幕录制权限，跳过画面和声音的检查");
        assert_eq!(rep.capture_failed, 0, "缺少权限不应报告为采集失败");
        assert_eq!(rep.audio_failed, 0, "缺少权限不应报告为无法采集声音");
        assert_eq!(rep.frames, 0);
    }
    if tone_ok && capture {
        assert_eq!(rep.audio_failed, 0, "能采集到声音时不应报告失败");
        // 测试音持续播放 6 秒，每包 20 ms：应收到约 300 包
        assert!(rep.audio >= 150, "声音包太少: {}", rep.audio);
        // 环回采集的是音量调节之后的声音，电平取决于系统音量（-60 dBFS 的测试音在 100% 音量时 RMS ≈ 7e-4）
        assert!(
            rep.audio_rms > 1e-5,
            "声音能量异常（系统音量是否为 0？）: {}",
            rep.audio_rms
        );
        assert_eq!(rep.audio_late, 0, "关闭声音后仍在发送");
        println!(
            "采集到收到：画面最多 {:.1} ms，声音最多 {:.1} ms",
            rep.video_age_ms, rep.audio_age_ms
        );
        assert!(
            rep.audio_age_ms < 500.0,
            "声音时间戳与画面不在同一个时钟上？{:.1} ms",
            rep.audio_age_ms
        );
    }
    assert!(rep.hello, "没有收到 Hello");
    assert!(rep.pongs >= 3, "Ping/Pong 不工作");
    if capture {
        assert!(rep.frames >= 2, "收到的视频帧太少");
        assert!(rep.keyframes >= 1);
        assert!(!rep.encoders.is_empty());
        assert!(rep.encoders.len() >= 2, "切换编码格式后应重建编码器");
        assert!(rep.decoded >= 1, "H.264 码流无法解码");
        // 画面静止时会重新编码上一帧（时间戳不变），所以只检查不离谱
        assert!(
            rep.video_age_ms < 10_000.0,
            "画面时间戳异常: {:.1} ms",
            rep.video_age_ms
        );
    }

    // 会话结束后被控端应释放会话
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        hub.borrow().host.controllers.is_empty(),
        "会话没有被清理: {:?}",
        hub.borrow().host.controllers
    );
}

/// 服务器重启后，被控端应自动重连并取回原控制码。会自行启动/重启服务器进程。
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e2e_reconnect_keeps_code() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn")
        .try_init();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let exe = root.join(format!(
        "target/debug/airhand-server{}",
        std::env::consts::EXE_SUFFIX
    ));
    let spawn = || {
        std::process::Command::new(&exe)
            .env("AIRHAND_LISTEN", "127.0.0.1:4433")
            .env("AIRHAND_CERT", root.join("target/e2e/cert.pem"))
            .env("AIRHAND_KEY", root.join("target/e2e/key.pem"))
            .env("RUST_LOG", "warn")
            .spawn()
            .expect("启动服务器（先 cargo build -p airhand-server）")
    };
    let mut server = spawn();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone());
    let code = wait_code(&hub).await;
    println!("初始控制码 {code}");

    server.kill().unwrap();
    let _ = server.wait();
    let t = Instant::now();
    let mut rx = hub.subscribe();
    while matches!(rx.borrow_and_update().host.server, ServerState::Connected) {
        let _ = tokio::time::timeout(Duration::from_secs(20), rx.changed()).await;
        assert!(t.elapsed() < Duration::from_secs(25), "没有检测到掉线");
    }
    println!(
        "检测到掉线，用时 {:.1}s: {:?}",
        t.elapsed().as_secs_f32(),
        hub.borrow().host.server
    );

    let mut server = spawn();
    let t = Instant::now();
    let code2 = wait_code(&hub).await;
    println!(
        "重连成功，用时 {:.1}s，控制码 {code2}",
        t.elapsed().as_secs_f32()
    );
    assert_eq!(code, code2, "重连后控制码应保持不变");

    // 托盘"更换控制码"
    _host.send(crate::host::HostCmd::NewCode);
    let t = Instant::now();
    let code3 = loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let st = hub.borrow().clone();
        if let (ServerState::Connected, Some(c)) = (&st.host.server, st.host.code)
            && c != code2
        {
            break c;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "更换控制码超时");
    };
    println!(
        "已更换控制码 {code2} -> {code3}，用时 {:.1}s",
        t.elapsed().as_secs_f32()
    );
    server.kill().unwrap();
    let _ = server.wait();
}
