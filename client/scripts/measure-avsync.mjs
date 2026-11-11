// 音画同步实测：被控端用 ffplay 同时播放"白屏闪一下 + 4 kHz 短音"，在真实的控制台（WebView2）里
// 测量闪白被画出来的时刻和短音被播放的时刻，分别在关闭/打开音画同步时各测一段。
//
// 需要：被控端与控制端在同一台机器（ffplay 窗口出现在被控端屏幕左上角）、ffplay 在 PATH 里。
//   1. 启动服务器和被控端（被控端请用 release 构建，见 README"开发"一节）
//   2. AIRHAND_DEV_CONTROLLER=1 WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9333" npm run tauri dev
//   3. node scripts/measure-avsync.mjs <控制码> [配对码]（对方要配对码、这个控制台还没和它配对过时给）
//
// 会以约 -30 dBFS 播放 30 秒的 4 kHz 短音（每秒 50 ms）。
//
// 控制台的画面和声音在 Worker 里解码、绘制（src/lib/console-worker.ts），页面里只有 AudioContext 和音频线程，
// 所以要连两个 DevTools target：
// * 页面：控制端的声音输出被改接到静音的 AnalyserNode（同机测试时不会形成回声环路），
//   声音的播放时刻 = 分析器看到短音的音频时间，经 getOutputTimestamp 换算为本地时间；
// * Worker：每次画完一帧读一个像素（看闪白）、解码后的声音（核对出声时刻）、音画同步收到的声音时钟、
//   每帧送去解码时比计划晚了多少（开发构建里 Worker 的 self.__airhand.views 就是各会话的画面）。
//   Worker 的 performance.now() 从它自己创建时算起：都记成绝对时间（performance.timeOrigin + now），最后换到页面的时基。
import { spawn } from "node:child_process";

const [code, pairing] = process.argv.slice(2);
const DISPLAY_MS = 8; // 与 avsync.ts 一致：画到 Canvas 后到显示的估计值
const DEVTOOLS = "http://127.0.0.1:9333";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const targets = async () => (await fetch(`${DEVTOOLS}/json`)).json();

/** 连一个 DevTools target（页面或 Worker） */
async function attach(target) {
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => (ws.onopen = r));
  let id = 0;
  const pending = new Map();
  ws.onmessage = (m) => {
    const msg = JSON.parse(m.data);
    if (msg.id && pending.has(msg.id)) {
      pending.get(msg.id)(msg);
      pending.delete(msg.id);
    }
  };
  const send = (method, params = {}) =>
    new Promise((r) => {
      const i = ++id;
      pending.set(i, r);
      ws.send(JSON.stringify({ id: i, method, params }));
    });
  const evaluate = async (expr) => {
    const r = await send("Runtime.evaluate", { expression: expr, returnByValue: true, awaitPromise: true });
    if (r.result?.exceptionDetails) throw new Error(JSON.stringify(r.result.exceptionDetails).slice(0, 500));
    return r.result?.result?.value;
  };
  return { send, evaluate, close: () => ws.close() };
}

