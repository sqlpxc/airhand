import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteInput, isLockShortcut, physicalCode } from "../src/lib/input.ts";

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

/** 假的 document：指针锁定马上成功、马上解除（要在创建 RemoteInput 之前装上） */
function fakeLockDocument(canvas: any) {
  const handlers = new Map<string, () => void>();
  const doc: any = {
    pointerLockElement: null,
    addEventListener(name: string, fn: () => void) { handlers.set(name, fn); },
    exitPointerLock() { doc.pointerLockElement = null; handlers.get("pointerlockchange")?.(); },
  };
  canvas.requestPointerLock = () => {
    doc.pointerLockElement = canvas;
    handlers.get("pointerlockchange")?.();
    return Promise.resolve();
  };
  (globalThis as any).document = doc;
  return () => { delete (globalThis as any).document; };
}

test("Ctrl / Command 按左右互换，开关不改变按住的组合或菜单发出的按键", async () => {
  const { canvas, fire } = fakeCanvas();
  const sent: string[] = [];
  const input = new RemoteInput(canvas, () => true, events => {
    for (const e of events) if (e.type === "key") sent.push(`${e.code}${e.down ? "+" : "-"}`);
  });
  input.interactive = true;
  let swap = false;
  input.shouldSwapCtrlCommand = () => swap;
  const down = (code: string, repeat = false) => fire("keydown", { code, repeat });
  const up = (code: string) => fire("keyup", { code });
  down("ControlLeft");
  swap = true;
  down("ControlLeft", true); down("MetaLeft"); up("MetaLeft"); up("ControlLeft");
  await Promise.resolve();
  assert.deepEqual(sent.splice(0), ["ControlLeft+", "ControlLeft+", "MetaLeft+", "MetaLeft-", "ControlLeft-"]);
  for (const side of ["Left", "Right"]) {
    for (const [local, remote] of [["Control", "Meta"], ["Meta", "Control"]]) {
      down(local + side); down("KeyC"); up("KeyC"); up(local + side);
      await Promise.resolve();
      assert.deepEqual(sent.splice(0), [remote + side + "+", "KeyC+", "KeyC-", remote + side + "-"]);
    }
  }
  // sendCombo 的键码已经是远端语义，直接入队不能再互换。
  input.enqueue([{ type: "key", code: "MetaLeft", down: true }, { type: "key", code: "MetaLeft", down: false }], true);
  await Promise.resolve();
  assert.deepEqual(sent, ["MetaLeft+", "MetaLeft-"]);
  input.dispose();
});

test("Mac 控制端按本地 Command 清理漏报的 keyup，不跟随远端映射", async (t) => {
  t.mock.getter(globalThis, "navigator", () => ({ platform: "MacIntel" } as Navigator));
  // IS_MAC 在模块加载时判断；用独立的模块实例模拟 Mac 控制端。
  const moduleUrl = new URL("../src/lib/input.ts?mac-controller", import.meta.url);
  const { RemoteInput: MacInput } = await import(moduleUrl.href) as typeof import("../src/lib/input.ts");
  const { canvas, fire } = fakeCanvas();
  const sent: string[] = [];
  const input = new MacInput(canvas, () => true, events => {
    for (const e of events) if (e.type === "key" && !e.down) sent.push(e.code);
  });
  input.interactive = true;
  input.shouldSwapCtrlCommand = () => true;
  fire("keydown", { code: "ControlLeft" }); fire("keydown", { code: "KeyC" });
  fire("keyup", { code: "ControlLeft" });
  await Promise.resolve();
  assert.deepEqual(sent.splice(0), ["MetaLeft"], "本机 Ctrl 不冲刷其他键");
  // 本机 Command 的按下可能发生在画面获得焦点之前。
  fire("keyup", { code: "MetaLeft" });
  await Promise.resolve();
  assert.deepEqual(sent, ["KeyC", "ControlLeft"]);
  input.dispose();
});

