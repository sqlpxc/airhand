// 输入：把本机的鼠标、键盘变成发给被控端的输入事件，合并后成批发送。
// 断开 / 换了会话之后，还没发出去的输入全部丢掉：延迟的定时器绝不能把旧输入发给新的会话。
//
// 鼠标有两种发法（见 airhand_proto::session 的"鼠标"）：
// * 绝对坐标：本机指针在画面的哪里，被控端的光标就在哪里；
// * 相对移动：发移动了多少（mouseDelta）。这时锁定本机指针（Pointer Lock）：指针不会碰到屏幕边就停下，
//   移动量也是鼠标的原始计数（unadjustedMovement）。
//
// 三种鼠标模式（MouseMode，用户在工具栏的鼠标菜单里选，按设备记住）：
// * 自动（默认）：平时发绝对坐标；被控端的光标隐藏了（游戏里按住右键转视角、第一人称游戏）时自动锁定、
//   改发相对移动，光标重新出现时解除。锁定不了（浏览器不支持、被拒绝）也照样发相对移动；
// * 桌面：一直发绝对坐标，从不锁定（看视频、画图这些会隐藏光标、却不该锁定的软件）。对方隐藏光标时本机显示普通的指针；
// * 游戏：一直锁定、发相对移动，被控端把光标画进画面里（QualitySettings.drawCursor，session.ts 加上），
//   本机指针不显示：看到的就是对方真实的光标，不用管它隐藏没有。没锁定的时候（还没点过画面、按 Esc 放开了、
//   切到别的窗口回来）鼠标什么也不发：点一下画面重新锁定，这一下不发给对方（游戏里点一下可能就是开枪）；
//   键盘照常发。
//
// 自动模式什么时候锁定：
// * 被控端光标隐藏时按下鼠标键，或者按着键时光标隐藏了（游戏按住右键转视角）：马上锁定；
// * 被控端光标隐藏时只是移动（第一人称游戏）：持续移动一小会儿光标还是隐藏的才锁定。
//   Windows 打字时会暂时隐藏光标，鼠标一动就重新出现，这种情况不锁定；
// * 被控端光标重新出现：自动锁定的解除锁定，本机指针回到锁定时的位置（游戏也会把光标放回原处）；
// * 按着鼠标键时光标出现了先不算，松开所有键再解除。这是防御性的：万一转视角时光标隐藏 / 出现来回跳（被控端的
//   检测抖动、个别游戏自己在闪），跟着它就会解除、再锁定，浏览器每次都提示"按 Esc 显示光标"，中间还改发绝对坐标，
//   视角乱跳。实际游戏里还没见过这样跳：以前魔兽世界里按住右键转视角时反复锁定、解除，是锁定后把按着的键当成了松开、
//   给对方发了松开，游戏真的退出了转视角、光标真的出现了（见 buttonsAfter）。代价：游戏在按着键时真的把光标显示出来
//   （例如转视角时弹出菜单），要等松开才改回绝对坐标，这期间看不到光标；
// * 用户按 Esc 解除了锁定：到下一次点击之前不再自动锁定，指针照常显示。
//
// 不管哪种模式，锁定期间按键的状态只跟着按下 / 松开走，不看单纯移动的 buttons（Windows 上锁定时它是 0，见 buttonsAfter）。
//
// 怎么放开鼠标（解除锁定）：
// * 浏览器自己的 Esc：只在 Esc 没被键盘锁定时有用。全屏时键盘整个被锁定（见 actions.ts），
//   Esc 交给网页、发给对方（游戏要用），浏览器不再用它解除锁定，而这时指针也碰不到顶端、叫不出工具栏；
// * 所以锁定时长按 Esc（ESC_HOLD_MS）由这里自己解除：短按照常马上发给对方，按住期间不发自动重复。
// 放开之后：自动模式到下一次点击之前不再自动锁定，游戏模式点一下画面重新锁定。
//
// Ctrl+Alt+Shift+M：进入 / 退出游戏模式（退出时回到之前的自动或桌面模式）。
//
// 同时看对方的好几个显示器（多屏）时，画面是几个 canvas 拼在一起的：事件挂在包着它们的元素上，绝对坐标是指针所在
// （落在缝里时是最近）的那一路画面上的位置，带着第几路（见 screens.ts 的 hitTest）。
//
// 画面可能在主窗口打开的弹出窗口里（一个显示器单独一个窗口，见 popout.ts）：那里的画面另有一个 RemoteInput。
// 指针锁定、设备像素比例都按画面所在的窗口算（element.ownerDocument），不是主窗口的。
import type { InputEvent, MouseButton, MouseMode } from "./api";
import { isPasteKey } from "./files.ts";

