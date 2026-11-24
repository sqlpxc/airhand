import XCTest
@testable import Airhand

/// 和 Rust、TypeScript、Kotlin 共用的协议样例（crates/airhand-link/tests/protocol.json）：iOS 的解析和拼法对不上就失败
final class ProtocolTests: XCTestCase {
    private static let fixture: JSON = {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../../crates/airhand-link/tests/protocol.json")
        return Json.parse(try! Data(contentsOf: url))!
    }()

    private func hex(_ s: String) -> Data {
        var d = Data()
        var i = s.startIndex
        while i < s.endIndex {
            let j = s.index(i, offsetBy: 2)
            d.append(UInt8(s[i..<j], radix: 16)!)
            i = j
        }
        return d
    }

    /// 两个 JSON 值相等（字典不看键的顺序，数字不看类型）
    private func same(_ a: Any, _ b: Any) -> Bool {
        let opts: JSONSerialization.WritingOptions = [.sortedKeys]
        let da = try! JSONSerialization.data(withJSONObject: ["v": a], options: opts)
        let db = try! JSONSerialization.data(withJSONObject: ["v": b], options: opts)
        return NSDictionary(dictionary: Json.parse(da)!) == NSDictionary(dictionary: Json.parse(db)!)
    }

    func testMediaPackets() {
        let samples = (Self.fixture["media"] as! [Any]).compactMap { $0 as? JSON }
        XCTAssertFalse(samples.isEmpty)
        for s in samples {
            let name = s.str("name") ?? "?"
            let data = hex(s.str("hex")!)
            data.withUnsafeBytes { b in
                XCTAssertEqual(Int(Media.kind(b)), s.int("kind"), name)
                let payload = Data(b[Media.headerLen(b)...])
                XCTAssertEqual(payload, hex(s.str("payload") ?? ""), name)
                switch Media.kind(b) {
                case Media.video:
                    let v = Media.video(b)!
                    XCTAssertEqual(Int(v.seq), s.int("seq"), name)
                    XCTAssertEqual(v.key, s.bool("key"), name)
                    XCTAssertEqual(v.codec.wire, s.str("codec"), name)
                    XCTAssertEqual(v.width, s.int("width"), name)
                    XCTAssertEqual(v.height, s.int("height"), name)
                    XCTAssertEqual(v.captureUs, (s["captureUs"] as! NSNumber).int64Value, name)
                case Media.audio:
                    XCTAssertEqual(Int(Media.seq(b)), s.int("seq"), name)
                    XCTAssertEqual(b[1] & 1 != 0, s.bool("start"), name)
                    XCTAssertEqual(Int64(bitPattern: Media.u64(b, 16)), (s["captureUs"] as! NSNumber).int64Value, name)
                case Media.cursor:
                    let c = Media.cursor(b)
                    XCTAssertEqual(c.visible, s.bool("visible"), name)
                    XCTAssertEqual(c.slot, s.int("slot"), name)
                    XCTAssertEqual(c.cached, s.bool("cached"), name)
                    XCTAssertEqual(c.inFrame, s.bool("inFrame"), name)
                    if let shape = s.obj("shape") {
                        XCTAssertEqual(c.shape?.w, shape.int("width"), name)
                        XCTAssertEqual(c.shape?.h, shape.int("height"), name)
                        XCTAssertEqual(c.shape?.hx, shape.int("hotX"), name)
                        XCTAssertEqual(c.shape?.hy, shape.int("hotY"), name)
                    } else {
                        XCTAssertNil(c.shape, name)
                    }
                default:
                    XCTFail("unknown kind in \(name)")
                }
            }
        }
    }

    func testMicPacketRoundTrips() {
        let p = Media.audioPacket(seq: 3, start: true, captureUs: 123_456_809_012, opus: Data([0xfc, 0xff, 0xfe]))
        let audio = (Self.fixture["media"] as! [Any]).compactMap { $0 as? JSON }.first { $0.str("name") == "audio" }!
        XCTAssertEqual(p, hex(audio.str("hex")!))
    }

    func testInputEvents() {
        let mine: [JSON] = [
            Input.move(0.5, 0.25), Input.delta(-3, 7), Input.button("right", true), Input.wheel(0, -120), Input.key("KeyA", false),
            Input.text("你好"), Input.releaseAll(), Input.touch(1, "down", 0.25, 0.75), Input.system(.back),
        ]
        let theirs = Self.fixture["input"] as! [Any]
        XCTAssertEqual(mine.count, theirs.count)
        for (a, b) in zip(mine, theirs) { XCTAssertTrue(same(a, b), "\(a) != \(b)") }
    }

