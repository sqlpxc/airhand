// 把类型化的问题 / 提示变成当前界面语言的文字。
//
// 只在显示时调用（组件渲染时），所以切换语言后已经显示的问题和提示也会跟着变。
// 词条在 i18n.ts 的 problem.* / notice.*；每种 kind 都必须有词条（下面的 `as const` 让 TypeScript 检查）。
import { toProblem, type FileError, type Problem, type RemoteNotice } from "./api";
import { t } from "./i18n";
import type { RemoteSession } from "./session";

/** 本机产生的提示 */
export type LocalNotice =
  | { kind: "h264Undecodable" }
  | { kind: "codecFallback"; codec: string; to: string }
  | { kind: "noOpus" }
  /** 对方是 Android 手机，没有开启 Airhand 的"远程操作"（被控端发来的 missingPermissions） */
  | { kind: "androidNoInput" }
  /** 剪贴板里的文件传不了（对方那边的、这边的，见 files.ts） */
  | { kind: "fileError"; name: string; error: FileError }
  /** 对方的版本旧，不支持剪贴板里的文件 */
  | { kind: "filesUnsupported" };

/** 被控端发来的"已恢复"：只用来撤下之前的提示，本身不显示 */
export type RecoveredNotice = Extract<RemoteNotice, { kind: "captureRecovered" | "audioRecovered" }>;

/** "已恢复"撤下的是哪种提示 */
export const RECOVERS: Record<RecoveredNotice["kind"], SessionNotice["kind"]> = {
  captureRecovered: "captureFailed",
  audioRecovered: "audioFailed",
};

/** 会话画面上显示的提示：被控端发来的，或者本机产生的 */
export type SessionNotice = Exclude<RemoteNotice, RecoveredNotice> | LocalNotice;

export function problemText(p: Problem): string {
  const { kind, ...params } = p;
  return t(`problem.${kind}` as const, params);
}

export function noticeText(n: SessionNotice): string {
  if (n.kind === "fileError") return t("notice.fileError", { name: n.name, reason: t(`fileError.${n.error}` as const) });
  const { kind, ...params } = n;
  return t(`notice.${kind}` as const, params);
}

/** 会话现在的状态（标签的提示、多屏总览、全屏时的切换菜单）：连着时是帧率和延迟 */
export function sessionStateText(s: RemoteSession): string {
  switch (s.state) {
    case "live":
      return `${Math.round(s.stats.fps)} fps · ${s.stats.rttMs != null ? Math.round(s.stats.rttMs) : "—"} ms`;
    case "reconnecting":
      return t("common.reconnecting");
    case "waiting":
      return t("common.waiting");
    case "connecting":
      return t("common.connecting");
    case "closed":
      return s.closedReason ? problemText(s.closedReason) : t("common.disconnected");
  }
}

/** 任意异常的说明文字（Rust 命令失败时是 Problem） */
export function errorText(e: unknown): string {
  return problemText(toProblem(e));
}