const IS_MAC = typeof navigator !== "undefined" && /Mac/.test(navigator.platform || navigator.userAgent);
const CTRL_COMMAND_SWAP = new Map([
  ["ControlLeft", "MetaLeft"], ["MetaLeft", "ControlLeft"],
  ["ControlRight", "MetaRight"], ["MetaRight", "ControlRight"],
]);
const META_KEYS = new Set(["MetaLeft", "MetaRight"]);
const MODIFIERS = new Set(["ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight", "CapsLock", "Fn"]);
/** PointerEvent.buttons 的位 → 鼠标键 */
const BUTTON_BITS: [number, MouseButton][] = [[1, "left"], [2, "right"], [4, "middle"], [8, "back"], [16, "forward"]];
/** 被控端光标隐藏着、只是移动（没按键）时，持续这么久光标还是隐藏的才锁定（见文件开头） */
const AUTO_LOCK_AFTER_MS = 300;
/**
 * 鼠标移动（不急的输入）两批之间至少隔这么久（毫秒）。前沿节流：离上一批已经超过这么久就马上发（同一个任务里的一起发），
 * 否则等到那个时候再发合并好的。指针事件的频率不超过 1000 / MOVE_INTERVAL_MS 时每一下都不用等
 */
const MOVE_INTERVAL_MS = 8;
/** 锁定时按住 Esc 这么久就解除锁定（见文件开头） */
const ESC_HOLD_MS = 1000;
/**
 * 锁定被拒绝之后，自动锁定（光标隐藏着一直在移动时）这么久之内不再试：被拒绝时每次移动都请求的话，一秒钟能失败几十次
 * （例如刚解除就又要锁定，浏览器还不让）
 */
const LOCK_RETRY_MS = 500;

/**
 * 这个键盘事件是哪个物理键（W3C `KeyboardEvent.code`），认不出时是空字符串。
 *
 * Windows 11 上用新版微软输入法（微软拼音、日文 IME 等，TSF）时，右 Shift 的按键消息带着扩展键标志（扫描码成了 E0 36，
 * 本来是 36），Chromium（WebView2、Chrome、Edge）认不出这个扫描码：`code` 是空的，`location` 也是 0，只有 `key` 还是 "Shift"。
 * 左 Shift 不受影响，所以没有 `code` 的 Shift 就当右 Shift。不这样的话右 Shift 整个不发给对方，按住右 Shift 打 1 出来的是 1
 */
export function physicalCode(e: KeyboardEvent): string {
  if (e.code && e.code !== "Unidentified") return e.code;
  // location 1 = DOM_KEY_LOCATION_LEFT
  if (e.key === "Shift") return e.location === 1 ? "ShiftLeft" : "ShiftRight";
  return "";
}

/** 进入 / 退出游戏模式的快捷键：Ctrl+Alt+Shift+M */
export function isLockShortcut(e: KeyboardEvent): boolean {
  return e.code === "KeyM" && e.ctrlKey && e.altKey && e.shiftKey && !e.metaKey;
}

type Lockable = HTMLElement & { requestPointerLock(options?: { unadjustedMovement?: boolean }): Promise<void> | void };

