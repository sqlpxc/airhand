import Foundation

/// 视频编码格式，与 Rust 的 `airhand_proto::session::Codec` 对应（JSON 里是 `wire`，媒体包头里是 `id`）
enum Codec: Int, CaseIterable, Codable, Identifiable {
    case h264 = 0, h265 = 1, av1 = 2

    var id: Int { rawValue }
    var wire: String { ["h264", "h265", "av1"][rawValue] }
    var label: String { ["H.264", "H.265 / HEVC", "AV1"][rawValue] }
    var short: String { ["H.264", "HEVC", "AV1"][rawValue] }

    static func fromWire(_ s: String?) -> Codec? {
        guard let s else { return nil }
        return allCases.first { $0.wire.caseInsensitiveCompare(s) == .orderedSame }
    }

    /// 压缩率从高到低（与桌面控制台相同：AV1 > HEVC > H.264）
    static let priority: [Codec] = [.av1, .h265, .h264]
}

/// 画质设置，与 `QualitySettings` 对应
struct Quality: Equatable {
    var fps: Int
    var kbps: Int
    var maxShort: Int
    var adaptive = true
    var codec: Codec = .h264

    var json: JSON {
        ["fps": fps, "bitrateKbps": kbps, "maxShort": maxShort, "adaptive": adaptive, "codec": codec.wire]
    }
}

/// 画质预设：与桌面控制台（client/src/lib/presets.ts）、Android 相同
enum Preset: String, CaseIterable, Identifiable {
    case saver, balanced, smooth, hd, native

    var id: String { rawValue }
    var fps: Int { [24, 30, 60, 60, 60][index] }
    var kbps: Int { [1200, 4000, 8000, 16000, 30000][index] }
    var maxShort: Int { [720, 1080, 1080, 1440, 0][index] }
    private var index: Int { Preset.allCases.firstIndex(of: self)! }

    func quality(_ codec: Codec) -> Quality { Quality(fps: fps, kbps: kbps, maxShort: maxShort, adaptive: true, codec: codec) }

    static let `default` = Preset.balanced
    static func of(_ id: String?) -> Preset { Preset(rawValue: id ?? "") ?? .default }
    static func match(_ q: Quality) -> Preset? { allCases.first { $0.fps == q.fps && $0.kbps == q.kbps && $0.maxShort == q.maxShort } }
}

/// 显示给用户的问题，与 Rust 的 `airhand_link::Problem` 对应（JSON 以 kind 区分）
struct Problem: Equatable, Error {
    var kind: String
    var detail: String? = nil
    var server: Int? = nil
    var client: Int? = nil
    var peer: Int? = nil
    var ours: Int? = nil

    /// 对方有意结束，或者要用户来决定（对方的身份变了、要配对码、版本不兼容），不应该自动重连
    var isFinal: Bool { Problem.final.contains(kind) }

    /// 端到端握手的问题：要用户输入配对码
    var needsPairing: Bool {
        ["pairingRequired", "pairingFailed", "pairingUnavailable", "hostKeyChanged"].contains(kind)
    }

    static let final: Set<String> = [
        "kicked", "codeChanged", "hostKeyChanged", "pairingRequired", "pairingFailed", "pairingUnavailable", "peerIncompatible",
    ]

    static func parse(_ o: JSON?) -> Problem {
        guard let o else { return Problem(kind: "failed", detail: "?") }
        return Problem(kind: o.str("kind") ?? "failed", detail: o.str("detail"), server: o.int("server"),
                       client: o.int("client"), peer: o.int("peer"), ours: o.int("ours"))
    }
}

/// 与中转服务器的连接状态
enum ServerState: Equatable {
    case connecting, connected, disconnected(Problem)

    static func parse(_ o: JSON?) -> ServerState {
        switch o?.str("state") {
        case "connected": return .connected
        case "disconnected": return .disconnected(Problem.parse(o?.obj("reason")))
        default: return .connecting
        }
    }

    var isConnected: Bool { if case .connected = self { return true } else { return false } }
}

struct DisplayInfo: Equatable, Identifiable {
    var index: Int
    var name: String
    var width: Int
    var height: Int
    var primary: Bool
    var id: Int { index }

    var json: JSON { ["index": index, "name": name, "x": 0, "y": 0, "width": width, "height": height, "primary": primary] }

    static func parse(_ o: JSON) -> DisplayInfo {
        DisplayInfo(index: o.int("index") ?? 0, name: o.str("name") ?? "", width: o.int("width") ?? 0,
                    height: o.int("height") ?? 0, primary: o.bool("primary"))
    }

    static func parseList(_ a: [Any]?) -> [DisplayInfo] { (a ?? []).compactMap { $0 as? JSON }.map(parse) }
}

