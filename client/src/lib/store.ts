// 全局状态：标签（会话 / 新建连接 / 多屏总览）、我的设备、服务器状态等。
// "我的设备"保存在 config.toml 里（见 actions.ts 的 saveDevices）；画面增强、本地光标是这台电脑自己显示的偏好，
// 和对方无关，存在 WebView / 浏览器的 localStorage 里；其余只在控制台打开期间有效。
import { useCallback, useSyncExternalStore } from "react";
import type { AppInfo, Codec, Problem, QualitySettings, SavedDevice, ServerState } from "./api";
import { fmtCode } from "./devices";
import { ENHANCE_LEVELS, type EnhanceLevel } from "./enhance";
import { DEFAULT_PRESET, type PresetId } from "./presets";
import type { RemoteSession } from "./session";

/** "new" = 新建连接标签，"grid" = 多屏总览，其余为会话 id */
export type View = "new" | "grid" | string;

/** 新建连接标签里正在输入的内容（切到别的标签再回来时还在） */
export interface Draft {
  digits: string[];
  preset: PresetId;
  /** 配对码（对方要求配对时才显示这一栏，见 ConnectView） */
  pairing: string;
  busy: boolean;
  /** 保存类型而不是文字：切换语言后错误提示也跟着变 */
  error: Problem | null;
}

/** 本次关闭的会话（Ctrl+Shift+T 重新打开最近关闭的一个） */
export interface Recent {
  code: string;
  name: string;
  settings: QualitySettings;
}

/** 等用户确认的操作：关闭控制台 / 切换为被控端都会断开所有会话 */
export type Confirm = null | { kind: "close" | "toHost"; sessions: number };

interface State {
  /** 顺序就是标签的顺序 */
  sessions: RemoteSession[];
  view: View;
  /** 新建连接标签（总在会话标签后面）；没有会话时一定有 */
  draft: Draft | null;
  recent: Recent[];
  /** 我的设备：连接过的电脑，最近连过的在前面 */
  devices: SavedDevice[];
  /** 我的设备能不能保存（桌面版：config.toml）；网页版只在本页面的内存里 */
  devicesSaved: boolean;
  confirm: Confirm;
  server: ServerState;
  /**
   * 最近一次连不上服务器的原因。断开后会自动重试（状态在 disconnected 和 connecting 之间来回），
   * 重试期间也保留它，界面就不会一会儿显示错误、一会儿又消失；连上后清除
   */
  serverProblem: Problem | null;
  app: AppInfo | null;
  decoders: Codec[];
  immersive: boolean;
  /** 画面增强（所有会话通用，见 enhance.ts） */
  enhance: EnhanceLevel;
  /**
   * 对方把光标画进了画面里时（拖动、缩放窗口等）也显示本机的光标：跟手，只是和画面里的那个一起有两个。
   * 关掉时只看画面里的。所有会话通用，默认打开（见 session.ts 的 applyCursor）
   */
  localCursor: boolean;
}

const MAX_RECENT = 8;
const ENHANCE_KEY = "airhand.enhance";
const LOCAL_CURSOR_KEY = "airhand.localCursor";

/** 上次选的画面增强（读不到就是关） */
export function loadEnhance(): EnhanceLevel {
  try {
    const v = globalThis.localStorage?.getItem(ENHANCE_KEY);
    return ENHANCE_LEVELS.find((l) => l === v) ?? "off";
  } catch {
    return "off";
  }
}

/** 本地光标的设置（读不到就是打开） */
export function loadLocalCursor(): boolean {
  try {
    return globalThis.localStorage?.getItem(LOCAL_CURSOR_KEY) !== "0";
  } catch {
    return true;
  }
}

/** 记下这台电脑的显示偏好 */
function saveLocal(key: string, value: string) {
  try {
    globalThis.localStorage?.setItem(key, value);
  } catch {
    // 存不了（例如浏览器禁用了存储）：只在这次有效
  }
}

export const emptyDraft = (): Draft => ({
  digits: Array(6).fill(""),
  preset: DEFAULT_PRESET,
  pairing: "",
  busy: false,
  error: null,
});

let state: State = {
  sessions: [],
  view: "new",
  draft: emptyDraft(),
  recent: [],
  devices: [],
  devicesSaved: false,
  confirm: null,
  server: { state: "connecting" },
  serverProblem: null,
  app: null,
  decoders: ["h264"],
  immersive: false,
  enhance: loadEnhance(),
  localCursor: loadLocalCursor(),
};

const listeners = new Set<() => void>();

function set(patch: Partial<State>) {
  state = { ...state, ...patch };
  syncVisibility();
  for (const l of listeners) l();
}

/** 只有正在显示的会话保持全帧率；只有当前查看的会话（不含多屏总览）播放声音 */
function syncVisibility() {
  for (const s of state.sessions) {
    s.setVisible(state.view === "grid" || state.view === s.id);
    s.setForeground(state.view === s.id);
  }
}

/** 多屏总览标签：两台以上才有意义；正在看的时候不消失 */
export const showGrid = (st: State) => st.sessions.length >= 2 || (st.view === "grid" && st.sessions.length > 0);

/**
 * 给用户看的服务器状态：连不上之后在重试的，连上之前都算离线（不在「离线」和「连接中」之间来回跳）；
 * 只有第一次连接（还没出过问题）时是连接中
 */
export type ServerStatus = "connected" | "connecting" | "offline";
export const serverStatus = (st: State): ServerStatus =>
  st.server.state === "connected" ? "connected" : st.serverProblem ? "offline" : "connecting";

/** 标签栏从左到右的顺序 */
export function tabOrder(st: State = state): View[] {
  return [...(showGrid(st) ? ["grid"] : []), ...st.sessions.map((s) => s.id), ...(st.draft ? ["new"] : [])];
}