/** 指针（屏幕坐标）在第几路画面上、在它上面的归一化坐标（0..1） */
export type Locate = (clientX: number, clientY: number) => { x: number; y: number; stream: number };

export class RemoteInput {
  /** 画面（几路画面拼在一起时是包着它们的元素）：事件挂在它上面，锁定指针也是它 */
  private canvas: HTMLElement;
  /** 画面所在的网页（主窗口的，或者弹出窗口的）：指针锁定在它上面；测试里没有时为 null */
  private readonly doc: Document | null;
  private locate: Locate | null;
  private send: (events: InputEvent[]) => void;
  private connected: () => boolean;
  private _interactive = false;
  private queue: InputEvent[] = [];
  private timer: ReturnType<typeof setTimeout> | undefined;
  private scheduled = false;
  /** 上一批发出去的时刻（performance.now()） */
  private lastSent = -Infinity;
  private generation = 0;
  private wheel = { x: 0, y: 0 };
  private pressed = new Set<string>();
  shouldSwapCtrlCommand: (() => boolean) | null = null;
  /** 一组按键全部松开前沿用按下时的映射，避免切换开关后松开了另一个键 */
  private swapForChord = false;
  private outgoingCode(code: string) {
    return this.swapForChord ? CTRL_COMMAND_SWAP.get(code) ?? code : code;
  }
  /** 按着的鼠标键（PointerEvent.buttons 的位） */
  private buttons = 0;
  /** 拦下来的快捷键：抬起时也不发 */
  private swallowed = new Set<string>();
  private abort = new AbortController();

  // ── 相对移动 ──
  /** 被控端的光标隐藏了（按着鼠标键时出现了先不算，见 setRemoteCursorHidden） */
  private remoteHidden = false;
  /** 被控端最新说的：光标隐藏着。按着键时 remoteHidden 可能还是 true，松开所有键时再跟上它 */
  private remoteHiddenNow = false;
  /** 鼠标模式（见文件开头） */
  private _mode: MouseMode = "auto";
  /** 进入游戏模式之前的模式：Ctrl+Alt+Shift+M 退出游戏模式时回到它 */
  private lastMode: Exclude<MouseMode, "game"> = "auto";
  /** 游戏模式下点画面重新锁定的那一下：按下到松开都不发给对方 */
  private swallowing = false;
  /** 请求了锁定，还没有结果 */
  private lockPending = false;
  /** 锁定被拒绝了：自动锁定到这个时刻之前不再试（见 LOCK_RETRY_MS） */
  private lockRetryAt = 0;
  /** 是我们自己解除的锁定（不是用户按了 Esc） */
  private unlocking = false;
  /** 用户按 Esc 解除了锁定：到下一次点击之前不自动锁定 */
  private userUnlocked = false;
  /** 被控端光标隐藏着、没锁定时，开始移动的时刻 */
  private hiddenMoveSince: number | null = null;
  /** 锁定时拿到的是鼠标的原始计数 */
  private raw = false;
  /** 还不够一个计数的移动量 */
  private frac = { x: 0, y: 0 };
  /** 锁定时按下了 Esc、还没松开：不发它的自动重复 */
  private escHeld = false;
  /** 按住 Esc 够久就解除锁定的定时器 */
  private escTimer: ReturnType<typeof setTimeout> | undefined;

  /** 鼠标的方式变了（相对移动 / 锁定 / 鼠标模式），界面据此刷新指针和工具栏 */
  onModeChange: (() => void) | null = null;
  /** 鼠标模式变了（工具栏、快捷键）：会话据此让对方画不画光标、记进我的设备 */
  onMouseMode: ((mode: MouseMode) => void) | null = null;
  /**
   * 粘贴键先让浏览器触发 paste 事件（网页版：里面可能有这里复制的文件，见 files.ts），这个键等它处理完再发。
   * 不设置时粘贴键和别的键一样直接发（桌面版的剪贴板由 Rust 端自己同步）
   */
  onPaste: ((e: ClipboardEvent) => void) | null = null;

