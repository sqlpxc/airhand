// "我的设备"的列表操作
import assert from "node:assert/strict";
import { test } from "node:test";
import type { SavedDevice } from "../src/lib/api.ts";
import {
  MAX_DEVICES,
  deviceTitle,
  forgetDevice,
  matchDevice,
  patchDevice,
  relativeTime,
  remember,
  renameDevice,
  restoreDevice,
} from "../src/lib/devices.ts";

const q = { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true, codec: "h264" as const };
const seen = (code: string, name = `PC-${code}`) => ({ code, name, os: "Windows 11", resume: [1, 2, 3], quality: q });

test("连上就记下，最近的在前面，再连一次只是更新", () => {
  let list: SavedDevice[] = [];
  list = remember(list, seen("482913"), 1000);
  list = remember(list, seen("731205"), 2000);
  assert.deepEqual(
    list.map((d) => d.code),
    ["731205", "482913"],
  );
  list = remember(list, { ...seen("482913", "DESIGN-WS"), resume: null, quality: null }, 3000);
  assert.equal(list.length, 2);
  assert.equal(list[0].code, "482913");
  assert.equal(list[0].name, "DESIGN-WS");
  assert.equal(list[0].lastUsed, 3000);
  // 这次没有的（凭证、画质）沿用上次的
  assert.deepEqual(list[0].resume, [1, 2, 3]);
  assert.deepEqual(list[0].quality, q);
});

test("鼠标模式按设备记住：连上时记下、换了就更新，这次没说就沿用上次的", () => {
  let list: SavedDevice[] = remember([], seen("482913"), 1);
  assert.equal(list[0].mouse, null, "auto by default");
  list = remember(list, { ...seen("482913"), mouse: "game" }, 2);
  assert.equal(list[0].mouse, "game");
  list = remember(list, seen("482913"), 3);
  assert.equal(list[0].mouse, "game");
  list = patchDevice(list, "482913", { mouse: "desktop" });
  assert.equal(list[0].mouse, "desktop");
  // 没记下的设备不变
  assert.equal(patchDevice(list, "111111", { mouse: "game" }), list);
});

test("Ctrl / Command 互换只记在选中的设备，重连保留，也能关掉", () => {
  let list = remember([], seen("482913"), 1);
  list = remember(list, seen("731205"), 2);
  list = patchDevice(list, "482913", { swapCtrlCommand: true });
  list = remember(list, seen("482913"), 3);
  assert.deepEqual(list.map(d => [d.code, d.swapCtrlCommand]), [["482913", true], ["731205", false]]);
  assert.equal(remember(list, { ...seen("482913"), swapCtrlCommand: false }, 4)[0].swapCtrlCommand, false);
});

test("自己起的名字：重连后保留；改回对方的名字或清空就是不另起名字", () => {
  let list = remember([], seen("482913"), 1);
  list = renameDevice(list, "482913", "  书房  ");
  assert.equal(list[0].alias, "书房");
  assert.equal(deviceTitle(list[0]), "书房");
  list = remember(list, seen("482913", "NEW-NAME"), 2);
  assert.equal(list[0].alias, "书房");
  assert.equal(list[0].name, "NEW-NAME");
  assert.equal(renameDevice(list, "482913", "NEW-NAME")[0].alias, null);
  assert.equal(renameDevice(list, "482913", " ")[0].alias, null);
  // 没有名字时显示控制码
  assert.equal(deviceTitle({ ...list[0], alias: null, name: "" }), "482 913");
});

test("移除后撤销：回到原来的位置", () => {
  let list: SavedDevice[] = [];
  for (const [i, c] of ["111111", "222222", "333333"].entries()) list = remember(list, seen(c), i);
  const removed = list[1];
  list = forgetDevice(list, removed.code);
  assert.deepEqual(
    list.map((d) => d.code),
    ["333333", "111111"],
  );
  list = restoreDevice(list, removed);
  assert.deepEqual(
    list.map((d) => d.code),
    ["333333", "222222", "111111"],
  );
});

test(`最多 ${MAX_DEVICES} 台，最久没用的先去掉`, () => {
  let list: SavedDevice[] = [];
  for (let i = 0; i < MAX_DEVICES + 5; i++) list = remember(list, seen(String(100000 + i)), i);
  assert.equal(list.length, MAX_DEVICES);
  assert.equal(list[0].code, String(100000 + MAX_DEVICES + 4));
  assert.ok(!list.some((d) => d.code === "100000"));
});

test("搜索名字或控制码", () => {
  const d = renameDevice(remember([], seen("482913", "DESIGN-WS"), 1), "482913", "书房")[0];
  assert.ok(matchDevice(d, "书"));
  assert.ok(matchDevice(d, "design"));
  assert.ok(matchDevice(d, "482 9"));
  assert.ok(matchDevice(d, "  "));
  assert.ok(!matchDevice(d, "999"));
  assert.ok(!matchDevice(d, "mac"));
});

test("多久以前", () => {
  const now = Date.UTC(2026, 10, 8, 12);
  assert.equal(relativeTime(now - 5_000, now, "en"), "now");
  assert.equal(relativeTime(now - 5 * 60_000, now, "en"), "5 minutes ago");
  assert.equal(relativeTime(now - 26 * 3600_000, now, "en"), "yesterday");
  assert.equal(relativeTime(now - 3 * 86400_000, now, "zh-CN"), "3天前");
  assert.equal(relativeTime(0, now, "en"), "");
});
