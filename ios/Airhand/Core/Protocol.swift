import Foundation

/// 媒体包：画面、光标、声音（格式见 `crates/airhand-proto/src/media.rs`；被控端打包，Rust 核心原样交过来）。
///
/// 头（小端）+ 数据：`0 类型 u8（1 画面 / 2 光标 / 3 声音）| 1 标志 u8 | 2 编码格式 u8（光标：槽位）| 3 头的长度 u8 |
///  4 序号 u32 | 8 宽 u16 | 10 高 u16 | 12 热点 x u16 | 14 热点 y u16 | 16 采集时间 u64（微秒，被控端时钟）`
/// 头可以变长：负载一定从“头的长度”处取。
enum Media {
    static let header = 24
    static let video: UInt8 = 1
    static let cursor: UInt8 = 2
    static let audio: UInt8 = 3

    struct Video {
        var seq: UInt32
        var key: Bool
        var codec: Codec
        var width: Int
        var height: Int
        var captureUs: Int64
    }

    struct Cursor {
        var visible: Bool
        /// 新的形状（RGBA 跟在头后面）：宽、高、热点
        var shape: (w: Int, h: Int, hx: Int, hy: Int)?
        var slot: Int
        var cached: Bool
        var inFrame: Bool
    }

    static func headerLen(_ b: UnsafeRawBufferPointer) -> Int {
        guard b.count >= header else { return 0 }
        let n = Int(b[3])
        return n >= header && n <= b.count ? n : 0
    }

    static func kind(_ b: UnsafeRawBufferPointer) -> UInt8 { headerLen(b) == 0 ? 0 : b[0] }

    static func u16(_ b: UnsafeRawBufferPointer, _ i: Int) -> Int { Int(b[i]) | Int(b[i + 1]) << 8 }
    static func u32(_ b: UnsafeRawBufferPointer, _ i: Int) -> UInt32 { b.loadUnaligned(fromByteOffset: i, as: UInt32.self).littleEndian }
    static func u64(_ b: UnsafeRawBufferPointer, _ i: Int) -> UInt64 { b.loadUnaligned(fromByteOffset: i, as: UInt64.self).littleEndian }

    static func seq(_ b: UnsafeRawBufferPointer) -> UInt32 { u32(b, 4) }

    /// 画面包的头；编码格式不认识时为 nil（丢掉这一帧，但照样确认）
    static func video(_ b: UnsafeRawBufferPointer) -> Video? {
        guard let c = Codec(rawValue: Int(b[2])) else { return nil }
        return Video(seq: u32(b, 4), key: b[1] & 1 != 0, codec: c, width: u16(b, 8), height: u16(b, 10), captureUs: Int64(bitPattern: u64(b, 16)))
    }

    static func cursor(_ b: UnsafeRawBufferPointer) -> Cursor {
        let f = b[1]
        return Cursor(visible: f & 1 != 0,
                      shape: f & 2 != 0 ? (u16(b, 8), u16(b, 10), u16(b, 12), u16(b, 14)) : nil,
                      slot: Int(b[2]), cached: f & 4 != 0, inFrame: f & 8 != 0)
    }

    /// 声音包（麦克风）：类型 3，标志 bit0 = 静音后的第一包
    static func audioPacket(seq: UInt32, start: Bool, captureUs: UInt64, opus: Data) -> Data {
        var h = [UInt8](repeating: 0, count: header)
        h[0] = audio
        h[1] = start ? 1 : 0
        h[3] = UInt8(header)
        withUnsafeBytes(of: seq.littleEndian) { for i in 0..<4 { h[4 + i] = $0[i] } }
        withUnsafeBytes(of: captureUs.littleEndian) { for i in 0..<8 { h[16 + i] = $0[i] } }
        var d = Data(h)
        d.append(opus)
        return d
    }
}

/// 发给 Rust 核心的命令（`ah_ctl_call`）：`connect` 加上和桌面版、网页版一样的会话命令
enum Commands {
    private static func cmd(_ name: String, _ sid: UInt32) -> JSON { ["cmd": name, "sid": sid] }

