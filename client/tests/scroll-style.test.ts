import assert from "node:assert/strict";
import { test } from "node:test";
import { RemoteInput } from "../src/lib/input.ts";
import { ViewScroll, edgeDepth, followScroll } from "../src/lib/scroll-style.ts";

test("跟随指针：窗口两头对着画面两头，指针出了窗口也不越界，画面放得下时不滚", () => {
  const cases: [string, number, number, number, number][] = [
    ["最左", 0, 1000, 3000, 0],
    ["中间", 500, 1000, 3000, 1000],
    ["最右", 1000, 1000, 3000, 2000],
    ["拖到窗口外左边", -50, 1000, 3000, 0],
    ["拖到窗口外右边", 1200, 1000, 3000, 2000],
    ["画面放得下", 700, 1000, 800, 0],
  ];
  for (const [name, p, view, content, want] of cases) assert.equal(followScroll(p, view, content), want, name);
});

test("边缘深度：只在边上一圈里有，越靠外越深，窗口太小时两圈不重叠", () => {
  const cases: [string, number, number, number, number][] = [
    ["里面", 500, 1000, 64, 0],
    ["刚到左边那一圈", 64, 1000, 64, 0],
    ["左边最外", 0, 1000, 64, -64],
    ["右边最外", 1000, 1000, 64, 64],
    ["拖到窗口外", 1300, 1000, 64, 64],
    ["窗口只有 100：中间以左往左", 30, 100, 64, -20],
    ["窗口只有 100：正中不动", 50, 100, 64, 0],
  ];
  for (const [name, p, view, edge, want] of cases) assert.equal(edgeDepth(p, view, edge), want, name);
});

/**
 * 假的 .canvas-host：在网页的 (0, 40) 处（上面是工具栏），显示区域 1000×600，画面 3000×2000，现在滚到 (1000, 700)。
 * `fire` 发一个指针事件（默认是没按键的鼠标移动），返回事件，看它有没有被拦下
 */
function fakeHost() {
  const handlers = new Map<string, (e: any) => void>();
  const frames: ((now: number) => void)[] = [];
  const host: any = {
    clientWidth: 1000,
    clientHeight: 600,
    scrollWidth: 3000,
    scrollHeight: 2000,
    scrollLeft: 1000,
    scrollTop: 700,
    getBoundingClientRect: () => ({ left: 0, top: 40 }),
    addEventListener(name: string, fn: (e: any) => void) { handlers.set(name, fn); },
    ownerDocument: {
      defaultView: {
        devicePixelRatio: 1,
        requestAnimationFrame: (cb: (now: number) => void) => frames.push(cb),
        cancelAnimationFrame() {},
      },
    },
  };
  const fire = (type: string, init: any = {}) => {
    const e = {
      type, pointerType: "mouse", button: -1, buttons: 0, target: host, stopped: false,
      stopPropagation() { e.stopped = true; },
      ...init,
    };
    handlers.get(type)?.(e);
    return e;
  };
  return { host, fire, frames, handlers };
}

/** 移动系统指针：记下来，由测试决定什么时候移好、移不了 */
function fakeWarp() {
  const calls: { x: number; y: number; done: () => void; fail: () => void }[] = [];
  const warp = (x: number, y: number) => new Promise<void>((done, fail) => calls.push({ x, y, done, fail: () => fail(new Error("no")) }));
  return { warp, calls };
}

const settle = () => new Promise((r) => setImmediate(r));

test("边缘滚动（桌面版）：推进去多深画面移多深，系统指针退回那一圈外面，按退回的位置发给对方", async () => {
  const { host, fire } = fakeHost();
  const { warp, calls } = fakeWarp();
  const moved: unknown[] = [];
  const s = new ViewScroll(() => "edge", (at) => moved.push(at), warp);
  s.attach(host);

  // 先进到里面；推进左边那一圈 54 像素
  assert.equal(fire("pointermove", { clientX: 500, clientY: 340 }).stopped, false);
  const pushed = fire("pointermove", { clientX: 10, clientY: 340 });
  assert.equal(host.scrollLeft, 946);
  assert.equal(host.scrollTop, 700);
  // 画面移了、指针还没退回来：这一下不发给对方
  assert.equal(pushed.stopped, true);
  assert.deepEqual(calls.map(({ x, y }) => [x, y]), [[65, 340]]);

  // 指针退回来之前来的移动是旧的位置：不推、不发
  assert.equal(fire("pointermove", { clientX: 5, clientY: 340 }).stopped, true);
  assert.equal(host.scrollLeft, 946);
  assert.equal(calls.length, 1);

  // 退回来了：按退回的位置发（macOS 移动指针时不产生移动事件）
  calls[0].done();
  await settle();
  assert.deepEqual(moved, [{ clientX: 65, clientY: 340 }]);
  // 系统产生的那次移动（Windows、X11）在那一圈外面：照常发
  assert.equal(fire("pointermove", { clientX: 65, clientY: 340 }).stopped, false);
  assert.equal(host.scrollLeft, 946);

  // 拖动时（按着键）一样推、一样不发；同时按下别的键的那一下照常发，带着按键的变化
  assert.equal(fire("pointermove", { clientX: 990, clientY: 610, buttons: 1 }).stopped, true);
  assert.equal(host.scrollLeft, 1000);
  assert.equal(host.scrollTop, 734);
  assert.deepEqual(calls.map(({ x, y }) => [x, y]).at(-1), [935, 575]);
  calls.at(-1)!.done();
  await settle();
  assert.equal(fire("pointermove", { clientX: 10, clientY: 340, button: 2, buttons: 3 }).stopped, false);
  assert.equal(host.scrollLeft, 946);
});

