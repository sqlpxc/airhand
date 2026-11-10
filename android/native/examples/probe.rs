//! 协议级的控制端（开发调试用）：连接一个控制码，统计收到的画面 / 声音，可选地发几个输入事件。
//! 用来在电脑上测试 Android 被控端，不需要图形界面：
//!
//! ```text
//! cargo run --example probe -- <控制码> [秒数] [--tap x,y] [--touch] [--pinch] [--action back]
//!                                [--text 文字] [--audio] [--server host:port] [--pairing 配对码]
//! ```
//!
//! 每次运行是一个新的控制端身份（端到端加密）：被控端要配对码的话用 `--pairing`。
//!
//! `--tap 0.5,0.5` 在画面中间点一下（归一化坐标，默认用鼠标左键，加 `--touch` 改用触摸）；
//! `--pinch` 在画面中间双指张开；`--action` 发系统操作；`--text` 打字；`--audio` 打开声音。
//! 计时从被控端同意（收到 Hello）开始。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airhand_android::rt::rt;
use airhand_link::controller::{
    Command, ConnectOptions, ControllerService, Frontend, SessionEvent, SessionUi, UiClosed,
};
use airhand_link::secure::Identity;
use airhand_link::{LinkConfig, ServerState};
use airhand_proto::media::{Kind, VideoHeader};
use airhand_proto::session::{
    Codec, InputEvent, MouseButton, QualitySettings, SystemAction, TouchPhase,
};

#[derive(Default)]
struct Counters {
    video: AtomicU64,
    keyframes: AtomicU64,
    bytes: AtomicU64,
    audio: AtomicU64,
    cursor: AtomicU64,
    size: Mutex<(Codec, u16, u16)>,
    events: Mutex<Vec<String>>,
    /// 收到了被控端的 Hello（被控端的用户同意了）
    hello: std::sync::atomic::AtomicBool,
}

/// 会话建立之后才知道 sid：画面到了用它来确认
type AckTarget = Arc<Mutex<Option<(Arc<ControllerService>, u32)>>>;

struct Ui(Arc<Counters>, AckTarget);

