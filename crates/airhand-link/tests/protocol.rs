//! 三种界面（桌面 / 网页控制台的 TypeScript、Android 的 Kotlin）和这里共用的协议样例：`tests/protocol.json`。
//!
//! * 媒体包：界面按 `airhand_proto::media` 的格式解析（`client/src/lib/media.ts`、`android/.../controller/Protocol.kt`）。
//!   其中有一个头更长的包（以后的版本在头里加了字段）：界面要按头里的长度找负载；
//! * 输入事件、界面命令、连接选项：界面拼出来的 JSON 要和这里的一样（`airhand_link::controller::Command`、
//!   `ConnectOptions`），全都是 camelCase。
//!
//! 这个测试检查文件和 Rust 的实现一致；另外两边的测试（`npm test`、`./gradlew testDebugUnitTest`）读同一个文件。
//! 改了格式之后用 `AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol` 重新生成，再让另外两边跟上。

use airhand_link::controller::{Command, ConnectOptions};
use airhand_proto::media::{self, AudioHeader, CursorSlot, CursorState, VideoHeader};
use airhand_proto::session::{
    Codec, CursorShape, InputEvent, MouseButton, QualitySettings, SystemAction, TouchPhase,
};
use serde_json::{Value, json};

fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

/// 交给界面的包（线上的包去掉长度前缀）
fn packet(header: &[u8; media::HEADER_LEN], payload: &[u8]) -> Vec<u8> {
    media::frame(header, &[payload])[4..].to_vec()
}

fn inputs() -> Vec<InputEvent> {
    vec![
        InputEvent::MouseMove { x: 0.5, y: 0.25 },
        InputEvent::MouseDelta { dx: -3, dy: 7 },
        InputEvent::MouseButton {
            button: MouseButton::Right,
            down: true,
        },
        InputEvent::Wheel { dx: 0, dy: -120 },
        InputEvent::Key {
            code: "KeyA".into(),
            down: false,
        },
        InputEvent::Text {
            text: "你好".into(),
        },
        InputEvent::ReleaseAll,
        InputEvent::Touch {
            id: 1,
            phase: TouchPhase::Down,
            x: 0.25,
            y: 0.75,
        },
        InputEvent::System {
            action: SystemAction::Back,
        },
    ]
}

fn settings() -> QualitySettings {
    QualitySettings {
        fps: 60,
        bitrate_kbps: 8000,
        max_short: 1080,
        adaptive: true,
        codec: Codec::H265,
    }
}