    /// `privacy`：要不要防窥模式（重连时带上现在的开关，对方一开始就保持着，它的屏幕不会在重连的间隙露出来）
    static func connect(code: String, resume: String?, settings: Quality, hostKey: String?, pairing: String?, audio: Bool,
                        privacy: Bool = false) -> JSON {
        ["cmd": "connect", "code": code, "resume": Resume.toJson(resume), "hostKey": hostKey ?? NSNull(),
         "pairing": pairing ?? NSNull(), "settings": settings.json, "audio": audio, "privacy": privacy]
    }

    static func input(_ sid: UInt32, _ events: [JSON]) -> JSON { var c = cmd("input", sid); c["events"] = events; return c }
    static func settings(_ sid: UInt32, _ q: Quality) -> JSON { var c = cmd("settings", sid); c["settings"] = q.json; return c }
    static func keyframe(_ sid: UInt32) -> JSON { cmd("keyframe", sid) }
    static func display(_ sid: UInt32, _ index: Int) -> JSON { var c = cmd("display", sid); c["index"] = index; return c }
    static func audio(_ sid: UInt32, _ on: Bool) -> JSON { var c = cmd("audio", sid); c["enabled"] = on; return c }
    static func mic(_ sid: UInt32, _ on: Bool) -> JSON { var c = cmd("mic", sid); c["enabled"] = on; return c }
    /// 打开 / 关闭对方的防窥模式（对方说过它的状态之后才有用，见 `RemoteSession.setPrivacy`）
    static func privacy(_ sid: UInt32, _ on: Bool) -> JSON { var c = cmd("privacy", sid); c["on"] = on; return c }
    static func clipboard(_ sid: UInt32, _ text: String) -> JSON { var c = cmd("clipboard", sid); c["text"] = text; return c }
    static func disconnect(_ sid: UInt32) -> JSON { cmd("disconnect", sid) }
}

/// 重连凭证：Rust 那边是 16 个字节（JSON 数组），这里存成十六进制字符串
enum Resume {
    static func toJson(_ hex: String?) -> Any {
        guard let hex, hex.count == 32 else { return NSNull() }
        var a: [Int] = []
        var i = hex.startIndex
        for _ in 0..<16 {
            let j = hex.index(i, offsetBy: 2)
            guard let v = Int(hex[i..<j], radix: 16) else { return NSNull() }
            a.append(v)
            i = j
        }
        return a
    }

    static func fromJson(_ a: [Any]?) -> String? {
        guard let a, a.count == 16 else { return nil }
        return a.map { String(format: "%02x", (($0 as? NSNumber)?.intValue ?? 0) & 0xff) }.joined()
    }
}

/// 输入事件（与 Rust 的 `airhand_proto::session::InputEvent` 的 JSON 形状相同）
enum Input {
    static func move(_ x: Double, _ y: Double) -> JSON { ["type": "mouseMove", "x": min(max(x, 0), 1), "y": min(max(y, 0), 1)] }
    /// 相对移动：约等于被控端的像素
    static func delta(_ dx: Int, _ dy: Int) -> JSON { ["type": "mouseDelta", "dx": dx, "dy": dy] }
    static func button(_ b: String, _ down: Bool) -> JSON { ["type": "mouseButton", "button": b, "down": down] }
    /// 滚轮：120 = 一格，向上 / 向右为正
    static func wheel(_ dx: Int, _ dy: Int) -> JSON {
        ["type": "wheel", "dx": min(max(dx, -32000), 32000), "dy": min(max(dy, -32000), 32000)]
    }
    /// 物理按键，取值为 W3C `KeyboardEvent.code`
    static func key(_ code: String, _ down: Bool) -> JSON { ["type": "key", "code": code, "down": down] }
    static func text(_ s: String) -> JSON { ["type": "text", "text": s] }
    static func touch(_ id: Int, _ phase: String, _ x: Double, _ y: Double) -> JSON {
        ["type": "touch", "id": id, "phase": phase, "x": min(max(x, 0), 1), "y": min(max(y, 0), 1)]
    }
    static func system(_ a: SystemAction) -> JSON { ["type": "system", "action": a.rawValue] }
    static func releaseAll() -> JSON { ["type": "releaseAll"] }

