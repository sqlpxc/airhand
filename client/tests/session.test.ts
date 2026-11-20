import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { ConnectionAttempt } from "../src/lib/connection-attempt.ts";
import { renderPlan } from "../src/lib/enhance.ts";
import { fitSize, stricter, viewMaxShort } from "../src/lib/fit.ts";
import * as media from "../src/lib/media.ts";
import { hitTest, layoutTiles, placeDisplays } from "../src/lib/screens.ts";
import { RemoteInput } from "../src/lib/input.ts";

/** 沙箱里建的对象原型不同，按 JSON 比较 */
const plain = (v: unknown) => JSON.parse(JSON.stringify(v));

/** 假的 canvas：记下样式，位置从样式里读（设备像素比例是 1） */
function fakeCanvas() {
  const style: any = {};
  const px = (v: string | undefined) => parseFloat(v ?? "0") || 0;
  return {
    style,
    removed: false,
    remove() { this.removed = true; },
    getBoundingClientRect: () => ({ left: px(style.left), top: px(style.top), width: px(style.width), height: px(style.height) }),
  };
}

/** 假的弹出窗口（见 popout.ts）：显示区域固定，关掉时和真的一样调用 onClosed */
function fakePopout(w: number, h: number) {
  const p: any = {
    screen: { style: {}, addEventListener() {}, focus() {}, getBoundingClientRect: () => ({ left: 0, top: 0 }) },
    viewport: { w, h }, dpr: 1, closed: false, onClosed: null, onResize: null, onChange: null,
    focus() {},
    close() {
      if (p.closed) return;
      p.closed = true;
      p.onClosed?.();
    },
  };
  return p;
}

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
    fitSize, stricter, viewMaxShort, renderPlan, media, hitTest, layoutTiles, placeDisplays, RemoteInput, devicePixelRatio: 1,
    codecUsable: () => true, bestCodec: () => "h264",
    store: { get: () => storeState },
    toProblem: (e: any) => e,
    api: {
      connect: (c: any, o: any, _view: any, onEvent: any) => {
        sink = onEvent;
        return connect ? connect(c, o.resume, o) : new Promise(res => { resolve = res; });
      },
      command: async (c: any) => {
        calls.push(
          c.cmd === "settings" ? [c.cmd, c.sid, c.settings]
          : c.cmd === "mic" ? [c.cmd, c.sid, c.enabled]
          : c.cmd === "view" ? [c.cmd, c.sid, plain(c.streams)]
          : [c.cmd, c.sid],
        );
      },
    },
    // 延时发送用的定时器：由测试手动触发
    window: { setTimeout: (fn: () => void) => { timers.push(fn); return timers.length; } },
  };
  vm.runInNewContext(js, sandbox);
  const session = Object.create(sandbox.RemoteSession.prototype);
  Object.assign(session, {
    attempt: new ConnectionAttempt(), disposed: false, sid: null, state: "connecting", settings: {}, visible: true,
    sentView: null, viewAt: -Infinity, viewport: null, displays: [], currentDisplay: 0,
    shown: [0], maxStreams: 1, allDisplays: false, wantName: null, sentLayouts: [],
    // 第 0 路的 canvas（构造函数里建的）；之后要的假的 canvas 由 newCanvas 建（记着它在哪个窗口里）
    canvases: [fakeCanvas()], canvasHome: [null], screen: { style: {}, getBoundingClientRect: () => ({ left: 0, top: 0 }) },
    newCanvas(home: any) { const c: any = fakeCanvas(); c.home = home; return c; },
    popouts: new Map(), opening: new Map(), cursorCss: new WeakMap(),
    view: { setSync() {}, setAudio() {}, setLayout() {}, dispose() {}, addCanvas() {} }, frames: [{ w: 16, h: 9 }],
    input: { reset() {} },
    cursorSlots: new Map(), resume: null, hostKey: null, safety: null, pairing: null,
    onlineAt: -1, attemptAt: 0, retries: 0,
    emit() {}, syncAudio() {}, syncMic() {}, applySyncState() {}, onConnected() { calls.push("connected"); },
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
  h.event({ type: "closed", reason: { kind: "codeChanged" }, retry: false });
  h.resolve();
  await connecting;
  assert.equal(h.session.state, "closed");
  assert.equal(h.session.sid, null);
  assert.equal(h.session.closedReason.kind, "codeChanged");
  assert.deepEqual(h.calls, [["disconnect", 7]]);
});