test("边缘滚动：移不了系统指针（Linux 的 Wayland）时这一下按原处重发，马上改为持续移动", async () => {
  const { host, fire, frames } = fakeHost();
  const { warp, calls } = fakeWarp();
  const moved: unknown[] = [];
  const s = new ViewScroll(() => "edge", (at) => moved.push(at), warp);
  s.attach(host);
  fire("pointermove", { clientX: 500, clientY: 340 });
  fire("pointermove", { clientX: 10, clientY: 340 });
  assert.equal(host.scrollLeft, 946);
  calls[0].fail();
  await settle();
  // 指针还在原处，指着的地方随画面变了
  assert.deepEqual(moved, [{ clientX: 10, clientY: 340 }]);
  // 停着不动也接着移
  assert.equal(frames.length, 1);
  frames[0](performance.now() + 100);
  assert.ok(host.scrollLeft < 946);
  assert.deepEqual(moved.at(-1), undefined);
  assert.equal(moved.length, 2);
  // 以后不再试着移动系统指针
  fire("pointermove", { clientX: 8, clientY: 340 });
  assert.equal(calls.length, 1);
});

test("边缘滚动：触摸、手写笔推不回去，持续移动", () => {
  const { host, fire, frames } = fakeHost();
  const { warp, calls } = fakeWarp();
  const s = new ViewScroll(() => "edge", () => {}, warp);
  s.attach(host);
  fire("pointermove", { pointerType: "touch", clientX: 500, clientY: 340 });
  assert.equal(fire("pointermove", { pointerType: "touch", clientX: 10, clientY: 340 }).stopped, false);
  assert.equal(calls.length, 0);
  assert.equal(frames.length, 1);
});

test("边缘滚动：从外面进来先不移；离开画面里面的元素不算离开", () => {
  const { host, fire, frames } = fakeHost();
  const s = new ViewScroll(() => "edge", () => {});
  s.attach(host);
  // 从工具栏下来，直接进到上边那一圈：不移
  fire("pointermove", { clientX: 500, clientY: 50 });
  assert.equal(frames.length, 0);
  fire("pointermove", { clientX: 500, clientY: 340 });
  // 按下时 RemoteInput 捕获指针、多屏时换了一块画面：里面的元素收到 pointerleave，还在画面上
  fire("pointerleave", { target: {} });
  fire("pointermove", { clientX: 10, clientY: 340 });
  assert.equal(frames.length, 1);
  // 真的离开了：要再进到里面一次
  fire("pointerleave");
  fire("pointermove", { clientX: 10, clientY: 340 });
  assert.equal(frames.length, 1);
});

test("自动滚动：跟着移动走；按下时不移，点到的就是看到的地方", () => {
  const { host, fire, handlers } = fakeHost();
  const s = new ViewScroll(() => "auto", () => {});
  s.attach(host);
  fire("pointermove", { clientX: 1000, clientY: 640 });
  assert.equal(host.scrollLeft, 2000);
  assert.equal(host.scrollTop, 1400);
  assert.equal(handlers.has("pointerdown"), false);
});

test("画面在指针下面移动了：按给的位置重发，以后没给位置时也按它", async () => {
  const batches: any[][] = [];
  const handlers = new Map<string, (e: any) => void>();
  const canvas: any = {
    addEventListener(name: string, fn: (e: any) => void) { handlers.set(name, fn); },
    getBoundingClientRect: () => ({ left: 0, top: 0, width: 100, height: 100 }),
  };
  const input = new RemoteInput(canvas, () => true, (events) => batches.push(events));
  input.interactive = true;
  input.repoint({ clientX: 25, clientY: 50 });
  await new Promise((r) => setTimeout(r, 20));
  input.repoint();
  await new Promise((r) => setTimeout(r, 20));
  assert.deepEqual(batches, [[{ type: "mouseMove", x: 0.25, y: 0.5 }], [{ type: "mouseMove", x: 0.25, y: 0.5 }]]);
  input.dispose();
});
