// 网页版：浏览器打开 airhand-web 的端口，控制端服务跑在那台机器上（web/src/console.rs）。
//
// 命令、事件和画面都走同一条 WebSocket（协议见 console.rs）：
// * 命令 `{"id", "cmd", ...参数}`；对会话的操作就是 Command（与桌面版相同）。
//   画面确认、输入事件这类高频消息不带 id，不等回复；
// * 媒体包是二进制：前 4 字节是会话的 ch（connect 时由这里选），后面与桌面版相同（见 media.ts）；
// * 收到画面就回确认（二进制，见 ack.ts；连着到的几帧只确认最后一帧），被控端的在途帧控制因此把浏览器这段网络也算进去；
// * 剪贴板在浏览器里同步（navigator.clipboard），规则与桌面版相同：只同步连接之后的变化，并抑制回声；
// * 显示的延迟 = 控制端服务到被控端的往返 + 浏览器到控制端服务的往返。
import type {
  AppInfo,
  Backend,
  Command,
  ConnectOptions,
  Problem,
  ServerState,
  SessionEvent,
  SessionInfo,
  SessionSink,
} from "../api";
import { AckCoalescer, ackMessage } from "../ack";
import { getLang } from "../i18n";
import { HEADER_LEN, videoSeq } from "../media";

type Incoming =
  | { type: "reply"; id: number; ok?: unknown; err?: Problem }
  | { type: "server"; state: ServerState }
  | { type: "event"; ch: number; event: SessionEvent }
  | { type: "hostOnline"; code: number };

interface Chan {
  sink: SessionSink;
  /** connect 返回之后才知道 */
  sid: number | null;
  /** 是否同步剪贴板 */
  clipboard: boolean;
  /** 双方已经一致的剪贴板文本；null 表示还没读到过本机剪贴板（第一次读到的内容不发送） */
  synced: string | null;
  /** sid 还不知道时收到的最新一帧，知道之后补发确认（确认是累积的） */
  unacked: number | null;
  /** 画面确认（合并连着到的几帧） */
  acks: AckCoalescer;
}

/** 与 airhand-web 的连接断了 */
const LOST: Problem = { kind: "consoleLost" };
/** 与 airhand_proto::session::MAX_CLIPBOARD_TEXT 相同（按字符数粗略限制，服务端还会按字节检查） */
const MAX_CLIPBOARD = 1 << 20;
const RECONNECT_MAX_MS = 5000;
const PING_MS = 2000;
const CLIPBOARD_POLL_MS = 1000;
/** 二进制消息：ch(4) + 媒体包的头（最短的） */
const MIN_BINARY = 4 + HEADER_LEN;

class WebConsole implements Backend {
  readonly kind = "web" as const;
  /** 在浏览器的标签页里，没有自己的窗口 */
  readonly frame = null;

