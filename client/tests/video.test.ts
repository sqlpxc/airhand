import assert from "node:assert/strict";
import { test } from "node:test";
import { VideoPlayer } from "../src/lib/video.ts";

test("decoder configure errors recover; late output is closed without drawing", () => {
  const previous = (globalThis as any).VideoDecoder;
  const chunks = (globalThis as any).EncodedVideoChunk;
  const decoders: any[] = [];
  let fail = true;
  let drawn = 0, closed = 0, unsupported = 0, keys = 0;
  (globalThis as any).VideoDecoder = class {
    state = "unconfigured";
    decodeQueueSize = 0;
    callbacks: any;
    constructor(callbacks: any) { this.callbacks = callbacks; decoders.push(this); }
    configure() { if (fail) throw new Error("unsupported config"); this.state = "configured"; }
    close() { this.state = "closed"; }
    decode() {}
  };
  (globalThis as any).EncodedVideoChunk = class { constructor(_: any) {} };
  const player = new VideoPlayer({
    canvas: { width: 16, height: 16 } as any,
    ctx: { drawImage() { drawn++; } } as any,
    sync: { onVideo: (_h: number, now: number) => now } as any,
    requestKeyframe() { keys++; }, unsupported() { unsupported++; }, recovered() {}, resized() {},
  });
  const key = { seq: 1, key: true, codec: "av1" as const, width: 16, height: 16, captureUs: 0 };
  const data = new Uint8Array(1);
  try {
    assert.doesNotThrow(() => player.push(key, data));
    assert.equal(unsupported, 1);
    assert.equal(keys, 1);
    assert.equal(decoders[0].state, "closed");
    fail = false;
    player.push(key, data);
    player.reset();
    decoders[1].callbacks.output({ close() { closed++; } });
    assert.equal(closed, 1);
    assert.equal(drawn, 0);
  } finally {
    player.reset();
    (globalThis as any).VideoDecoder = previous;
    (globalThis as any).EncodedVideoChunk = chunks;
  }
});

test("画面增强：canvas 变成显示的大小、用最后一帧马上重画；增强不了时用浏览器缩放；报的是画面的大小", () => {
  const previous = (globalThis as any).VideoDecoder;
  const chunks = (globalThis as any).EncodedVideoChunk;
  const decoders: any[] = [];
  (globalThis as any).VideoDecoder = class {
    state = "unconfigured";
    decodeQueueSize = 0;
    callbacks: any;
    constructor(callbacks: any) { this.callbacks = callbacks; decoders.push(this); }
    configure() { this.state = "configured"; }
    close() { this.state = "closed"; }
    decode() {}
  };
  (globalThis as any).EncodedVideoChunk = class { constructor(_: any) {} };
  const canvas = { width: 16, height: 16 };
  const draws: any[][] = [];
  const ctx: any = { drawImage: (...args: any[]) => draws.push([...args, ctx.imageSmoothingQuality]) };
  const sizes: number[][] = [];
  const enhanced: any[][] = [];
  let enhanceWorks = true;
  const enhancedCanvas = { width: 0, height: 0 };
  const player = new VideoPlayer({
    canvas: canvas as any, ctx,
    sync: { onVideo: (_h: number, now: number) => now } as any,
    requestKeyframe() {}, unsupported() {}, recovered() {},
    resized: (w, h) => sizes.push([w, h]),
    enhance: (frame, w, h, level) => {
      enhanced.push([frame.timestamp, w, h, level]);
      return enhanceWorks ? enhancedCanvas as any : null;
    },
  });
  const frame = (timestamp: number, w = 1280, h = 720) => ({
    timestamp, displayWidth: w, displayHeight: h, closed: false, close() { this.closed = true; },
  });
  try {
    player.push({ seq: 1, key: true, codec: "av1", width: 1280, height: 720, captureUs: 0 }, new Uint8Array(1));
    const output = (f: any) => decoders[0].callbacks.output(f);

    // 不增强：和以前一样按画面的大小画
    const a = frame(1);
    output(a);
    assert.deepEqual(sizes, [[1280, 720]]);
    assert.deepEqual([canvas.width, canvas.height], [1280, 720]);
    assert.deepEqual(draws.at(-1)!.slice(0, 3), [a, 0, 0]);
    assert.equal(a.closed, false, "最后一帧留着重画用");
    const b = frame(2);
    output(b);
    assert.equal(a.closed, true);
    assert.deepEqual(sizes, [[1280, 720]], "大小没变不再报");

    // 打开增强：没有新的帧也马上按显示的大小重画
    player.setLayout({ w: 1920, h: 1080 }, "medium");
    assert.deepEqual(enhanced, [[2, 1920, 1080, "medium"]]);
    assert.deepEqual([canvas.width, canvas.height], [1920, 1080]);
    assert.deepEqual(draws.at(-1)!.slice(0, 3), [enhancedCanvas, 0, 0]);
    assert.deepEqual(sizes, [[1280, 720]], "报的还是画面的大小，不是 canvas 的");
    const n = draws.length;
    player.setLayout({ w: 1920, h: 1080 }, "medium");
    assert.equal(draws.length, n, "没变不重画");

    // 新的帧也增强
    output(frame(3));
    assert.deepEqual(enhanced.at(-1), [3, 1920, 1080, "medium"]);

    // 增强不了（没有 WebGL2、出错）：用浏览器的高质量缩放画到显示的大小
    enhanceWorks = false;
    player.setLayout({ w: 2560, h: 1440 }, "high");
    assert.deepEqual([canvas.width, canvas.height], [2560, 1440]);
    assert.deepEqual(draws.at(-1)!.slice(1), [0, 0, 2560, 1440, "high"]);

    // 要缩小：交给浏览器
    enhanceWorks = true;
    const calls = enhanced.length;
    player.setLayout({ w: 1000, h: 600 }, "high");
    assert.equal(enhanced.length, calls);
    assert.deepEqual([canvas.width, canvas.height], [1280, 720]);

    // 关掉：回到画面的大小
    player.setLayout(null, "off");
    assert.deepEqual([canvas.width, canvas.height], [1280, 720]);
    assert.deepEqual(draws.at(-1)!.length, 4);

    // 增强的结果画不上去（浏览器不支持）：这次和以后都用浏览器缩放，不再增强
    const drawImage = ctx.drawImage;
    ctx.drawImage = (...args: any[]) => {
      if (args[0] === enhancedCanvas) throw new Error("unsupported");
      return drawImage(...args);
    };
    player.setLayout({ w: 1920, h: 1080 }, "low");
    assert.deepEqual(draws.at(-1)!.slice(1, 5), [0, 0, 1920, 1080]);
    const tried = enhanced.length;
    player.setLayout({ w: 2560, h: 1440 }, "low");
    assert.equal(enhanced.length, tried);
    assert.deepEqual(draws.at(-1)!.slice(1, 5), [0, 0, 2560, 1440]);

    const last = frame(4);
    output(last);
    player.dispose();
    assert.equal(last.closed, true, "不要这个画面了：最后一帧也放掉");
  } finally {
    player.dispose();
    (globalThis as any).VideoDecoder = previous;
    (globalThis as any).EncodedVideoChunk = chunks;
  }
});
