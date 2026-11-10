import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { videoSeq } from "../src/lib/media.ts";
import { ACK_INTERVAL_MS, AckCoalescer } from "../src/lib/ack.ts";

test("desktop ACK follows WebView receipt and handles frames before connect reply", async () => {
  const source = fs.readFileSync(new URL("../src/lib/backend/tauri.ts", import.meta.url), "utf8")
    .replace(/^import[\s\S]*?from "[^"]+";\r?\n/gm, "");
  const js = stripTypeScriptTypes(source).replace(/\bexport\s+/g, "") + "\nglobalThis.backend = tauriBackend();";
  const channels: any[] = [];
  const calls: any[] = [];
  let resolve: (v: any) => void = () => {};
  const sandbox: any = {
    Channel: class { constructor() { channels.push(this); } },
    getCurrentWindow: () => ({}),
    videoSeq,
    AckCoalescer,
    invoke: (command: string, args: any) => {
      calls.push([command, args]);
      return command === "ctrl_connect" ? new Promise(r => { resolve = r; }) : Promise.resolve();
    },
  };
  vm.runInNewContext(js, sandbox);
  // 沙箱里建的对象原型不同，按 JSON 比较
  const call = (i: number) => JSON.parse(JSON.stringify(calls[i]));
  let received = 0;
  const settings = { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" };
  const opts = { resume: null, hostKey: "ab".repeat(32), pairing: "482913", settings, audio: true };
  const connected = sandbox.backend.connect("123456", opts, { packet() { received++; }, event() {} }, true);
  // 连接选项（记下的对方公钥、配对码、开始时的画质和声音）原样交给 Rust 的 ctrl_connect（就是 ConnectOptions 的 JSON）
  assert.deepEqual(JSON.parse(JSON.stringify(calls[0][1].opts)), opts);
  assert.equal(calls[0][1].code, "123456");
  const pkt = new ArrayBuffer(24);
  const dv = new DataView(pkt); dv.setUint8(0, 1); dv.setUint8(3, 24); dv.setUint32(4, 9, true);
  assert.equal(calls.length, 1);
  channels[0].onmessage(pkt);
  assert.equal(received, 1);
  assert.equal(calls.length, 1); // SID not known yet
  resolve({ sid: 5 });
  await connected;
  assert.deepEqual(call(1), ["ctrl", { command: { cmd: "ack", sid: 5, seq: 9 } }]);
  // 紧跟着到的画面：确认合并，最多晚 ACK_INTERVAL_MS 发出去，确认的是最新的那一帧（见 ack.ts）
  dv.setUint32(4, 10, true);
  channels[0].onmessage(pkt);
  dv.setUint32(4, 11, true);
  channels[0].onmessage(pkt);
  await new Promise((r) => setTimeout(r, ACK_INTERVAL_MS + 20));
  const acked = calls.length;
  assert.ok(acked === 3 || acked === 4, `${acked} calls`);
  assert.deepEqual(call(acked - 1), ["ctrl", { command: { cmd: "ack", sid: 5, seq: 11 } }]);
  // 光标包、声音包不确认
  const cursor = new ArrayBuffer(24);
  new DataView(cursor).setUint8(0, 2);
  new DataView(cursor).setUint8(3, 24);
  channels[0].onmessage(cursor);
  await new Promise((r) => setTimeout(r, ACK_INTERVAL_MS + 20));
  assert.equal(calls.length, acked);
  assert.equal(received, 4);
  // 对会话的操作都走同一个 ctrl 命令
  await sandbox.backend.command({ cmd: "keyframe", sid: 5 });
  assert.deepEqual(call(acked), ["ctrl", { command: { cmd: "keyframe", sid: 5 } }]);
});
