// 界面文案：中英文词条一致、语言识别规则与 Rust 端相同（src-tauri/src/i18n.rs）
import assert from "node:assert/strict";
import { test } from "node:test";
import { getLang, langFromTag, setLang, t, type MsgKey } from "../src/lib/i18n.ts";
import zh from "../src/locales/zh.ts";

test("语言标签：中文用中文，其它一律英文", () => {
  for (const tag of ["zh", "zh-CN", "zh-Hans-CN", "zh-TW", "zh_CN.UTF-8", "ZH-cn", " zh-SG "]) {
    assert.equal(langFromTag(tag), "zh", tag);
  }
  for (const tag of ["en", "en-US", "fr-FR", "ja", "zhx", "", undefined, null]) {
    assert.equal(langFromTag(tag), "en", String(tag));
  }
});

/** 两种语言下同一词条用到的 {参数} 必须相同 */
test("中英文词条的参数一致", async () => {
  const keys = Object.keys(zh) as MsgKey[];
  assert.ok(keys.length > 50, "没有读到词条");

  const params = (s: string) => [...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]).sort();
  const before = getLang();
  try {
    for (const key of keys) {
      // 用特殊值填参数：替换后不能留下未替换的 {x}，也必须用上所有参数
      setLang("zh");
      const zhRaw = t(key);
      const names = params(zhRaw);
      const args = Object.fromEntries(names.map((n) => [n, `<${n}>`]));
      for (const lang of ["zh", "en"] as const) {
        setLang(lang);
        // 函数形式的词条（处理单复数）没有原文可比，只检查输出
        const out = t(key, { ...args, n: 2 });
        assert.doesNotMatch(out, /\{\w+\}/, `${lang} ${key} 有未替换的参数: ${out}`);
        for (const n of names) {
          if (n !== "n") assert.ok(out.includes(`<${n}>`), `${lang} ${key} 没有用到参数 {${n}}: ${out}`);
        }
        assert.ok(out.trim().length > 0, `${lang} ${key} 为空`);
      }
    }
  } finally {
    setLang(before);
  }
});

test("参数替换与单复数", () => {
  const before = getLang();
  try {
    setLang("en");
    assert.equal(t("session.display", { n: 2 }), "Display 2");
    assert.equal(t("session.typed", { n: 1 }), "Typed 1 character");
    assert.equal(t("session.typed", { n: 5 }), "Typed 5 characters");
    assert.equal(t("grid.summary", { n: 1 }), "1 device · Click a screen to take control");
    setLang("zh");
    assert.equal(t("session.display", { n: 2 }), "显示器 2");
    assert.equal(t("session.typed", { n: 5 }), "已键入 5 个字符");
  } finally {
    setLang(before);
  }
});

/** 从 Rust 源码里取出某个枚举的变体名（转成 serde 的 camelCase） */
async function rustVariants(file: string, enumName: string): Promise<string[]> {
  const fs = await import("node:fs/promises");
  const src = await fs.readFile(new URL(file, import.meta.url), "utf8");
  const body = src.split(`pub enum ${enumName} {`)[1]?.split("\n}")[0];
  assert.ok(body, `${file} 里没有找到 enum ${enumName}`);
  return [...body.matchAll(/^ {4}([A-Z]\w*)/gm)].map((m) => m[1][0].toLowerCase() + m[1].slice(1));
}

/** Rust 端每一种问题 / 提示，前端两种语言都要有词条（Rust 只传类型，文字全在前端） */
test("Rust 的 Problem / NoticeView 在两种语言里都有词条", async () => {
  const problems = await rustVariants("../../crates/airhand-link/src/problem.rs", "Problem");
  const notices = await rustVariants("../../crates/airhand-link/src/controller/session.rs", "NoticeView");
  for (const k of notices.filter((k) => k.endsWith("Recovered"))) {
    assert.ok(notices.includes(k.replace(/Recovered$/, "Failed")), `${k} 没有对应的 Failed 提示`);
  }
  assert.ok(problems.length >= 15 && problems.includes("certMismatch"), `解析结果不对: ${problems}`);
  assert.deepEqual(notices, [
    "noDisplays",
    "wayland",
    "captureFailed",
    "captureRecovered",
    "audioFailed",
    "audioRecovered",
    "missingPermissions",
  ]);
  const before = getLang();
  try {
    for (const lang of ["zh", "en"] as const) {
      setLang(lang);
      // xxxRecovered 只用来撤下对应的 xxxFailed，本身不显示（见 messages.ts 的 RECOVERS），不需要词条
      const shown = notices.filter((k) => !k.endsWith("Recovered"));
      for (const key of [...problems.map((k) => `problem.${k}`), ...shown.map((k) => `notice.${k}`)]) {
        const text = t(key as MsgKey, { detail: "D", server: 3, client: 2 });
        assert.notEqual(text, key, `${lang} 缺少词条 ${key}`);
      }
    }
  } finally {
    setLang(before);
  }
});