const page = await attach((await targets()).find((t) => t.type === "page"));
const click = async (selector) => {
  const rect = await page.evaluate(`(() => { const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
  for (const type of ["mousePressed", "mouseReleased"]) {
    await page.send("Input.dispatchMouseEvent", { type, x: rect.x, y: rect.y, button: "left", clickCount: 1 });
  }
};

const GOERTZEL = `const TONE = 4000, W = 192, H = 48;
  const goertzel = (x, off, n, sr) => {
    const w = 2 * Math.PI * TONE / sr, c = 2 * Math.cos(w);
    let s1 = 0, s2 = 0;
    for (let i = 0; i < n; i++) { const s = x[off + i] + c * s1 - s2; s2 = s1; s1 = s; }
    return Math.sqrt(Math.max(0, s1 * s1 + s2 * s2 - c * s1 * s2)) * 2 / n;
  };`;

// 页面：声音真正被播放的时刻
const pagePatch = `(() => {
  const P = (window.__probe = { amp: [], clock: [], ctx: null });
  ${GOERTZEL}
  const OrigAC = window.AudioContext;
  window.AudioContext = class extends OrigAC { constructor(...a) { super(...a); P.ctx = this; } };
  const connect = AudioNode.prototype.connect;
  AudioNode.prototype.connect = function (dest, ...rest) {
    if (this instanceof AudioWorkletNode && dest instanceof AudioDestinationNode) {
      this.port.addEventListener("message", (e) => { if (e.data && e.data.type === "clock") P.clock.push([e.data.ctxTime, e.data.hostUs / 1000]); });
      const ctx = this.context;
      const N = 2048;
      const an = new AnalyserNode(ctx, { fftSize: N });
      const mute = new GainNode(ctx, { gain: 0 });
      connect.call(this, an);
      connect.call(an, mute);
      connect.call(mute, dest);
      const buf = new Float32Array(N);
      let lastEnd = 0;
      setInterval(() => {
        an.getFloatTimeDomainData(buf);
        const end = ctx.currentTime, ts = ctx.getOutputTimestamp(), sr = ctx.sampleRate;
        if (!ts.performanceTime) return;
        const start = end - N / sr;
        const from = Math.max(0, Math.floor((lastEnd - start) * sr) - W);
        for (let i = from; i + W <= N; i += H) {
          const tc = start + (i + W / 2) / sr;
          if (tc <= lastEnd - W / 2 / sr) continue;
          P.amp.push([tc, ts.performanceTime + (tc - ts.contextTime) * 1000, goertzel(buf, i, W, sr)]);
        }
        lastEnd = end - W / 2 / sr;
      }, 5);
      return dest;
    }
    return connect.call(this, dest, ...rest);
  };
})();`;

// Worker：画面被画出来的时刻、解码后的声音
const workerPatch = `(() => {
  const P = (self.__probe = { draws: [], dec: [], decFrames: [], aoff: [], late: [], host: new Map() });
  const origin = performance.timeOrigin;
  ${GOERTZEL}
  // 解码后的声音（按送入顺序与 chunk 的时间戳 = 采集时间配对），用来独立核对"被控端哪一刻出声"
  const chunkTs = [];
  const OrigChunk = self.EncodedAudioChunk;
  self.EncodedAudioChunk = class extends OrigChunk { constructor(init) { super(init); chunkTs.push(init.timestamp); } };
  const OrigAD = self.AudioDecoder;
  self.AudioDecoder = class extends OrigAD {
    constructor(init) {
      const out = init.output;
      super({ ...init, output: (d) => {
        const host = chunkTs.shift();
        const n = d.numberOfFrames;
        if (P.decFrames.length < 6) P.decFrames.push(n);
        const x = new Float32Array(n);
        d.copyTo(x, { planeIndex: 0, format: "f32-planar" });
        const first = host + (960 - n) / 48000 * 1e6;
        for (let i = 0; i + W <= n; i += H) P.dec.push([first / 1000 + (i + W / 2) / 48, goertzel(x, i, W, 48000)]);
        out(d);
      } });
    }
  };
  // 画面：每画一帧读一个像素（ffplay 窗口在被控端屏幕左上角）。帧的时间戳是序号 × 1000，采集时间在送去解码时记下
  const draw = OffscreenCanvasRenderingContext2D.prototype.drawImage;
  OffscreenCanvasRenderingContext2D.prototype.drawImage = function (...a) {
    const host = P.host.get(a[0].timestamp);
    P.host.delete(a[0].timestamp);
    const r = draw.apply(this, a);
    const cw = this.canvas.width, ch = this.canvas.height;
    const p = this.getImageData(Math.round(40 * cw / 1920), Math.round(40 * ch / 1080), 1, 1).data;
    P.draws.push([origin + performance.now(), (p[0] + p[1] + p[2]) / 3, host === undefined ? 0 : host / 1000]);
    return r;
  };
})()`;

// Worker：第一个会话的画面一建好（点连接时）就接上音画同步和解码（返回的 Promise 到那时才完成）。
// 接在原型上：要配对码时第一次连接的会话会被丢掉，输入配对码后连上的是新的会话
const viewPatch = `new Promise((resolve) => {
  const P = self.__probe, origin = performance.timeOrigin;
  const t = setInterval(() => {
    const v = [...self.__airhand.views.values()][0];
    if (!v) return;
    clearInterval(t);
    const sync = Object.getPrototypeOf(v.sync), on = sync.onAudioClock;
    sync.onAudioClock = function (h, l, a) { P.aoff.push([origin + l, l - h / 1000, a]); return on.call(this, h, l, a); };
    const video = Object.getPrototypeOf(v.video), decode = video.decode;
    video.decode = function (x) {
      P.host.set(x.seq * 1000, x.hostUs);
      if (P.host.size > 256) P.host.delete(P.host.keys().next().value);
      P.late.push([x.hostUs / 1000, performance.now() - x.due, this.decodeAvg]);
      return decode.call(this, x);
    };
    resolve(true);
  }, 2);
})`;

await page.send("Page.enable");
await page.send("Runtime.enable");
await page.send("Page.addScriptToEvaluateOnNewDocument", { source: pagePatch });
const before = new Set((await targets()).map((t) => t.id));
await page.send("Page.reload", { ignoreCache: true });
// 重新加载后的新 Worker
let workerTarget;
for (let i = 0; i < 100 && !workerTarget; i++) {
  await sleep(100);
  workerTarget = (await targets()).find((t) => t.type === "worker" && t.url.includes("console-worker") && !before.has(t.id));
}
if (!workerTarget) throw new Error("the console's Worker didn't show up (is this a dev build?)");
const worker = await attach(workerTarget);
await worker.evaluate(workerPatch);
await sleep(2000);

const viewHooked = worker.evaluate(viewPatch);
await page.evaluate(`document.querySelector('.code-box').focus()`);
for (const ch of code) {
  await page.send("Input.insertText", { text: ch });
  await sleep(60);
}
await sleep(300);
await click(".primary-btn");
await sleep(2000);
if (pairing && (await page.evaluate(`!!document.querySelector('.pairing-input')`))) {
  await page.evaluate(`document.querySelector('.pairing-input').focus()`);
  await page.send("Input.insertText", { text: pairing });
  await sleep(300);
  await click(".primary-btn");
}
await viewHooked;
await sleep(2500);

// 被控端（同一台机器）：左上角置顶的小窗口每秒闪白 100 ms，同时播放 50 ms 的 4 kHz 短音
const graph =
  "color=c=black:s=320x240:r=60,drawbox=x=0:y=0:w=iw:h=ih:color=white:t=fill:enable='lt(mod(t,1),0.1)'[out0];" +
  "aevalsrc='0.03*sin(2*PI*4000*t)*lt(mod(t,1),0.05)':s=48000:c=stereo[out1]";
const ff = spawn("ffplay", ["-f", "lavfi", "-i", graph, "-t", "32", "-x", "320", "-y", "240", "-left", "0", "-top", "0", "-noborder", "-alwaysontop", "-autoexit", "-loglevel", "error"], { stdio: "ignore" });
ff.on("error", (e) => {
  console.error(`can't start ffplay: ${e.message}`);
  process.exit(1);
});
await sleep(2500);