const LOCK_SHORTCUT = { code: "KeyM", ctrlKey: true, altKey: true, shiftKey: true, metaKey: false, repeat: false };
const ESC = { code: "Escape", repeat: false };

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

  // 光标出现了，可左键还按着：松开之前照样发相对移动；松开那一下也是（不带绝对坐标），之后回到绝对坐标
  input.setRemoteCursorHidden(false);
  assert.equal(input.relative, true);
  fire("pointerup", { button: 0, buttons: 0, movementX: 1, movementY: 1, clientX: 10, clientY: 10 });
  assert.equal(input.relative, false);
  assert.equal(modes, 2);
  fire("pointermove", { buttons: 0, movementX: 1, movementY: 1, clientX: 20, clientY: 20 });
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(batches.flat(), [
    { type: "mouseDelta", dx: 1, dy: 1 },
    { type: "mouseButton", button: "left", down: false },
    { type: "mouseMove", x: 0.2, y: 0.2 },
  ]);
  input.dispose();
});

test("按着键时被控端光标来回跳（防御性，见 input.ts 开头）：一直锁定、发相对移动，松开所有键才解除", async (t) => {
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  let lockRequests = 0;
  const request = canvas.requestPointerLock;
  canvas.requestPointerLock = () => { lockRequests++; return request(); };
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;

  fire("pointerdown", { button: 2, buttons: 2, clientX: 50, clientY: 50, pointerId: 1, movementX: 0, movementY: 0 });
  input.setRemoteCursorHidden(true);
  assert.equal(input.locked, true);
  await Promise.resolve();
  batches.length = 0;

  // 转视角期间光标出现、隐藏来回跳：不解除、不再请求锁定，一直发移动量
  for (let i = 0; i < 5; i++) {
    input.setRemoteCursorHidden(false);
    assert.equal(input.locked, true);
    assert.equal(input.relative, true);
    fire("pointermove", { buttons: 2, movementX: 2, movementY: 0, clientX: 50, clientY: 50 });
    input.setRemoteCursorHidden(true);
  }
  assert.equal(lockRequests, 1);

  // 最后一次是隐藏：松开之后照样锁着，等光标真的出现了才解除
  fire("pointerup", { button: 2, buttons: 0, movementX: 0, movementY: 0, clientX: 50, clientY: 50 });
  assert.equal(input.locked, true);
  input.setRemoteCursorHidden(false);
  assert.equal(input.locked, false);

  // 再转一次，最后一次是出现：松开的那一下（还是相对移动）之后马上解除
  fire("pointerdown", { button: 2, buttons: 2, clientX: 50, clientY: 50, pointerId: 1, movementX: 0, movementY: 0 });
  input.setRemoteCursorHidden(true);
  input.setRemoteCursorHidden(false);
  assert.equal(input.locked, true);
  fire("pointerup", { button: 2, buttons: 0, movementX: 3, movementY: 0, clientX: 50, clientY: 50 });
  assert.equal(input.locked, false);
  assert.equal(input.relative, false);
  assert.equal(lockRequests, 2);

  await new Promise((r) => setTimeout(r, 20));
  // 转视角期间只发移动量；唯一的绝对坐标是第二次按下之前（那时光标显示着）
  const sent = batches.flat();
  assert.equal(sent.filter(e => e.type === "mouseDelta").reduce((n, e) => n + e.dx, 0), 13);
  assert.deepEqual(sent.filter(e => e.type !== "mouseDelta"), [
    { type: "mouseButton", button: "right", down: false },
    { type: "mouseMove", x: 0.5, y: 0.5 },
    { type: "mouseButton", button: "right", down: true },
    { type: "mouseButton", button: "right", down: false },
  ]);
  input.dispose();
});