    func testCommands() {
        let theirs = (Self.fixture["commands"] as! [Any]).compactMap { $0 as? JSON }
        func find(_ cmd: String) -> JSON { theirs.first { $0.str("cmd") == cmd }! }
        XCTAssertTrue(same(Commands.input(3, [Input.delta(1, 2), Input.releaseAll()]), find("input")))
        XCTAssertTrue(same(Commands.settings(3, Quality(fps: 60, kbps: 8000, maxShort: 1080, adaptive: true, codec: .h265)), find("settings")))
        XCTAssertTrue(same(Commands.keyframe(3), find("keyframe")))
        XCTAssertTrue(same(Commands.display(3, 1), find("display")))
        XCTAssertTrue(same(Commands.audio(3, true), find("audio")))
        XCTAssertTrue(same(Commands.mic(3, true), find("mic")))
        XCTAssertTrue(same(Commands.clipboard(3, "hi"), find("clipboard")))
        XCTAssertTrue(same(Commands.privacy(3, true), find("privacy")))
    }

    /// 会话事件 `privacy` 的状态（Rust 的 `PrivacyState`，camelCase）；不认识的当作开不了
    func testPrivacyStates() {
        XCTAssertEqual(PrivacyState.allCases.map(\.rawValue), ["unavailable", "notAllowed", "off", "starting", "on", "failed"])
        XCTAssertEqual(PrivacyState.fromWire("notAllowed"), .notAllowed)
        XCTAssertEqual(PrivacyState.fromWire("somethingNew"), .unavailable)
        XCTAssertEqual(PrivacyState.fromWire(nil), .unavailable)
        XCTAssertEqual(PrivacyState.allCases.filter(\.active), [.starting, .on])
        XCTAssertEqual(PrivacyState.allCases.filter(\.usable), [.off, .starting, .on, .failed])
    }

    func testConnectOptions() {
        let o = Self.fixture.obj("connectOptions")!
        let s = o.obj("settings")!
        let q = Quality(fps: s.int("fps")!, kbps: s.int("bitrateKbps")!, maxShort: s.int("maxShort")!, adaptive: s.bool("adaptive"),
                        codec: Codec.fromWire(s.str("codec"))!)
        let resume = Resume.fromJson(o.arr("resume"))
        XCTAssertNotNil(resume)
        var c = Commands.connect(code: "123456", resume: resume, settings: q, hostKey: o.str("hostKey"), pairing: o.str("pairing"), audio: o.bool("audio"),
                                 privacy: o.bool("privacy"))
        XCTAssertTrue(o.bool("privacy"))
        c["cmd"] = nil
        c["code"] = nil
        XCTAssertTrue(same(c, o), "\(c) != \(o)")
    }

    func testCodes() {
        XCTAssertEqual(Code.parse("123 456"), "123456")
        XCTAssertEqual(Code.parse("123-456"), "123456")
        XCTAssertNil(Code.parse("012345"))
        XCTAssertNil(Code.parse("12345"))
        XCTAssertEqual(Code.find("控制码：482 913，配对码 305127"), "482913")
        XCTAssertNil(Code.find("1234567"))
        XCTAssertEqual(Code.format("482913"), "482 913")
    }

    func testKeyMap() {
        XCTAssertEqual(KeyMap.code(hid: 0x04), "KeyA")
        XCTAssertEqual(KeyMap.code(hid: 0x27), "Digit0")
        XCTAssertEqual(KeyMap.code(hid: 0x1e), "Digit1")
        XCTAssertEqual(KeyMap.code(hid: 0x3a), "F1")
        XCTAssertEqual(KeyMap.code(hid: 0xe3), "MetaLeft")
        XCTAssertEqual(KeyMap.forChar("C")?.0, "KeyC")
        XCTAssertEqual(KeyMap.forChar("?")?.1, true)
    }

    func testInputCoalescing() {
        var p: [JSON] = []
        Input.append(&p, Input.move(0.1, 0.1))
        Input.append(&p, Input.move(0.2, 0.2))
        Input.append(&p, Input.delta(1, 2))
        Input.append(&p, Input.delta(3, 4))
        Input.append(&p, Input.button("left", true))
        Input.append(&p, Input.delta(1, 1))
        XCTAssertEqual(p.count, 4)
        XCTAssertEqual(p[0].dbl("x"), 0.2)
        XCTAssertEqual(p[1].int("dx"), 4)
        XCTAssertEqual(p[1].int("dy"), 6)
    }
}