fn commands() -> Vec<(Value, Command)> {
    let settings = settings();
    vec![
        (
            json!({ "cmd": "input", "sid": 3, "events": [{ "type": "mouseDelta", "dx": 1, "dy": 2 }, { "type": "releaseAll" }] }),
            Command::Input {
                sid: 3,
                events: vec![
                    InputEvent::MouseDelta { dx: 1, dy: 2 },
                    InputEvent::ReleaseAll,
                ],
            },
        ),
        (
            json!({ "cmd": "settings", "sid": 3, "settings": { "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "h265" } }),
            Command::Settings { sid: 3, settings },
        ),
        (
            json!({ "cmd": "keyframe", "sid": 3 }),
            Command::Keyframe { sid: 3 },
        ),
        (
            json!({ "cmd": "display", "sid": 3, "index": 1 }),
            Command::Display { sid: 3, index: 1 },
        ),
        (
            json!({ "cmd": "audio", "sid": 3, "enabled": true }),
            Command::Audio {
                sid: 3,
                enabled: true,
            },
        ),
        (
            json!({ "cmd": "clipboard", "sid": 3, "text": "hi" }),
            Command::Clipboard {
                sid: 3,
                text: "hi".into(),
            },
        ),
        (
            json!({ "cmd": "clipboardSync", "sid": 3, "enabled": false }),
            Command::ClipboardSync {
                sid: 3,
                enabled: false,
            },
        ),
        (
            json!({ "cmd": "ack", "sid": 3, "seq": 99 }),
            Command::Ack { sid: 3, seq: 99 },
        ),
        (
            json!({ "cmd": "disconnect", "sid": 3 }),
            Command::Disconnect { sid: 3 },
        ),
    ]
}

/// `connect` 的选项（三种界面的 `connect` 命令都是这些字段加上 `code`）
fn connect_options() -> (Value, ConnectOptions) {
    (
        json!({
            "resume": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            "hostKey": "ab".repeat(32),
            "pairing": "482913",
            "settings": { "fps": 60, "bitrateKbps": 8000, "maxShort": 1080, "adaptive": true, "codec": "h265" },
            "audio": true
        }),
        ConnectOptions {
            resume: Some([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]),
            host_key: Some("ab".repeat(32)),
            pairing: Some("482913".into()),
            settings: settings(),
            audio: true,
        },
    )
}

fn fixture() -> Value {
    let capture_us = 123_456_789_012u64;
    let video = VideoHeader {
        seq: 9,
        key: true,
        codec: Codec::AV1,
        width: 1920,
        height: 1080,
        capture_us,
    };
    let audio = AudioHeader {
        seq: 3,
        start: true,
        capture_us: capture_us + 20_000,
    };
    let delta = VideoHeader {
        seq: 10,
        key: false,
        codec: Codec::H265,
        width: 1280,
        height: 720,
        capture_us: capture_us + 16_667,
    };
    let shape = CursorShape {
        width: 2,
        height: 1,
        hot_x: 1,
        hot_y: 0,
        rgba: vec![255, 0, 0, 255, 0, 0, 255, 128],
    };
    let cursor = media::cursor_packet(&CursorState {
        visible: true,
        shape: Some(CursorSlot {
            slot: 7,
            image: Some(shape.clone()),
        }),
    });
    let cached = media::cursor_packet(&CursorState {
        visible: true,
        shape: Some(CursorSlot {
            slot: 7,
            image: None,
        }),
    });
    let hidden = media::cursor_packet(&CursorState {
        visible: false,
        shape: None,
    });
    // 以后的版本在头里加了一个 8 字节的扩展（类型 0xee、长度 6，头的长度是 32）：
    // 负载照样从头的长度处开始，不认识的扩展跳过
    let longer = {
        let mut p = AudioHeader {
            seq: 4,
            start: false,
            capture_us: capture_us + 40_000,
        }
        .encode()
        .to_vec();
        p[3] = 32;
        p.extend_from_slice(&[0xee, 6, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee]);
        p.extend_from_slice(&[0xab, 0xcd]);
        p
    };
    json!({
        "about": "Generated by crates/airhand-link/tests/protocol.rs. Shared by the Rust, TypeScript and Kotlin tests.",
        "media": [
            {
                "name": "keyframe", "hex": hex(&packet(&video.encode(), &[7, 8, 9])),
                "kind": 1, "seq": 9, "key": true, "codec": "av1", "width": 1920, "height": 1080,
                "captureUs": capture_us, "payload": "070809"
            },
            {
                "name": "delta frame", "hex": hex(&packet(&delta.encode(), &[1])),
                "kind": 1, "seq": 10, "key": false, "codec": "h265", "width": 1280, "height": 720,
                "captureUs": capture_us + 16_667, "payload": "01"
            },
            {
                "name": "audio", "hex": hex(&packet(&audio.encode(), &[0xfc, 0xff, 0xfe])),
                "kind": 3, "seq": 3, "start": true, "captureUs": capture_us + 20_000, "payload": "fcfffe"
            },
            {
                "name": "cursor", "hex": hex(&cursor), "kind": 2, "visible": true, "slot": 7, "cached": false,
                "shape": { "width": 2, "height": 1, "hotX": 1, "hotY": 0 }, "payload": hex(&shape.rgba)
            },
            {
                "name": "cached cursor", "hex": hex(&cached), "kind": 2, "visible": true, "slot": 7, "cached": true,
                "shape": null, "payload": ""
            },
            {
                "name": "hidden cursor", "hex": hex(&hidden), "kind": 2, "visible": false, "slot": 0, "cached": false,
                "shape": null, "payload": ""
            },
            {
                "name": "audio with a longer header from a newer version", "hex": hex(&longer),
                "kind": 3, "seq": 4, "start": false, "captureUs": capture_us + 40_000, "payload": "abcd"
            }
        ],
        "input": serde_json::to_value(inputs()).unwrap(),
        "commands": commands().into_iter().map(|(v, _)| v).collect::<Vec<_>>(),
        "connectOptions": connect_options().0,
    })
}

fn path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/protocol.json")
}

#[test]
fn fixture_matches_the_implementation() {
    let want = fixture();
    let text = serde_json::to_string_pretty(&want).unwrap() + "\n";
    let contract_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../client/tests/protocol-contract.ts");
    // 字面量由 Rust 生成并由 tsc 检查，不能把 JSON.parse 的 any 当作类型契约。
    let contract_data = json!({
        "input": want["input"], "commands": want["commands"], "connectOptions": want["connectOptions"],
    });
    let contract = format!(
        "// Generated by airhand-link/tests/protocol.rs; update with AIRHAND_UPDATE_FIXTURES=1.\n\
         import type {{ InputEvent, Command, ConnectOptions }} from \"../src/lib/api\";\n\
         export const contract = {} satisfies {{ input: InputEvent[]; commands: Command[]; connectOptions: ConnectOptions }};\n",
        serde_json::to_string_pretty(&contract_data).unwrap(),
    );
    if std::env::var_os("AIRHAND_UPDATE_FIXTURES").is_some() {
        std::fs::write(path(), &text).unwrap();
        std::fs::write(&contract_path, &contract).unwrap();
    }
    // Windows 上检出的文件可能是 CRLF
    assert_eq!(
        std::fs::read_to_string(contract_path)
            .unwrap()
            .replace("\r\n", "\n"),
        contract,
        "the TypeScript contract fixture is out of date: AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol"
    );
    let have: Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
    assert_eq!(
        have, want,
        "tests/protocol.json is out of date: AIRHAND_UPDATE_FIXTURES=1 cargo test -p airhand-link --test protocol"
    );
}

#[test]
fn json_shapes_roundtrip() {
    let f = fixture();
    let events: Vec<InputEvent> = serde_json::from_value(f["input"].clone()).unwrap();
    assert_eq!(events, inputs());
    for (json, cmd) in commands() {
        let parsed: Command =
            serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(parsed, cmd);
        assert_eq!(parsed.sid(), 3);
    }
    // 认不出的命令是错误（界面会收到 failed）
    assert!(serde_json::from_value::<Command>(json!({ "cmd": "nope", "sid": 1 })).is_err());
    let (json, opts) = connect_options();
    assert_eq!(
        serde_json::from_value::<ConnectOptions>(json).unwrap(),
        opts
    );
    // 都可以不带（第一次连、默认画质、不要声音）
    assert_eq!(
        serde_json::from_value::<ConnectOptions>(json!({})).unwrap(),
        ConnectOptions::default()
    );
}

#[test]
fn media_packets_decode() {
    let f = fixture();
    let unhex = |s: &str| -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    };
    let v = unhex(f["media"][0]["hex"].as_str().unwrap());
    let h = VideoHeader::decode(&v).unwrap();
    assert_eq!(
        (h.seq, h.key, h.codec, h.width),
        (9, true, Codec::AV1, 1920)
    );
    assert_eq!(media::payload(&v), Some(&[7, 8, 9][..]));
    let a = unhex(f["media"][2]["hex"].as_str().unwrap());
    assert!(AudioHeader::decode(&a).unwrap().start);
    for m in f["media"].as_array().unwrap() {
        let p = unhex(m["hex"].as_str().unwrap());
        assert_eq!(
            hex(media::payload(&p).unwrap()),
            m["payload"].as_str().unwrap(),
            "{}",
            m["name"]
        );
    }
}
