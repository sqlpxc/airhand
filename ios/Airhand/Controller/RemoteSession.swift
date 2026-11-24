import CoreGraphics
import Foundation
import Observation
import UIKit

/// 远端光标：形状由被控端发来，位置由本机决定（触控板模式下本机画出来）
struct CursorState {
    var visible = true
    var image: CGImage? = nil
    var hotX = 0
    var hotY = 0
    var inFrame = false
}

/// 一个远程会话（看 / 操作一台设备）。连接、断线重连、等对方上线都在这里，界面只观察状态。
///
/// * 第一次连接失败（控制码不对、对方不在线、要配对码……）：`.failed`，回到首页显示原因；
/// * 连上之后断了：带着重连凭证自动重连（`.reconnecting`）；对方离线了就等它上线（`.waitingHost`）；
/// * 对方断开了我们 / 换了控制码：`.ended`，不再重连。
///
/// 和 Android 的 RemoteSession.kt 是同一套逻辑。
@Observable
final class RemoteSession {
    enum Phase: Equatable {
        case connecting, live, reconnecting(Problem), waitingHost(Problem?), failed(Problem), ended(Problem), closed

        var isActive: Bool {
            switch self {
            case .connecting, .live, .reconnecting, .waitingHost: return true
            default: return false
            }
        }
    }

    let code: String
    private(set) var phase: Phase = .connecting
    var hostName: String
    private(set) var hostOs: String
    private(set) var hello: HostHello?
    private(set) var displays: [DisplayInfo] = []
    private(set) var currentDisplay = 0
    private(set) var encoder: EncoderInfo?
    private(set) var stats: Stats?
    /// 被控端的提示，按类别（capture / audio / permissions / other）各保留一条
    private(set) var notices: [String: Notice] = [:]
    private(set) var videoSize: CGSize?
    private(set) var audioOn: Bool
    private(set) var avSyncOn: Bool
    /// 锁定鼠标：一直只发移动了多少（被控端隐藏光标时会自动这样，这是手动打开的）
    var mouseLock = false
    private(set) var audioError: String?
    private(set) var quality: Quality
    /// 安全码（连上之后才有）：对方屏幕上显示的是同一个
    private(set) var safety: String?
    /// 被控端能不能接本机的麦克风（ready / busy / noDevice ...），nil：不支持
    private(set) var micState: String?
    private(set) var micOn = false
    /// 对方的防窥模式（Hello 之后对方会说，之后有变化再说）。对方不支持（Linux、手机、旧版本）、还没说、
    /// 重连的间隙里为 nil：不显示这个开关。整台被控端一个状态，别的控制端打开 / 关掉了这里也跟着变
    private(set) var privacyState: PrivacyState?
    /// 没能打开防窥的原因（英文技术信息，对方给的）
    private(set) var privacyDetail: String?
    /// 远端光标（UIKit 的画面直接观察，不经过 SwiftUI）
    @ObservationIgnored var onCursor: ((CursorState) -> Void)?
    @ObservationIgnored var onPhase: ((Phase) -> Void)?
    @ObservationIgnored private(set) var cursor = CursorState()

    /// 对方的平台：连上之后以对方报告的为准，之前按系统名称猜
    var os: OsKind {
        if let p = hello?.platform, p != .unknown { return p }
        return OsKind.of(hostOs)
    }

    /// 只能看（对方是 iPhone / iPad）
    var viewOnly: Bool { os == .ios }