test("按显示区域请求分辨率：空闲时马上发，连续变化时停下来再发，原始大小时不限", () => {
  const h = sessionHarness();
  const s = h.session;
  Object.assign(s, {
    sid: 7, scaleMode: "fit", interactive: true,
    settings: { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" },
    displays: [{ index: 0, name: "A", width: 2880, height: 1800 }], viewport: { w: 1600, h: 1000 },
  });
  const views = () => h.calls.filter((c) => c[0] === "view").map((c) => c[2]);

  s.updateView();
  assert.deepEqual(views(), [[{ display: 0, maxShort: 1000 }]], "空闲时马上发");
  s.updateView();
  assert.equal(views().length, 1, "没变不重发");

  // 拖动窗口：连续变化只在停下来之后发最后一次
  s.viewport = { w: 1500, h: 900 };
  s.updateView();
  s.viewport = { w: 1400, h: 850 };
  s.updateView();
  assert.equal(views().length, 1);
  h.timers.at(-1)!();
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 850 }]);

  // 用户选的上限不和它混在一起：画质设置照原样发，被控端取较严的
  s.settings = { ...s.settings, maxShort: 720 };
  s.pushSettings();
  assert.equal(h.calls.filter((c) => c[0] === "settings").at(-1)[2].maxShort, 720);

  // 原始大小：不按窗口限制
  s.scaleMode = "native";
  s.viewAt = -Infinity;
  s.updateView();
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 0 }]);
});

test("多屏：同时看所有显示器时每一路按它在拼好的画面里的大小要分辨率，选了的那个在第 0 路；切换马上发", () => {
  const h = sessionHarness();
  const s = h.session;
  // 左边 4K、右边 1080p，底边对齐；窗口 2880×1080
  const displays = [
    { index: 0, name: "A", x: 0, y: 0, width: 3840, height: 2160, primary: true },
    { index: 1, name: "B", x: 3840, y: 1080, width: 1920, height: 1080, primary: false },
  ];
  Object.assign(s, { sid: 7, scaleMode: "fit", interactive: true, viewport: { w: 2880, h: 1080 }, emit() {}, releaseAll() {} });
  s.onEvent({ type: "hello", hostname: "pc", os: "Windows", version: "1", displays, current: 1, shown: [1], maxStreams: 4,
    codecs: ["h264"], platform: "windows", touch: false, actions: [] });
  const views = () => h.calls.filter((c) => c[0] === "view").map((c) => c[2]);
  assert.equal(s.canShowAll, true);
  // 只看一个：对方开始时给的那个
  assert.deepEqual(views(), [[{ display: 1, maxShort: 0 }]]);
  s.setAllDisplays(true);
  // 拼起来 5760×2160，缩到 2880×1080（一半）：选了的 B 在第 0 路，短边各自减半（不低于 360）
  assert.deepEqual(views().at(-1), [{ display: 1, maxShort: 540 }, { display: 0, maxShort: 1080 }]);
  // 对方按这个开始发两路：画面按位置摆好，鼠标在哪一块上就是哪一路
  s.onEvent({ type: "displays", displays, current: 1, shown: [1, 0] });
  assert.equal(s.canvases.length, 2);
  s.hostEl = { getBoundingClientRect: () => ({ width: 2880, height: 1080 }) };
  s.frames = [{ w: 960, h: 540 }, { w: 1920, h: 1080 }];
  s.applyCursor = () => {};
  s.layout();
  assert.equal(s.screen.style.width, "2880px");
  const [b, a] = s.canvases.map((c: any) => c.getBoundingClientRect());
  assert.deepEqual(plain(a), { left: 0, top: 0, width: 1920, height: 1080 });
  assert.deepEqual(plain(b), { left: 1920, top: 540, width: 960, height: 540 });
  assert.deepEqual(plain(s.locate(2000, 600)), { x: 80 / 960, y: 60 / 540, stream: 0 });
  assert.deepEqual(plain(s.locate(100, 100)), { x: 100 / 1920, y: 100 / 1080, stream: 1 });
  // 落在 B 上面的空白里：最近的是 B 的上边
  assert.deepEqual(plain(s.locate(2400, 100)), { x: 0.5, y: 0, stream: 0 });
  // 只看 A：马上发，一路
  s.selectDisplay(0);
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 1080 }]);
  assert.equal(s.allDisplays, false);
  // 对方换了显示器的顺序（插拔）：按名字还是看 A（它现在是第 1 个）
  s.onEvent({ type: "displays", displays: [displays[1], displays[0]], current: 1, shown: [1] });
  assert.deepEqual(views().at(-1), [{ display: 1, maxShort: 1080 }]);
  const sent = views().length;
  s.onEvent({ type: "displays", displays: [displays[1], displays[0]], current: 1, shown: [1] });
  assert.equal(views().length, sent, "没变不重发");
});