export const store = {
  get: () => state,
  subscribe: (fn: () => void) => {
    listeners.add(fn);
    return () => listeners.delete(fn);
  },
  setServer: (server: ServerState) => {
    const was = state.server.state;
    const serverProblem =
      server.state === "disconnected" ? server.reason : server.state === "connected" ? null : state.serverProblem;
    set({ server, serverProblem });
    if (server.state === "connected" && was !== "connected") for (const s of state.sessions) s.serverConnected();
  },
  /** 在等的被控端重新上线了（见 RemoteSession.hostOnline） */
  hostOnline: (code: number) => {
    for (const s of state.sessions) s.hostOnline(code);
  },
  setApp: (app: AppInfo) => set({ app }),
  setDecoders: (decoders: Codec[]) => set({ decoders }),
  setView: (view: View) => {
    if (view === "new" && !state.draft) return store.newTab();
    set({ view, immersive: view === "new" || view === "grid" ? false : state.immersive });
  },
  setImmersive: (immersive: boolean) => set({ immersive }),
  /** 画面增强：记下来，所有会话马上按它重画 */
  setEnhance: (enhance: EnhanceLevel) => {
    if (enhance === state.enhance) return;
    saveLocal(ENHANCE_KEY, enhance);
    set({ enhance });
    for (const s of state.sessions) s.refreshLayout();
  },
  /** 画面里有光标时显不显示本机的光标：记下来，所有会话马上换指针 */
  setLocalCursor: (localCursor: boolean) => {
    if (localCursor === state.localCursor) return;
    saveLocal(LOCAL_CURSOR_KEY, localCursor ? "1" : "0");
    set({ localCursor });
    for (const s of state.sessions) s.refreshCursor();
  },

  // ── 标签 ──

  /** 打开新建连接标签（已经有了就切过去） */
  newTab: () => set({ draft: state.draft ?? emptyDraft(), view: "new", immersive: false }),
  patchDraft: (patch: Partial<Draft>) => {
    if (state.draft) set({ draft: { ...state.draft, ...patch } });
  },
  /** 关闭新建连接标签。没有会话时它是唯一的标签，不能关 */
  closeDraft: () => {
    if (!state.draft || state.sessions.length === 0) return;
    // 它在最右边：关掉后显示左边那个
    const view = state.view === "new" ? state.sessions[state.sessions.length - 1].id : state.view;
    set({ draft: null, view });
  },
  /** 连上了：新建连接标签变成这个会话的标签 */
  add: (s: RemoteSession) =>
    set({
      sessions: [...state.sessions, s],
      view: s.id,
      draft: null,
      recent: state.recent.filter((r) => r.code !== s.code),
    }),
  /** 关闭会话标签（断开连接）。和浏览器一样，关掉当前标签后显示右边的，没有就显示左边的 */
  remove: (id: string) => {
    const idx = state.sessions.findIndex((s) => s.id === id);
    if (idx < 0) return;
    const s = state.sessions[idx];
    s.dispose();
    const sessions = state.sessions.filter((x) => x.id !== id);
    const recent = [
      { code: s.code, name: s.hostName, settings: s.settings },
      ...state.recent.filter((r) => r.code !== s.code),
    ].slice(0, MAX_RECENT);
    let { view, draft } = state;
    if (sessions.length === 0) {
      draft = draft ?? emptyDraft();
      view = "new";
    } else if (view === id) {
      view = sessions[idx]?.id ?? (draft ? "new" : sessions[idx - 1].id);
    }
    set({ sessions, view, draft, recent, immersive: view === state.view ? state.immersive : false });
  },
  /** 拖动标签：把会话移到第 `to` 个（只在会话之间排序） */
  move: (id: string, to: number) => {
    const from = state.sessions.findIndex((s) => s.id === id);
    if (from < 0 || from === to || to < 0 || to >= state.sessions.length) return;
    const sessions = [...state.sessions];
    const [s] = sessions.splice(from, 1);
    sessions.splice(to, 0, s);
    set({ sessions });
  },
  /** 切到相邻的标签（循环） */
  cycle: (delta: number) => {
    const tabs = tabOrder();
    const i = tabs.indexOf(state.view);
    store.setView(tabs[(i + delta + tabs.length) % tabs.length]);
  },
  /** Ctrl+1…8 切到第 n 个标签，Ctrl+9 切到最后一个（和浏览器一样） */
  selectTab: (n: number) => {
    const tabs = tabOrder();
    const v = n >= 9 ? tabs[tabs.length - 1] : tabs[n - 1];
    if (v) store.setView(v);
  },
  setDevices: (devices: SavedDevice[], saved = state.devicesSaved) => set({ devices, devicesSaved: saved }),

  setConfirm: (confirm: Confirm) => set({ confirm }),
};

export function useStore<T>(select: (s: State) => T): T {
  return useSyncExternalStore(store.subscribe, () => select(state));
}

/** 会话显示的名字：在我的设备里起过名字就用它，否则用对方的设备名，都没有就是控制码 */
export function useSessionTitle(s: RemoteSession): string {
  const alias = useStore((st) => st.devices.find((d) => d.code === s.code)?.alias ?? null);
  return alias || s.hostName || fmtCode(s.code);
}

/** 订阅单个会话的变化 */
export function useSession(s: RemoteSession | undefined): number {
  const sub = useCallback(
    (fn: () => void) => {
      if (!s) return () => {};
      const off = s.subscribe(fn);
      return () => void off();
    },
    [s],
  );
  const get = useCallback(() => (s ? s.getVersion() : 0), [s]);
  return useSyncExternalStore(sub, get);
}
