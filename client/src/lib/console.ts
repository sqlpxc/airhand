// 控制台和控制端服务之间的连接，桌面版和网页版共用：两边只是 WebSocket 的地址不同
// （backend/tauri.ts：本程序只监听 127.0.0.1 的服务；backend/web.ts：airhand-web），再加上各自的功能。
//
// 协议见 crates/airhand-link/src/console.rs。WebSocket、画面和声音的解码、绘制都在 Worker 里（console-worker.ts），
// 这里（主线程）只处理 JSON：
// * 命令 `{"id", "cmd", ...参数}`；对会话的操作就是 Command。输入这类高频消息不带 id，不等回复。
//   命令都经同一个 Worker、同一条 WebSocket，按调用的先后到达；
// * 会话的画面是一个 canvas：交给 Worker（transferControlToOffscreen）之后由它绘制，这边是 MediaView；
// * 画面确认由 Worker 收到画面时自己发（二进制，见 ack.ts）；
// * 显示的延迟 = 控制端服务到被控端的往返 + 这里到控制端服务的往返（网页版是网络，桌面版是本机）。
import type {
  AppInfo,
  Codec,
  Command,
  ConnectOptions,
  FileEntry,
  FileError,
  Problem,
  ServerState,
  SessionEvent,
  SessionInfo,
} from "./api";
import { FILE_ERRORS, type LocalFiles } from "./files";

/** 读文件时抛出来的东西变成 FileError（不认识的当作读不了） */
function fileError(e: unknown): FileError {
  return typeof e === "string" && (FILE_ERRORS as string[]).includes(e) ? (e as FileError) : "io";
}
import type { AvSyncStats } from "./avsync";
import type { FromWorker, ToWorker } from "./console-worker";
import type { Area, EnhanceLevel } from "./enhance";
import ConsoleWorker from "./console-worker?worker";

/** 会话的画面在 Worker 里，它告诉主线程的 */
export interface ViewOutput {
  /** 第 stream 路画面的大小（像素）变了 */
  size(stream: number, w: number, h: number): void;
  /** 光标包（见 media.ts） */
  cursor(pkt: Uint8Array): void;
  /** 本机实际解不了这种格式 */
  undecodable(codec: Codec): void;
  /** 解出了画面（之前的解码问题没了） */
  recovered(): void;
  /** 约每秒一次：平均解码耗时、音画同步 */
  stats(decodeMs: number, sync: AvSyncStats): void;
}

/** 主线程对一个会话的画面（在 Worker 里）的操作 */
export interface MediaView {
  readonly id: number;
  /** 音画同步是否生效（开关打开且正在播放声音） */
  setSync(on: boolean): void;
  /** 声音输出：音频线程的端口（见 audio.ts）；null = 不播放 */
  setAudio(port: MessagePort | null): void;
  /** 本机麦克风：采集它的音频线程的端口（见 mic.ts），Worker 编码之后发给被控端；null = 不发 */
  setMic(port: MessagePort | null): void;
  /** 声音时钟（见 avsync.ts）：`localMs` 是主线程 performance.now() 的时基 */
  audioClock(hostUs: number, localMs: number, appliedMs: number): void;
  /** 请被控端给每一路画面编一个关键帧（例如刚切回这个会话）。和解码出错时 Worker 自己要的一起限流 */
  requestKeyframe(): void;
  /** 第 stream 路画面（多屏）画在 canvas 上：要看这一路之前交过去（之后主线程不能再画它） */
  addCanvas(stream: number, canvas: HTMLCanvasElement): void;
  /** 第 stream 路画面怎么显示：显示区域（设备像素；null = 1:1 显示）、画面增强的强度（见 enhance.ts） */
  setLayout(stream: number, area: Area | null, enhance: EnhanceLevel): void;
  /** 会话关掉了 */
  dispose(): void;
}

type Incoming =
  | { type: "reply"; id: number; ok?: unknown; err?: Problem }
  | { type: "server"; state: ServerState }
  | { type: "event"; ch: number; event: SessionEvent }
  | { type: "hostOnline"; code: number }
  /** 被控端要这里复制的文件（见 files.ts）：列出目录 */
  | { type: "fileList"; req: number; path: string[] }
  /** 读一块（二进制地答 `3 | req | 错误 | 数据`） */
  | { type: "fileRead"; req: number; path: string[]; offset: number; len: number };