test("锁定时单纯移动的 buttons 是 0（Windows 的 Raw Input）：不当成松开，按键状态只跟着按下 / 松开", async (t) => {
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  let lockRequests = 0;
  const request = canvas.requestPointerLock;
  canvas.requestPointerLock = () => { lockRequests++; return request(); };
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;

  // 实测（WebView2 157）：按着右键拖动，游戏隐藏光标、锁定之后，每个 pointermove 都是 buttons 0、button -1
  fire("pointerdown", { button: 2, buttons: 2, clientX: 50, clientY: 50, pointerId: 1, movementX: 0, movementY: 0 });
  fire("pointermove", { button: -1, buttons: 2, movementX: 4, movementY: 0, clientX: 54, clientY: 50 });
  input.setRemoteCursorHidden(true);
  assert.equal(input.locked, true);
  for (let i = 0; i < 5; i++) {
    fire("pointermove", { button: -1, buttons: 0, movementX: 5, movementY: 0, clientX: 54, clientY: 50 });
  }
  // 被控端光标闪了一下：照样锁着
  input.setRemoteCursorHidden(false);
  input.setRemoteCursorHidden(true);
  // 锁定时又按下左键（同时按两个键，只在 pointermove 里出现，button 是变化的那个键）
  fire("pointermove", { button: 0, buttons: 3, movementX: 0, movementY: 0, clientX: 54, clientY: 50 });
  fire("pointermove", { button: -1, buttons: 0, movementX: 1, movementY: 0, clientX: 54, clientY: 50 });
  assert.equal(input.locked, true);
  assert.equal(lockRequests, 1);

  // 真的松开：Raw Input 先来一个 buttons 0 的移动，再是 pointerup
  fire("pointermove", { button: -1, buttons: 0, movementX: 0, movementY: 0, clientX: 54, clientY: 50 });
  fire("pointerup", { button: 0, buttons: 2, movementX: 0, movementY: 0, clientX: 54, clientY: 50 });
  fire("pointerup", { button: 2, buttons: 0, movementX: 0, movementY: 0, clientX: 54, clientY: 50 });
  input.setRemoteCursorHidden(false);
  assert.equal(input.locked, false);

  await new Promise((r) => setTimeout(r, 20));
  const sent = batches.flat();
  assert.deepEqual(sent.filter(e => e.type === "mouseButton"), [
    { type: "mouseButton", button: "right", down: true },
    { type: "mouseButton", button: "left", down: true },
    { type: "mouseButton", button: "left", down: false },
    { type: "mouseButton", button: "right", down: false },
  ]);
  // 锁定之后只发移动量
  assert.equal(sent.filter(e => e.type === "mouseDelta").reduce((n, e) => n + e.dx, 0), 26);
  input.dispose();
});

test("按着键时失去焦点（全部松开）：按着键期间出现的光标也就算数了", (t) => {
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  const input = new RemoteInput(canvas, () => true, () => {});
  input.interactive = true;
  fire("pointerdown", { button: 2, buttons: 2, clientX: 0, clientY: 0, pointerId: 1, movementX: 0, movementY: 0 });
  input.setRemoteCursorHidden(true);
  input.setRemoteCursorHidden(false);
  assert.equal(input.locked, true);
  fire("blur", {});
  assert.equal(input.locked, false);
  assert.equal(input.relative, false);
  input.dispose();
});

test("右 Shift 没有 code（Windows 11 + 微软输入法下的 Chromium）：当成 ShiftRight 发，按下、松开都是", async () => {
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  // 抓到的真实事件：key 是 Shift，code 是空的，location 是 0
  const rshift = { key: "Shift", code: "", location: 0, repeat: false };
  fire("keydown", rshift);
  fire("keydown", { key: "!", code: "Digit1", location: 0, repeat: false });
  fire("keyup", { key: "!", code: "Digit1", location: 0, repeat: false });
  fire("keydown", { ...rshift, repeat: true });
  fire("keyup", rshift);
  // 别的认不出的键照样不发
  fire("keydown", { key: "Unidentified", code: "", location: 0, repeat: false });
  await Promise.resolve();
  assert.deepEqual(batches.flat(), [
    { type: "key", code: "ShiftRight", down: true },
    { type: "key", code: "Digit1", down: true },
    { type: "key", code: "Digit1", down: false },
    { type: "key", code: "ShiftRight", down: true },
    { type: "key", code: "ShiftRight", down: false },
  ]);
  assert.equal(physicalCode({ key: "Shift", code: "", location: 1 } as KeyboardEvent), "ShiftLeft");
  assert.equal(physicalCode({ key: "Shift", code: "ShiftLeft", location: 1 } as KeyboardEvent), "ShiftLeft");
  input.dispose();
});