const phase = async (name, ms) => {
  const t0 = await page.evaluate("performance.now()");
  await sleep(ms);
  const t1 = await page.evaluate("performance.now()");
  return { name, t0, t1 };
};
const off = await phase("关闭同步", 11000);
await click('.tb-btn.icon[title^="声音"]');
await sleep(400);
const sw = await page.evaluate(`(() => { const l = [...document.querySelectorAll('.audio-menu .dd-item')].find(e => e.textContent.includes('音画同步')); l.querySelector('input').click(); return l.textContent; })()`);
await sleep(300);
await page.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
await click(".stage");
await sleep(5000); // 让同步收敛
const on = await phase("打开同步", 11000);
const hint = await page.evaluate(`[...document.querySelectorAll('.addr-stats span')].map(e => e.textContent).join(' | ')`);
ff.kill();

const pp = await page.evaluate(`(() => { const P = window.__probe; return { origin: performance.timeOrigin, amp: P.amp, clock: P.clock, outputLatency: P.ctx && P.ctx.outputLatency, baseLatency: P.ctx && P.ctx.baseLatency, rate: P.ctx && P.ctx.sampleRate }; })()`);
const wp = await worker.evaluate(`(() => { const P = self.__probe; return { draws: P.draws, dec: P.dec, decFrames: P.decFrames, aoff: P.aoff, late: P.late }; })()`);
const s = await page.evaluate(`(() => { const s = window.__airhand.store.get().sessions[0]; return { sync: s.syncStats, jitter: s.audio.stats }; })()`);
page.close();
worker.close();
// Worker 记的绝对时间 → 页面的时基
const local = (abs) => abs - pp.origin;
const P = {
  ...pp,
  ...wp,
  draws: wp.draws.map(([t, v, host]) => [local(t), v, host]),
  aoff: wp.aoff.map(([t, off, applied]) => [local(t), off, applied]),
};

