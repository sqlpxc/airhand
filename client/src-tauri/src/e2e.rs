//! 端到端测试：真实服务器 + 真实被控端（屏幕采集 + 编码）+ 协议级控制端。
//!
//! 先在另一个终端启动服务器（不固定证书）：
//! ```text
//! AIRHAND_CERT=target/e2e/cert.pem AIRHAND_KEY=target/e2e/key.pem cargo run -p airhand-server
//! ```
//! 然后运行：`cargo test --lib e2e -- --ignored --nocapture`

use std::sync::Arc;
use std::time::{Duration, Instant};

use airhand_link::controller::{Established, FromHost, establish};
use airhand_link::host::Router;
use airhand_link::merge::Video;
use airhand_link::net::Signal;
use airhand_link::secure::{self, Identity, InitiatorParams, Sealer};
use airhand_link::wire::{read_msg, write_msg};
use airhand_proto::handshake::SEAL_OVERHEAD;
use airhand_proto::media::{self, AudioHeader, VideoHeader};
use airhand_proto::session::{
    ByeReason, Codec, ControllerDatagram, ControllerMsg, HostMsg, InputEvent, MicState, Notice,
    QualitySettings,
};
use airhand_proto::signal::{ClientMsg, ConnectError, ServerMsg, connect_outcome};
use airhand_proto::{MAX_SIGNAL_MSG, Role};

use airhand_link::ServerState;

use crate::host::HostService;
use crate::status;

/// 控制通道（或者大块通道）的发送端：加密之后写进去
struct Ctl {
    send: quinn::SendStream,
    sealer: Sealer,
}

