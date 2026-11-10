// 和 Rust、Android 共用的协议样例（crates/airhand-link/tests/protocol.json，由 Rust 的测试生成）：
// 媒体包按 media.ts 解析出来要和 Rust 打包时一样；输入事件、命令、连接选项的 JSON 形状要和 api.ts 的类型对得上。
import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs";
import * as media from "../src/lib/media.ts";
import type { Command, ConnectOptions, InputEvent } from "../src/lib/api.ts";

const fixture = JSON.parse(
  fs.readFileSync(new URL("../../crates/airhand-link/tests/protocol.json", import.meta.url), "utf8"),
);

const bytes = (hex: string) => Uint8Array.from(hex.match(/../g) ?? [], (b) => parseInt(b, 16));
const hex = (b: Uint8Array) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");

test("媒体包：头和数据按 media.ts 解析，负载从头里写的长度处开始", () => {
  assert.ok(fixture.media.length >= 7);
  // 有一个包的头比 24 字节长（以后的版本加了字段）
  assert.ok(fixture.media.some((m: { hex: string }) => bytes(m.hex)[3] > media.HEADER_LEN));
  for (const m of fixture.media) {
    const pkt = bytes(m.hex);
    const dv = new DataView(pkt.buffer);
    assert.equal(media.kind(dv), m.kind, m.name);
    assert.equal(hex(media.payload(pkt, dv)), m.payload, m.name);
    if (m.kind === media.VIDEO) {
      const h = media.videoHeader(dv);
      assert.deepEqual(h, { seq: m.seq, key: m.key, codec: m.codec, width: m.width, height: m.height, captureUs: m.captureUs }, m.name);
      assert.equal(media.videoSeq(dv), m.seq);
    } else if (m.kind === media.AUDIO) {
      assert.deepEqual(media.audioHeader(dv), { seq: m.seq, start: m.start, captureUs: m.captureUs }, m.name);
      assert.equal(media.videoSeq(dv), null);
    } else {
      assert.deepEqual(media.cursorHeader(dv), { visible: m.visible, slot: m.slot, shape: m.shape, cached: m.cached }, m.name);
    }
  }
  assert.equal(media.kind(new DataView(new ArrayBuffer(10))), 0, "不到一个头长");
  // 不认识的编码格式：不猜成 H.264（序号照样读得出来，收到就确认）
  const first = bytes(fixture.media[0].hex);
  first[2] = 9;
  assert.equal(media.videoHeader(new DataView(first.buffer)), null);
  assert.equal(media.videoSeq(new DataView(first.buffer)), fixture.media[0].seq);
  // 头的长度不对（比 24 短、比包还长）：不是合法的包
  for (const bad of [23, 200]) {
    const p = bytes(fixture.media[0].hex);
    p[3] = bad;
    assert.equal(media.kind(new DataView(p.buffer)), 0, `头的长度 ${bad}`);
    assert.equal(media.videoSeq(new DataView(p.buffer)), null);
  }
});

test("输入事件、命令和连接选项的 JSON 形状与 Rust 一致", () => {
  // 样例里的每一种输入事件、每一个命令 api.ts 都有（新加了 Rust 这边要记得补上类型）
  const events: InputEvent[] = fixture.input;
  const commands: Command[] = fixture.commands;
  const kinds = events.map((e) => e.type);
  assert.deepEqual(kinds, ["mouseMove", "mouseDelta", "mouseButton", "wheel", "key", "text", "releaseAll", "touch", "system"]);
  const delta: InputEvent = { type: "mouseDelta", dx: -3, dy: 7 };
  assert.deepEqual(events[1], delta);
  const button: InputEvent = { type: "mouseButton", button: "right", down: true };
  assert.deepEqual(events[2], button);
  const cmds = commands.map((c) => c.cmd).sort();
  assert.deepEqual(cmds, ["ack", "audio", "clipboard", "clipboardSync", "disconnect", "display", "input", "keyframe", "settings"]);
  const settings = commands.find((c) => c.cmd === "settings");
  assert.deepEqual(settings, {
    cmd: "settings",
    sid: 3,
    settings: { fps: 60, bitrateKbps: 8000, maxShort: 1080, adaptive: true, codec: "h265" },
  });
  // 连接选项：字段和 ConnectOptions 一样
  const opts: ConnectOptions = fixture.connectOptions;
  assert.deepEqual(Object.keys(opts).sort(), ["audio", "hostKey", "pairing", "resume", "settings"]);
  assert.equal(opts.settings.codec, "h265");
});