/// 系统操作，与 Rust 的 `airhand_proto::session::SystemAction` 对应
enum SystemAction: String, CaseIterable {
    case back, home, recents, notifications, quickSettings, screenshot, lockScreen, powerMenu, volumeUp, volumeDown
}

/// 被控端是哪种系统：决定默认的操作方式、快捷键和导航键
enum OsKind: String {
    case windows, mac, linux, android, ios, unknown = ""

    static func fromWire(_ s: String?) -> OsKind {
        guard let s, !s.isEmpty else { return .unknown }
        return OsKind(rawValue: s) ?? .unknown
    }

    static func of(_ os: String?) -> OsKind {
        let s = (os ?? "").lowercased()
        if s.hasPrefix("android") { return .android }
        if s.hasPrefix("ios") || s.hasPrefix("ipados") || s.contains("iphone") || s.contains("ipad") { return .ios }
        if s.contains("windows") { return .windows }
        if s.contains("mac") || s.contains("os x") { return .mac }
        return s.trimmingCharacters(in: .whitespaces).isEmpty ? .unknown : .linux
    }

    var isPhone: Bool { self == .android || self == .ios }

    var symbol: String {
        switch self {
        case .windows: return "pc"
        case .mac: return "laptopcomputer"
        case .linux: return "desktopcomputer"
        case .android: return "smartphone"
        case .ios: return "iphone"
        case .unknown: return "display"
        }
    }
}

/// 被控端的防窥模式（会话事件 `privacy` 的 `state`），与 Rust 的 `airhand_proto::session::PrivacyState` 对应。
/// 对方是装了虚拟显示器驱动的 Windows、允许了“辅助功能”的 Mac 时才会说；不支持的被控端不说，界面也就没有这个开关
enum PrivacyState: String, CaseIterable {
    /// 开不了：Windows 没装虚拟显示器驱动 / 不是为所有用户安装的；Mac 没允许“辅助功能”
    case unavailable
    /// 被控端的设置不允许（config.toml 的 privacy_mode）
    case notAllowed
    case off
    /// 正在打开（Windows 换成虚拟显示器要几秒钟）
    case starting
    case on
    /// 刚才没能打开（对方已经恢复原样），detail 是原因
    case failed

    /// 不认识的（对方版本新增的；Rust 那边已经报成 unavailable，这里再保险一次）当作开不了
    static func fromWire(_ s: String?) -> PrivacyState { s.flatMap(PrivacyState.init(rawValue:)) ?? .unavailable }

    /// 开关是开着的：开着或者正在打开
    var active: Bool { self == .on || self == .starting }
    /// 这台被控端能开（开关可以点）
    var usable: Bool { self != .unavailable && self != .notAllowed }
}

struct HostHello {
    var hostname: String
    var os: String
    var version: String
    var displays: [DisplayInfo]
    var current: Int
    var codecs: [Codec]
    var platform: OsKind
    var touch: Bool
    var actions: [SystemAction]
}

struct EncoderInfo: Equatable {
    var codec: Codec
    var name: String
    var hardware: Bool
    var width: Int
    var height: Int
}

/// 被控端发来的提示（`NoticeView`）
struct Notice: Equatable {
    var kind: String
    var detail: String?
    var screen = false
    var input = false
}

struct Stats: Equatable {
    var fps: Double
    var kbps: Double
    var rttMs: Double?
    var width: Int
    var height: Int
    var direct: Bool
}

enum Code {
    /// 用户输入的控制码 → 6 位数字（允许空格 / 短横线），不合法返回 nil。规则与 `airhand_proto::parse_code` 相同
    static func parse(_ input: String) -> String? {
        let digits = input.filter { !$0.isWhitespace && $0 != "-" }
        guard digits.count == 6, digits.allSatisfy({ $0.isASCII && $0.isNumber }), digits.first != "0" else { return nil }
        return digits
    }

    /// 从任意文本里找出一个控制码（例如粘贴的是“控制码：123 456”）
    static func find(_ text: String?) -> String? {
        guard let text, text.count <= 200 else { return nil }
        let re = try! NSRegularExpression(pattern: #"(?<!\d)([1-9]\d{2})[\s-]?(\d{3})(?!\d)"#)
        let ns = text as NSString
        guard let m = re.firstMatch(in: text, range: NSRange(location: 0, length: ns.length)) else { return nil }
        return ns.substring(with: m.range(at: 1)) + ns.substring(with: m.range(at: 2))
    }

    static func format(_ code: String) -> String {
        code.count == 6 ? "\(code.prefix(3)) \(code.suffix(3))" : code
    }
}
