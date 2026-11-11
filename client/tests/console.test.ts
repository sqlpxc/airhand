// 控制台和控制端服务之间的连接（console.ts）和它的 Worker（console-worker.ts）：用假的 Worker / WebSocket 跑真的代码。
import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { ACK_INTERVAL_MS, AckCoalescer, ackMessage } from "../src/lib/ack.ts";
import { AvSync } from "../src/lib/avsync.ts";
import * as media from "../src/lib/media.ts";

/** 去掉 import / export 和类型，放进沙箱里跑 */
function load(file: string, sandbox: object, exports: string[]) {
  const source = fs.readFileSync(new URL(`../src/lib/${file}`, import.meta.url), "utf8")
    .replace(/^import[\s\S]*?from "[^"]+";\r?\n/gm, "");
  // 沙箱跑的是脚本不是模块，没有 import.meta（这里当成发布构建）
  const js = stripTypeScriptTypes(source).replace(/\bexport\s+/g, "").replace(/import\.meta\.env\.DEV/g, "false")
    + exports.map((e) => `\nglobalThis.${e} = ${e};`).join("");
  vm.runInNewContext(js, sandbox);
}

/** 沙箱里建的对象原型不同，按 JSON 比较 */
const plain = (v: unknown) => JSON.parse(JSON.stringify(v));
const tick = () => new Promise((r) => setTimeout(r, 0));

function linkHarness() {
  const posted: any[] = [];
  let worker: any;
  const sandbox: any = {
    console, JSON, performance, setTimeout, clearTimeout,
    setInterval: () => 0,
    ConsoleWorker: class {
      onmessage: ((e: { data: unknown }) => void) | null = null;
      constructor() { worker = this; }
      postMessage(m: any) { posted.push(m); }
    },
  };
  load("console.ts", sandbox, ["ConsoleLink", "LOST"]);
  const link = new sandbox.ConsoleLink(async () => "ws://127.0.0.1:1/token");
  const fromWorker = (m: unknown) => worker.onmessage({ data: m });
  const text = (m: unknown) => fromWorker({ t: "text", text: JSON.stringify(m) });
  const sent = () => posted.filter((m) => m.t === "send").map((m) => JSON.parse(m.text));
  return { link, posted, fromWorker, text, sent, LOST: sandbox.LOST };
}

