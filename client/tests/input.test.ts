import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteInput, isLockShortcut } from "../src/lib/input.ts";

/** 记下注册的事件处理函数，测试里直接调用 */
function fakeCanvas() {
  const handlers = new Map<string, (e: any) => void>();
  const locks: any[] = [];
  const canvas: any = {
    addEventListener(name: string, fn: (e: any) => void) { handlers.set(name, fn); },
    focus() {},
    setPointerCapture() {},
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
    requestPointerLock(options?: any) { locks.push(options); return Promise.resolve(); },
  };
  const fire = (name: string, e: any) => handlers.get(name)!({ preventDefault() {}, stopPropagation() {}, ...e });
  return { canvas, fire, locks };
}

test("pending input cannot leak into a replacement session", async () => {
  const batches: unknown[] = [];
  const input = new RemoteInput({ addEventListener() {} } as any, () => true, events => batches.push(events));
  input.enqueue([{ type: "key", code: "KeyA", down: true }], true);
  input.reset();
  input.enqueue([{ type: "key", code: "KeyB", down: false }], true);
  await Promise.resolve();
  assert.deepEqual(batches, [[{ type: "key", code: "KeyB", down: false }]]);
  input.dispose();
});

test("mouse moves coalesce, ordered key edges stay in bounded batches", async () => {
  const batches: any[][] = [];
  const input = new RemoteInput({ addEventListener() {} } as any, () => true, events => batches.push(events));
  for (let i = 0; i < 1000; i++) input.enqueue([{ type: "mouseMove", x: i / 1000, y: 0 }], false);
  input.enqueue([{ type: "releaseAll" }], true);
  await Promise.resolve();
  assert.equal(batches[0].length, 2);
  for (let i = 0; i < 200; i++) input.enqueue([{ type: "key", code: "KeyA", down: i % 2 === 0 }], true);
  await Promise.resolve();
  assert.ok(batches.every(b => b.length <= 64));
  assert.equal(batches.slice(1).flat().length, 200);
  input.dispose();
});

test("鼠标移动前沿节流：闲着之后的第一下马上发，紧跟着的合并到间隔到了再发", async () => {
  const batches: any[][] = [];
  const input = new RemoteInput({ addEventListener() {} } as any, () => true, events => batches.push(events));
  input.enqueue([{ type: "mouseMove", x: 0.1, y: 0 }], false);
  await Promise.resolve();
  assert.deepEqual(batches, [[{ type: "mouseMove", x: 0.1, y: 0 }]]);
  // 刚发过：等到间隔到了，只发最后一个位置
  input.enqueue([{ type: "mouseMove", x: 0.2, y: 0 }], false);
  input.enqueue([{ type: "mouseMove", x: 0.3, y: 0 }], false);
  await Promise.resolve();
  assert.equal(batches.length, 1);
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(batches, [[{ type: "mouseMove", x: 0.1, y: 0 }], [{ type: "mouseMove", x: 0.3, y: 0 }]]);
  // 点击不等：和排着的移动一起马上发
  input.enqueue([{ type: "mouseMove", x: 0.4, y: 0 }], false);
  input.enqueue([{ type: "mouseButton", button: "left", down: true }], true);
  await Promise.resolve();
  assert.deepEqual(batches[2], [{ type: "mouseMove", x: 0.4, y: 0 }, { type: "mouseButton", button: "left", down: true }]);
  input.dispose();
});

test("相对移动加起来，不越过点击", async () => {
  const batches: any[][] = [];
  const input = new RemoteInput({ addEventListener() {} } as any, () => true, events => batches.push(events));
  for (let i = 0; i < 10; i++) input.enqueue([{ type: "mouseDelta", dx: 2, dy: -1 }], false);
  input.enqueue([{ type: "mouseButton", button: "right", down: false }], true);
  input.enqueue([{ type: "mouseDelta", dx: 1, dy: 1 }], true);
  await Promise.resolve();
  assert.deepEqual(batches[0], [
    { type: "mouseDelta", dx: 20, dy: -10 },
    { type: "mouseButton", button: "right", down: false },
    { type: "mouseDelta", dx: 1, dy: 1 },
  ]);
  input.dispose();
});

test("被控端光标隐藏时：按着右键就锁定、发相对移动，松开时不带绝对坐标", async () => {
  const batches: any[][] = [];
  const { canvas, fire, locks } = fakeCanvas();
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  let modes = 0;
  input.onModeChange = () => modes++;

  // 光标可见：绝对坐标，按下右键前先移到按下的位置
  fire("pointerdown", { button: 2, buttons: 2, clientX: 50, clientY: 25, pointerId: 1, movementX: 0, movementY: 0 });
  await Promise.resolve();
  assert.deepEqual(batches.shift(), [{ type: "mouseMove", x: 0.5, y: 0.25 }, { type: "mouseButton", button: "right", down: true }]);
  assert.equal(input.relative, false);

  // 游戏开始转视角：光标隐藏了。按着键，马上请求锁定（原始计数）
  input.setRemoteCursorHidden(true);
  assert.equal(input.relative, true);
  assert.deepEqual(locks, [{ unadjustedMovement: true }]);
  assert.equal(modes, 1);

  // 还没锁定也发相对移动（CSS 像素 × devicePixelRatio，这里没有 devicePixelRatio 就是 1），小数部分留到下次
  fire("pointermove", { buttons: 2, movementX: 3.5, movementY: -2, clientX: 99, clientY: 99 });
  fire("pointermove", { buttons: 2, movementX: 0.5, movementY: 0, clientX: 100, clientY: 100 });
  // 同时按下左键（WoW 里两个键一起按是往前走）：只在 pointermove 里出现
  fire("pointermove", { buttons: 3, movementX: 0, movementY: 0, clientX: 100, clientY: 100 });
  fire("pointerup", { button: 2, buttons: 1, movementX: 0, movementY: 0, clientX: 100, clientY: 100 });
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(batches.flat(), [
    { type: "mouseDelta", dx: 4, dy: -2 },
    { type: "mouseButton", button: "left", down: true },
    { type: "mouseButton", button: "right", down: false },
  ]);
  batches.length = 0;

  // 光标出现了：回到绝对坐标
  input.setRemoteCursorHidden(false);
  assert.equal(input.relative, false);
  fire("pointermove", { buttons: 0, movementX: 1, movementY: 1, clientX: 10, clientY: 10 });
  await new Promise((r) => setTimeout(r, 20));
  // 先移到松开的位置再松开
  assert.deepEqual(batches.flat(), [{ type: "mouseMove", x: 0.1, y: 0.1 }, { type: "mouseButton", button: "left", down: false }]);
  input.dispose();
});

test("手动锁定的快捷键不发给被控端", async () => {
  const batches: any[][] = [];
  const { canvas, fire, locks } = fakeCanvas();
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  const m = { code: "KeyM", ctrlKey: true, altKey: true, shiftKey: true, metaKey: false, repeat: false };
  assert.ok(isLockShortcut(m as KeyboardEvent));
  fire("keydown", m);
  fire("keyup", m);
  await Promise.resolve();
  assert.equal(input.manualLock, true);
  assert.equal(input.relative, true);
  assert.equal(locks.length, 1);
  assert.deepEqual(batches, []);
  fire("keydown", m);
  assert.equal(input.manualLock, false);
  input.dispose();
});