    @ObservationIgnored private weak var app: AppModel?
    @ObservationIgnored private let client: ControllerClient
    @ObservationIgnored private let saved: SavedDevice?
    @ObservationIgnored private var resume: String?
    @ObservationIgnored private var hostKey: String?
    @ObservationIgnored private var pairing: String?
    @ObservationIgnored private let wake = NSCondition()
    @ObservationIgnored private var woken = false
    @ObservationIgnored private var closed = false
    @ObservationIgnored private var background = false
    /// 远程画面不在屏幕上（回到了首页）：降到 5 fps，声音照常
    @ObservationIgnored private var hidden = false
    @ObservationIgnored private var connectedOnce = false
    @ObservationIgnored private var handle: Int64 = 0
    @ObservationIgnored private var sid: Int64 = -1
    @ObservationIgnored private var cursorSlots: [Int: CursorState] = [:]
    @ObservationIgnored let sync = AvSync()
    @ObservationIgnored private(set) var decoder: VideoDecoder!
    @ObservationIgnored private var audio: AudioPlayer?
    @ObservationIgnored private var audioToken: Int64 = 0
    @ObservationIgnored private let audioLock = NSLock()
    @ObservationIgnored private var mic: MicCapture?
    @ObservationIgnored private var sender: InputSender!
    @ObservationIgnored private var sentSettings: Quality?
    @ObservationIgnored private var sentAudio = false
    /// 要防窥模式（开关）：重连时在连接选项里带上，对方就一直保持着。跟着对方说的状态变。只在主线程上用
    @ObservationIgnored private var privacyWanted = false
    @ObservationIgnored private var zoomShown: CGSize?
    @ObservationIgnored private var zoomWanted: CGSize?
    @ObservationIgnored private var zoomWork: DispatchWorkItem?
    @ObservationIgnored private var frameLimit: (Codec, Int)?
    @ObservationIgnored private var handlerIds: [UUID] = []
    @ObservationIgnored private var grantName = ""
    @ObservationIgnored private var grantOs = ""
    @ObservationIgnored var onClipboardFiles: ((Int64, [JSON]) -> Void)?

    init(app: AppModel, code: String, resume: String?, hostKey: String?, pairing: String?) {
        self.app = app
        self.client = app.controller
        self.code = code
        self.resume = resume
        self.hostKey = hostKey
        self.pairing = pairing
        saved = app.devices.get(code)
        hostName = saved?.title ?? Code.format(code)
        hostOs = saved?.hostOs ?? ""
        audioOn = app.settings.audio
        avSyncOn = app.settings.avSync
        let codec = VideoDecoder.decodable.first { Codec.priority.contains($0) } ?? .h264
        quality = Preset.of(app.settings.preset).quality(codec)
        decoder = VideoDecoder(sync: sync, requestKeyframe: { [weak self] in
            self?.command { Commands.keyframe($0) }
        }, onTooLarge: { [weak self] c, w, h in self?.onTooLarge(c, w, h) })
        sender = InputSender { [weak self] events in
            guard let self, !self.viewOnly else { return }
            self.command { Commands.input($0, events) }
        }
    }

    func start() {
        handlerIds.append(client.onHostOnline { [weak self] c in if c == self?.code { self?.signal() } })
        var was: ServerState?
        handlerIds.append(client.onServer { [weak self] s in
            if s.isConnected, was?.isConnected != true { self?.signal() }
            was = s
        })
        let t = Thread { [weak self] in self?.run() }
        t.name = "airhand-session"
        t.qualityOfService = .userInteractive
        t.start()
    }

    // ───────────────────────────── 连接 ─────────────────────────────

    private func signal() {
        wake.lock()
        woken = true
        wake.broadcast()
        wake.unlock()
    }

    private func sleep(_ seconds: Double) {
        wake.lock()
        if !woken && !closed { _ = wake.wait(until: Date().addingTimeInterval(seconds)) }
        woken = false
        wake.unlock()
    }

    private func main(_ f: @escaping () -> Void) { DispatchQueue.main.async(execute: f) }

    private func setPhase(_ p: Phase) {
        main {
            guard self.phase != .closed || p == .closed else { return }
            self.phase = p
            self.onPhase?(p)
        }
    }