  /** `locate`：指针在第几路画面上（多屏）；不给就是整个元素都是第 0 路画面 */
  constructor(canvas: HTMLElement, connected: () => boolean, send: (events: InputEvent[]) => void, locate?: Locate) {
    this.canvas = canvas; this.connected = connected; this.send = send;
    this.locate = locate ?? null;
    this.doc = canvas.ownerDocument ?? (typeof document !== "undefined" ? document : null);
    this.bind();
  }

  get interactive() { return this._interactive; }
  set interactive(v: boolean) {
    this._interactive = v;
    if (!v) {
      this.cancelEscHold();
      this.unlock();
      this.swallowing = false;
    }
  }

  /** 鼠标模式（见文件开头） */
  get mode(): MouseMode {
    return this._mode;
  }

  /**
   * 换鼠标模式。要在用户操作里调用（点菜单、按快捷键）：游戏模式马上锁定指针，没有用户操作时浏览器不让锁定
   * （那就等用户点一下画面）。会话刚建好时用记着的模式不算换，不通知（`notify` 为 false）
   */
  setMode(mode: MouseMode, notify = true) {
    if (mode !== "game") this.lastMode = mode;
    if (mode === this._mode) {
      // 游戏模式里再选一次游戏模式：重新锁定（例如放开了鼠标之后）
      if (mode === "game") this.lock();
      return;
    }
    this._mode = mode;
    this.userUnlocked = false;
    this.hiddenMoveSince = null;
    this.frac = { x: 0, y: 0 };
    if (mode === "game") this.lock();
    // 自动模式下对方的光标还隐藏着（例如从游戏模式回来时游戏还在转视角）：接着锁定
    else if (mode === "desktop" || !this.remoteHidden) this.unlock();
    this.onModeChange?.();
    if (notify) this.onMouseMode?.(mode);
  }

  /** Ctrl+Alt+Shift+M：进入 / 退出游戏模式（退出时回到之前的模式）。要在用户操作里调用 */
  toggleGame() {
    this.setMode(this._mode === "game" ? this.lastMode : "game");
  }

  /** 本机指针锁定在画面上了 */
  get locked(): boolean {
    return this.doc?.pointerLockElement === this.canvas;
  }

  /** 鼠标现在发相对移动 */
  get relative(): boolean {
    switch (this._mode) {
      // 浏览器不能锁定指针时游戏模式也照样发相对移动（指针会停在屏幕边上）
      case "game": return this.locked || !this.canLock;
      case "desktop": return false;
      default: return this.locked || (this.remoteHidden && !this.userUnlocked);
    }
  }

  /** 游戏模式下指针没有锁定（还没点过画面、放开了鼠标、切走过）：鼠标不发给对方，点一下画面重新锁定 */
  get gamePaused(): boolean {
    return this._mode === "game" && !this.relative;
  }

  /** 这个浏览器能锁定指针 */
  private get canLock(): boolean {
    return typeof (this.canvas as unknown as Partial<Lockable>).requestPointerLock === "function";
  }

  reset() {
    this.generation++;
    clearTimeout(this.timer);
    this.timer = undefined;
    this.scheduled = false;
    this.queue = [];
    this.pressed.clear();
    this.buttons = 0;
    this.wheel = { x: 0, y: 0 };
    this.frac = { x: 0, y: 0 };
    this.remoteHidden = false;
    this.remoteHiddenNow = false;
    this.hiddenMoveSince = null;
    this.userUnlocked = false;
    this.swallowing = false;
    this.cancelEscHold();
    this.unlock();
  }
  dispose() { this.reset(); this.abort.abort(); }
  releaseAll() {
    this.pressed.clear();
    this.buttons = 0;
    this.enqueue([{ type: "releaseAll" }], true);
    this.settleRemoteCursor();
  }
  focus() { this.canvas.focus({ preventScroll: true }); }