// ── 分析 ──
const onsets = (series, getT, getAmp, minGapMs) => {
  const peak = series.map(getAmp).sort((a, b) => a - b)[Math.floor(series.length * 0.995)] ?? 0;
  const thr = peak * 0.35;
  const out = [];
  let last = -Infinity;
  for (const x of series) {
    const t = getT(x);
    if (getAmp(x) > thr && t - last > minGapMs) out.push(x);
    if (getAmp(x) > thr) last = t;
  }
  return { out, thr, peak };
};
// 画面：由暗变亮的那一次绘制
const flashes = [];
for (let i = 1; i < P.draws.length; i++) {
  if (P.draws[i][1] > 170 && P.draws[i - 1][1] < 80) flashes.push({ local: P.draws[i][0] + DISPLAY_MS, host: P.draws[i][2] });
}
// 声音：播放时刻（分析器）与采集时刻（音频线程的时钟报告换算）
const heard = onsets(P.amp, (x) => x[1], (x) => x[2], 500);
const hostOfCtx = (tc) => {
  let best = null;
  for (const c of P.clock) if (c[0] <= tc && (!best || c[0] > best[0])) best = c;
  return best ? best[1] + (tc - best[0]) * 1000 : NaN;
};
const beeps = heard.out.map((x) => ({ local: x[1], host: hostOfCtx(x[0]) }));
// 解码后的声音里找到的出声时刻（只依赖包时间戳，与音频线程无关），用来核对上面的采集时刻
const decoded = onsets(P.dec, (x) => x[0], (x) => x[1], 500).out.map((x) => x[0]);

const stat = (v) => {
  if (v.length === 0) return "—";
  const m = v.reduce((a, b) => a + b, 0) / v.length;
  return `${m.toFixed(1)} ms（${Math.min(...v).toFixed(1)} ~ ${Math.max(...v).toFixed(1)}，n=${v.length}）`;
};
console.log(`解码器输出帧数（前几包）: ${P.decFrames.join(", ")}；outputLatency=${(P.outputLatency * 1000).toFixed(1)} ms baseLatency=${(P.baseLatency * 1000).toFixed(1)} ms rate=${P.rate}`);
console.log(`闪白 ${flashes.length} 次，短音 ${beeps.length} 次，解码出的短音 ${decoded.length} 次；开关: ${sw.trim().slice(0, 20)}…`);
const check = beeps.map((b) => decoded.reduce((best, d) => (Math.abs(d - b.host) < Math.abs(best - b.host) ? d : best), Infinity) - b.host).filter((d) => Math.abs(d) < 100);
console.log(`核对：声音采集时刻（音频线程时钟）− 解码数据里的出声时刻 = ${stat(check)}`);
for (const ph of [off, on]) {
  const host = [], local = [], added = [];
  for (const f of flashes.filter((f) => f.local >= ph.t0 && f.local < ph.t1)) {
    const b = beeps.find((b) => Math.abs(b.host - f.host) < 300);
    if (!b) continue;
    host.push(b.host - f.host);
    local.push(b.local - f.local);
    added.push(b.local - f.local - (b.host - f.host));
  }
  console.log(`\n【${ph.name}】`);
  console.log(`  被控端：短音采集时刻 − 闪白采集时刻 = ${stat(host)}`);
  console.log(`  控制端：短音播放时刻 − 闪白显示时刻 = ${stat(local)}`);
  console.log(`  传输引入的音画差（正数 = 声音晚）    = ${stat(added)}`);
  console.log(`  每次: ${added.map((x) => x.toFixed(0)).join(" ")}`);
  const ao = P.aoff.filter((x) => x[0] >= ph.t0 && x[0] < ph.t1).map((x) => x[1]);
  if (ao.length) console.log(`  声音偏移 A 样本: ${(Math.max(...ao) - Math.min(...ao)).toFixed(1)} ms 范围（n=${ao.length}）`);
  const lt = P.late.map((x) => x[1]);
  if (ph === on && lt.length) console.log(`  送解码比计划晚: 平均 ${(lt.reduce((a, b) => a + b, 0) / lt.length).toFixed(1)} ms，最多 ${Math.max(...lt).toFixed(1)} ms`);
}
console.log(`\n结束时：${JSON.stringify(s.sync)}；抖动缓冲 ${JSON.stringify(s.jitter)}；工具栏: ${hint}`);