    private func run() {
        client.acquire()
        defer {
            stopAudio()
            client.release()
            if closed { setPhase(.closed) }
        }
        var attempts = 0
        while !closed {
            if let p = waitServer(connectedOnce ? .infinity : 12) {
                setPhase(.failed(p))
                break
            }
            if closed { break }
            let h = client.handle
            // 画质、显示器、防窥这些状态都在主线程上改：在主线程上读
            let (settings, privacy) = DispatchQueue.main.sync { (currentSettings(), privacyWanted) }
            sentSettings = settings
            let withAudio = audioOn && !background
            sentAudio = withAudio
            // 防窥模式也在握手里就说：断线重连时对方一直保持着，它的屏幕不会在重连的间隙露出来
            let cmd = Commands.connect(code: code, resume: resume, settings: settings, hostKey: hostKey, pairing: pairing, audio: withAudio,
                                       privacy: privacy)
            let r = Json.parse(Native.ctlCall(h, Json.string(cmd))) ?? ["err": ["kind": "failed", "detail": "bad reply"]]
            if closed {
                if let ok = r.obj("ok"), let s = ok.int("sid") { _ = Native.ctlCall(h, Json.string(Commands.disconnect(UInt32(s)))) }
                break
            }
            if let ok = r.obj("ok"), let s = ok.int("sid") {
                attempts = 0
                onConnected(h, ok, UInt32(s))
                guard let end = pump(h, UInt32(s)) else { break }
                sid = -1
                // 对方还没重新说它的防窥模式（要不要开着留着，重连时告诉它）
                main {
                    self.privacyState = nil
                    self.privacyDetail = nil
                }
                decoder.resetStream()
                stopAudio()
                stopMic()
                if closed { break }
                if !end.retry {
                    setPhase(connectedOnce ? .ended(end.problem) : .failed(end.problem))
                    break
                }
                setPhase(.reconnecting(end.problem))
                sleep(0.8)
                continue
            }
            let p = Problem.parse(r.obj("err"))
            attempts += 1
            switch p.kind {
            case "hostOffline":
                setPhase(.waitingHost(p))
                sleep(60)
            case "kicked":
                let retryPlain = resume != nil && !connectedOnce
                resume = nil
                if retryPlain { continue }
                setPhase(connectedOnce ? .ended(p) : .failed(p))
                return
            case "notConnected", "serverTimeout", "serverLost":
                if !connectedOnce && attempts >= 3 {
                    setPhase(.failed(p))
                    return
                }
                if connectedOnce { setPhase(.reconnecting(p)) }
                sleep(1.6)
            default:
                setPhase(connectedOnce ? .ended(p) : .failed(p))
                return
            }
        }
    }