  enqueue(events: InputEvent[], urgent: boolean) {
    if (!this.connected()) return;
    for (const event of events) {
      const last = this.queue[this.queue.length - 1];
      // 连续的绝对移动只留最后一个，连续的相对移动加起来（不越过点击和按键）
      if (event.type === "mouseMove" && last?.type === "mouseMove") {
        this.queue[this.queue.length - 1] = event;
      } else if (event.type === "mouseDelta" && last?.type === "mouseDelta") {
        this.queue[this.queue.length - 1] = { type: "mouseDelta", dx: last.dx + event.dx, dy: last.dy + event.dy };
      } else this.queue.push(event);
      // Bound even a synchronous sendText loop. Keyboard edges stay ordered.
      if (this.queue.length >= 64) this.flush();
    }
    // 已经约好在这个任务结束时发了：一起发
    if (this.scheduled || this.queue.length === 0) return;
    const gen = this.generation;
    const wait = urgent ? 0 : this.lastSent + MOVE_INTERVAL_MS - performance.now();
    if (wait <= 0) {
      this.scheduled = true;
      queueMicrotask(() => { if (gen === this.generation) this.flush(); });
    } else if (this.timer === undefined) {
      this.timer = setTimeout(() => { if (gen === this.generation) this.flush(); }, wait);
    }
  }
  private flush() {
    this.scheduled = false;
    clearTimeout(this.timer); this.timer = undefined;
    const batch = this.queue; this.queue = [];
    if (batch.length && this.connected()) {
      this.lastSent = performance.now();
      this.send(batch);
    }
  }

  // ───────────────────────── 相对移动 / 锁定 ─────────────────────────

  /** 被控端的光标隐藏 / 出现了 */
  setRemoteCursorHidden(hidden: boolean) {
    this.remoteHiddenNow = hidden;
    // 按着键时（游戏按住键转视角）光标出现了：松开所有键再算（见文件开头、settleRemoteCursor）
    if (!hidden && this.remoteHidden && this.buttons !== 0) return;
    this.applyRemoteHidden(hidden);
  }

  /** 松开了所有鼠标键：按着键期间光标出现了的，现在解除 */
  private settleRemoteCursor() {
    if (this.buttons === 0 && this.remoteHidden && !this.remoteHiddenNow) this.applyRemoteHidden(false);
  }

  private applyRemoteHidden(hidden: boolean) {
    if (this.remoteHidden === hidden) return;
    this.remoteHidden = hidden;
    this.hiddenMoveSince = null;
    // 只有自动模式跟着对方的光标锁定、解除；别的模式只是记下来（换回自动模式时用）
    if (this._mode === "auto") {
      if (hidden) {
        // 按着键时光标隐藏了（游戏按住右键转视角）：马上锁定，按下鼠标键的那一下还算用户操作
        if (this._interactive && this.buttons !== 0 && !this.userUnlocked) this.lock();
      } else {
        this.userUnlocked = false;
        this.unlock();
      }
      this.frac = { x: 0, y: 0 };
    }
    this.onModeChange?.();
  }

  /** 用户放开鼠标（长按 Esc）：和浏览器的 Esc 一样，自动模式到下一次点击之前不再自动锁定，游戏模式点一下画面再锁定 */
  release() {
    if (this._mode === "auto" && this.remoteHidden) this.userUnlocked = true;
    this.hiddenMoveSince = null;
    this.frac = { x: 0, y: 0 };
    this.unlock();
    this.onModeChange?.();
  }

  /** 锁定时按下了 Esc：开始计时，按住够久就解除 */
  private holdEsc() {
    this.cancelEscHold();
    this.escHeld = true;
    this.escTimer = setTimeout(() => {
      this.escTimer = undefined;
      // 这期间可能已经解除了（例如游戏打开菜单，光标出现了）
      if (this.relative) this.release();
    }, ESC_HOLD_MS);
  }
  private cancelEscHold() {
    this.escHeld = false;
    clearTimeout(this.escTimer);
    this.escTimer = undefined;
  }

