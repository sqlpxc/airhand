import assert from "node:assert/strict";
import { test } from "node:test";
import { VideoPlayer } from "../src/lib/video.ts";

test("feedback reports decoder progress independently of receipt and ignores old config callbacks", () => {
  const oldDecoder = (globalThis as any).VideoDecoder;
  const oldChunk = (globalThis as any).EncodedVideoChunk;
  const oldInterval = globalThis.setInterval;
  const oldClear = globalThis.clearInterval;
  const timers = new Map<number, () => void>();
  let next = 0;
  let decoder: any;
  const reports: any[] = [];
  (globalThis as any).setInterval = (fn: () => void) => { timers.set(++next, fn); return next; };
  (globalThis as any).clearInterval = (id: number) => timers.delete(id);
  (globalThis as any).VideoDecoder = class {
    state = "unconfigured"; decodeQueueSize = 0;
    callbacks: any;
    constructor(callbacks: any) { this.callbacks = callbacks; decoder = this; }
    configure() { this.state = "configured"; }
    decode() {}
    close() { this.state = "closed"; }
  };
  (globalThis as any).EncodedVideoChunk = class { constructor(_: any) {} };
  let delay = 0;
  const player = new VideoPlayer({
    canvas: { width: 16, height: 16 } as any,
    ctx: { drawImage() {} } as any,
    sync: { onVideo: (_h: number, now: number) => now + delay } as any,
    requestKeyframe() {}, unsupported() {}, recovered() {}, resized() {},
    feedback: f => reports.push(f),
  });
  const context = { configId: 1, instance: { streamId: 0, generation: 0 } };
  const packet = { seq: 1, key: true, codec: "av1" as const, width: 16, height: 16, captureUs: 0, context };
  const sample = () => { for (const tick of timers.values()) tick(); return reports.at(-1); };
  const output = (seq: number) => decoder.callbacks.output({ timestamp: seq * 1000, displayWidth: 16, displayHeight: 16, close() {} });
  try {
    player.push(packet, new Uint8Array(1));
    assert.equal(sample().receivedSeq, 1);
    assert.equal(reports.at(-1).decodedSeq, null);
    assert.equal(reports.at(-1).pendingFrames, 1);
    player.push({ ...packet, seq: 2, key: false }, new Uint8Array(1));
    output(1);
    const f = sample();
    assert.equal(f.receivedSeq, 2);
    assert.equal(f.decodedSeq, 1);
    assert.equal(f.presentedSeq, 1);
    assert.equal(f.pendingFrames, 1);
    player.push({ ...packet, seq: 3, context: { ...context, configId: 2 } }, new Uint8Array(1));
    output(2);
    assert.equal(sample().decodedSeq, null, "output from the previous config is not current progress");
    output(3);
    assert.equal(sample().presentedSeq, 3);
    player.reset();
    assert.equal(timers.size, 0, "session reset stops feedback");
    delay = 500;
    player.push(packet, new Uint8Array(1));
    assert.equal(sample().pendingFrames, 0, "intentional A/V waiting is not decoder backlog");
  } finally {
    player.reset();
    (globalThis as any).VideoDecoder = oldDecoder;
    (globalThis as any).EncodedVideoChunk = oldChunk;
    globalThis.setInterval = oldInterval;
    globalThis.clearInterval = oldClear;
  }
});

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