    /// 等到连上服务器。返回 nil 表示连上了；第一次连接时等不到就返回原因
    private func waitServer(_ limit: Double) -> Problem? {
        let start = Date()
        while !closed {
            let s = client.state
            switch s {
            case .connected:
                if client.handle != 0 { return nil }
            case .disconnected(let reason):
                if !connectedOnce && ["incompatible", "certMismatch", "failed"].contains(reason.kind) { return reason }
                if Date().timeIntervalSince(start) > limit { return reason }
            case .connecting:
                if Date().timeIntervalSince(start) > limit { return Problem(kind: "serverTimeout") }
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        return nil
    }

    private func onConnected(_ h: Int64, _ ok: JSON, _ s: UInt32) {
        handle = h
        sid = Int64(s)
        cursorSlots.removeAll()
        resume = Resume.fromJson(ok.arr("resume")) ?? resume
        if let k = ok.str("hostKey"), !k.isEmpty { hostKey = k }
        let safety = ok.str("safety")
        pairing = nil
        grantName = ok.str("hostName") ?? ""
        grantOs = ok.str("hostOs") ?? ""
        main {
            self.safety = safety?.isEmpty == false ? safety : nil
            if !self.grantName.isEmpty, self.saved?.alias.isEmpty != false { self.hostName = self.grantName }
            if !self.grantOs.isEmpty { self.hostOs = self.grantOs }
            self.micState = nil
            self.micOn = false
            // 防窥模式：对方在 Hello 之后重新说（要不要开着已经在握手里告诉它了）
            self.privacyState = nil
            self.privacyDetail = nil
        }
        if audioOn && !background { startAudio(tell: !sentAudio) } else if sentAudio { command { Commands.audio($0, false) } }
        if background { applyBackground() }
        Log.info("connected to \(code) (\(grantName), \(grantOs))")
    }

    private func onHello() {
        app?.devices.connected(code: code, name: grantName, os: grantOs, resume: resume ?? "", hostKey: hostKey)
        connectedOnce = true
        setPhase(.live)
    }

    /// 读会话数据直到会话结束。返回 (原因, 是否重连)；用户关闭时返回 nil
    private func pump(_ h: Int64, _ s: UInt32) -> (problem: Problem, retry: Bool)? {
        while !closed {
            if client.handle != h { return (Problem(kind: "serverLost", detail: "connection restarted"), true) }
            switch Native.ctlPollSession(h, s, 250) {
            case .timeout: continue
            case .gone: return (Problem(kind: "sessionEnded"), true)
            case .data(let b):
                if b.first == UInt8(ascii: "{") {
                    guard let e = Json.parse(b) else { continue }
                    if e.str("type") == "closed" {
                        _ = Native.ctlCall(h, Json.string(Commands.disconnect(s)))
                        return (Problem.parse(e.obj("reason")), (e["retry"] as? Bool) ?? true)
                    }
                    handleEvent(e)
                } else {
                    handlePacket(h, s, b)
                }
            }
        }
        return nil
    }

    private func handlePacket(_ h: Int64, _ s: UInt32, _ b: Data) {
        b.withUnsafeBytes { raw in
            switch Media.kind(raw) {
            case Media.video:
                guard let v = Media.video(raw) else {
                    Native.ctlAck(h, s, Media.seq(raw))
                    return
                }
                let size = CGSize(width: v.width, height: v.height)
                if videoSizeCache != size {
                    videoSizeCache = size
                    main { self.videoSize = size }
                }
                decoder.submit(VideoPacket(seq: v.seq, key: v.key, codec: v.codec, width: v.width, height: v.height,
                                           ptsUs: v.captureUs, data: b, offset: Media.headerLen(raw)) {
                    Native.ctlAck(h, s, v.seq)
                })
            case Media.cursor:
                let c = Media.cursor(raw)
                var shape: CursorState?
                if let sh = c.shape {
                    if let img = Self.cursorImage(raw, sh.w, sh.h) {
                        shape = CursorState(visible: c.visible, image: img, hotX: sh.hx, hotY: sh.hy)
                        cursorSlots[c.slot] = shape
                    }
                } else if c.cached {
                    shape = cursorSlots[c.slot]
                }
                var next = shape ?? cursor
                next.visible = c.visible
                next.inFrame = c.inFrame
                cursor = next
                main { self.onCursor?(next) }
            case Media.audio:
                let hl = Media.headerLen(raw)
                let start = raw[1] & 1 != 0
                let pts = Int64(bitPattern: Media.u64(raw, 16))
                audioLock.lock()
                let a = audio
                audioLock.unlock()
                a?.submit(AudioPacket(start: start, ptsUs: pts, data: b.subdata(in: hl..<b.count), arrivedNs: Clock.nowNs))
            default: break
            }
        }
    }

    @ObservationIgnored private var videoSizeCache: CGSize?

    private func handleEvent(_ e: JSON) {
        switch e.str("type") {
        case "hello":
            let codecs = e.strings("codecs").compactMap(Codec.fromWire)
            let h = HostHello(hostname: e.str("hostname") ?? "", os: e.str("os") ?? "", version: e.str("version") ?? "",
                              displays: DisplayInfo.parseList(e.arr("displays")), current: e.int("current") ?? 0,
                              codecs: codecs, platform: OsKind.fromWire(e.str("platform")), touch: e.bool("touch"),
                              actions: e.strings("actions").compactMap { SystemAction(rawValue: $0) })
            main {
                self.hello = h
                self.displays = h.displays
                self.currentDisplay = h.current
                if !h.hostname.isEmpty, self.saved?.alias.isEmpty != false { self.hostName = h.hostname }
                let best = self.pickCodec(codecs)
                if self.quality.codec != best {
                    var q = self.quality
                    q.codec = best
                    self.setQuality(q)
                } else {
                    self.refreshSettings()
                }
            }
            onHello()
        case "encoder":
            let info = EncoderInfo(codec: Codec.fromWire(e.str("codec")) ?? .h264, name: e.str("name") ?? "", hardware: e.bool("hardware"),
                                   width: e.int("width") ?? 0, height: e.int("height") ?? 0)
            main { self.encoder = info }
        case "displays":
            let list = DisplayInfo.parseList(e.arr("displays"))
            let cur = e.int("current") ?? 0
            main {
                self.displays = list
                self.currentDisplay = cur
                self.frameLimit = nil
                self.refreshSettings()
            }
        case "stats":
            let st = Stats(fps: e.dbl("fps") ?? 0, kbps: e.dbl("kbps") ?? 0, rttMs: e.dbl("rttMs"), width: e.int("width") ?? 0,
                           height: e.int("height") ?? 0, direct: e.bool("direct"))
            main { self.stats = st }
        case "notice":
            guard let o = e.obj("notice") else { return }
            let n = Notice(kind: o.str("kind") ?? "", detail: o.str("detail"), screen: o.bool("screen"), input: o.bool("input"))
            main {
                var m = self.notices
                switch n.kind {
                case "captureRecovered": m["capture"] = nil
                case "audioRecovered": m["audio"] = nil
                case "captureFailed": m["capture"] = n
                case "audioFailed": m["audio"] = n
                case "missingPermissions": m["permissions"] = (!n.screen && !n.input) ? nil : n
                default: m[n.kind] = n
                }
                self.notices = m
            }
        case "clipboard":
            if let text = e.str("text"), !text.isEmpty {
                main {
                    UIPasteboard.general.string = text
                    self.app?.show(L.t("clipboard_received"))
                }
            }
        case "clipboardFiles":
            let items = (e.arr("items") ?? []).compactMap { $0 as? JSON }
            let clip = e.int("clip") ?? 0
            guard handle != 0, sid >= 0, !items.isEmpty else { return }
            let r = Json.parse(Native.ctlCall(handle, Json.string(["cmd": "remoteFiles", "sid": sid, "clip": clip])))
            if let key = (r?["ok"] as? NSNumber)?.int64Value {
                main { ClipFiles.received(key: key, items: items) }
            }
        case "fileError":
            Log.warn("a clipboard file couldn't be transferred: \(e.str("name") ?? "") (\(e.str("error") ?? ""))")
        case "mic":
            let state = e.str("state")
            main {
                self.micState = state
                if state != "ready" && state != "preparing" && self.micOn { self.setMic(false) }
            }
        case "privacy":
            let state = PrivacyState.fromWire(e.str("state"))
            let detail = e.str("detail")
            main {
                let was = self.privacyState
                // 开关跟着对方说的变：别的控制端打开 / 关掉了、没能打开，都以对方的为准
                self.privacyWanted = state.active
                self.privacyState = state
                self.privacyDetail = detail
                // 没能打开（Windows 换虚拟显示器要几秒，用户可能已经关了菜单）：提示一下
                if state == .failed, was != .failed { self.app?.show(L.t("privacy_failed", detail ?? "—")) }
            }
        default: break
        }
    }

    // ───────────────────────────── 画质 ─────────────────────────────

    private func currentSettings() -> Quality {
        var q = quality
        q.maxShort = decodableMaxShort(q)
        if background { q.fps = 2 } else if hidden { q.fps = min(q.fps, 5) }
        return q
    }

    private func decodableMaxShort(_ q: Quality) -> Int {
        var s = q.maxShort
        if let d = displays.first(where: { $0.index == currentDisplay }), let z = zoomShown {
            s = VideoFit.looser(s, VideoFit.zoomMaxShort(d.width, d.height, z.width, z.height))
        }
        if let (c, limit) = frameLimit, c == q.codec { s = VideoFit.stricter(s, limit) }
        return s
    }

    private func onTooLarge(_ c: Codec, _ w: Int, _ h: Int) {
        main {
            let short = min(w, h)
            let limit = short > 1440 ? 1440 : (short > 1080 ? 1080 : 720)
            self.frameLimit = (c, limit)
            Log.info("\(w)x\(h) doesn't fit the \(c.label) decoder, asking for a short side of at most \(limit)")
            self.refreshSettings()
        }
    }

    private func refreshSettings() {
        if currentSettings() != sentSettings { sendSettings() }
    }

    private func sendSettings() {
        let s = currentSettings()
        sentSettings = s
        command { Commands.settings($0, s) }
    }

    // ───────────────────────────── 命令 ─────────────────────────────

    /// 给当前会话发命令。没连上时丢掉
    func command(_ make: @escaping (UInt32) -> JSON) {
        let h = handle
        let s = sid
        guard h != 0, s >= 0, !closed else { return }
        let cmd = make(UInt32(s))
        let r = Native.ctlCall(h, Json.string(cmd))
        if r.contains("\"err\"") { Log.warn("\(cmd.str("cmd") ?? "?") failed: \(r)") }
    }

    /// 输入事件（触控、键盘）。`now=false` 的会和后面的鼠标移动合并
    func input(_ events: [JSON], now: Bool = true) {
        if !viewOnly, phase == .live { sender.send(events, now: now) }
    }

    func setQuality(_ q: Quality) {
        quality = q
        sendSettings()
    }

    /// 画面放大了 / 缩回去了：放大时向对方要放大后看得清的分辨率，手指停下来一会儿才发
    func setZoom(_ zoomed: Bool, shown: CGSize) {
        let want = zoomed ? shown : nil
        if want == zoomWanted { return }
        zoomWanted = want
        zoomWork?.cancel()
        let w = DispatchWorkItem { [weak self] in
            self?.zoomShown = want
            self?.refreshSettings()
        }
        zoomWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: w)
    }

    /// 双方都支持的编码格式（对方能编码、本机能硬件解码），压缩率从高到低
    var usableCodecs: [Codec] { Codec.priority.filter { (hello?.codecs ?? []).contains($0) && VideoDecoder.decodable.contains($0) } }

    private func pickCodec(_ host: [Codec]) -> Codec {
        let usable = Codec.priority.filter { host.contains($0) && VideoDecoder.decodable.contains($0) }
        if let p = Codec.fromWire(app?.settings.codec), usable.contains(p) { return p }
        return usable.first ?? .h264
    }

    /// 选编码格式（nil = 自动）：记进设置，以后的会话也用它
    func setCodec(_ c: Codec?) {
        app?.settings.codec = c?.wire ?? ""
        guard let host = hello?.codecs else { return }
        let next = pickCodec(host)
        if quality.codec != next {
            var q = quality
            q.codec = next
            setQuality(q)
        }
    }

    func selectDisplay(_ index: Int) { command { Commands.display($0, index) } }

    func setAudio(_ on: Bool) {
        audioOn = on
        if on { startAudio() } else { stopAudio() }
    }

    func setAvSync(_ on: Bool) {
        avSyncOn = on
        app?.settings.avSync = on
        audioLock.lock()
        let playing = audio != nil
        audioLock.unlock()
        sync.setEnabled(on && playing)
        decoder.refreshTiming()
    }

    private func startAudio(tell: Bool = true) {
        guard !closed, !background, audioOn else { return }
        audioLock.lock()
        if audio == nil {
            let token = sync.reset()
            audioToken = token
            sync.setEnabled(avSyncOn)
            audio = AudioPlayer(sync: sync, token: token) { [weak self] error in
                guard let self else { return }
                self.audioLock.lock()
                if self.audioToken == token {
                    self.audio = nil
                    self.audioToken = self.sync.reset()
                    self.sync.setEnabled(false)
                }
                self.audioLock.unlock()
                self.decoder.refreshTiming()
                self.main { self.audioError = error }
            }
            main { self.audioError = nil }
        }
        audioLock.unlock()
        if tell { command { Commands.audio($0, true) } }
    }

    private func stopAudio() {
        command { Commands.audio($0, false) }
        audioLock.lock()
        let a = audio
        audio = nil
        audioToken = sync.reset()
        audioLock.unlock()
        a?.release()
        sync.setEnabled(false)
        decoder.refreshTiming()
    }

    // ── 麦克风 ──

    /// 打开 / 关闭本机麦克风（转发给被控端，它那边的程序当作一个麦克风用）
    func setMic(_ on: Bool) {
        if on == micOn { return }
        if on {
            guard micState == "ready" else {
                app?.show(L.t(micState == "busy" ? "mic_busy" : "mic_unavailable"))
                return
            }
            MicCapture.requestPermission { [weak self] granted in
                guard let self else { return }
                guard granted else {
                    self.app?.show(L.t("mic_denied"))
                    return
                }
                let h = self.handle
                let s = self.sid
                guard h != 0, s >= 0 else { return }
                self.command { Commands.mic($0, true) }
                let m = MicCapture { packet in Native.ctlMic(h, UInt32(s), packet) }
                do {
                    try m.start()
                    self.mic = m
                    self.micOn = true
                    self.app?.show(L.t("mic_on_toast"))
                } catch {
                    Log.warn("can't start the microphone: \(error)")
                    self.command { Commands.mic($0, false) }
                    self.app?.show(L.t("mic_unavailable"))
                }
            }
        } else {
            stopMic()
        }
    }

    private func stopMic() {
        let m = mic
        mic = nil
        m?.stop()
        if micOn || m != nil { command { Commands.mic($0, false) } }
        main { self.micOn = false }
    }

    // ── 防窥模式 ──

    /// 打开 / 关闭对方的防窥模式：对方所有的显示器黑屏、键盘鼠标不起作用，只有控制端看得到、操作得了。
    /// 先显示成正在打开 / 已关闭，对方换好了再说（Windows 换成虚拟显示器要几秒）。对方还没说它的状态时只记下来
    func setPrivacy(_ on: Bool) {
        guard !viewOnly else { return }
        if let s = privacyState, !s.usable { return }
        privacyWanted = on
        guard privacyState != nil, handle != 0, sid >= 0, !closed else { return }
        command { Commands.privacy($0, on) }
        privacyDetail = nil
        privacyState = on ? .starting : .off
    }

    // ── 其他 ──

    /// 系统操作（只有对方在 Hello 里列出的才发）
    func system(_ a: SystemAction) {
        if hello?.actions.contains(a) == true { input([Input.system(a)]) }
    }

    func sendClipboard(_ text: String) {
        if !text.isEmpty { command { Commands.clipboard($0, text) } }
    }

    /// 以键盘输入的方式把文本打到对方当前的输入框里
    func typeText(_ text: String) {
        var rest = Substring(text)
        while !rest.isEmpty {
            let chunk = rest.prefix(512)
            input([Input.text(String(chunk))])
            rest = rest.dropFirst(chunk.count)
        }
    }

    /// 把本机的文件放进对方的剪贴板（对方粘贴时才传）。文件描述符不管成不成都归这里：
    /// 交给 Rust 就由 Rust 关（出错时也是），没连着时在这里关
    func offerFiles(_ files: [JSON]) -> Bool {
        let h = handle
        let s = sid
        guard h != 0, s >= 0, !closed else {
            for f in files { if let fd = f.int("fd") { Darwin.close(Int32(fd)) } }
            return false
        }
        let r = Native.ctlCall(h, Json.string(["cmd": "offerFiles", "sid": s, "files": files]))
        return !r.contains("\"err\"")
    }

    func retryNow() { signal() }

    func setHidden(_ h: Bool) {
        if hidden == h { return }
        hidden = h
        sendSettings()
    }

    /// 画面不在屏幕上（应用切到后台）：降到很低的帧率，回来时恢复
    func setBackground(_ bg: Bool) {
        if background == bg { return }
        background = bg
        applyBackground()
    }

    private func applyBackground() {
        sendSettings()
        if background {
            stopAudio()
            stopMic()
        } else if audioOn {
            startAudio()
        }
        // 回到前台：系统可能已经让解码器失效了，从关键帧重新开始
        if !background { decoder.restart() }
    }

    func close() {
        if closed { return }
        closed = true
        signal()
        handlerIds.forEach { client.removeHandler($0) }
        let h = handle
        let s = sid
        if h != 0, s >= 0 {
            DispatchQueue.global().async { _ = Native.ctlCall(h, Json.string(Commands.disconnect(UInt32(s)))) }
        }
        stopMic()
        stopAudio()
        decoder.release()
        if case .failed = phase {} else { phase = .closed }
    }

    /// 光标形状：头后面的 RGBA（非预乘）→ CGImage
    static func cursorImage(_ b: UnsafeRawBufferPointer, _ w: Int, _ h: Int) -> CGImage? {
        let o = Media.headerLen(b)
        guard w > 0, h > 0, w <= 256, h <= 256, b.count >= o + w * h * 4 else { return nil }
        let data = Data(bytes: b.baseAddress!.advanced(by: o), count: w * h * 4)
        guard let provider = CGDataProvider(data: data as CFData) else { return nil }
        return CGImage(width: w, height: h, bitsPerComponent: 8, bitsPerPixel: 32, bytesPerRow: w * 4,
                       space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                       provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)
    }
}

/// 向对方要多大的画面（被控端只认“短边上限”），和 Android 的 VideoFit 一样
enum VideoFit {
    static let zoomSteps = [720, 1080, 1440, 2160]

    /// 双指放大之后要的短边上限（按档位往上取，够到对方屏幕的短边就是原始分辨率 0）
    static func zoomMaxShort(_ srcW: Int, _ srcH: Int, _ shownW: CGFloat, _ shownH: CGFloat) -> Int {
        let short = min(srcW, srcH)
        guard short > 0, shownW > 0, shownH > 0 else { return 0 }
        let need = CGFloat(short) * min(shownW / CGFloat(srcW), shownH / CGFloat(srcH)) / 1.04
        guard let step = zoomSteps.first(where: { CGFloat($0) >= need }) else { return 0 }
        return step >= short ? 0 : step
    }

    static func stricter(_ a: Int, _ b: Int) -> Int { a == 0 ? b : (b == 0 ? a : min(a, b)) }
    static func looser(_ a: Int, _ b: Int) -> Int { (a == 0 || b == 0) ? 0 : max(a, b) }
}