  private lock() {
    const c = this.canvas as unknown as Lockable;
    if (!this._interactive || this.locked || this.lockPending || typeof c.requestPointerLock !== "function") return;
    this.lockPending = true;
    const attempt = (raw: boolean): Promise<void> => {
      let r: Promise<void> | void;
      try {
        r = raw ? c.requestPointerLock({ unadjustedMovement: true }) : c.requestPointerLock();
      } catch (e) {
        return Promise.reject(e);
      }
      // 返回 Promise 的浏览器才认识 unadjustedMovement（不认识时会拒绝）
      this.raw = raw && r instanceof Promise;
      return Promise.resolve(r);
    };
    attempt(true)
      .catch((e: unknown) => ((e as { name?: string })?.name === "NotSupportedError" ? attempt(false) : Promise.reject(e)))
      // 锁定不了（例如没有用户操作、浏览器不允许）：自动模式照样发相对移动，只是指针会停在屏幕边上；
      // 游戏模式等用户点画面
      .catch(() => {})
      .finally(() => { this.lockPending = false; });
  }

  private unlock() {
    if (!this.locked) return;
    this.unlocking = true;
    this.doc?.exitPointerLock();
  }

  private onLockChange() {
    this.lockPending = false;
    this.frac = { x: 0, y: 0 };
    if (!this.locked) {
      // 用户按了 Esc（或者浏览器收回了锁定）：自动锁定到下一次点击之前不再来；游戏模式等用户点画面
      if (!this.unlocking && this._mode === "auto" && this.remoteHidden) this.userUnlocked = true;
      this.unlocking = false;
    }
    this.onModeChange?.();
  }

  /** 相对移动：鼠标移动了多少 */
  private moveBy(e: PointerEvent) {
    // 锁定时拿到的是原始计数；否则是 CSS 像素，换算成设备像素（约等于被控端的像素；按画面所在的窗口算）
    const dpr = this.doc?.defaultView?.devicePixelRatio ?? (typeof devicePixelRatio === "number" ? devicePixelRatio : 1);
    const scale = this.locked && this.raw ? 1 : dpr || 1;
    this.frac.x += e.movementX * scale;
    this.frac.y += e.movementY * scale;
    const dx = Math.trunc(this.frac.x), dy = Math.trunc(this.frac.y);
    if (!dx && !dy) return;
    this.frac.x -= dx; this.frac.y -= dy;
    this.enqueue([{ type: "mouseDelta", dx, dy }], false);
  }

  // ───────────────────────── 事件 ─────────────────────────

  /** 绝对坐标：在第几路画面上的哪里（第 0 路不写 stream） */
  private pos(e: MouseEvent): { x: number; y: number; stream?: number } {
    if (this.locate) {
      const { x, y, stream } = this.locate(e.clientX, e.clientY);
      return stream ? { x, y, stream } : { x, y };
    }
    const r = this.canvas.getBoundingClientRect();
    return { x: Math.min(1, Math.max(0, (e.clientX - r.left) / Math.max(1, r.width))),
      y: Math.min(1, Math.max(0, (e.clientY - r.top) / Math.max(1, r.height))) };
  }

  /** 按着的鼠标键变了：发按下 / 松开（同时按几个键时，后按的键也只在 pointermove 里出现） */
  private buttonEvents(now: number): InputEvent[] {
    const out: InputEvent[] = [];
    const changed = now ^ this.buttons;
    for (const [bit, button] of BUTTON_BITS) {
      if (changed & bit) out.push({ type: "mouseButton", button, down: (now & bit) !== 0 });
    }
    this.buttons = now;
    return out;
  }

