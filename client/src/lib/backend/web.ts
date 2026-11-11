// 网页版：浏览器打开 airhand-web 的端口，控制端服务跑在那台机器上（web/src/console.rs）。
//
// 会话和桌面版一样走 WebSocket（console.ts），连的是同一个端口的 `/ws`。网页版自己的：
// * 剪贴板在浏览器里同步（navigator.clipboard），规则与桌面版相同：只同步连接之后的变化，并抑制回声；
// * 没有被控端模式、托盘和窗口；"我的设备"不保存。
import type { AppInfo, Backend, Command, ConnectOptions, Problem, SessionEvent, SessionInfo } from "../api";
import { ConsoleLink, type MediaView } from "../console";
import { getLang } from "../i18n";

/** 一个会话的剪贴板同步 */
interface Clip {
  /** connect 返回之后才知道 */
  sid: number | null;
  /** 是否同步 */
  enabled: boolean;
  /** 双方已经一致的剪贴板文本；null 表示还没读到过本机剪贴板（第一次读到的内容不发送） */
  synced: string | null;
}

/** 与 airhand_proto::session::MAX_CLIPBOARD_TEXT 相同（按字符数粗略限制，服务端还会按字节检查） */
const MAX_CLIPBOARD = 1 << 20;
const CLIPBOARD_POLL_MS = 1000;