final class CodecTests: XCTestCase {
    func testAnnexBToLengthPrefixed() {
        // SPS、PPS、IDR（三字节和四字节的起始码都有）
        let stream: [UInt8] = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9, 9]
        stream.withUnsafeBytes { b in
            XCTAssertEqual(AnnexB.split(b).count, 3)
            let out = AnnexB.toLengthPrefixed(b, codec: .h264)
            XCTAssertEqual([UInt8](out), [0, 0, 0, 4, 0x65, 9, 9, 9])
        }
    }

    func testParameterSets() {
        let data = Data([1, 0, 0, 0] + [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9])
        let p = VideoPacket(seq: 1, key: true, codec: .h264, width: 2, height: 2, ptsUs: 0, data: data, offset: 4) {}
        XCTAssertEqual(VideoDecoder.parameterSets(p), [Data([0x67, 1, 2]), Data([0x68, 3])])
    }

    func testAV1Obus() {
        // 时间分隔符（类型 2）、序列头（类型 1）、帧（类型 6），都带长度字段
        let stream: [UInt8] = [0x12, 0, 0x0a, 3, 1, 2, 3, 0x32, 2, 7, 7]
        stream.withUnsafeBytes { b in
            let obus = AV1.obus(b)
            XCTAssertEqual(obus.map(\.type), [2, 1, 6])
            XCTAssertEqual(AV1.sequenceHeader(b), Data([0x0a, 3, 1, 2, 3]))
            XCTAssertEqual([UInt8](AV1.stripTemporalDelimiters(b)), [0x0a, 3, 1, 2, 3, 0x32, 2, 7, 7])
        }
    }

    func testAV1ConfigRecord() {
        // 按规范拼一个序列头：main 档次、级别 8、1920×1080、8 位 4:2:0
        var bits: [Int] = []
        func put(_ v: Int, _ n: Int) { for i in (0..<n).reversed() { bits.append((v >> i) & 1) } }
        put(0, 3); put(0, 1); put(0, 1)           // profile, still_picture, reduced_still_picture_header
        put(0, 1); put(0, 1); put(0, 5)           // timing_info_present, initial_display_delay_present, operating_points_cnt_minus_1
        put(0, 12); put(8, 5); put(0, 1)          // operating_point_idc, seq_level_idx (8), seq_tier
        put(10, 4); put(10, 4); put(1919, 11); put(1079, 11)
        put(0, 1)                                 // frame_id_numbers_present
        put(1, 1); put(1, 1); put(1, 1)           // use_128x128, filter_intra, intra_edge
        put(0, 4); put(1, 1); put(0, 1); put(0, 1) // interintra…dual_filter, enable_order_hint, jnt_comp, ref_frame_mvs
        put(1, 1); put(1, 1); put(6, 3)           // seq_choose_screen_content_tools, seq_choose_integer_mv, order_hint_bits_minus_1
        put(0, 1); put(1, 1); put(1, 1)           // superres, cdef, restoration
        put(0, 1); put(0, 1); put(0, 1); put(0, 1); put(0, 2) // high_bitdepth, mono, color_description, color_range, chroma_sample_position
        put(0, 1); put(0, 1); put(1, 1)           // separate_uv_delta_q, film_grain, trailing one
        while bits.count % 8 != 0 { bits.append(0) }
        var payload: [UInt8] = []
        for i in stride(from: 0, to: bits.count, by: 8) { payload.append(UInt8(bits[i..<i + 8].reduce(0) { $0 << 1 | $1 })) }
        let obu = [0x0a, UInt8(payload.count)] + payload
        let c = AV1.configRecord(sequenceHeaderObu: Data(obu))
        XCTAssertEqual(c.map { [UInt8]($0.prefix(4)) }, [0x81, 8, 0x0c, 0])
        XCTAssertEqual(c?.count, 4 + obu.count)
    }

    func testOpusRoundTrip() throws {
        let enc = try OpusEncoder(channels: 1, bitrate: 32000)
        let dec = try OpusDecoder(channels: 1)
        var phase = 0.0
        var peak = 0
        var packets = 0
        for _ in 0..<20 {
            var pcm = [Int16](repeating: 0, count: 960)
            for i in 0..<960 {
                pcm[i] = Int16(sin(phase) * 10000)
                phase += 2 * .pi * 440 / 48000
            }
            guard let packet = pcm.withUnsafeBufferPointer({ enc.encode($0) }) else { continue }
            packets += 1
            let n = packet.withUnsafeBytes { dec.decode($0) }
            peak = max(peak, dec.pcm[0..<n].map { abs(Int($0)) }.max() ?? 0)
        }
        XCTAssertGreaterThan(packets, 15)
        XCTAssertGreaterThan(peak, 5000)
    }

    func testJitterBufferStartsAfterTarget() {
        let j = JitterBuffer()
        var pcm = [Int16](repeating: 1000, count: 1920)
        let l = UnsafeMutablePointer<Float>.allocate(capacity: 480), r = UnsafeMutablePointer<Float>.allocate(capacity: 480)
        defer { l.deallocate(); r.deallocate() }
        // 一包（20 ms）不够开始播放（目标 60 ms）
        pcm.withUnsafeBufferPointer { j.push($0, frames: 960, start: true, hostUs: 0, arrivedMs: 0) }
        XCTAssertNil(j.pull(left: l, right: r, frames: 480, finishBurst: false))
        for k in 1..<5 {
            pcm.withUnsafeBufferPointer { j.push($0, frames: 960, start: false, hostUs: Int64(k) * 20_000, arrivedMs: Double(k) * 20) }
        }
        let c = j.pull(left: l, right: r, frames: 480, finishBurst: false)
        XCTAssertNotNil(c)
        XCTAssertGreaterThan(r[479], 0.02)
        pcm[0] = 0
    }
}