test("Ctrl+Alt+Shift+M 进入 / 退出游戏模式（退出时回到之前的模式），快捷键不发给被控端", async () => {
  const batches: any[][] = [];
  const { canvas, fire, locks } = fakeCanvas();
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  const modes: string[] = [];
  input.onMouseMode = (m) => modes.push(m);
  input.interactive = true;
  assert.equal(input.mode, "auto");
  input.setMode("desktop");
  assert.ok(isLockShortcut(LOCK_SHORTCUT as KeyboardEvent));
  fire("keydown", LOCK_SHORTCUT);
  fire("keyup", LOCK_SHORTCUT);
  await Promise.resolve();
  assert.equal(input.mode, "game");
  // 进入游戏模式就请求锁定（原始计数）；这里没有真的锁定：等用户点画面，鼠标先不发
  assert.deepEqual(locks, [{ unadjustedMovement: true }]);
  assert.equal(input.gamePaused, true);
  assert.deepEqual(batches, []);
  fire("keydown", LOCK_SHORTCUT);
  assert.equal(input.mode, "desktop");
  assert.deepEqual(modes, ["desktop", "game", "desktop"]);
  // 会话刚建好时用记着的模式：不算换，不通知
  input.setMode("game", false);
  assert.deepEqual(modes, ["desktop", "game", "desktop"]);
  input.dispose();
});

test("游戏模式：一直锁定、只发移动量，不管对方的光标；放开之后鼠标什么也不发，点一下画面重新锁定（这一下不发）", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  input.setMode("game");
  assert.equal(input.locked, true);
  assert.equal(input.relative, true);
  // 对方的光标出现、隐藏都不管：一直锁定
  input.setRemoteCursorHidden(true);
  input.setRemoteCursorHidden(false);
  assert.equal(input.locked, true);
  const at = { clientX: 5, clientY: 5, pointerId: 1, movementX: 0, movementY: 0 };
  fire("pointermove", { ...at, button: -1, buttons: 0, movementX: 3, movementY: -1 });
  fire("pointerdown", { ...at, button: 0, buttons: 1 });
  fire("pointerup", { ...at, button: 0, buttons: 0 });

  // 长按 Esc：放开鼠标，还是游戏模式
  fire("keydown", ESC);
  t.mock.timers.tick(1000);
  assert.equal(input.locked, false);
  assert.equal(input.gamePaused, true);
  assert.equal(input.mode, "game");
  fire("keyup", ESC);
  // 放开之后：移动、滚轮、点击都不发（不发绝对坐标，对方的光标留在原处）
  fire("pointermove", { ...at, button: -1, buttons: 0, movementX: 9, movementY: 9 });
  fire("wheel", { deltaX: 0, deltaY: 100, deltaMode: 0 });
  // 点一下画面：重新锁定，这一下从按下到松开都不发
  fire("pointerdown", { ...at, button: 0, buttons: 1 });
  assert.equal(input.locked, true);
  fire("pointerup", { ...at, button: 0, buttons: 0 });
  fire("pointermove", { ...at, button: -1, buttons: 0, movementX: 2, movementY: 0 });
  t.mock.timers.tick(20);
  await Promise.resolve();
  assert.deepEqual(batches.flat(), [
    { type: "mouseDelta", dx: 3, dy: -1 },
    { type: "mouseButton", button: "left", down: true },
    { type: "mouseButton", button: "left", down: false },
    { type: "key", code: "Escape", down: true },
    { type: "key", code: "Escape", down: false },
    { type: "mouseDelta", dx: 2, dy: 0 },
  ]);

  // 换成桌面模式：解除锁定，发绝对坐标
  input.setMode("desktop");
  assert.equal(input.locked, false);
  assert.equal(input.relative, false);
  input.dispose();
});