/** 二进制消息的类型：这里复制的文件的内容（console.rs 的 BINARY_FILE） */
const BINARY_FILE = 3;
const NO_ERROR = 0xffffffff;
/** 最多记着这么多次复制的文件（最新的和之前的：被控端可能还在从之前的那次复制） */
const KEEP_LOCAL = 2;
/** 等一块被控端的文件最多这么久 */
const READ_TIMEOUT_MS = 120_000;

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Problem) => void;
  timer: ReturnType<typeof setTimeout>;
}

/** 会话的一次连接（connect 时选的 ch） */
interface Chan {
  /** connect 返回之后才知道 */
  sid: number | null;
  onEvent: (ev: SessionEvent) => void;
}

/** 与控制端服务的连接断了 */
export const LOST: Problem = { kind: "consoleLost" };
const RECONNECT_MAX_MS = 5000;
const PING_MS = 2000;
const CALL_TIMEOUT_MS = 30_000;
const MAX_PENDING = 128;

export class ConsoleLink {
  private readonly url: () => Promise<string>;
  private readonly worker: Worker;
  private connected = false;
  private retries = 0;
  private nextId = 1;
  private nextCh = 1;
  private nextView = 1;
  private pending = new Map<number, Pending>();
  private openWaiters: (() => void)[] = [];
  private chans = new Map<number, Chan>();
  private views = new Map<number, ViewOutput>();
  private status: ServerState = { state: "connecting" };
  private statusListeners = new Set<(s: ServerState) => void>();
  private onlineListeners = new Set<(code: number) => void>();
  /** 到控制端服务的往返时间（毫秒，平滑后） */
  private rtt: number | null = null;
  /** 这里复制的文件（最新的在后面，见 files.ts） */
  private local: LocalFiles[] = [];
  /** 在等的被控端的文件的一块 */
  private reads = new Map<number, { resolve: (d: Uint8Array) => void; reject: (e: FileError) => void; timer: ReturnType<typeof setTimeout> }>();
  private nextRead = 1;

  /** `url`：控制端服务的 WebSocket 地址（每次连接前取一次） */
  constructor(url: () => Promise<string>) {
    this.url = url;
    this.worker = new ConsoleWorker();
    this.worker.onmessage = (e: MessageEvent<FromWorker>) => this.onWorker(e.data);
    void this.open();
    setInterval(() => void this.ping(), PING_MS);
  }

  // ───────────────────────── 连接 ─────────────────────────

  private async open() {
    try {
      this.post({ t: "open", url: await this.url() });
    } catch (e) {
      console.warn("can't get the console's address", e);
      this.reopen();
    }
  }

  private reopen() {
    const delay = Math.min(500 * 2 ** this.retries, RECONNECT_MAX_MS);
    this.retries++;
    setTimeout(() => void this.open(), delay);
  }

  private onClose() {
    this.connected = false;
    this.rtt = null;
    for (const r of this.reads.values()) {
      clearTimeout(r.timer);
      r.reject("closed");
    }
    this.reads.clear();
    for (const p of this.pending.values()) {
      clearTimeout(p.timer);
      p.reject(LOST);
    }
    this.pending.clear();
    // 服务那边的会话随这条连接一起结束了（Worker 已经扔掉了它们的通道）：告诉会话，它们会自己重试
    const chans = [...this.chans.values()];
    this.chans.clear();
    for (const c of chans) if (c.sid !== null) c.onEvent({ type: "closed", reason: LOST, retry: true });
    this.setStatus({ state: "disconnected", reason: LOST });
    this.reopen();
  }

  private whenOpen(): Promise<void> {
    if (this.connected) return Promise.resolve();
    return new Promise((resolve) => this.openWaiters.push(resolve));
  }

  private post(m: ToWorker, transfer: Transferable[] = []) {
    this.worker.postMessage(m, transfer);
  }

