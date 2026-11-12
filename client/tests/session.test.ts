import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { ConnectionAttempt } from "../src/lib/connection-attempt.ts";
import { renderPlan } from "../src/lib/enhance.ts";
import { fitSize, stricter, viewMaxShort } from "../src/lib/fit.ts";
import * as media from "../src/lib/media.ts";

/** 沙箱里建的对象原型不同，按 JSON 比较 */
const plain = (v: unknown) => JSON.parse(JSON.stringify(v));

// Load the actual session class with browser/backend dependencies replaced by
// small fakes. This tests delivery ordering without a WebView or network.
function sessionHarness(connect?: (code: string, resume: any, opts: any) => Promise<any>) {
  const source = fs.readFileSync(new URL("../src/lib/session.ts", import.meta.url), "utf8")
    .replace(/^import[\s\S]*?from "[^"]+";\r?\n/gm, "");
  const js = stripTypeScriptTypes(source).replace(/\bexport\s+/g, "") + "\nglobalThis.RemoteSession = RemoteSession;";
  let sink: any;
  let resolve: (value: any) => void = () => {};
  const calls: any[] = [];
  const timers: (() => void)[] = [];
  const storeState: any = { enhance: "off", decoders: ["h264"] };
  const sandbox: any = { console, performance, clearTimeout, setTimeout, queueMicrotask, ConnectionAttempt,
    fitSize, stricter, viewMaxShort, renderPlan, media, devicePixelRatio: 1,
    store: { get: () => storeState },
    toProblem: (e: any) => e,
    api: {
      connect: (c: any, o: any, _view: any, onEvent: any) => {
        sink = onEvent;
        return connect ? connect(c, o.resume, o) : new Promise(res => { resolve = res; });
      },
      command: async (c: any) => { calls.push(c.cmd === "settings" ? [c.cmd, c.sid, c.settings] : [c.cmd, c.sid]); },
    },
    // 延时发送用的定时器：由测试手动触发
    window: { setTimeout: (fn: () => void) => { timers.push(fn); return timers.length; } },
  };
  vm.runInNewContext(js, sandbox);
  const session = Object.create(sandbox.RemoteSession.prototype);
  Object.assign(session, {
    attempt: new ConnectionAttempt(), disposed: false, sid: null, state: "connecting", settings: {}, visible: true,
    viewLimit: 0, viewLimitAt: -Infinity, viewport: null, displays: [], currentDisplay: 0,
    view: { setSync() {}, setAudio() {}, setLayout() {}, dispose() {} }, frame: { w: 16, h: 9 }, input: { reset() {} },
    cursorSlots: new Map(), resume: null, hostKey: null, safety: null, pairing: null,
    onlineAt: -1, attemptAt: 0, retries: 0,
    emit() {}, syncAudio() {}, applySyncState() {}, onConnected() { calls.push("connected"); },
  });
  return { session, calls, timers, store: storeState, event: (ev: any) => sink(ev), resolve: () => resolve({ sid: 7, resume: [], hostName: "host", hostOs: "Android" }) };
}

test("文字分块保留 emoji 的代理对，每块都能独立编码成 UTF-8", () => {
  const { session } = sessionHarness();
  const pieces: string[] = [];
  session.input = { enqueue(events: any[]) { pieces.push(...events.map(e => e.text)); } };
  const text = "x".repeat(511) + "😀" + "𠮷".repeat(600) + "\r\n末尾";
  session.sendText(text);
  assert.equal(pieces.join(""), text.replace(/\r\n/g, "\r"));
  for (const piece of pieces) {
    assert.ok(Array.from(piece).length <= 512);
    assert.equal(Buffer.from(piece, "utf8").toString("utf8"), piece);
  }
});

test("Closed before connect reply cannot resurrect SID, erase reason or remember a device", async () => {
  const h = sessionHarness();
  const connecting = h.session.start();
  h.event({ type: "closed", reason: { kind: "declined" }, retry: false });
  h.resolve();
  await connecting;
  assert.equal(h.session.state, "closed");
  assert.equal(h.session.sid, null);
  assert.equal(h.session.closedReason.kind, "declined");
  assert.deepEqual(h.calls, [["disconnect", 7]]);
});