test("桌面模式：对方隐藏了光标也发绝对坐标、不锁定；换回自动模式就按对方的光标来", async (t) => {
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  input.setMode("desktop");
  input.setRemoteCursorHidden(true);
  fire("pointerdown", { button: 2, buttons: 2, clientX: 50, clientY: 25, pointerId: 1, movementX: 0, movementY: 0 });
  assert.equal(input.locked, false);
  assert.equal(input.relative, false);
  await Promise.resolve();
  assert.deepEqual(batches.flat(), [{ type: "mouseMove", x: 0.5, y: 0.25 }, { type: "mouseButton", button: "right", down: true }]);
  fire("pointerup", { button: 2, buttons: 0, clientX: 50, clientY: 25, pointerId: 1, movementX: 0, movementY: 0 });
  input.setMode("auto");
  assert.equal(input.relative, true);
  input.dispose();
});

test("全屏时 Esc 交给了网页：锁定时短按照常发给对方，长按在本地解除，按住时不发自动重复", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const batches: any[][] = [];
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  const input = new RemoteInput(canvas, () => true, events => batches.push(events));
  input.interactive = true;
  fire("keydown", LOCK_SHORTCUT);
  fire("keyup", LOCK_SHORTCUT);
  assert.equal(input.mode, "game");
  assert.equal(input.locked, true);
  const escEdges = [{ type: "key", code: "Escape", down: true }, { type: "key", code: "Escape", down: false }];

  // 短按：发给对方（游戏要用），不解除
  fire("keydown", ESC);
  t.mock.timers.tick(500);
  fire("keyup", ESC);
  t.mock.timers.tick(1000);
  await Promise.resolve();
  assert.equal(input.locked, true);
  assert.deepEqual(batches.flat(), escEdges);
  batches.length = 0;

  // 长按：按下照常发，按够 1 秒解除；从按下到松开都不发自动重复
  fire("keydown", ESC);
  fire("keydown", { ...ESC, repeat: true });
  t.mock.timers.tick(999);
  assert.equal(input.locked, true);
  t.mock.timers.tick(1);
  assert.equal(input.locked, false);
  assert.equal(input.relative, false);
  // 还是游戏模式：点一下画面再锁定
  assert.equal(input.gamePaused, true);
  fire("keydown", { ...ESC, repeat: true });
  fire("keyup", ESC);
  await Promise.resolve();
  assert.deepEqual(batches.flat(), escEdges);
  input.dispose();
});

test("长按 Esc 也能放开自动锁定（被控端光标隐藏着），点一下又锁定", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const { canvas, fire } = fakeCanvas();
  t.after(fakeLockDocument(canvas));
  const input = new RemoteInput(canvas, () => true, () => {});
  input.interactive = true;
  const click = { button: 0, buttons: 1, clientX: 0, clientY: 0, pointerId: 1, movementX: 0, movementY: 0 };
  input.setRemoteCursorHidden(true);
  fire("pointerdown", click);
  fire("pointerup", { ...click, buttons: 0 });
  assert.equal(input.locked, true);
  assert.equal(input.mode, "auto");

  fire("keydown", ESC);
  t.mock.timers.tick(1000);
  fire("keyup", ESC);
  assert.equal(input.locked, false);
  // 和按 Esc 一样：到下一次点击之前不自动锁定，发绝对坐标
  assert.equal(input.relative, false);

  fire("pointerdown", click);
  assert.equal(input.locked, true);
  input.dispose();
});