const settings = { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" };
const opts = { resume: null, hostKey: "ab".repeat(32), pairing: "482913", settings, audio: true };

test("连接：先把 ch 绑到画面上再发 connect，回复之后告诉 Worker sid；事件按 ch 交给会话，结束时解绑", async () => {
  const h = linkHarness();
  await tick();
  assert.deepEqual(plain(h.posted), [{ t: "open", url: "ws://127.0.0.1:1/token" }]);
  h.fromWorker({ t: "open" });

  const out: any[] = [];
  const canvas = { transferControlToOffscreen: () => ({ offscreen: true }) };
  const view = h.link.view(canvas, {
    size: (w: number, hh: number) => out.push(["size", w, hh]),
    cursor: () => {}, undecodable: () => {}, recovered: () => {}, stats: () => {},
  });
  assert.deepEqual(plain(h.posted.at(-1)), { t: "view", id: view.id, canvas: { offscreen: true } });
  h.fromWorker({ t: "size", id: view.id, w: 1920, h: 1080 });
  assert.deepEqual(out, [["size", 1920, 1080]]);

  const events: any[] = [];
  const connected = h.link.connect("123456", opts, view, (ev: any) => events.push(ev), true);
  const bind = h.posted.findIndex((m) => m.t === "bind");
  const send = h.posted.findIndex((m) => m.t === "send");
  assert.ok(bind >= 0 && bind < send, "先绑定，会话的第一批数据可能比回复先到");
  const call = h.sent()[0];
  // connect 命令：ConnectOptions 的字段原样带上，加上控制码、ch、剪贴板开关
  assert.deepEqual(call, { id: call.id, cmd: "connect", ...opts, code: "123456", ch: h.posted[bind].ch, clipboard: true });
  const ch = call.ch;

  // 回复之前就到的事件
  h.text({ type: "event", ch, event: { type: "awaitingApproval" } });
  h.text({ type: "reply", id: call.id, ok: { sid: 5 } });
  assert.equal((await connected).sid, 5);
  assert.deepEqual(plain(h.posted.at(-1)), { t: "sid", ch, sid: 5 });
  // 别的 ch 的事件不给它
  h.text({ type: "event", ch: ch + 1, event: { type: "awaitingApproval" } });
  assert.deepEqual(events.map((e) => e.type), ["awaitingApproval"]);

  // 高频的不带 id；其它的等回复
  void h.link.command({ cmd: "input", sid: 5, events: [{ type: "releaseAll" }] });
  assert.deepEqual(h.sent().at(-1), { cmd: "input", sid: 5, events: [{ type: "releaseAll" }] });
  const keyframe = h.link.command({ cmd: "keyframe", sid: 5 });
  const k = h.sent().at(-1);
  assert.equal(k.cmd, "keyframe");
  h.text({ type: "reply", id: k.id, err: { kind: "noSession" } });
  await assert.rejects(keyframe, (e: any) => e.kind === "noSession");

  h.text({ type: "event", ch, event: { type: "closed", reason: { kind: "hostEnded" }, retry: false } });
  assert.equal(events.at(-1).type, "closed");
  assert.deepEqual(plain(h.posted.at(-1)), { t: "unbind", ch });
  // 结束之后的事件不再给它
  h.text({ type: "event", ch, event: { type: "awaitingApproval" } });
  assert.equal(events.length, 2);
});

test("断开会话时解绑；与控制端服务的连接断了：等着的命令失败，会话收到可以重试的 closed，然后重连", async () => {
  const h = linkHarness();
  await tick();
  h.fromWorker({ t: "open" });
  const statuses: any[] = [];
  h.link.onServerStatus((s: any) => statuses.push(s));
  h.text({ type: "server", state: { state: "connected" } });
  const view = h.link.view({ transferControlToOffscreen: () => ({}) }, {});

  const a: any[] = [];
  const first = h.link.connect("111111", opts, view, (ev: any) => a.push(ev), false);
  h.text({ type: "reply", id: h.sent()[0].id, ok: { sid: 1 } });
  await first;
  void h.link.command({ cmd: "disconnect", sid: 1 });
  assert.deepEqual(plain(h.posted.at(-2)), { t: "unbind", ch: h.sent()[0].ch });

  const b: any[] = [];
  const live = h.link.connect("222222", opts, view, (ev: any) => b.push(ev), false);
  h.text({ type: "reply", id: h.sent().at(-1).id, ok: { sid: 2 } });
  await live;
  const pending = h.link.connect("333333", opts, view, () => assert.fail("还没连上的会话由 connect 失败告诉它"), false);

  const opened = h.posted.filter((m) => m.t === "open").length;
  h.fromWorker({ t: "closed" });
  await assert.rejects(pending, (e: any) => e === h.LOST);
  assert.deepEqual(plain(b), [{ type: "closed", reason: { kind: "consoleLost" }, retry: true }]);
  assert.deepEqual(plain(statuses.at(-1)), { state: "disconnected", reason: { kind: "consoleLost" } });
  // 没连上时命令直接失败
  await assert.rejects(h.link.command({ cmd: "input", sid: 2, events: [] }), (e: any) => e === h.LOST);
  await new Promise((r) => setTimeout(r, 600));
  assert.equal(h.posted.filter((m) => m.t === "open").length, opened + 1, "过一会儿重连");
});

function workerHarness() {
  const posted: { m: any; transfer: unknown[] }[] = [];
  const sockets: any[] = [];
  const players: any[] = [];
  const self: any = { postMessage: (m: any, o: { transfer: unknown[] }) => posted.push({ m, transfer: o.transfer }) };
  class FakeSocket {
    static OPEN = 1;
    readyState = 0;
    bufferedAmount = 0;
    sent: unknown[] = [];
    closed: unknown[] | null = null;
    binaryType = "";
    url: string;
    onopen: () => void = () => {};
    onmessage: (e: { data: unknown }) => void = () => {};
    onclose: () => void = () => {};
    constructor(url: string) { this.url = url; sockets.push(this); }
    send(d: unknown) { this.sent.push(d); }
    close(...args: unknown[]) { this.closed = args; }
  }
  const sandbox: any = {
    self, console, JSON, performance, setTimeout, clearTimeout,
    setInterval: () => 0,
    WebSocket: FakeSocket,
    AckCoalescer, ackMessage, AvSync, media,
    AudioStream: class { push() {} reset() {} setPort() {} setExtraDelay() {} },
    VideoPlayer: class {
      decodeAvg = 0;
      pushed: number[] = [];
      resets = 0;
      options: any;
      constructor(options: any) { this.options = options; players.push(this); }
      push(h: { seq: number }) { this.pushed.push(h.seq); }
      reset() { this.resets++; }
      flush() {}
    },
  };
  load("console-worker.ts", sandbox, []);
  const toWorker = (m: unknown) => self.onmessage({ data: m });
  return { posted, sockets, players, toWorker };
}

/** `ch | 媒体包`（头 24 字节 + 负载） */
function packet(ch: number, kind: number, seq: number, payload = [7]) {
  const b = new Uint8Array(4 + media.HEADER_LEN + payload.length);
  const dv = new DataView(b.buffer);
  dv.setUint32(0, ch, true);
  b[4] = kind;
  b[5] = 1; // 关键帧 / 可见
  b[7] = media.HEADER_LEN;
  dv.setUint32(8, seq, true);
  b.set(payload, 4 + media.HEADER_LEN);
  return b.buffer;
}

const acks = (ws: any) => ws.sent.filter((d: unknown) => d instanceof ArrayBuffer).map((d: ArrayBuffer) => [...new Uint8Array(d)]);

test("Worker：媒体包按 ch 交给画面，画面一到就确认（sid 还不知道时等知道了再补），光标交给主线程", async () => {
  const w = workerHarness();
  w.toWorker({ t: "open", url: "ws://127.0.0.1:1/token" });
  const ws = w.sockets[0];
  assert.equal(ws.binaryType, "arraybuffer");
  ws.readyState = 1;
  ws.onopen();
  assert.deepEqual(plain(w.posted.map((p) => p.m)), [{ t: "open" }]);

  const canvas = { width: 16, height: 9, getContext: () => ({}) };
  w.toWorker({ t: "view", id: 1, canvas });
  w.toWorker({ t: "bind", ch: 3, id: 1 });
  const player = w.players[0];
  assert.equal(player.resets, 1, "新的连接从头开始");

  ws.onmessage({ data: packet(3, media.VIDEO, 9) });
  assert.deepEqual(player.pushed, [9]);
  assert.deepEqual(acks(ws), [], "还不知道 sid");
  w.toWorker({ t: "sid", ch: 3, sid: 5 });
  assert.deepEqual(acks(ws), [[...new Uint8Array(ackMessage(5, 9))]]);

  // 紧跟着到的几帧：合并，确认最新的那一帧
  ws.onmessage({ data: packet(3, media.VIDEO, 10) });
  ws.onmessage({ data: packet(3, media.VIDEO, 11) });
  await new Promise((r) => setTimeout(r, ACK_INTERVAL_MS + 20));
  assert.deepEqual(acks(ws).at(-1), [...new Uint8Array(ackMessage(5, 11))]);
  assert.ok(acks(ws).length <= 3);

  // 不认识的编码格式：照样确认，但不解码
  const odd = packet(3, media.VIDEO, 12);
  new Uint8Array(odd)[6] = 9;
  ws.onmessage({ data: odd });
  await new Promise((r) => setTimeout(r, ACK_INTERVAL_MS + 20));
  assert.deepEqual(acks(ws).at(-1), [...new Uint8Array(ackMessage(5, 12))]);
  assert.deepEqual(player.pushed, [9, 10, 11]);

  // 光标：整个缓冲区转给主线程
  const cursor = packet(3, media.CURSOR, 0);
  ws.onmessage({ data: cursor });
  const c = w.posted.at(-1)!;
  assert.equal(c.m.t, "cursor");
  assert.equal(c.m.id, 1);
  assert.equal(c.m.pkt.byteOffset, 4);
  assert.equal(c.transfer.length, 1);
  assert.equal(c.transfer[0], cursor);

  // 文本原样交给主线程；别的 ch 的包不要
  ws.onmessage({ data: '{"type":"server"}' });
  assert.deepEqual(plain(w.posted.at(-1)!.m), { t: "text", text: '{"type":"server"}' });
  ws.onmessage({ data: packet(4, media.VIDEO, 1) });
  assert.deepEqual(player.pushed, [9, 10, 11]);

  // 换了新的连接（重连）：旧 ch 作废
  w.toWorker({ t: "bind", ch: 6, id: 1 });
  ws.onmessage({ data: packet(3, media.VIDEO, 13) });
  ws.onmessage({ data: packet(6, media.VIDEO, 1) });
  assert.deepEqual(player.pushed, [9, 10, 11, 1]);

  // 主线程的命令原样发出去
  w.toWorker({ t: "send", text: '{"cmd":"input"}' });
  assert.equal(ws.sent.at(-1), '{"cmd":"input"}');

  // 断了：通道全部作废，告诉主线程
  ws.onclose();
  assert.deepEqual(plain(w.posted.at(-1)!.m), { t: "closed" });
  ws.onmessage({ data: packet(6, media.VIDEO, 2) });
  assert.deepEqual(player.pushed, [9, 10, 11, 1]);
});

test("Worker：关键帧不管是解码出错要的还是主线程要的，都在这里一起限流；还不知道 sid 时不要", async () => {
  const w = workerHarness();
  w.toWorker({ t: "open", url: "ws://x" });
  const ws = w.sockets[0];
  ws.readyState = 1;
  w.toWorker({ t: "view", id: 1, canvas: { width: 16, height: 9, getContext: () => ({}) } });
  w.toWorker({ t: "bind", ch: 3, id: 1 });
  const decoder = () => w.players[0].options.requestKeyframe();
  const sent = () => ws.sent.filter((d: unknown) => typeof d === "string").map((d: string) => JSON.parse(d));

  decoder();
  w.toWorker({ t: "keyframe", id: 1 });
  assert.deepEqual(sent(), [], "还没连上");
  w.toWorker({ t: "sid", ch: 3, sid: 5 });
  w.toWorker({ t: "keyframe", id: 1 });
  assert.deepEqual(sent(), [{ cmd: "keyframe", sid: 5 }]);
  // 紧跟着的：不管谁要的都不再发
  decoder();
  w.toWorker({ t: "keyframe", id: 1 });
  assert.equal(sent().length, 1);
  await new Promise((r) => setTimeout(r, 520));
  decoder();
  assert.equal(sent().length, 2);
});

test("Worker：发不出去的积压太多就断开", () => {
  const w = workerHarness();
  w.toWorker({ t: "open", url: "ws://x" });
  const ws = w.sockets[0];
  ws.readyState = 1;
  ws.bufferedAmount = 3 * 1024 * 1024;
  w.toWorker({ t: "send", text: "{}" });
  assert.deepEqual(ws.sent, []);
  assert.equal(ws.closed[0], 1013);
});