test("按显示区域请求分辨率：空闲时马上发，连续变化时停下来再发，原始大小时不限", () => {
  const h = sessionHarness();
  const s = h.session;
  Object.assign(s, {
    sid: 7, scaleMode: "fit", interactive: true,
    settings: { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" },
    displays: [{ index: 0, width: 2880, height: 1800 }], viewport: { w: 1600, h: 1000 },
  });
  const sent = () => h.calls.filter((c) => c[0] === "settings").map((c) => c[2].maxShort);

  s.updateViewLimit();
  assert.deepEqual(sent(), [1000], "空闲时马上发");
  s.updateViewLimit();
  assert.deepEqual(sent(), [1000], "没变不重发");

  // 拖动窗口：连续变化只在停下来之后发最后一次
  s.viewport = { w: 1500, h: 900 };
  s.updateViewLimit();
  s.viewport = { w: 1400, h: 850 };
  s.updateViewLimit();
  assert.deepEqual(sent(), [1000]);
  h.timers.at(-1)!();
  assert.deepEqual(sent(), [1000, 850]);

  // 用户选的上限更严时按用户的
  s.settings = { ...s.settings, maxShort: 720 };
  s.pushSettings();
  assert.equal(sent().at(-1), 720);

  // 原始大小：不按窗口限制
  s.settings = { ...s.settings, maxShort: 0 };
  s.scaleMode = "native";
  s.viewLimitAt = -Infinity;
  s.updateViewLimit();
  assert.equal(sent().at(-1), 0);
});

test("画面增强：只给正在操作的会话，关着时不发显示区域；增强时和 1:1 一样对齐到设备像素", () => {
  const h = sessionHarness();
  const s = h.session;
  const layouts: any[] = [];
  const style: any = {};
  const area = (width: number, height: number) => ({ getBoundingClientRect: () => ({ width, height }) });
  Object.assign(s, {
    view: { setLayout: (a: any, l: string) => layouts.push([a, l]) },
    hostEl: area(1920, 1080),
    canvas: { style, getBoundingClientRect: () => ({ left: 10.3, top: 5.6 }) },
    frame: { w: 1280, h: 720 }, scaleMode: "fit", interactive: true,
    applyCursor() {}, updateViewLimit() {},
  });
  s.layout();
  assert.deepEqual(plain(layouts), [[null, "off"]]);
  assert.equal(style.width, "1920px");
  assert.equal(style.left, "", "浏览器要放大，对齐也没用");
  // 关着时窗口变了不用告诉 Worker
  s.hostEl = area(1600, 900);
  s.layout();
  assert.equal(layouts.length, 1);

  h.store.enhance = "medium";
  s.refreshLayout();
  assert.deepEqual(plain(layouts.at(-1)), [{ w: 1600, h: 900 }, "medium"]);
  assert.equal(style.width, "1600px");
  assert.notEqual(style.left, "", "canvas 就是显示的大小：对齐到设备像素");
  s.layout();
  assert.equal(layouts.length, 2, "没变不重发");

  // 多屏总览的缩略图：不增强
  s.interactive = false;
  s.layout();
  assert.deepEqual(plain(layouts.at(-1)), [null, "off"]);
  // 原始大小（1:1）显示：没有显示区域
  Object.assign(s, { interactive: true, scaleMode: "native" });
  s.layout();
  assert.deepEqual(plain(layouts.at(-1)), [null, "medium"]);
});

const granted = { sid: 7, resume: [9], hostName: "host", hostOs: "Windows", hostKey: "ab".repeat(32), safety: "123-456" };

test("开始时的画质和要不要声音在连接选项里（对方在握手里就知道），连上之后不再另外发", async () => {
  let opts: any;
  const h = sessionHarness(async (_code, _resume, o) => {
    opts = o;
    return granted;
  });
  const s = h.session;
  let enabled = 0;
  const settings = { fps: 60, bitrateKbps: 8000, maxShort: 1080, adaptive: true, codec: "h265" };
  Object.assign(s, {
    settings, audioEnabled: true, foreground: true, audioOn: false,
    audio: { enable() { enabled++; }, disable() {} },
  });
  // 用真正的 syncAudio：连上之后声音已经开着，不用再发 audio 命令
  delete s.syncAudio;
  await s.start();
  // 沙箱里建的对象原型不同，按 JSON 比较
  assert.deepEqual(JSON.parse(JSON.stringify(opts.settings)), settings);
  assert.equal(opts.audio, true);
  assert.equal(s.audioOn, true);
  assert.equal(enabled, 1);
  assert.deepEqual(h.calls.filter((c) => Array.isArray(c) && (c[0] === "audio" || c[0] === "settings")), []);

  // 不在前台（例如切到了别的标签）：开始时不要声音
  let opts2: any;
  const h2 = sessionHarness(async (_code, _resume, o) => {
    opts2 = o;
    return granted;
  });
  Object.assign(h2.session, { settings, audioEnabled: true, foreground: false });
  await h2.session.start();
  assert.equal(opts2.audio, false);
});

test("端到端：记下对方的公钥、之后只认它；要配对码时不自动重连，输入之后带着它连一次", async () => {
  const tried: any[] = [];
  let answer: () => Promise<any> = async () => { throw { kind: "pairingRequired" }; };
  const h = sessionHarness(async (_code, _resume, opts) => {
    tried.push({ hostKey: opts.hostKey, pairing: opts.pairing });
    return answer();
  });
  await h.session.connect().catch((e: any) => h.session.scheduleRetry(e));
  assert.equal(h.session.state, "closed", "要用户输入配对码，不自动重连");
  assert.equal(h.timers.length, 0);

  answer = async () => granted;
  h.session.pairWith("482 913");
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(tried.at(-1), { hostKey: null, pairing: "482913" });
  assert.equal(h.session.hostKey, granted.hostKey);
  assert.equal(h.session.safety, "123-456");

  // 之后重连：带着记下的公钥，不再带配对码
  h.session.sid = null;
  await h.session.connect();
  assert.deepEqual(tried.at(-1), { hostKey: granted.hostKey, pairing: null });

  // 对方的身份变了：不自动重连，也不能不验证就换成新的；用户输入配对码重新配对，配对码认的是对方，记下新的
  answer = async () => { throw { kind: "hostKeyChanged" }; };
  h.session.sid = null;
  await h.session.connect().catch((e: any) => h.session.scheduleRetry(e));
  assert.equal(h.session.state, "closed");
  assert.equal("trustNewIdentity" in h.session, false);
  const changed = { ...granted, hostKey: "cd".repeat(32) };
  answer = async () => changed;
  h.session.pairWith("135 790");
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(tried.at(-1), { hostKey: granted.hostKey, pairing: "135790" });
  assert.equal(h.session.hostKey, changed.hostKey);
});

test("从我的设备打开不在线的设备：带着凭证等它上线，不再拿控制码试一次（那样会算一次猜错）", async () => {
  const tried: any[] = [];
  const h = sessionHarness(async (_code, resume) => {
    tried.push(resume);
    throw { kind: "hostOffline" };
  });
  h.session.resume = [1, 2, 3];
  await h.session.open();
  assert.deepEqual(tried, [[1, 2, 3]]);
  assert.equal(h.session.state, "waiting");
});

test("凭证作废了（kicked）：还没连上过就用控制码直接连，连上过就不再自动重连", async () => {
  const tried: any[] = [];
  const h = sessionHarness(async (_code, resume) => {
    tried.push(resume);
    if (resume !== null) throw { kind: "kicked" };
    return granted;
  });
  h.session.resume = [1];
  await h.session.open();
  assert.deepEqual(tried, [[1], null]);
  assert.equal(h.session.sid, 7);
  assert.deepEqual(h.session.resume, [9]);

  // 连上过之后断了，重连时凭证已经作废（断线期间对方断开了所有控制端）：不自动用控制码连回去
  tried.length = 0;
  h.session.sid = null;
  await h.session.connect().catch((e: any) => h.session.scheduleRetry(e));
  assert.deepEqual(tried, [[9]]);
  assert.equal(h.session.state, "closed");
  assert.equal(h.session.closedReason.kind, "kicked");
  assert.equal(h.session.resume, null, "之后手动重连是普通的连接请求");
});

test("光标形状按槽位缓存：只带槽位的包换成缓存里的形状，新的会话清空", () => {
  const h = sessionHarness();
  const s = h.session;
  Object.assign(s, { input: { reset() {}, setRemoteCursorHidden() {} }, applyCursor() {} });
  const header = (flags: number, slot: number, w = 0) => {
    const b = new Uint8Array(media.HEADER_LEN + w * w * 4);
    b[0] = media.CURSOR;
    b[1] = flags;
    b[2] = slot;
    b[3] = media.HEADER_LEN;
    const dv = new DataView(b.buffer);
    dv.setUint16(8, w, true);
    dv.setUint16(10, w, true);
    b.fill(slot, media.HEADER_LEN);
    return b;
  };
  const push = (b: Uint8Array) => s.onCursor(b);
  push(header(1 | 2, 3, 2));
  const three = s.cursorRaw;
  assert.equal(three.w, 2);
  push(header(1 | 2, 5, 1));
  assert.equal(s.cursorRaw.w, 1);
  push(header(1 | 4, 3));
  assert.equal(s.cursorRaw, three, "槽位 3 里的形状");
  assert.equal(three.rgba[0], 3);
  // 缓存里没有的槽位（不应该发生）：保持原样
  push(header(1 | 4, 9));
  assert.equal(s.cursorRaw, three);
  // 只改可见性
  push(header(0, 0));
  assert.equal(s.cursorRaw, three);
  assert.equal(s.cursorVisible, false);
  // 新的会话：槽位从头开始
  void s.start();
  assert.equal(s.cursorSlots.size, 0);
});

test("光标画在画面里（对方拖动、缩放窗口）：不显示本地光标，鼠标照常用绝对坐标", () => {
  const s = sessionHarness().session;
  const style: any = {};
  const hidden: boolean[] = [];
  Object.assign(s, { canvas: { style }, input: { relative: false, setRemoteCursorHidden(h: boolean) { hidden.push(h); } } });
  const push = (flags: number) => {
    const b = new Uint8Array(media.HEADER_LEN);
    b[0] = media.CURSOR;
    b[1] = flags;
    b[3] = media.HEADER_LEN;
    s.onCursor(b);
  };
  push(1);
  assert.equal(style.cursor, "default", "还没有形状：普通的指针");
  push(1 | 8);
  assert.equal(style.cursor, "none");
  push(1);
  assert.equal(style.cursor, "default");
  assert.deepEqual(hidden, [false, false, false], "一直没有改发相对移动");
});

test("stale completion cannot finish a newer attempt", () => {
  const attempt = new ConnectionAttempt();
  const old = attempt.begin();
  attempt.invalidate();
  const current = attempt.begin();
  attempt.finish(old);
  assert.equal(attempt.accepts(old), false);
  assert.equal(attempt.accepts(current), true);
  assert.equal(attempt.attempting, true);
  attempt.finish(current);
  assert.equal(attempt.attempting, false);
});
