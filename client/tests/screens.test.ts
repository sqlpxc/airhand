import assert from "node:assert/strict";
import { test } from "node:test";
import { hitTest, layoutTiles, placeDisplays } from "../src/lib/screens.ts";

const d = (x: number, y: number, width: number, height: number) => ({ x, y, width, height });

test("显示器按对方桌面上的位置摆，原点挪到左上角；位置对不上（重叠）时排成一排", () => {
  // 副屏在主屏左边、往下错开一点（桌面坐标可以是负的）
  assert.deepEqual(placeDisplays([d(0, 0, 1920, 1080), d(-1280, 200, 1280, 1024)]), [
    { x: 1280, y: 0, w: 1920, h: 1080 },
    { x: 0, y: 200, w: 1280, h: 1024 },
  ]);
  // 上下摆的
  assert.deepEqual(placeDisplays([d(0, 0, 2560, 1440), d(320, -1080, 1920, 1080)]), [
    { x: 0, y: 1080, w: 2560, h: 1440 },
    { x: 320, y: 0, w: 1920, h: 1080 },
  ]);
  // 重叠（例如对方报的坐标和大小不是一种单位）：从左到右一排
  assert.deepEqual(placeDisplays([d(0, 0, 2880, 1800), d(1440, 0, 1920, 1080)]), [
    { x: 0, y: 0, w: 2880, h: 1800 },
    { x: 2880, y: 0, w: 1920, h: 1080 },
  ]);
});

test("摆好的几块整体等比缩放进显示区域，边对齐到整数设备像素、相邻的正好接上", () => {
  const placed = placeDisplays([d(0, 0, 1920, 1080), d(1920, 0, 1920, 1080), d(3840, 0, 1920, 1080)]);
  const { tiles, w, h, scale } = layoutTiles(placed, { w: 2000, h: 1000 });
  assert.equal(scale, 2000 / 5760);
  assert.equal(w, 2000);
  assert.equal(h, Math.round(1080 * scale));
  // 每一块的右边就是下一块的左边：不留缝、不重叠
  assert.equal(tiles[0].x + tiles[0].w, tiles[1].x);
  assert.equal(tiles[1].x + tiles[1].w, tiles[2].x);
  assert.equal(tiles[2].x + tiles[2].w, w);
  for (const t of tiles) assert.ok(Number.isInteger(t.x) && Number.isInteger(t.w));
  // 原始大小：对方的一个像素一个设备像素
  const native = layoutTiles(placed, null);
  assert.deepEqual([native.w, native.h, native.scale], [5760, 1080, 1]);
  assert.deepEqual(native.tiles[1], { x: 1920, y: 0, w: 1920, h: 1080 });
  assert.deepEqual(layoutTiles([], { w: 10, h: 10 }).tiles, []);
});

test("指针落在哪一路画面上：里面就是它，缝里、外面时找最近的一块，坐标夹在 0..1", () => {
  const rects = [
    { left: 0, top: 0, width: 100, height: 100 },
    { left: 100, top: 50, width: 50, height: 50 },
  ];
  assert.deepEqual(hitTest(rects, 50, 50), { index: 0, x: 0.5, y: 0.5 });
  assert.deepEqual(hitTest(rects, 125, 75), { index: 1, x: 0.5, y: 0.5 });
  // 第二块上面的空白：离第二块的上边最近
  assert.deepEqual(hitTest(rects, 145, 10), { index: 1, x: 0.9, y: 0 });
  // 外面：最近的那一块的边上
  assert.deepEqual(hitTest(rects, -20, 120), { index: 0, x: 0, y: 1 });
  assert.equal(hitTest([], 0, 0), null);
});