impl SessionUi for Ui {
    fn packet(&self, p: Vec<u8>) -> Result<(), UiClosed> {
        let c = &self.0;
        match Kind::of(&p) {
            Some(Kind::Video) => {
                let Some(v) = VideoHeader::decode(&p) else {
                    return Ok(());
                };
                c.video.fetch_add(1, Ordering::Relaxed);
                if v.key {
                    c.keyframes.fetch_add(1, Ordering::Relaxed);
                }
                c.bytes.fetch_add(p.len() as u64, Ordering::Relaxed);
                *c.size.lock().unwrap() = (v.codec, v.width, v.height);
                // 浏览器模式：收到就确认
                if let Some((svc, sid)) = self.1.lock().unwrap().as_ref() {
                    let _ = svc.ack(*sid, v.seq);
                }
            }
            Some(Kind::Cursor) => {
                c.cursor.fetch_add(1, Ordering::Relaxed);
            }
            Some(Kind::Audio) => {
                c.audio.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        Ok(())
    }

    fn event(&self, e: SessionEvent) -> Result<(), UiClosed> {
        let s = serde_json::to_string(&e).unwrap_or_default();
        if !matches!(e, SessionEvent::Stats { .. }) {
            println!("event: {s}");
        }
        if matches!(e, SessionEvent::Hello { .. }) {
            self.0.hello.store(true, Ordering::Relaxed);
        }
        self.0.events.lock().unwrap().push(s);
        Ok(())
    }
}

fn main() {
    airhand_android::rt::init_logging("info,quinn=warn");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = args.first().expect("usage: probe <code> [seconds]").clone();
    let secs: u64 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(8);
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let server = flag("--server").unwrap_or_else(|| "127.0.0.1:4433".into());
    let tap = flag("--tap").map(|s| {
        let (x, y) = s.split_once(',').expect("--tap x,y");
        (x.parse::<f32>().unwrap(), y.parse::<f32>().unwrap())
    });
    let text = flag("--text");
    let audio = args.iter().any(|a| a == "--audio");
    let touch = args.iter().any(|a| a == "--touch");
    let pinch = args.iter().any(|a| a == "--pinch");
    let action = flag("--action").map(|a| {
        serde_json::from_value::<SystemAction>(serde_json::Value::String(a.clone()))
            .unwrap_or_else(|_| panic!("unknown action {a}"))
    });

    let cfg = Arc::new(LinkConfig::new(server, "", "probe", "probe").unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    let report = Arc::new(move |s: ServerState| {
        let _ = tx.send(s);
    });
    let svc = Arc::new(ControllerService::start(
        cfg,
        rt().handle().clone(),
        report,
        Arc::new(|code| println!("host {code} is back online")),
        Identity::generate(),
    ));
    loop {
        match rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no server state")
        {
            ServerState::Connected => break,
            other => println!("server: {other:?}"),
        }
    }
    let counters = Arc::new(Counters::default());
    let slot = Arc::new(Mutex::new(None));
    let settings = QualitySettings {
        fps: 30,
        bitrate_kbps: 4000,
        max_short: 720,
        adaptive: true,
        codec: Codec::H264,
    };
    // 开始时的画质和声音在握手里就告诉被控端
    let info = rt()
        .block_on(svc.connect(
            &code,
            ConnectOptions {
                pairing: flag("--pairing"),
                settings,
                audio,
                ..Default::default()
            },
            Arc::new(Ui(counters.clone(), slot.clone())),
            Frontend::Browser,
        ))
        .unwrap_or_else(|p| panic!("connect failed: {p}"));
    println!("connected: {} ({})", info.host_name, info.host_os);
    *slot.lock().unwrap() = Some((svc.clone(), info.sid));
    let sid = info.sid;
    let input = |events: Vec<InputEvent>| {
        svc.command(Command::Input { sid, events })
            .unwrap_or_else(|p| panic!("input failed: {p}"))
    };
    // 被控端可能要先请它的用户同意：等到 Hello 再开始计时
    let asked = std::time::Instant::now();
    while !counters.hello.load(Ordering::Relaxed) {
        if asked.elapsed() > Duration::from_secs(70) {
            println!("no Hello from the host");
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if counters.hello.load(Ordering::Relaxed) {
        println!("host said hello after {} ms", asked.elapsed().as_millis());
    }

    std::thread::sleep(Duration::from_secs(secs / 2));
    let touch_ev =
        |id: u8, phase: TouchPhase, x: f32, y: f32| InputEvent::Touch { id, phase, x, y };
    if let (Some((x, y)), true) = (tap, touch) {
        println!("touch tap at {x},{y}");
        input(vec![touch_ev(0, TouchPhase::Down, x, y)]);
        std::thread::sleep(Duration::from_millis(80));
        input(vec![touch_ev(0, TouchPhase::Up, x, y)]);
    } else if let Some((x, y)) = tap {
        println!("tap at {x},{y}");
        input(vec![
            InputEvent::MouseMove { x, y },
            InputEvent::MouseButton {
                button: MouseButton::Left,
                down: true,
            },
        ]);
        std::thread::sleep(Duration::from_millis(80));
        input(vec![InputEvent::MouseButton {
            button: MouseButton::Left,
            down: false,
        }]);
    }
    if pinch {
        println!("pinch out at the center");
        input(vec![
            touch_ev(0, TouchPhase::Down, 0.45, 0.5),
            touch_ev(1, TouchPhase::Down, 0.55, 0.5),
        ]);
        for i in 1..=15 {
            std::thread::sleep(Duration::from_millis(30));
            let d = 0.05 + 0.02 * i as f32;
            input(vec![
                touch_ev(0, TouchPhase::Move, 0.5 - d, 0.5),
                touch_ev(1, TouchPhase::Move, 0.5 + d, 0.5),
            ]);
        }
        std::thread::sleep(Duration::from_millis(30));
        input(vec![
            touch_ev(0, TouchPhase::Up, 0.15, 0.5),
            touch_ev(1, TouchPhase::Up, 0.85, 0.5),
        ]);
    }
    if let Some(a) = action {
        println!("system action {a:?}");
        input(vec![InputEvent::System { action: a }]);
    }
    if let Some(t) = text {
        println!("typing {t:?}");
        input(vec![InputEvent::Text { text: t }]);
    }
    std::thread::sleep(Duration::from_secs(secs - secs / 2));

    let c = &counters;
    let (codec, w, h) = *c.size.lock().unwrap();
    println!(
        "frames={} keyframes={} bytes={} audio={} cursor={} last={}x{} codec={:?}",
        c.video.load(Ordering::Relaxed),
        c.keyframes.load(Ordering::Relaxed),
        c.bytes.load(Ordering::Relaxed),
        c.audio.load(Ordering::Relaxed),
        c.cursor.load(Ordering::Relaxed),
        w,
        h,
        codec
    );
    if let Some(stats) = c
        .events
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e.contains("\"stats\""))
    {
        println!("last stats: {stats}");
    }
    svc.disconnect(info.sid);
    std::thread::sleep(Duration::from_millis(300));
}