  private call<T>(cmd: string, args: object = {}): Promise<T> {
    if (!this.connected) return Promise.reject(LOST);
    if (this.pending.size >= MAX_PENDING) {
      this.post({ t: "close", reason: "too many pending calls" });
      return Promise.reject(LOST);
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(LOST);
        // 回复丢了（不应该发生）：断开重来，免得那边留下没人管的会话
        this.post({ t: "close", reason: "command timeout" });
      }, CALL_TIMEOUT_MS);
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, timer });
      this.post({ t: "send", text: JSON.stringify({ id, cmd, ...args }) });
    });
  }

  /** 不要回复的命令 */
  private notify(c: Command): Promise<void> {
    if (!this.connected) return Promise.reject(LOST);
    this.post({ t: "send", text: JSON.stringify(c) });
    return Promise.resolve();
  }

  private setStatus(s: ServerState) {
    this.status = s;
    for (const l of this.statusListeners) l(s);
  }

  private async ping() {
    if (!this.connected) return;
    const t0 = performance.now();
    try {
      await this.call("ping");
    } catch {
      return;
    }
    const ms = performance.now() - t0;
    this.rtt = this.rtt === null ? ms : this.rtt * 0.7 + ms * 0.3;
  }

  // ───────────────────────── 收到的 ─────────────────────────

  private onWorker(m: FromWorker) {
    switch (m.t) {
      case "open": {
        this.connected = true;
        this.retries = 0;
        const waiters = this.openWaiters;
        this.openWaiters = [];
        for (const w of waiters) w();
        return;
      }
      case "closed":
        this.onClose();
        return;
      case "text":
        this.onText(m.text);
        return;
      case "cursor":
        this.views.get(m.id)?.cursor(m.pkt);
        return;
      case "size":
        this.views.get(m.id)?.size(m.stream, m.w, m.h);
        return;
      case "undecodable":
        this.views.get(m.id)?.undecodable(m.codec);
        return;
      case "recovered":
        this.views.get(m.id)?.recovered();
        return;
      case "stats":
        this.views.get(m.id)?.stats(m.decodeMs, m.sync);
        return;
      case "fileData": {
        const r = this.reads.get(m.req);
        if (!r) return;
        this.reads.delete(m.req);
        clearTimeout(r.timer);
        if (m.err === NO_ERROR) r.resolve(m.data);
        else r.reject(FILE_ERRORS[m.err] ?? "io");
        return;
      }
    }
  }

  private onText(text: string) {
    let msg: Incoming;
    try {
      msg = JSON.parse(text) as Incoming;
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
      case "fileList":
        void this.answerList(msg.req, msg.path);
        return;
      case "fileRead":
        void this.answerRead(msg.req, msg.path, msg.offset, msg.len);
        return;
    }
  }

  /** 复制了这一项的那一次（新的优先） */
  private localFor(path: string[]): LocalFiles | null {
    for (let i = this.local.length - 1; i >= 0; i--) if (this.local[i].has(path[0] ?? "")) return this.local[i];
    return null;
  }

  private async answerList(req: number, path: string[]) {
    let reply: object;
    try {
      const files = this.localFor(path);
      if (!files) throw "expired";
      reply = { cmd: "fileListed", req, entries: await files.list(path) };
    } catch (e) {
      reply = { cmd: "fileListed", req, error: fileError(e) };
    }
    this.post({ t: "send", text: JSON.stringify(reply) });
  }

  private async answerRead(req: number, path: string[], offset: number, len: number) {
    let data: Uint8Array = new Uint8Array(0);
    let err = NO_ERROR;
    try {
      const files = this.localFor(path);
      if (!files) throw "expired";
      data = await files.read(path, offset, len);
    } catch (e) {
      err = FILE_ERRORS.indexOf(fileError(e));
    }
    const buf = new ArrayBuffer(9 + data.length);
    const dv = new DataView(buf);
    dv.setUint8(0, BINARY_FILE);
    dv.setUint32(1, req, true);
    dv.setUint32(5, err, true);
    new Uint8Array(buf, 9).set(data);
    this.post({ t: "binary", data: buf }, [buf]);
  }

  private onEvent(ch: number, ev: SessionEvent) {
    const chan = this.chans.get(ch);
    if (!chan) return;
    // 被控端测的是它到控制端服务的往返，再加上这一段
    if (ev.type === "stats" && ev.rttMs !== null && this.rtt !== null) ev = { ...ev, rttMs: ev.rttMs + this.rtt };
    if (ev.type === "closed") this.unbind(ch);
    chan.onEvent(ev);
  }

  private unbind(ch: number) {
    if (this.chans.delete(ch)) this.post({ t: "unbind", ch });
  }

  // ───────────────────────── Backend 的会话部分 ─────────────────────────

  async appInfo(): Promise<AppInfo> {
    // 刚打开时可能还没连上（或者服务暂时不在）：等连上再问
    for (;;) {
      await this.whenOpen();
      try {
        return await this.call<AppInfo>("appInfo");
      } catch (e) {
        if (e !== LOST) throw e;
      }
    }
  }

  async serverStatus(): Promise<ServerState> {
    return this.status;
  }

  onServerStatus(cb: (s: ServerState) => void): () => void {
    this.statusListeners.add(cb);
    return () => void this.statusListeners.delete(cb);
  }

  onHostOnline(cb: (code: number) => void): () => void {
    this.onlineListeners.add(cb);
    return () => void this.onlineListeners.delete(cb);
  }

  view(canvas: HTMLCanvasElement, out: ViewOutput): MediaView {
    const id = this.nextView++;
    const offscreen = canvas.transferControlToOffscreen();
    this.views.set(id, out);
    this.post({ t: "view", id, canvas: offscreen }, [offscreen]);
    return {
      id,
      setSync: (on) => this.post({ t: "sync", id, on }),
      setAudio: (port) => this.post({ t: "audio", id, port }, port ? [port] : []),
      setMic: (port) => this.post({ t: "mic", id, port }, port ? [port] : []),
      // Worker 的 performance.now() 从它自己创建时算起：换成绝对时间交过去
      audioClock: (hostUs, localMs, appliedMs) =>
        this.post({ t: "clock", id, hostUs, at: performance.timeOrigin + localMs, appliedMs }),
      requestKeyframe: () => this.post({ t: "keyframe", id }),
      addCanvas: (stream, canvas) => {
        const offscreen = canvas.transferControlToOffscreen();
        this.post({ t: "canvas", id, stream, canvas: offscreen }, [offscreen]);
      },
      setLayout: (stream, area, enhance) => this.post({ t: "layout", id, stream, area, enhance }),
      dispose: () => {
        this.views.delete(id);
        this.post({ t: "drop", id });
      },
    };
  }

  async connect(
    code: string,
    opts: ConnectOptions,
    view: MediaView,
    onEvent: (ev: SessionEvent) => void,
    clipboard: boolean,
  ): Promise<SessionInfo> {
    const ch = this.nextCh++;
    const chan: Chan = { sid: null, onEvent };
    this.chans.set(ch, chan);
    // 先告诉 Worker 这个 ch 的画面画在哪里：会话的第一批数据可能比 connect 的回复先到
    this.post({ t: "bind", ch, id: view.id });
    try {
      // connect 命令是控制码、ch、剪贴板开关加上 ConnectOptions 的字段（和 Android 一样）
      const info = await this.call<SessionInfo>("connect", { ...opts, code, ch, clipboard });
      chan.sid = info.sid;
      if (this.chans.get(ch) === chan) this.post({ t: "sid", ch, sid: info.sid });
      return info;
    } catch (e) {
      this.unbind(ch);
      throw e;
    }
  }

  // ───────────────────────── 剪贴板里的文件（见 files.ts） ─────────────────────────

  async offerFiles(sid: number, files: LocalFiles): Promise<void> {
    this.local.push(files);
    while (this.local.length > KEEP_LOCAL) this.local.shift();
    await this.call<void>("offerFiles", { sid, items: files.items() });
  }

  async remoteList(sid: number, clip: number, path: string[], recursive: boolean): Promise<FileEntry[]> {
    const r = await this.call<{ entries: FileEntry[]; error: FileError | null }>("remoteList", {
      sid,
      clip,
      path,
      recursive,
    });
    if (r.error) throw r.error;
    return r.entries;
  }

  remoteRead(
    sid: number,
    clip: number,
    file: number,
    path: string[],
    size: number,
    mtime: number | null,
    offset: number,
    len: number,
  ): Promise<Uint8Array> {
    if (!this.connected) return Promise.reject("closed" satisfies FileError);
    const req = this.nextRead++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.reads.delete(req);
        reject("closed" satisfies FileError);
      }, READ_TIMEOUT_MS);
      this.reads.set(req, { resolve, reject, timer });
      this.post({
        t: "send",
        text: JSON.stringify({ cmd: "remoteRead", sid, clip, file, req, path, size, mtime, offset, len }),
      });
    });
  }

  remoteClose(sid: number, file: number) {
    if (this.connected) this.post({ t: "send", text: JSON.stringify({ cmd: "remoteClose", sid, file }) });
  }

  /** 对会话的操作。输入这类高频消息不等回复 */
  command(c: Command): Promise<void> {
    switch (c.cmd) {
      case "input":
      case "ack":
        return this.notify(c);
      case "disconnect":
        for (const [ch, chan] of this.chans) if (chan.sid === c.sid) this.unbind(ch);
        return this.notify(c);
      default:
        return this.call<void>(c.cmd, c);
    }
  }
}
