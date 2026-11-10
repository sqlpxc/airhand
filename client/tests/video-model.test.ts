import assert from "node:assert/strict";
import { test } from "node:test";
import { VideoFeedbackCollector, receiverCapabilities } from "../src/lib/video-model.ts";
import { contract } from "./protocol-contract.ts";

const first = { configId: 7, instance: { streamId: 0, generation: 0 } };
test("receiver capabilities preserve unknown limits; feedback matches the shared Rust contract", () => {
  assert.deepEqual(receiverCapabilities(["h264", "h264"]), { codecs: [{ codec: "h264", maxWidth: null, maxHeight: null, maxFps: null }] });
  const f = new VideoFeedbackCollector();
  f.received(8, first);
  f.decoded(8, first, 7);
  f.presented(8, first);
  f.sample(0, 0);
  f.received(9, first);
  assert.deepEqual(f.sample(1, 12), contract.videoModel.feedback);
  assert.equal(f.sample(0, 0), null);
  assert.ok(f.sample(1, 200), "stalled decoder continues to report even without new network input");
});

test("late output cannot update the next config; cumulative telemetry survives coalescing", () => {
  const f = new VideoFeedbackCollector();
  f.received(8, first);
  f.decoded(8, first, 4);
  f.dropped(2);
  f.error();
  f.sample(0, 0);
  f.received(9, first);
  assert.equal(f.sample(0, 0)!.droppedFrames, 2);
  const next = { ...first, configId: 8 };
  f.received(10, next);
  f.decoded(9, first, 100);
  f.presented(9, first);
  const sample = f.sample(0, 0)!;
  assert.equal(sample.decodedSeq, null);
  assert.equal(sample.presentedSeq, null);
  assert.equal(sample.decoderErrors, 0);
  assert.equal(sample.sampleId, 1);
  f.reset();
  assert.equal(f.sample(0, 0), null);
});

test("receipt is not presentation, and progress stays monotonic across sequence wrap", () => {
  const f = new VideoFeedbackCollector();
  f.received(0xffffffff, first);
  assert.equal(f.sample(0, 0)!.decodedSeq, null);
  f.decoded(0xffffffff, first, 4);
  f.presented(0xffffffff, first);
  f.received(0, first);
  f.decoded(0, first, 4);
  f.presented(0, first);
  f.decoded(0xffffffff, first, 4);
  f.presented(0xffffffff, first);
  const sample = f.sample(0, 0)!;
  assert.equal(sample.decodedSeq, 0);
  assert.equal(sample.presentedSeq, 0);
});