/** 从 Rust 源码里取出一个开放枚举（open_enum!）在 JSON 里的名字：`Back = 0 => "back",` 里的 "back" */
async function rustOpenEnumNames(file: string, enumName: string): Promise<string[]> {
  const fs = await import("node:fs/promises");
  const src = await fs.readFile(new URL(file, import.meta.url), "utf8");
  const body = src.split(`pub enum ${enumName} {`)[1]?.split("\n    }")[0];
  assert.ok(body, `${file} 里没有找到 enum ${enumName}`);
  return [...body.matchAll(/^\s+[A-Z]\w* = \d+ => "(\w+)",/gm)].map((m) => m[1]);
}

/** 系统操作（对方是手机时的快捷键菜单）：每一种在两种语言里都有词条（词条用 JSON 里的名字） */
test("Rust 的 SystemAction 在两种语言里都有词条", async () => {
  const actions = await rustOpenEnumNames("../../crates/airhand-proto/src/session.rs", "SystemAction");
  assert.ok(actions.includes("back") && actions.includes("quickSettings") && actions.length >= 10, `解析结果不对: ${actions}`);
  const before = getLang();
  try {
    for (const lang of ["zh", "en"] as const) {
      setLang(lang);
      for (const a of actions) {
        const key = `action.${a}` as MsgKey;
        assert.notEqual(t(key), key, `${lang} 缺少词条 ${key}`);
      }
    }
  } finally {
    setLang(before);
  }
});

test("问题文字：版本不兼容按版本号提示更新哪一方，技术信息原样附在后面", () => {
  const before = getLang();
  try {
    setLang("zh");
    assert.match(t("problem.incompatible", { server: 3, client: 2 }), /请更新 Airhand/);
    assert.match(t("problem.incompatible", { server: 1, client: 2 }), /请更新服务器/);
    assert.equal(t("notice.captureFailed", { detail: "AcquireNextFrame: 0x887A0026" }), "对方电脑的屏幕采集失败，正在重试：AcquireNextFrame: 0x887A0026");
    setLang("en");
    assert.match(t("problem.incompatible", { server: 3, client: 2 }), /please update Airhand/);
    assert.match(t("problem.incompatible", { server: 1, client: 2 }), /please update the server/);
    assert.equal(t("problem.kicked"), "The remote user disconnected you");
  } finally {
    setLang(before);
  }
});

test("对方 Mac 缺少权限：按缺少的是哪一项说明", () => {
  const before = getLang();
  try {
    for (const lang of ["zh", "en"] as const) {
      setLang(lang);
      const both = t("notice.missingPermissions", { screen: true, input: true });
      const screen = t("notice.missingPermissions", { screen: true, input: false });
      const input = t("notice.missingPermissions", { screen: false, input: true });
      assert.equal(new Set([both, screen, input]).size, 3, `${lang}: 三种情况应各不相同`);
      const [rec, acc, reopen] = lang === "zh" ? ["屏幕与系统录音", "辅助功能", "重新打开"] : ["Screen & System Audio Recording", "Accessibility", "reopen"];
      assert.ok(both.includes(rec) && both.includes(acc) && both.includes(reopen), both);
      assert.ok(screen.includes(rec) && !screen.includes(acc) && screen.includes(reopen), screen);
      // 辅助功能授权后立即生效，不需要重新打开
      assert.ok(input.includes(acc) && !input.includes(rec) && !input.includes(reopen), input);
    }
  } finally {
    setLang(before);
  }
});
