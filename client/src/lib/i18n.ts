// 界面语言：简体中文 / 英文。
//
// 当前语言由 Rust 端决定（选过的记在 config.toml 的 language 里，没选过就跟随系统语言，不是中文就用英文；
// 托盘菜单里也能切换），打开控制台时通过 window.__AIRHAND_LANG__ 注入，之后的切换通过 "lang-changed" 事件同步。
// 切换时调用 api.setLanguage，由 Rust 端记进配置文件（网页版控制台没有配置文件，不保存）。词条在 src/locales/。
import { useSyncExternalStore } from "react";
import en from "../locales/en.ts";
import zh from "../locales/zh.ts";

export type Lang = "zh" | "en";

export type Params = Record<string, string | number | boolean>;
/** 文案：字符串里的 {name} 会被参数替换；需要处理单复数等情况时写成函数 */
export type Msg = string | ((p: Params) => string);
/** 全部词条（以 locales/zh.ts 为准） */
export type MsgKey = keyof typeof zh;

const DICT: Record<Lang, Record<MsgKey, Msg>> = { zh, en };

/** 与 Rust 端 i18n::Lang::from_tag 相同的规则：中文用中文，其它一律英文 */
export function langFromTag(tag: string | undefined | null): Lang {
  return /^zh(?:[-_.@]|$)/i.test(tag?.trim() ?? "") ? "zh" : "en";
}

function initialLang(): Lang {
  const injected = (globalThis as { __AIRHAND_LANG__?: unknown }).__AIRHAND_LANG__;
  if (injected === "zh" || injected === "en") return injected;
  // 没有注入（例如直接用浏览器打开开发服务器）：按 WebView 的系统语言
  return langFromTag(globalThis.navigator?.language);
}

let lang: Lang = initialLang();
const listeners = new Set<() => void>();

function applyDocument() {
  if (typeof document === "undefined") return;
  document.documentElement.lang = lang === "zh" ? "zh-CN" : "en";
  document.title = t("app.title");
}
applyDocument();

export const getLang = () => lang;

/** 只切换界面（通知 Rust 端请用 api.setLanguage） */
export function setLang(next: Lang) {
  if (next === lang) return;
  lang = next;
  applyDocument();
  for (const l of listeners) l();
}

function subscribe(fn: () => void) {
  listeners.add(fn);
  return () => void listeners.delete(fn);
}

export function t(key: MsgKey, params?: Params): string {
  // 词条缺失（例如 Rust 端新增了问题类型而前端还没补）时显示 key，而不是出错
  const m = DICT[lang][key] ?? key;
  if (typeof m === "function") return m(params ?? {});
  if (!params) return m;
  return m.replace(/\{(\w+)\}/g, (all, k: string) => (k in params ? String(params[k]) : all));
}

/** 组件里使用：语言切换时重新渲染 */
export function useT(): typeof t {
  useSyncExternalStore(subscribe, getLang);
  return t;
}

export function useLang(): Lang {
  return useSyncExternalStore(subscribe, getLang);
}