  /**
   * 这个指针事件之后按着哪些键（PointerEvent.buttons 的位）。`move`：是 pointermove。
   *
   * 锁定时单纯的移动（button 是 -1 的 pointermove）不改按键状态：Windows 上的 Chromium / WebView2 按
   * unadjustedMovement 锁定之后，移动改从 Raw Input（WM_INPUT）拿，而 Raw Input 只有按下 / 松开的变化、
   * 没有"现在按着哪些键"，这些 pointermove 的 buttons 是 0（新版 Chromium 自己记了按键状态，可锁定之前就按着的键
   * 它还是不知道）。游戏里按着右键转视角正好是先按下、光标隐藏了才锁定：把这个 0 当成松开，就会给对方发右键松开，
   * 游戏退出转视角、光标出现、解除锁定，下一次移动又按下、又锁定……一秒钟好几次，每次都提示"按 Esc 显示光标"，
   * 视角走走停停。按下 / 松开（pointerdown / pointerup，同时按几个键时 button 不是 -1 的 pointermove）
   * 来自普通的鼠标消息，它们的 buttons 是对的
   */
  private buttonsAfter(e: PointerEvent, move: boolean): number {
    if (move && this.locked && !(e.button >= 0)) return this.buttons;
    return e.buttons;
  }

  /** 一个指针事件：位置（绝对坐标时）+ 按键变化。`move`：是 pointermove */
  private pointer(e: PointerEvent, urgent: boolean, move = false) {
    // 游戏模式没锁定：什么也不发（不发绝对坐标，对方的光标留在原处）；点画面重新锁定的那一下也不发
    if (this.gamePaused || this.swallowing) return;
    const buttons = this.buttonEvents(this.buttonsAfter(e, move));
    if (this.relative) {
      this.moveBy(e);
      if (this._mode === "auto" && !this.locked && this.remoteHidden && !this.userUnlocked && move) {
        // 光标隐藏着一直在移动（第一人称游戏）：锁定，指针才不会碰到屏幕边就停下。刚被拒绝过就等一会儿再试
        const now = performance.now();
        if (this.hiddenMoveSince === null) this.hiddenMoveSince = now;
        else if (now - this.hiddenMoveSince >= AUTO_LOCK_AFTER_MS && now >= this.lockRetryAt) this.lock();
      }
      if (buttons.length) this.enqueue(buttons, true);
    } else {
      // 绝对坐标：先移到按下 / 松开的位置
      this.enqueue([{ type: "mouseMove", ...this.pos(e) }, ...buttons], urgent || buttons.length > 0);
    }
    // 松开最后一个键（照上面的方式发出去之后）：按着键期间光标出现了的，现在解除
    if (buttons.length) this.settleRemoteCursor();
  }