test("弹出窗口：一个显示器放到单独的窗口里，主窗口拼剩下的；每一路按各自的窗口要分辨率；关掉窗口就放回来", () => {
  const h = sessionHarness();
  const s = h.session;
  const displays = [
    { index: 0, name: "A", x: 0, y: 0, width: 3840, height: 2160, primary: true },
    { index: 1, name: "B", x: 3840, y: 1080, width: 1920, height: 1080, primary: false },
  ];
  const added: any[] = [];
  Object.assign(s, {
    sid: 7, scaleMode: "fit", interactive: true, viewport: { w: 2880, h: 1080 }, emit() {},
    hostEl: { getBoundingClientRect: () => ({ width: 2880, height: 1080 }) },
    input: { mode: "auto", releaseAll() {}, reset() {}, setRemoteCursorHidden() {} },
    applyCursor() {}, cursorVisible: true,
  });
  s.view = { ...s.view, addCanvas: (i: number, c: any) => added.push([i, c.home ?? null]) };
  s.onEvent({ type: "hello", hostname: "pc", os: "Windows", version: "1", displays, current: 0, shown: [0], maxStreams: 4,
    codecs: ["h264"], platform: "windows", touch: false, actions: [] });
  s.state = "live";
  const views = () => h.calls.filter((c) => c[0] === "view").map((c) => c[2]);

  // 只看 A 的时候：A 是主窗口里仅有的一个，不能弹出去；B 可以（多要一路）
  assert.equal(s.canPopOut(0), false);
  assert.equal(s.canPopOut(1), true);
  // 同时看两个：哪个都能弹出去，但主窗口里要留一个
  s.setAllDisplays(true);
  s.onEvent({ type: "displays", displays, current: 0, shown: [0, 1] });
  s.frames = [{ w: 1920, h: 1080 }, { w: 960, h: 540 }];
  assert.equal(s.canPopOut(0), true);
  assert.equal(s.canPopOut(1), true);

  const p = fakePopout(1920, 1080);
  s.attachPopout("B", p);
  assert.equal(s.isPopped(1), true);
  assert.equal(s.canPopOut(0), false, "主窗口里只剩 A 了");
  // B 的那一路换了一个在弹出窗口里的 canvas 交给 Worker
  assert.deepEqual(added.at(-1), [1, p]);
  assert.equal(s.canvases[1].home, p);
  // A 在主窗口里独占整个显示区域，B 按弹出窗口的大小（1:1，不限）
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 1080 }, { display: 1, maxShort: 0 }]);
  assert.equal(s.screen.style.width, "1920px");
  assert.equal(p.screen.style.width, "1920px", "画面还是旧的大小：小于窗口，等比放大到放得下");
  // 主窗口里的鼠标只会落在 A 上；弹出窗口里的是 B 那一路
  assert.deepEqual(plain(s.locate(1900, 1000)), { x: 1900 / 1920, y: 1000 / 1080, stream: 0 });
  assert.equal(s.locatePopout(p, 10, 10).stream, 1);

  // 用户关掉了弹出窗口：B 放回主窗口（又换一个 canvas），和之前一样拼在一起
  p.close();
  assert.equal(s.isPopped(1), false);
  assert.deepEqual(added.at(-1), [1, null]);
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 1080 }, { display: 1, maxShort: 540 }]);
  assert.equal(s.screen.style.width, "2880px");

  // 只看 A 的时候弹出 B：多要一路给它，弹出窗口里的 canvas 在对方发之前就建好
  s.setAllDisplays(false);
  s.onEvent({ type: "displays", displays, current: 0, shown: [0] });
  const q = fakePopout(1280, 720);
  s.attachPopout("B", q);
  assert.deepEqual(views().at(-1), [{ display: 0, maxShort: 1080 }, { display: 1, maxShort: 720 }]);
  assert.equal(s.canvases[1].home, q);
  // 对方把 B 拔掉了：关掉它的窗口
  s.onEvent({ type: "displays", displays: [displays[0]], current: 0, shown: [0] });
  assert.equal(q.closed, true);
  assert.equal(s.popoutWindows.length, 0);
});