  private ws: WebSocket | null = null;
  private retries = 0;
  private nextId = 1;
  private nextCh = 1;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Problem) => void; timer: number }>();
  private openWaiters: (() => void)[] = [];
  private chans = new Map<number, Chan>();
  private status: ServerState = { state: "connecting" };
  private statusListeners = new Set<(s: ServerState) => void>();
  private onlineListeners = new Set<(code: number) => void>();
  /** 浏览器到 airhand-web 的往返时间（毫秒，平滑后） */
  private rtt: number | null = null;
  /** 最近一次读到 / 写入的本机剪贴板文本 */
  private clipSeen: string | null = null;
  /** 页面没有焦点时浏览器不让写剪贴板：先记下，回到页面时再写 */
  private clipToWrite: { text: string; revision: number } | null = null;
  private clipRevision = 0;
  private clipWriteTask: Promise<void> | null = null;
  private clipBusy = false;

  constructor() {
    this.open();
    window.setInterval(() => void this.ping(), PING_MS);
    window.setInterval(() => {
      if (document.hasFocus()) void this.syncClipboard(false);
    }, CLIPBOARD_POLL_MS);
    // 在别的程序里复制完切回来：马上同步
    window.addEventListener("focus", () => void this.syncClipboard(false));
  }

  // ───────────────────────── 连接 ─────────────────────────

  private open() {
    const url = new URL("ws", location.href);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    url.search = "";
    url.hash = "";
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    ws.onopen = () => {
      this.retries = 0;
      const waiters = this.openWaiters;
      this.openWaiters = [];
      for (const w of waiters) w();
    };
    ws.onmessage = (e: MessageEvent<unknown>) => {
      if (this.ws === ws) this.onMessage(e.data);
    };
    // 出错之后总会触发 close
    ws.onclose = () => {
      if (this.ws === ws) this.onClose();
    };
  }

  private onClose() {
    this.ws = null;
    this.rtt = null;
    for (const p of this.pending.values()) { clearTimeout(p.timer); p.reject(LOST); }
    this.pending.clear();
    // 服务那边的会话随这条连接一起结束了：告诉正在进行的会话，它们会自己重试
    const chans = [...this.chans.values()];
    this.chans.clear();
    for (const c of chans) c.acks.dispose();
    for (const c of chans) if (c.sid !== null) c.sink.event({ type: "closed", reason: LOST, retry: true });
    this.setStatus({ state: "disconnected", reason: LOST });
    const delay = Math.min(500 * 2 ** this.retries, RECONNECT_MAX_MS);
    this.retries++;
    window.setTimeout(() => this.open(), delay);
  }

  private whenOpen(): Promise<void> {
    if (this.ws?.readyState === WebSocket.OPEN) return Promise.resolve();
    return new Promise((resolve) => this.openWaiters.push(resolve));
  }

  private socket(): WebSocket | null {
    return this.ws?.readyState === WebSocket.OPEN ? this.ws : null;
  }

  private call<T>(cmd: string, args: object = {}): Promise<T> {
    const ws = this.socket();
    if (!ws) return Promise.reject(LOST);
    if (this.pending.size >= 128) { ws.close(1013, "too many pending calls"); return Promise.reject(LOST); }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.pending.delete(id);
        reject(LOST);
        // Don't leave an orphan session if its connect reply was lost.
        ws.close(1013, "command timeout");
      }, 30_000);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      try { this.send(ws, JSON.stringify({ id, cmd, ...args })); }
      catch { clearTimeout(timer); this.pending.delete(id); reject(LOST); }
    });
  }

  private send(ws: WebSocket, text: string) {
    if (ws.bufferedAmount + new TextEncoder().encode(text).byteLength > 2 * 1024 * 1024) {
      ws.close(1013, "outgoing queue overflow");
      throw LOST;
    }
    ws.send(text);
  }

  /** 不需要回复的命令 */
  private notify(cmd: string, args: object): Promise<void> {
    const ws = this.socket();
    if (!ws) return Promise.reject(LOST);
    try { this.send(ws, JSON.stringify({ cmd, ...args })); }
    catch { return Promise.reject(LOST); }
    return Promise.resolve();
  }

  private setStatus(s: ServerState) {
    this.status = s;
    for (const l of this.statusListeners) l(s);
  }

  private async ping() {
    if (!this.socket()) return;
    const t0 = performance.now();
    try {
      await this.call("ping");
    } catch {
      return;
    }
    const ms = performance.now() - t0;
    this.rtt = this.rtt === null ? ms : this.rtt * 0.7 + ms * 0.3;
  }

  // ───────────────────────── 收到的消息 ─────────────────────────

  private onMessage(data: unknown) {
    if (data instanceof ArrayBuffer) {
      this.onBinary(data);
      return;
    }
    if (typeof data !== "string") return;
    let msg: Incoming;
    try {
      msg = JSON.parse(data) as Incoming;
    } catch {
      return;
    }
    switch (msg.type) {
      case "reply": {
        const p = this.pending.get(msg.id);
        if (!p) return;
        this.pending.delete(msg.id);
        clearTimeout(p.timer);
        if (msg.err !== undefined) p.reject(msg.err);
        else p.resolve(msg.ok);
        return;
      }
      case "server":
        this.setStatus(msg.state);
        return;
      case "event":
        this.onEvent(msg.ch, msg.event);
        return;
      case "hostOnline":
        for (const l of this.onlineListeners) l(msg.code);
        return;
    }
  }

  private onBinary(buf: ArrayBuffer) {
    if (buf.byteLength < MIN_BINARY) return;
    const dv = new DataView(buf);
    const chan = this.chans.get(dv.getUint32(0, true));
    if (!chan) return;
    // 画面：收到就确认
    const seq = videoSeq(new DataView(buf, 4));
    if (seq !== null) chan.acks.push(seq);
    chan.sink.packet(new Uint8Array(buf, 4));
  }

  /** 确认画面（二进制消息，不走 JSON）；还不知道 sid 时先记下来 */
  private ack(chan: Chan, seq: number) {
    if (chan.sid === null) {
      chan.unacked = seq;
      return;
    }
    const ws = this.socket();
    if (!ws) return;
    if (ws.bufferedAmount > 2 * 1024 * 1024) {
      ws.close(1013, "outgoing queue overflow");
      return;
    }
    ws.send(ackMessage(chan.sid, seq));
  }

  private onEvent(ch: number, ev: SessionEvent) {
    const chan = this.chans.get(ch);
    if (!chan) return;
    switch (ev.type) {
      case "clipboard":
        this.remoteClipboard(chan, ev.text);
        return;
      case "stats":
        // 被控端测的是它到 airhand-web 的往返，再加上浏览器这一段
        if (ev.rttMs !== null && this.rtt !== null) ev = { ...ev, rttMs: ev.rttMs + this.rtt };
        break;
      case "closed":
        this.chans.delete(ch);
        chan.acks.dispose();
        break;
    }
    chan.sink.event(ev);
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
    this.clipWriteTask = drain().finally(() => { this.clipWriteTask = null; });
    return this.clipWriteTask;
  }

  /** 被控端复制了文本 */
  private remoteClipboard(chan: Chan, text: string) {
    if (!chan.clipboard) return;
    chan.synced = text;
    void this.writeClipboard(text);
  }

  /** 本机剪贴板有变化就发给打开了同步的会话 */
  private async syncClipboard(ask: boolean) {
    if (this.clipBusy) return;
    this.clipBusy = true;
    try {
      if (document.hasFocus()) await this.flushClipboard();
      if (this.clipToWrite !== null || this.clipWriteTask !== null) return;
      if (![...this.chans.values()].some((c) => c.clipboard && c.sid !== null)) return;
      const revision = this.clipRevision;
      const text = await this.readClipboard(ask);
      // 读取期间收到远端更新：这个读结果属于更早的状态，不能回传。
      if (revision !== this.clipRevision || text === null || text === "" || text.length > MAX_CLIPBOARD) return;
      this.clipSeen = text;
      for (const c of this.chans.values()) {
        if (c.synced === null) {
          c.synced = text;
        } else if (c.sid !== null && c.synced !== text) {
          // 关闭同步的会话也要记下，否则重新打开时会把旧内容发过去
          c.synced = text;
          if (c.clipboard) void this.notify("clipboard", { sid: c.sid, text }).catch(() => {});
        }
      }
    } finally {
      this.clipBusy = false;
    }
  }

  // ───────────────────────── Backend ─────────────────────────

  async appInfo(): Promise<AppInfo> {
    // 页面刚打开时 WebSocket 可能还没连上（或者服务暂时不在）：等连上再问
    for (;;) {
      await this.whenOpen();
      try {
        const info = await this.call<Omit<AppInfo, "lang">>("appInfo");
        return { ...info, lang: getLang() };
      } catch (e) {
        if (e !== LOST) throw e;
      }
    }
  }

  serverStatus = async () => this.status;

  onServerStatus(cb: (s: ServerState) => void) {
    this.statusListeners.add(cb);
    return () => void this.statusListeners.delete(cb);
  }

  onHostOnline(cb: (code: number) => void) {
    this.onlineListeners.add(cb);
    return () => void this.onlineListeners.delete(cb);
  }

  async connect(code: string, opts: ConnectOptions, sink: SessionSink, clipboard: boolean): Promise<SessionInfo> {
    const ch = this.nextCh++;
    const chan: Chan = {
      sink,
      sid: null,
      clipboard,
      synced: this.clipSeen,
      unacked: null,
      acks: new AckCoalescer((seq) => this.ack(chan, seq)),
    };
    this.chans.set(ch, chan);
    try {
      // connect 命令是控制码、ch 加上 ConnectOptions 的字段（和桌面版、Android 一样）
      const info = await this.call<SessionInfo>("connect", { ...opts, code, ch });
      chan.sid = info.sid;
      if (chan.unacked !== null) {
        this.ack(chan, chan.unacked);
        chan.unacked = null;
      }
      return info;
    } catch (e) {
      chan.acks.dispose();
      if (this.chans.get(ch) === chan) this.chans.delete(ch);
      throw e;
    }
  }

  /** 对会话的操作。剪贴板同步在浏览器里做；输入、确认这类高频消息不等回复 */
  command(c: Command): Promise<void> {
    switch (c.cmd) {
      case "clipboardSync":
        return this.clipboardSync(c.sid, c.enabled);
      case "input":
      case "ack":
        return this.notify(c.cmd, c);
      case "disconnect":
        for (const [ch, chan] of this.chans) {
          if (chan.sid === c.sid) {
            chan.acks.dispose();
            this.chans.delete(ch);
          }
        }
        return this.notify(c.cmd, c);
      default:
        return this.call<void>(c.cmd, c);
    }
  }

  private async clipboardSync(sid: number, enabled: boolean) {
    const chan = [...this.chans.values()].find((c) => c.sid === sid);
    if (!chan) return;
    chan.clipboard = enabled;
    if (enabled) {
      // 用户刚打开同步：这时可以请求读剪贴板的权限。现有内容不发送，只同步之后的变化
      const revision = this.clipRevision;
      const text = await this.readClipboard(true);
      if (revision === this.clipRevision && this.clipToWrite === null && text !== null) {
        chan.synced = text;
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
  switchToHost = async () => {};
  setLanguage = async () => {};
  onLangChanged = () => () => {};
  setWindowFullscreen = async () => {};
  onWindowFullscreenExit = () => () => {};
  // 网页版不保存"我的设备"（不写浏览器的存储）：只在本页面的内存里，刷新后清空
  loadDevices = async () => null;
  saveDevices = async () => {};
}

export function webBackend(): Backend {
  return new WebConsole();
}