  private bind() {
    const c = this.canvas;
    const signal = this.abort.signal;
    const on = <K extends keyof HTMLElementEventMap>(name: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) =>
      c.addEventListener(name, fn as EventListener, { ...opts, signal });
    const doc = this.doc;
    if (doc) {
      doc.addEventListener("pointerlockchange", () => this.onLockChange(), { signal });
      doc.addEventListener("pointerlockerror", () => {
        this.lockPending = false;
        this.lockRetryAt = performance.now() + LOCK_RETRY_MS;
        this.onModeChange?.();
      }, { signal });
    }
    on("pointermove", e => { if (this._interactive) this.pointer(e, false, true); });
    on("pointerdown", e => {
      if (!this._interactive) return;
      e.preventDefault(); this.focus();
      // 游戏模式没锁定：这一下只是锁定，从按下到松开都不发给对方（游戏里点一下可能就是开枪、选中东西）
      if (this.gamePaused) {
        this.swallowing = true;
        this.lock();
        return;
      }
      // 锁定时不能再捕获指针（会抛异常），也用不着
      if (!this.locked) try { c.setPointerCapture(e.pointerId); } catch { /* 已经锁定 */ }
      // 点一下：Esc 之后暂停的自动锁定恢复；被控端光标隐藏着就锁定（这次按下就是用户操作）
      if (this.userUnlocked) { this.userUnlocked = false; this.onModeChange?.(); }
      if (this._mode === "auto" && this.remoteHidden) this.lock();
      this.pointer(e, true);
    });
    on("pointerup", e => {
      if (!this._interactive) return;
      e.preventDefault();
      // 重新锁定的那一下松开了（按下时一起按着的键都松开）：之后的照常发
      if (this.swallowing) {
        if (e.buttons === 0) this.swallowing = false;
        return;
      }
      this.pointer(e, true);
    });
    on("pointercancel", () => this.releaseAll());
    on("contextmenu", e => e.preventDefault());
    on("auxclick", e => e.preventDefault());
    on("wheel", e => {
      if (!this._interactive) return;
      e.preventDefault();
      if (this.gamePaused) return;
      const unit = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 360 : 1.2;
      this.wheel.x += e.deltaX * unit; this.wheel.y -= e.deltaY * unit;
      const dx = Math.trunc(this.wheel.x), dy = Math.trunc(this.wheel.y);
      if (!dx && !dy) return;
      this.wheel.x -= dx; this.wheel.y -= dy;
      const clamp = (n: number) => Math.max(-32000, Math.min(32000, n));
      this.enqueue([{ type: "wheel", dx: clamp(dx), dy: clamp(dy) }], true);
    }, { passive: false });
    on("paste", e => { if (this._interactive) this.onPaste?.(e); });
    on("keydown", e => {
      if (!this._interactive) return;
      // 粘贴键：不拦住浏览器，它接着触发 paste 事件（见 onPaste）；这个键放到它后面发
      const paste = this.onPaste !== null && !e.repeat && isPasteKey(e);
      if (!paste) e.preventDefault();
      e.stopPropagation();
      const code = physicalCode(e);
      if (!code) return;
      if (!this.pressed.size) this.swapForChord = this.shouldSwapCtrlCommand?.() ?? false;
      if (paste) {
        this.pressed.add(code);
        setTimeout(() => this.enqueue([{ type: "key", code, down: true }], true), 0);
        return;
      }
      if (isLockShortcut(e)) {
        if (!e.repeat) this.toggleGame();
        this.swallowed.add(code);
        return;
      }
      // 锁定时的 Esc（全屏时浏览器不再用它解除锁定）：照常发给对方，按住够久就解除，松开之前不发自动重复
      if (code === "Escape" && (this.escHeld || this.locked)) {
        if (e.repeat) return;
        this.holdEsc();
      }
      if (IS_MAC && code === "CapsLock") return this.tapCaps();
      if (e.repeat && !this.pressed.has(code)) return;
      this.pressed.add(code);
      this.enqueue([{ type: "key", code: this.outgoingCode(code), down: true }], true);
    });
    on("keyup", e => {
      if (!this._interactive) return;
      e.preventDefault(); e.stopPropagation();
      const code = physicalCode(e);
      if (!code) return;
      if (this.swallowed.delete(code)) return;
      if (code === "Escape") this.cancelEscHold();
      if (IS_MAC && code === "CapsLock") return this.tapCaps();
      this.pressed.delete(code);
      const events: InputEvent[] = [];
      if (IS_MAC && META_KEYS.has(code)) {
        for (const held of this.pressed) {
          if (MODIFIERS.has(held)) continue;
          this.pressed.delete(held);
          events.push({ type: "key", code: held, down: false });
        }
      }
      events.push({ type: "key", code: this.outgoingCode(code), down: false });
      this.enqueue(events, true);
    });
    on("blur", () => { this.cancelEscHold(); this.releaseAll(); });
  }

  private tapCaps() {
    this.enqueue([{ type: "key", code: "CapsLock", down: true }, { type: "key", code: "CapsLock", down: false }], true);
  }
}