test("画面增强：只给正在操作的会话，关着时不发显示区域；增强时和 1:1 一样对齐到设备像素", () => {
  const h = sessionHarness();
  const s = h.session;
  const layouts: any[] = [];
  const style: any = {};
  const area = (width: number, height: number) => ({ getBoundingClientRect: () => ({ width, height }) });
  Object.assign(s, {
    view: { setLayout: (_stream: number, a: any, l: string) => layouts.push([a, l]) },
    hostEl: area(1920, 1080),
    screen: { style, getBoundingClientRect: () => ({ left: 10.3, top: 5.6 }) },
    frames: [{ w: 1280, h: 720 }], scaleMode: "fit", interactive: true,
    applyCursor() {}, updateView() {},
  });
  s.layout();
  assert.deepEqual(plain(layouts), [[null, "off"]]);
  assert.equal(style.width, "1920px");
  assert.equal(s.canvases[0].style.width, "1920px");
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

test("游戏模式：连接选项里就让对方把光标画进画面；换了模式马上发新的画质；保存的画质里不带它", async () => {
  let opts: any;
  const h = sessionHarness(async (_code, _resume, o) => {
    opts = o;
    return granted;
  });
  const s = h.session;
  // 保存的画质里就算带着（之前存的、Rust 那边读回来的）也不算数：只看鼠标模式
  const settings = { fps: 60, bitrateKbps: 8000, maxShort: 1080, adaptive: true, codec: "h265", drawCursor: false };
  const input: any = { mode: "game", relative: true, reset() {} };
  Object.assign(s, { settings, input });
  await s.start();
  assert.equal(opts.settings.drawCursor, true);
  assert.equal(s.settings.drawCursor, false, "the saved quality isn't touched");
  assert.deepEqual(h.calls.filter((c) => Array.isArray(c) && c[0] === "settings"), []);

  // 换成自动模式：发不带 drawCursor 的画质
  input.mode = "auto";
  s.pushSettings();
  const sent = h.calls.filter((c) => Array.isArray(c) && c[0] === "settings");
  assert.equal(sent.length, 1);
  assert.equal("drawCursor" in sent[0][2], false);

  // 本机指针：游戏模式锁定着不显示，放开了（没锁定）显示普通的指针，好去点画面
  const style: any = {};
  Object.assign(s, { screen: { style } });
  input.mode = "game";
  s.refreshCursor();
  assert.equal(style.cursor, "none");
  input.relative = false;
  s.refreshCursor();
  assert.equal(style.cursor, "default");
});

test("对方是手机：没有鼠标模式，记着的游戏模式也改回自动（不让对方画光标）", () => {
  const h = sessionHarness();
  const s = h.session;
  const modes: any[] = [];
  const input: any = { mode: "game", reset() {}, setMode(m: string, notify: boolean) { modes.push([m, notify]); this.mode = m; } };
  Object.assign(s, { sid: 7, settings: { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" }, input });
  h.session.onEvent({
    type: "hello", hostname: "phone", os: "Android 15", platform: "android", actions: [], version: "0.3.0",
    displays: [], current: 0, shown: [], codecs: ["h264"],
  });
  assert.deepEqual(modes, [["auto", false]]);
  const sent = h.calls.filter((c) => Array.isArray(c) && c[0] === "settings");
  assert.equal(sent.length, 1);
  assert.equal("drawCursor" in sent[0][2], false);
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

test("光标画在画面里（对方拖动、缩放窗口）：默认照样显示本地光标，设置里关了就不显示；鼠标照常用绝对坐标", () => {
  const h = sessionHarness();
  const s = h.session;
  h.store.localCursor = true;
  const style: any = {};
  const hidden: boolean[] = [];
  Object.assign(s, { screen: { style }, input: { relative: false, setRemoteCursorHidden(h: boolean) { hidden.push(h); } } });
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
  assert.equal(style.cursor, "default", "默认照样显示本机的光标");
  h.store.localCursor = false;
  s.refreshCursor();
  assert.equal(style.cursor, "none", "关掉之后马上不显示");
  push(1);
  assert.equal(style.cursor, "default", "画面里没有光标了：照常显示");
  push(1 | 8);
  assert.equal(style.cursor, "none");
  assert.deepEqual(hidden, [false, false, false, false], "一直没有改发相对移动");
});

test("麦克风：对方说了能接、开关打开、正在查看时才告诉对方打开；对方 ready 才采集；切走、断开、本机不允许时关掉", async () => {
  const h = sessionHarness();
  const s = h.session;
  const capture: string[] = [];
  let refuse: string | null = null;
  Object.assign(s, {
    sid: 7, foreground: true, micEnabled: false, micState: null, micDetail: null, micError: null,
    micOn: false, micCapturing: false,
    mic: {
      start: async () => {
        capture.push("start");
        if (refuse) throw refuse;
      },
      stop: () => capture.push("stop"),
    },
  });
  delete s.syncMic;
  const mics = () => h.calls.filter((c) => Array.isArray(c) && c[0] === "mic").map((c) => c[2]);
  const settle = () => new Promise((r) => setTimeout(r, 0));

  // 对方还没说（版本旧的对方不会说）：开关打开也不发
  s.setMicEnabled(true);
  assert.deepEqual(mics(), []);
  // 对方最初没有 Steam，后来装上了：准备驱动期间不采集，也不重复发送打开请求
  s.onEvent({ type: "mic", state: "steamMissing", detail: null });
  s.onEvent({ type: "mic", state: "noDevice", detail: null });
  s.onEvent({ type: "mic", state: "preparing", detail: null });
  s.onEvent({ type: "mic", state: "setupFailed", detail: "restart required" });
  assert.equal(s.micDetail, "restart required");
  assert.deepEqual(mics(), [true]);
  assert.deepEqual(capture, []);
  // 设备有了：开始采集，不用再告诉对方
  s.onEvent({ type: "mic", state: "ready", detail: null });
  assert.deepEqual(capture, ["start"]);
  assert.equal(s.micLive, true);
  assert.deepEqual(mics(), [true]);
  // 对方打不开了：停止采集，对方还开着（它在重试）
  s.onEvent({ type: "mic", state: "failed", detail: "0x8889000A" });
  assert.deepEqual(capture, ["start", "stop"]);
  assert.equal(s.micDetail, "0x8889000A");
  s.onEvent({ type: "mic", state: "ready", detail: null });
  // 切到别的标签：对方放开设备，切回来再打开
  s.setForeground(false);
  assert.deepEqual(mics(), [true, false]);
  assert.deepEqual(capture, ["start", "stop", "start", "stop"]);
  s.setForeground(true);
  assert.deepEqual(mics(), [true, false, true]);
  // 本机不允许用麦克风：开关关掉、告诉对方，原因留着；再打开时清掉
  s.setMicEnabled(false);
  refuse = "denied";
  s.setMicEnabled(true);
  await settle();
  assert.equal(s.micError, "denied");
  assert.equal(s.micEnabled, false);
  assert.equal(s.micLive, false);
  assert.deepEqual(mics(), [true, false, true, false, true, false]);
  refuse = null;
  s.setMicEnabled(true);
  assert.equal(s.micError, null);
  assert.equal(s.micLive, true);
  // 会话断了：停止采集，不用告诉（对方已经不在了）；对方不支持时什么也不发
  const sent = mics().length;
  s.sid = null;
  s.syncMic();
  assert.equal(capture.at(-1), "stop");
  assert.equal(mics().length, sent);
  s.sid = 8;
  s.micState = "unsupported";
  s.syncMic();
  assert.equal(mics().length, sent);
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