impl Ctl {
    async fn send(&mut self, msg: &ControllerMsg) {
        let mut f = airhand_proto::framing::encode_with_spare(msg, SEAL_OVERHEAD);
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
    /// 被控端说的麦克风状态（同一个版本：都懂麦克风）
    mic: Option<MicState>,
}

/// 用配对码连上被控端 `code`（请求编号 `req_id`），做完端到端握手。和真正的控制端一样：开始时的画质和声音在握手里
async fn open_session(
    sig: &mut Signal,
    router: &Arc<Router>,
    code: u32,
    pairing: Option<&str>,
    settings: QualitySettings,
    audio: bool,
    req_id: u32,
) -> Established {
    let me = Identity::generate();
    let intro = secure::controller_intro("e2e", "test", settings, audio, Vec::new());
    let (initiator, init) = secure::initiate(InitiatorParams {
        code,
        me: &me,
        host_key: None,
        pairing,
        intro: &intro,
    })
    .unwrap();
    write_msg(
        &mut sig.send,
        &ClientMsg::Connect {
            req_id,
            code,
            resume: None,
            init,
        },
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    let r: ServerMsg = read_msg(&mut sig.recv, MAX_SIGNAL_MSG, &mut buf)
        .await
        .unwrap()
        .unwrap();
    let ServerMsg::ConnectResult {
        req_id: got,
        grant,
        error,
    } = r
    else {
        panic!("{r:?}")
    };
    assert_eq!(got, req_id);
    let grant = connect_outcome(grant, error).expect("服务器批准了连接");
    println!("会话已授权: {} ({})", grant.host_name, grant.host_os);
    // 在被控端开过来的控制通道上做完握手
    let link = establish(router, &sig.conn, grant.session_id, initiator)
        .await
        .expect("端到端握手");
    println!("端到端握手完成，安全码 {}", link.safety);
    link
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
    let mut sig = airhand_link::net::connect_signal(&crate::config::link(), Role::Controller, None)
        .await
        .expect("控制端连接服务器");
    // 被控端开过来的通道、发来的数据报按会话交给会话
    let (router, _router) = Router::start(&sig.conn);
    let mut buf = Vec::new();

    // 错误控制码
    let wrong = if code == 999_999 { 100_000 } else { code + 1 };
    write_msg(
        &mut sig.send,
        &ClientMsg::Connect {
            req_id: 1,
            code: wrong,
            resume: None,
            init: Vec::new(),
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

    // 正确控制码。端到端握手的第一条消息随请求一起发：第一次连，用配对码（测试里的被控端默认要配对码）。
    // 开始时就要声音，被控端按握手里的画质建立编码器。
    let settings = QualitySettings {
        codec: Codec::H264,
        fps: 30,
        ..Default::default()
    };
    let link = open_session(
        &mut sig,
        &router,
        code,
        pairing.as_deref(),
        settings,
        true,
        2,
    )
    .await;
    // 控制通道的发送端接着握手里的确认往下数
    let mut ctl = Ctl {
        send: link.control.0,
        sealer: link.control.1,
    };
    // 剪贴板走大块通道
    let mut bulk = Ctl {
        send: link.bulk.0,
        sealer: link.bulk.1,
    };
    // Ping、画面确认是控制数据报（这个协议级的控制端不建直连：只在中转上发）
    let datagrams = link.datagrams;
    let mut recv = link.receiver;
    let _registration = link.registration;
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
        mic: None,
    };
    // 被控端在同一进程里，可以直接读它的时钟：画面和声音的采集时间戳都应落在"刚刚"
    let age_ms = |capture_us: u64| {
        (crate::host::clock::us(Instant::now()) as f64 - capture_us as f64) / 1000.0
    };
    // 只用来确认关键帧解得出来：OpenH264 的解码器不支持硬件编码器那样用长期参考帧（见 airhand_link::refs），
    // 之后参考了长期参考帧的增量帧它解不出来（编码器层面的恢复由 mf.rs 的测试用 ffmpeg 检查）
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
            datagrams.send(&ControllerDatagram::Ping { t: 1 });
        }
        if !clip_written && started.elapsed() > Duration::from_secs(1) {
            let now = crate::clipboard::get_text().unwrap_or_default();
            assert_eq!(now, to_host, "被控端没有写入剪贴板");
            clip_written = true;
            crate::clipboard::set_text(&from_host).unwrap();
        }
        // 3 秒后改变编码尺寸并请求 AV1，验证运行时重建编码器（macOS 也要切换采集尺寸）
        if !switched && started.elapsed() > Duration::from_secs(3) {
            switched = true;
            let s = QualitySettings {
                codec: Codec::AV1,
                fps: 60,
                bitrate_kbps: 8000,
                max_short: 720,
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
            FromHost::Video {
                packet: p, arrived, ..
            } => {
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
                datagrams.send(&ControllerDatagram::Ack {
                    seq: v.seq,
                    arrived,
                    stream: v.stream,
                });
                continue;
            }
            // 画面通道上别的事（和控制端服务一样处理）：解不了的替界面确认、等不到了报告、等太久了要关键帧
            FromHost::VideoEvent(stream, ev) => {
                match ev {
                    Video::Skipped { seq, arrived } => datagrams.send(&ControllerDatagram::Ack {
                        seq,
                        arrived,
                        stream,
                    }),
                    Video::Lost { decoded } => {
                        datagrams.send(&ControllerDatagram::Lost { decoded, stream })
                    }
                    Video::Keyframe { after } => {
                        ctl.send(&ControllerMsg::RequestKeyframe { after, stream })
                            .await
                    }
                    Video::Frame(_) => {}
                }
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
            FromHost::Pong(_) => {
                rep.pongs += 1;
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
            HostMsg::Cursor { .. } => rep.cursor += 1,
            HostMsg::CursorImage { .. } => {}
            HostMsg::Clipboard { text } => {
                if text == to_host {
                    clip_echo += 1;
                } else if text == from_host {
                    clip_back = true;
                }
            }
            HostMsg::Bye(reason) => panic!("意外的 Bye: {reason:?}"),
            // 这个协议级的控制端不建直连（不回答，被控端就一直走中转）
            HostMsg::Direct(_) => {}
            // 剪贴板里的文件：这个测试不管（见 airhand_link::files 的测试、服务器的测试）
            HostMsg::Files(_) => {}
            HostMsg::Displays { .. } => {}
            HostMsg::Mic { state, detail } => {
                println!("麦克风: {state:?} {detail:?}");
                rep.mic = Some(state);
            }
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
    // 同一个版本的两端都懂麦克风：被控端会说它能不能接（Windows 看有没有 Steam Streaming Microphone）
    let mic = rep.mic.expect("被控端没有说麦克风的状态");
    assert_eq!(mic, crate::host::mic::probe(0).await.0);
    if capture {
        assert!(rep.frames >= 2, "收到的视频帧太少");
        assert!(rep.keyframes >= 1);
        assert!(!rep.encoders.is_empty());
        assert!(rep.encoders.len() >= 2, "切换编码格式后应重建编码器");
        assert!(rep.decoded >= 1, "H.264 的关键帧无法解码");
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

/// 多屏：同时看被控端的前两个显示器（每个显示器一路画面），再改回只看第二个。这台电脑只有一个显示器时只检查
/// 被控端说了最多能发几路、只看得到那一个，然后跳过。和 `e2e_roundtrip` 一样要先启动服务器
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn e2e_multi_display() {
    use airhand_proto::session::StreamView;
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn")
        .try_init();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone());
    let (code, pairing) = wait_host(&hub).await;
    let mut sig = airhand_link::net::connect_signal(&crate::config::link(), Role::Controller, None)
        .await
        .expect("控制端连接服务器");
    let (router, _router) = Router::start(&sig.conn);
    let settings = QualitySettings {
        codec: Codec::H264,
        fps: 30,
        ..Default::default()
    };
    let link = open_session(
        &mut sig,
        &router,
        code,
        pairing.as_deref(),
        settings,
        false,
        1,
    )
    .await;
    let mut ctl = Ctl {
        send: link.control.0,
        sealer: link.control.1,
    };
    let datagrams = link.datagrams;
    let mut recv = link.receiver;
    let _registration = link.registration;

    /// 每一路收到的帧：(第一帧是不是关键帧, 帧数, 最近一帧的宽高)
    #[derive(Default, Debug, Clone, Copy)]
    struct Got {
        first_key: Option<bool>,
        frames: u32,
        size: (u16, u16),
    }
    let mut got = [Got::default(); airhand_proto::MAX_STREAMS as usize];
    // 同时看两个显示器时收到的、被控端说的每一路是哪个显示器
    let mut multi: Option<([Got; airhand_proto::MAX_STREAMS as usize], Vec<u8>)> = None;
    let mut displays = Vec::new();
    let mut shown = Vec::new();
    let mut asked = 0u8;
    let started = Instant::now();
    let mut last_ping = Instant::now() - Duration::from_secs(1);
    let mut step_at = Instant::now();
    while started.elapsed() < Duration::from_secs(12) {
        if last_ping.elapsed() > Duration::from_millis(500) {
            last_ping = Instant::now();
            datagrams.send(&ControllerDatagram::Ping { t: 1 });
        }
        // Hello 之后看前两个显示器；4 秒之后改回只看第二个
        if asked == 1 && displays.len() >= 2 && step_at.elapsed() > Duration::from_secs(4) {
            asked = 2;
            step_at = Instant::now();
            multi = Some((got, shown.clone()));
            got = Default::default();
            ctl.send(&ControllerMsg::View {
                streams: vec![StreamView::display(1)],
            })
            .await;
        }
        let msg = match tokio::time::timeout(Duration::from_millis(500), recv.recv()).await {
            Ok(m) => m.expect("读取会话消息"),
            Err(_) => continue,
        };
        match msg {
            FromHost::Video {
                stream,
                packet,
                arrived,
                ..
            } => {
                let v = VideoHeader::decode(&packet).expect("画面包的头");
                assert_eq!(v.stream, stream);
                let g = &mut got[stream as usize];
                g.first_key.get_or_insert(v.key);
                g.frames += 1;
                g.size = (v.width, v.height);
                datagrams.send(&ControllerDatagram::Ack {
                    seq: v.seq,
                    arrived,
                    stream,
                });
            }
            FromHost::VideoEvent(stream, Video::Skipped { seq, arrived }) => {
                datagrams.send(&ControllerDatagram::Ack {
                    seq,
                    arrived,
                    stream,
                });
            }
            FromHost::VideoEvent(stream, Video::Lost { decoded }) => {
                datagrams.send(&ControllerDatagram::Lost { decoded, stream });
            }
            FromHost::VideoEvent(stream, Video::Keyframe { after }) => {
                ctl.send(&ControllerMsg::RequestKeyframe { after, stream })
                    .await;
            }
            FromHost::Control(HostMsg::Hello(h)) => {
                println!(
                    "显示器 {:?}，最多同时 {} 路",
                    h.displays
                        .iter()
                        .map(|d| (d.width, d.height))
                        .collect::<Vec<_>>(),
                    h.max_streams()
                );
                assert_eq!(h.max_streams(), airhand_proto::MAX_STREAMS);
                assert_eq!(h.shown, [h.current_display]);
                displays = h.displays;
                if displays.len() < 2 {
                    // 只有一个显示器：要看两个也只看得到一个
                    ctl.send(&ControllerMsg::View {
                        streams: vec![StreamView::display(0), StreamView::display(1)],
                    })
                    .await;
                    asked = 9;
                    continue;
                }
                asked = 1;
                step_at = Instant::now();
                ctl.send(&ControllerMsg::View {
                    streams: vec![
                        StreamView::display(0),
                        StreamView {
                            display: 1,
                            max_short: 720,
                        },
                    ],
                })
                .await;
            }
            FromHost::Control(HostMsg::Displays { shown: s, .. }) => {
                println!("每一路画面的显示器: {s:?}");
                shown = s;
            }
            FromHost::Control(HostMsg::Bye(reason)) => panic!("意外的 Bye: {reason:?}"),
            _ => {}
        }
        // 只有一个显示器时看一会儿就够了
        if (asked == 2 || asked == 9) && step_at.elapsed() > Duration::from_secs(3) {
            break;
        }
    }
    println!("{got:?}");
    if displays.len() < 2 {
        println!("这台电脑只有一个显示器，跳过多屏的检查");
        assert!(shown.is_empty() || shown == [0], "{shown:?}");
        assert!(got[1].frames == 0, "只有一个显示器却收到了第二路画面");
        return;
    }
    // 同时看两个：两路都从关键帧开始，第 1 路按它要的大小（短边不超过 720）
    let (both, both_shown) = multi.expect("没有改回只看一个");
    assert_eq!(both_shown, [0, 1]);
    for (stream, g) in both.iter().take(2).enumerate() {
        assert_eq!(g.first_key, Some(true), "第 {stream} 路");
        assert!(g.frames >= 1, "第 {stream} 路没有画面");
    }
    assert!(both[1].size.0.min(both[1].size.1) <= 720, "{:?}", both[1]);
    // 最后只看第二个显示器：第 0 路是它，从关键帧开始
    assert_eq!(shown, [1]);
    assert_eq!(got[0].first_key, Some(true));
    assert!(got[0].frames >= 1);
    let _ = ctl.send.finish();
}

/// 控制端的麦克风，从协议级的控制端一直到系统录音设备。每 20 ms 发一包 1 kHz 的声音，
/// 故意丢一部分数据报，再通过独立的录音客户端检查频率和连续性。先启动中转服务器。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a relay and the system audio server; on Windows may install the Steam driver"]
async fn e2e_mic() {
    use crate::host::mic::tests::goertzel;
    #[cfg(not(windows))]
    use crate::host::mic::unix_recorder::Recorder;
    #[cfg(windows)]
    use crate::host::mic::wasapi::tests::Recorder;
    use airhand_proto::datagram;

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info,quinn=warn,airhand_lib::host::mic=debug")
        .try_init();
    let hub = status::new_hub();
    let _host = HostService::start(hub.clone());
    let (code, pairing) = wait_host(&hub).await;
    let mut sig = airhand_link::net::connect_signal(&crate::config::link(), Role::Controller, None)
        .await
        .expect("控制端连接服务器");
    let (router, _router) = Router::start(&sig.conn);
    let link = open_session(
        &mut sig,
        &router,
        code,
        pairing.as_deref(),
        QualitySettings::default(),
        false,
        1,
    )
    .await;
    let mut ctl = Ctl {
        send: link.control.0,
        sealer: link.control.1,
    };
    let (session_id, mut mic_sealer, datagrams) = (link.session_id, link.mic, link.datagrams);
    let mut recv = link.receiver;
    let _registration = link.registration;

    // 和界面一样只发一次打开请求，缺驱动时等安装后报 ready，才开始发送声音。
    let mut enabled = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        assert!(Instant::now() < deadline, "等待虚拟麦克风就绪超时");
        datagrams.send(&ControllerDatagram::Ping { t: 1 });
        let Ok(Ok(FromHost::Control(HostMsg::Mic { state, detail }))) =
            tokio::time::timeout(Duration::from_millis(500), recv.recv()).await
        else {
            continue;
        };
        println!("麦克风: {state:?} {detail:?}");
        if state == MicState::SteamMissing && !enabled {
            println!("这台电脑上没有 Steam，跳过声音的检查");
            return;
        }
        assert!(
            matches!(
                state,
                MicState::Ready | MicState::NoDevice | MicState::Preparing
            ),
            "{state:?}: {detail:?}"
        );
        if !enabled {
            ctl.send(&ControllerMsg::Mic { enabled: true }).await;
            enabled = true;
            // Unix publishes its device on demand. Wait for the live lease's Ready.
            if !cfg!(windows) {
                continue;
            }
        }
        if state == MicState::Ready {
            break;
        }
    }
    let mut rec = Recorder::open().expect("从远端系统的虚拟麦克风录音");
    // 像界面的音频线程那样准时地每 20 ms 发一包（tokio 的定时器在 Windows 上可能晚 15 ms，发不够每秒 50 包）
    let conn = sig.conn.clone();
    let sender = std::thread::spawn(move || {
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Voip).unwrap();
        let (mut phase, mut opus_buf) = (0f64, vec![0u8; 1000]);
        let mut previous: Option<Vec<u8>> = None;
        let start = Instant::now();
        for seq in 1..=150u32 {
            let at = start + Duration::from_millis(20 * (seq as u64 - 1));
            std::thread::sleep(at.saturating_duration_since(Instant::now()));
            let pcm: Vec<f32> = (0..960)
                .map(|_| {
                    let v = phase.sin() as f32 * 0.5;
                    phase += 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
                    v
                })
                .collect();
            let len = enc.encode_float(&pcm, &mut opus_buf).unwrap();
            let h = AudioHeader {
                seq,
                start: seq == 1,
                capture_us: seq as u64 * 20_000,
            };
            let packet = media::frame(&h.encode(), &[&opus_buf[..len]])[4..].to_vec();
            // 和控制端一样带上上一包的副本；每 7 个数据报丢一个，下一个数据报里的副本把它补回来
            let plain = media::audio_datagram(&packet, previous.as_deref()).unwrap();
            previous = Some(packet);
            let d = mic_sealer
                .seal_datagram(session_id, datagram::AUDIO, &plain)
                .unwrap();
            if seq % 7 != 0 {
                conn.send_datagram(d.into()).unwrap();
            }
        }
    });
    // 被控端说的状态（打开设备之后还是 ready；打不开会说 failed）
    let (mut states, mut recorded) = (Vec::new(), Vec::new());
    let end = Instant::now() + Duration::from_millis(3_300);
    while Instant::now() < end {
        datagrams.send(&ControllerDatagram::Ping { t: 1 });
        rec.read(&mut recorded).unwrap();
        // 录音的缓冲只有 200 ms：每 20 ms 回来读一次
        let slice = Instant::now() + Duration::from_millis(20);
        while let Ok(Ok(m)) =
            tokio::time::timeout(slice.saturating_duration_since(Instant::now()), recv.recv()).await
        {
            if let FromHost::Control(HostMsg::Mic { state, detail }) = m {
                println!("麦克风: {state:?} {detail:?}");
                states.push(state);
            }
        }
    }
    sender.join().unwrap();
    rec.read(&mut recorded).unwrap();
    drop(rec);
    assert!(
        !states.contains(&MicState::Failed),
        "被控端打不开虚拟麦克风: {states:?}"
    );
    let voice: Vec<f32> = recorded
        .iter()
        .copied()
        .skip_while(|v| v.abs() < 0.05)
        .collect();
    let secs = voice.len() as f64 / 48_000.0;
    // 声音断没断：发了 3 秒，前 2.5 秒里每 10 ms 一段，没声音的段数（抖动缓冲断粮、丢了的包没补上）
    let windows: Vec<f32> = voice
        .chunks(480)
        .take(250)
        .map(|w| (w.iter().map(|v| v * v).sum::<f32>() / w.len() as f32).sqrt())
        .collect();
    let quiet = windows.iter().filter(|r| **r < 0.1).count();
    println!("前 {} 段 10 ms 里没声音的 {quiet} 段", windows.len());
    assert!(quiet <= 5, "声音断断续续：{quiet} 段没声音");
    let at = |f| goertzel(&voice, 48_000.0, f);
    let (tone, others) = (at(1000.0), [at(459.0), at(500.0), at(918.0), at(1088.0)]);
    println!("录到 {secs:.2} 秒声音：1 kHz {tone:.3}，459/500/918/1088 Hz {others:.4?}");
    assert!(secs > 2.0, "从虚拟麦克风录到的声音太少");
    assert!(tone > 0.1, "声音太小：{tone}");
    assert!(
        others.iter().all(|o| tone > o * 20.0),
        "声音变调了（虚拟麦克风两边的格式没对上？）"
    );

    // 关掉：被控端放开设备，还是 ready（能再打开）
    ctl.send(&ControllerMsg::Mic { enabled: false }).await;
    let off = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(FromHost::Control(HostMsg::Mic { state, .. })) = recv.recv().await {
                break state;
            }
        }
    })
    .await;
    // 状态没变（还是 ready）就不会再说一遍
    assert!(matches!(off, Err(_) | Ok(MicState::Ready)), "{off:?}");
    let _ = ctl.send.finish();
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