    /// 按下再松开一组键（先按下修饰键，再按下主键，倒序松开）
    static func chord(_ codes: [String]) -> [JSON] { codes.map { key($0, true) } + codes.reversed().map { key($0, false) } }

    /// 加到待发的一批里：紧挨着的绝对移动只留最后一个，相对移动加起来（和 Rust 的 `InputEvent::coalesce` 一样）
    static func append(_ pending: inout [JSON], _ e: JSON) {
        if let last = pending.last, let t = e.str("type"), t == last.str("type") {
            if t == "mouseMove" { pending[pending.count - 1] = e; return }
            if t == "mouseDelta" {
                pending[pending.count - 1] = delta((last.int("dx") ?? 0) + (e.int("dx") ?? 0), (last.int("dy") ?? 0) + (e.int("dy") ?? 0))
                return
            }
        }
        pending.append(e)
    }
}

/// 把输入事件合并后发出去：连续的绝对移动只保留最后一个、相对移动加起来，最多每 8 ms 发一批。
/// 发送在单独的队列上，触摸处理不会被 Rust 调用拖慢。
final class InputSender {
    private let queue = DispatchQueue(label: "airhand.input", qos: .userInteractive)
    private let lock = NSLock()
    private var pending: [JSON] = []
    private var scheduled = false
    private let deliver: ([JSON]) -> Void

    init(_ deliver: @escaping ([JSON]) -> Void) { self.deliver = deliver }

    func send(_ events: [JSON], now: Bool = true) {
        guard !events.isEmpty else { return }
        lock.lock()
        for e in events { Input.append(&pending, e) }
        let schedule = !scheduled
        scheduled = true
        lock.unlock()
        if now {
            queue.async { self.flush() }
        } else if schedule {
            queue.asyncAfter(deadline: .now() + .milliseconds(8)) { self.flush() }
        }
    }

    private func flush() {
        lock.lock()
        scheduled = false
        let batch = pending
        pending = []
        lock.unlock()
        if !batch.isEmpty { deliver(batch) }
    }
}

/// 按键 → W3C `KeyboardEvent.code`
enum KeyMap {
    /// UIKeyboardHIDUsage（HID 键码）→ code
    static func code(hid: Int) -> String? {
        switch hid {
        case 0x04...0x1d: return "Key" + String(UnicodeScalar(UInt8(65 + hid - 0x04)))
        case 0x1e...0x26: return "Digit\(hid - 0x1e + 1)"
        case 0x27: return "Digit0"
        case 0x28: return "Enter"
        case 0x29: return "Escape"
        case 0x2a: return "Backspace"
        case 0x2b: return "Tab"
        case 0x2c: return "Space"
        case 0x2d: return "Minus"
        case 0x2e: return "Equal"
        case 0x2f: return "BracketLeft"
        case 0x30: return "BracketRight"
        case 0x31, 0x32: return "Backslash"
        case 0x33: return "Semicolon"
        case 0x34: return "Quote"
        case 0x35: return "Backquote"
        case 0x36: return "Comma"
        case 0x37: return "Period"
        case 0x38: return "Slash"
        case 0x39: return "CapsLock"
        case 0x3a...0x45: return "F\(hid - 0x3a + 1)"
        case 0x46: return "PrintScreen"
        case 0x47: return "ScrollLock"
        case 0x48: return "Pause"
        case 0x49: return "Insert"
        case 0x4a: return "Home"
        case 0x4b: return "PageUp"
        case 0x4c: return "Delete"
        case 0x4d: return "End"
        case 0x4e: return "PageDown"
        case 0x4f: return "ArrowRight"
        case 0x50: return "ArrowLeft"
        case 0x51: return "ArrowDown"
        case 0x52: return "ArrowUp"
        case 0x53: return "NumLock"
        case 0x54: return "NumpadDivide"
        case 0x55: return "NumpadMultiply"
        case 0x56: return "NumpadSubtract"
        case 0x57: return "NumpadAdd"
        case 0x58: return "NumpadEnter"
        case 0x59...0x61: return "Numpad\(hid - 0x59 + 1)"
        case 0x62: return "Numpad0"
        case 0x63: return "NumpadDecimal"
        case 0x64: return "IntlBackslash"
        case 0x65: return "ContextMenu"
        case 0x67: return "NumpadEqual"
        case 0x68...0x73: return "F\(hid - 0x68 + 13)"
        case 0xe0: return "ControlLeft"
        case 0xe1: return "ShiftLeft"
        case 0xe2: return "AltLeft"
        case 0xe3: return "MetaLeft"
        case 0xe4: return "ControlRight"
        case 0xe5: return "ShiftRight"
        case 0xe6: return "AltRight"
        case 0xe7: return "MetaRight"
        default: return nil
        }
    }