/** 同一个地址（也可能在反向代理的子路径下）的 `ws` */
function consoleUrl(): string {
  const url = new URL("ws", location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  url.search = "";
  url.hash = "";
  return url.href;
}

class WebConsole extends ConsoleLink implements Backend {
  readonly kind = "web" as const;
  /** 在浏览器的标签页里，没有自己的窗口 */
  readonly frame = null;

  private clips = new Set<Clip>();
  /** 最近一次读到 / 写入的本机剪贴板文本 */
  private clipSeen: string | null = null;
  /** 页面没有焦点时浏览器不让写剪贴板：先记下，回到页面时再写 */
  private clipToWrite: { text: string; revision: number } | null = null;
  private clipRevision = 0;
  private clipWriteTask: Promise<void> | null = null;
  private clipBusy = false;

  constructor() {
    super(async () => consoleUrl());
    setInterval(() => {
      if (document.hasFocus()) void this.syncClipboard(false);
    }, CLIPBOARD_POLL_MS);
    // 在别的程序里复制完切回来：马上同步
    window.addEventListener("focus", () => void this.syncClipboard(false));
  }

  override async appInfo(): Promise<AppInfo> {
    // 界面语言由浏览器决定
    return { ...(await super.appInfo()), lang: getLang() };
  }

  override async connect(
    code: string,
    opts: ConnectOptions,
    view: MediaView,
    onEvent: (ev: SessionEvent) => void,
    clipboard: boolean,
  ): Promise<SessionInfo> {
    const clip: Clip = { sid: null, enabled: clipboard, synced: this.clipSeen };
    this.clips.add(clip);
    const events = (ev: SessionEvent) => {
      if (ev.type === "clipboard") return this.remoteClipboard(clip, ev.text);
      if (ev.type === "closed") this.clips.delete(clip);
      onEvent(ev);
    };
    try {
      const info = await super.connect(code, opts, view, events, clipboard);
      clip.sid = info.sid;
      return info;
    } catch (e) {
      this.clips.delete(clip);
      throw e;
    }
  }

  /** 剪贴板同步的开关在浏览器里 */
  override command(c: Command): Promise<void> {
    if (c.cmd === "clipboardSync") return this.clipboardSync(c.sid, c.enabled);
    if (c.cmd === "disconnect") for (const clip of this.clips) if (clip.sid === c.sid) this.clips.delete(clip);
    return super.command(c);
  }

  // ───────────────────────── 剪贴板 ─────────────────────────

  /** `ask`：没有权限时请求权限（只在用户操作时这样做，浏览器会弹出询问） */
  private async readClipboard(ask: boolean): Promise<string | null> {
    if (!navigator.clipboard?.readText) return null;
    if (!ask) {
      try {
        const p = await navigator.permissions.query({ name: "clipboard-read" as PermissionName });
        if (p.state !== "granted") return null;
      } catch {
        // 不支持查询这个权限（例如 Firefox）：只在用户操作时读
        return null;
      }
    }
    try {
      return await navigator.clipboard.readText();
    } catch {
      return null;
    }
  }

  private writeClipboard(text: string): Promise<void> {
    this.clipToWrite = { text, revision: ++this.clipRevision };
    return this.flushClipboard();
  }

  /** 串行写入，只保留最新待执行值；旧调用的失败不能恢复已经被替换的重试。 */
  private flushClipboard(): Promise<void> {
    if (this.clipWriteTask) return this.clipWriteTask;
    const drain = async () => {
      while (this.clipToWrite !== null) {
        const update = this.clipToWrite;
        try {
          await navigator.clipboard.writeText(update.text);
        } catch {
          if (this.clipToWrite === update) break; // 等下次获得焦点再试
          continue; // 已有更新的值，继续写最新的
        }
        if (this.clipToWrite === update) {
          this.clipSeen = update.text;
          this.clipToWrite = null;
        }
      }
    };
    this.clipWriteTask = drain().finally(() => {
      this.clipWriteTask = null;
    });
    return this.clipWriteTask;
  }

  /** 被控端复制了文本 */
  private remoteClipboard(clip: Clip, text: string) {
    if (!clip.enabled) return;
    clip.synced = text;
    void this.writeClipboard(text);
  }

  /** 本机剪贴板有变化就发给打开了同步的会话 */
  private async syncClipboard(ask: boolean) {
    if (this.clipBusy) return;
    this.clipBusy = true;
    try {
      if (document.hasFocus()) await this.flushClipboard();
      if (this.clipToWrite !== null || this.clipWriteTask !== null) return;
      if (![...this.clips].some((c) => c.enabled && c.sid !== null)) return;
      const revision = this.clipRevision;
      const text = await this.readClipboard(ask);
      // 读取期间收到远端更新：这个读结果属于更早的状态，不能回传。
      if (revision !== this.clipRevision || text === null || text === "" || text.length > MAX_CLIPBOARD) return;
      this.clipSeen = text;
      for (const c of this.clips) {
        if (c.synced === null) {
          c.synced = text;
        } else if (c.sid !== null && c.synced !== text) {
          // 关闭同步的会话也要记下，否则重新打开时会把旧内容发过去
          c.synced = text;
          if (c.enabled) void super.command({ cmd: "clipboard", sid: c.sid, text }).catch(() => {});
        }
      }
    } finally {
      this.clipBusy = false;
    }
  }

  private async clipboardSync(sid: number, enabled: boolean) {
    const clip = [...this.clips].find((c) => c.sid === sid);
    if (!clip) return;
    clip.enabled = enabled;
    if (enabled) {
      // 用户刚打开同步：这时可以请求读剪贴板的权限。现有内容不发送，只同步之后的变化
      const revision = this.clipRevision;
      const text = await this.readClipboard(true);
      if (revision === this.clipRevision && this.clipToWrite === null && text !== null) {
        clip.synced = text;
        this.clipSeen = text;
      }
    }
  }

  async localClipboardText() {
    const text = await this.readClipboard(true);
    if (text === null) throw { kind: "clipboardBlocked" } satisfies Problem;
    return text;
  }

  // 网页版没有被控端模式、托盘和窗口
  async switchToHost() {}
  async setLanguage() {}
  onLangChanged() {
    return () => {};
  }
  async setWindowFullscreen() {}
  onWindowFullscreenExit() {
    return () => {};
  }
  // 网页版不保存"我的设备"（不写浏览器的存储）：只在本页面的内存里，刷新后清空
  async loadDevices() {
    return null;
  }
  async saveDevices() {}
}

export function webBackend(): Backend {
  return new WebConsole();
}
