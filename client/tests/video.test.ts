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
