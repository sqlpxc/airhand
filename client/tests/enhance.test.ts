// 画面增强（enhance.ts）：canvas 画多大、什么时候增强。着色器本身要在浏览器里看（见 docs/development.md）
import assert from "node:assert/strict";
import { test } from "node:test";
import { enhance, renderPlan } from "../src/lib/enhance.ts";
import { fitSize } from "../src/lib/fit.ts";

test("关着：canvas 和画面一样大，由浏览器缩放", () => {
  assert.deepEqual(renderPlan(1280, 720, { w: 1920, h: 1080 }, "off"), { w: 1280, h: 720, enhance: false });
  assert.deepEqual(renderPlan(1280, 720, null, "off"), { w: 1280, h: 720, enhance: false });
});

test("要放大：canvas 就是显示的大小，与主线程的 fitSize 一致", () => {
  assert.deepEqual(renderPlan(1280, 720, { w: 1920, h: 1080 }, "medium"), { w: 1920, h: 1080, enhance: true });
  // 宽高比不同：留黑边，按 fitSize
  for (const [fw, fh, vw, vh] of [[1280, 720, 2000, 1000], [1920, 1080, 2560, 1600], [720, 1600, 1440, 2400]]) {
    const s = fitSize(fw, fh, vw, vh);
    assert.deepEqual(renderPlan(fw, fh, { w: vw, h: vh }, "low"), { w: s.w, h: s.h, enhance: true });
  }
});

test("1:1（原始大小显示、或者和显示区域差不多大）：只锐化", () => {
  assert.deepEqual(renderPlan(1920, 1080, null, "high"), { w: 1920, h: 1080, enhance: true });
  assert.deepEqual(renderPlan(1600, 1000, { w: 1650, h: 1030 }, "high"), { w: 1600, h: 1000, enhance: true });
});

test("要缩小（窗口刚变小、新尺寸的画面还没到）：交给浏览器，不增强", () => {
  assert.deepEqual(renderPlan(1920, 1080, { w: 1600, h: 900 }, "high"), { w: 1920, h: 1080, enhance: false });
  assert.deepEqual(renderPlan(0, 0, { w: 1600, h: 900 }, "high"), { w: 0, h: 0, enhance: false });
});

test("没有 OffscreenCanvas / WebGL2：返回 null，照原来的方法画", () => {
  assert.equal(enhance({ displayWidth: 16, displayHeight: 16 } as VideoFrame, 32, 32, "medium"), null);
});