    /// 美式键盘上打出这个字符的键：(code, 是否要按 Shift)。用于“Ctrl + 字母”这类组合键
    static func forChar(_ c: Character) -> (String, Bool)? {
        if let a = c.asciiValue {
            switch a {
            case 97...122: return ("Key" + String(c).uppercased(), false)
            case 65...90: return ("Key" + String(c), true)
            case 48...57: return ("Digit" + String(c), false)
            case 32: return ("Space", false)
            case 10, 13: return ("Enter", false)
            case 9: return ("Tab", false)
            default: break
            }
        }
        return symbols[c]
    }

    private static let symbols: [Character: (String, Bool)] = [
        "`": ("Backquote", false), "~": ("Backquote", true), "-": ("Minus", false), "_": ("Minus", true),
        "=": ("Equal", false), "+": ("Equal", true), "[": ("BracketLeft", false), "{": ("BracketLeft", true),
        "]": ("BracketRight", false), "}": ("BracketRight", true), "\\": ("Backslash", false), "|": ("Backslash", true),
        ";": ("Semicolon", false), ":": ("Semicolon", true), "'": ("Quote", false), "\"": ("Quote", true),
        ",": ("Comma", false), "<": ("Comma", true), ".": ("Period", false), ">": ("Period", true),
        "/": ("Slash", false), "?": ("Slash", true), "!": ("Digit1", true), "@": ("Digit2", true), "#": ("Digit3", true),
        "$": ("Digit4", true), "%": ("Digit5", true), "^": ("Digit6", true), "&": ("Digit7", true), "*": ("Digit8", true),
        "(": ("Digit9", true), ")": ("Digit0", true),
    ]

    /// 按键在界面上显示的名字
    static func label(_ code: String, os: OsKind = .windows) -> String {
        if code.hasPrefix("Key") { return String(code.dropFirst(3)) }
        if code.hasPrefix("Digit") { return String(code.dropFirst(5)) }
        if code.hasPrefix("Numpad"), code.count == 7 { return "Num" + String(code.dropFirst(6)) }
        let mac = os == .mac
        switch code {
        case "ControlLeft", "ControlRight": return mac ? "⌃" : "Ctrl"
        case "ShiftLeft", "ShiftRight": return mac ? "⇧" : "Shift"
        case "AltLeft", "AltRight": return mac ? "⌥" : "Alt"
        case "MetaLeft", "MetaRight": return mac ? "⌘" : "Win"
        case "Escape": return "Esc"
        case "Enter": return "↵"
        case "Backspace": return "⌫"
        case "Delete": return "Del"
        case "Space": return L.zh ? "空格" : "Space"
        case "ArrowUp": return "↑"
        case "ArrowDown": return "↓"
        case "ArrowLeft": return "←"
        case "ArrowRight": return "→"
        case "PageUp": return "PgUp"
        case "PageDown": return "PgDn"
        case "Backquote": return "`"
        case "Minus": return "-"
        case "Equal": return "="
        case "BracketLeft": return "["
        case "BracketRight": return "]"
        case "Backslash": return "\\"
        case "Semicolon": return ";"
        case "Quote": return "'"
        case "Comma": return ","
        case "Period": return "."
        case "Slash": return "/"
        case "PrintScreen": return "PrtSc"
        case "CapsLock": return "Caps"
        default: return code
        }
    }
}
