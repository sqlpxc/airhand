import assert from "node:assert/strict";
import { test } from "node:test";
import { edgeDepth, followScroll } from "../src/lib/scroll-style.ts";

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