/// 手势：点按、双指点按、双指滚动
final class GestureTests: XCTestCase {
    final class Out: Gestures.Out {
        var events: [String] = []
        func toRemote(_ p: CGPoint) -> CGPoint? { CGPoint(x: p.x / 1000, y: p.y / 500) }
        func contentSize() -> CGSize { CGSize(width: 1000, height: 500) }
        func move(_ nx: CGFloat, _ ny: CGFloat) { events.append("move") }
        func delta(_ nx: CGFloat, _ ny: CGFloat) { events.append("delta") }
        func button(_ b: String, _ down: Bool) { events.append("\(b)\(down ? "↓" : "↑")") }
        func wheel(_ dx: Int, _ dy: Int) { events.append("wheel") }
        func zoom(_ factor: CGFloat, _ focus: CGPoint) { events.append("zoom") }
        func pan(_ dx: CGFloat, _ dy: CGFloat) {}
        func follow() {}
        func haptic() {}
        func ripple(_ p: CGPoint) {}
        func toggleKeyboard() { events.append("keyboard") }
        func touch(_ id: Int, _ phase: String, _ nx: CGFloat, _ ny: CGFloat) { events.append("touch-\(phase)") }
    }

    func testTapIsLeftClick() {
        let o = Out()
        let g = Gestures()
        g.out = o
        g.down(.init(id: 1, x: 100, y: 100), 0)
        g.up(.init(id: 1, x: 100, y: 100), 80)
        XCTAssertEqual(o.events.filter { $0 != "move" }, ["left↓", "left↑"])
    }

    func testTwoFingerTapIsRightClick() {
        let o = Out()
        let g = Gestures()
        g.out = o
        let a = Gestures.P(id: 1, x: 100, y: 100), b = Gestures.P(id: 2, x: 200, y: 100)
        g.down(a, 0)
        g.pointerDown(b, [a, b], 10)
        g.pointerUp(b, remaining: 1, 80)
        g.up(a, 90)
        XCTAssertEqual(o.events.filter { $0 != "move" }, ["right↓", "right↑"])
    }

    func testTwoFingerDragScrolls() {
        let o = Out()
        let g = Gestures()
        g.out = o
        var a = Gestures.P(id: 1, x: 100, y: 100), b = Gestures.P(id: 2, x: 200, y: 100)
        g.down(a, 0)
        g.pointerDown(b, [a, b], 5)
        for i in 1...10 {
            a.y += 6; b.y += 6
            g.move([a, b], 5 + Double(i) * 16)
        }
        XCTAssertTrue(o.events.contains("wheel"))
        XCTAssertFalse(o.events.contains("left↓"))
    }

    func testMultitouchPassesFingers() {
        let o = Out()
        let g = Gestures()
        g.out = o
        g.mode = .multitouch
        let a = Gestures.P(id: 1, x: 100, y: 100)
        g.down(a, 0)
        g.move([Gestures.P(id: 1, x: 120, y: 100)], 16)
        g.up(Gestures.P(id: 1, x: 120, y: 100), 32)
        XCTAssertEqual(o.events, ["touch-down", "touch-move", "touch-up"])
    }
}
