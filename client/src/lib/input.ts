// 输入：把本机的鼠标、键盘变成发给被控端的输入事件，合并后成批发送。
// 断开 / 换了会话之后，还没发出去的输入全部丢掉：延迟的定时器绝不能把旧输入发给新的会话。
//
// 鼠标有两种方式（见 airhand_proto::session 的模块说明）：
// * 绝对坐标（平时）：本机指针在画面的哪里，被控端的光标就在哪里；
// * 相对移动（被控端的光标隐藏了，例如游戏里按住右键转视角、第一人称游戏；或者用户手动锁定了鼠标）：
//   发移动了多少（mouseDelta）。这时尽量锁定本机指针（Pointer Lock）：指针不会碰到屏幕边就停下，
//   移动量也是鼠标的原始计数（unadjustedMovement）。锁定不了（浏览器不支持、被拒绝）也照样发相对移动。
//
// 什么时候锁定：
// * 被控端光标隐藏时按下鼠标键，或者按着键时光标隐藏了（游戏按住右键转视角）：马上锁定；
// * 被控端光标隐藏时只是移动（第一人称游戏）：持续移动一小会儿光标还是隐藏的才锁定。
//   Windows 打字时会暂时隐藏光标，鼠标一动就重新出现，这种情况不锁定；
// * 被控端光标重新出现：自动锁定的解除锁定，本机指针回到锁定时的位置（游戏也会把光标放回原处）；
// * 用户按 Esc 解除了锁定：到下一次点击之前不再自动锁定，指针照常显示；
// * 手动锁定（Ctrl+Alt+Shift+M 或工具栏）：不管被控端光标，直到再切换一次或者按 Esc。
//
// 怎么解除：
// * 浏览器自己的 Esc：只在 Esc 没被键盘锁定时有用。全屏时键盘整个被锁定（见 actions.ts），
//   Esc 交给网页、发给对方（游戏要用），浏览器不再用它解除锁定，而这时指针也碰不到顶端、叫不出工具栏；
// * 所以锁定时长按 Esc（ESC_HOLD_MS）由这里自己解除：短按照常马上发给对方，按住期间不发自动重复；
// * Ctrl+Alt+Shift+M：正在发相对移动（手动或自动锁定）就解除，否则手动锁定。
// 这两种解除和浏览器的 Esc 一样：手动锁定结束，自动锁定到下一次点击之前不再来。
import type { InputEvent, MouseButton } from "./api";

const IS_MAC = typeof navigator !== "undefined" && /Mac/.test(navigator.platform || navigator.userAgent);
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

/** 锁定 / 解除鼠标的快捷键：Ctrl+Alt+Shift+M */
export function isLockShortcut(e: KeyboardEvent): boolean {
  return e.code === "KeyM" && e.ctrlKey && e.altKey && e.shiftKey && !e.metaKey;
}

type Lockable = HTMLElement & { requestPointerLock(options?: { unadjustedMovement?: boolean }): Promise<void> | void };

export class RemoteInput {
  private canvas: HTMLCanvasElement;
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
  /** 按着的鼠标键（PointerEvent.buttons 的位） */
  private buttons = 0;
  /** 拦下来的快捷键：抬起时也不发 */
  private swallowed = new Set<string>();
  private abort = new AbortController();

  // ── 相对移动 ──
  /** 被控端的光标隐藏了 */
  private remoteHidden = false;
  /** 用户手动锁定了鼠标 */
  private manual = false;
  /** 请求了锁定，还没有结果 */
  private lockPending = false;
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

  /** 鼠标的方式变了（相对移动 / 锁定），界面据此刷新指针和工具栏 */
  onModeChange: (() => void) | null = null;

  constructor(canvas: HTMLCanvasElement, connected: () => boolean, send: (events: InputEvent[]) => void) {
    this.canvas = canvas; this.connected = connected; this.send = send;
    this.bind();
  }

  get interactive() { return this._interactive; }
  set interactive(v: boolean) {
    this._interactive = v;
    if (!v) {
      this.manual = false;
      this.cancelEscHold();
      this.unlock();
    }
  }

  /** 本机指针锁定在画面上了 */
  get locked(): boolean {
    return typeof document !== "undefined" && document.pointerLockElement === this.canvas;
  }

  /** 鼠标现在发相对移动 */
  get relative(): boolean {
    return this.locked || this.manual || (this.remoteHidden && !this.userUnlocked);
  }

  /** 用户手动锁定了鼠标（工具栏显示为按下） */
  get manualLock(): boolean {
    return this.manual;
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
    this.hiddenMoveSince = null;
    this.userUnlocked = false;
    this.cancelEscHold();
    this.unlock();
  }
  dispose() { this.manual = false; this.reset(); this.abort.abort(); }
  releaseAll() { this.pressed.clear(); this.buttons = 0; this.enqueue([{ type: "releaseAll" }], true); }
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
    if (this.remoteHidden === hidden) return;
    this.remoteHidden = hidden;
    this.hiddenMoveSince = null;
    if (hidden) {
      // 按着键时光标隐藏了（游戏按住右键转视角）：马上锁定，按下鼠标键的那一下还算用户操作
      if (this._interactive && this.buttons !== 0 && !this.userUnlocked) this.lock();
    } else {
      this.userUnlocked = false;
      if (!this.manual) this.unlock();
    }
    this.frac = { x: 0, y: 0 };
    this.onModeChange?.();
  }

  /** 手动锁定 / 解锁鼠标（需要在用户操作里调用：点击、按键） */
  toggleLock() {
    this.manual = !this.manual;
    if (this.manual) {
      this.userUnlocked = false;
      this.lock();
    } else if (!this.remoteHidden) {
      this.unlock();
    }
    this.onModeChange?.();
  }

  /** 用户解除锁定（长按 Esc、快捷键）：和浏览器的 Esc 一样，手动锁定结束，自动锁定到下一次点击之前不再来 */
  release() {
    this.manual = false;
    if (this.remoteHidden) this.userUnlocked = true;
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
      // 锁定不了（例如没有用户操作、浏览器不允许）：照样发相对移动，只是指针会停在屏幕边上
      .catch(() => {})
      .finally(() => { this.lockPending = false; });
  }

  private unlock() {
    if (!this.locked) return;
    this.unlocking = true;
    document.exitPointerLock();
  }

  private onLockChange() {
    this.lockPending = false;
    this.frac = { x: 0, y: 0 };
    if (!this.locked) {
      if (!this.unlocking) {
        // 用户按了 Esc（或者浏览器收回了锁定）：手动锁定就此结束；自动锁定到下一次点击之前不再来
        if (this.manual) this.manual = false;
        else if (this.remoteHidden) this.userUnlocked = true;
      }
      this.unlocking = false;
    }
    this.onModeChange?.();
  }

  /** 相对移动：鼠标移动了多少 */
  private moveBy(e: PointerEvent) {
    // 锁定时拿到的是原始计数；否则是 CSS 像素，换算成设备像素（约等于被控端的像素）
    const scale = this.locked && this.raw ? 1 : (typeof devicePixelRatio === "number" ? devicePixelRatio : 1) || 1;
    this.frac.x += e.movementX * scale;
    this.frac.y += e.movementY * scale;
    const dx = Math.trunc(this.frac.x), dy = Math.trunc(this.frac.y);
    if (!dx && !dy) return;
    this.frac.x -= dx; this.frac.y -= dy;
    this.enqueue([{ type: "mouseDelta", dx, dy }], false);
  }

  // ───────────────────────── 事件 ─────────────────────────

  private pos(e: MouseEvent) {
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

  /** 一个指针事件：位置（绝对坐标时）+ 按键变化 */
  private pointer(e: PointerEvent, urgent: boolean) {
    const buttons = this.buttonEvents(e.buttons);
    if (this.relative) {
      this.moveBy(e);
      if (!this.locked && this.remoteHidden && !this.userUnlocked && e.type === "pointermove") {
        // 光标隐藏着一直在移动（第一人称游戏）：锁定，指针才不会碰到屏幕边就停下
        const now = performance.now();
        if (this.hiddenMoveSince === null) this.hiddenMoveSince = now;
        else if (now - this.hiddenMoveSince >= AUTO_LOCK_AFTER_MS) this.lock();
      }
      if (buttons.length) this.enqueue(buttons, true);
      return;
    }
    // 绝对坐标：先移到按下 / 松开的位置
    this.enqueue([{ type: "mouseMove", ...this.pos(e) }, ...buttons], urgent || buttons.length > 0);
  }

  private bind() {
    const c = this.canvas;
    const signal = this.abort.signal;
    const on = <K extends keyof HTMLElementEventMap>(name: K, fn: (e: HTMLElementEventMap[K]) => void, opts?: AddEventListenerOptions) =>
      c.addEventListener(name, fn as EventListener, { ...opts, signal });
    if (typeof document !== "undefined") {
      document.addEventListener("pointerlockchange", () => this.onLockChange(), { signal });
      document.addEventListener("pointerlockerror", () => {
        this.lockPending = false;
        if (this.manual) { this.manual = false; this.onModeChange?.(); }
      }, { signal });
    }
    on("pointermove", e => { if (this._interactive) this.pointer(e, false); });
    on("pointerdown", e => {
      if (!this._interactive) return;
      e.preventDefault(); this.focus();
      // 锁定时不能再捕获指针（会抛异常），也用不着
      if (!this.locked) try { c.setPointerCapture(e.pointerId); } catch { /* 已经锁定 */ }
      // 点一下：Esc 之后暂停的自动锁定恢复；被控端光标隐藏着就锁定（这次按下就是用户操作）
      if (this.userUnlocked) { this.userUnlocked = false; this.onModeChange?.(); }
      if (this.remoteHidden || this.manual) this.lock();
      this.pointer(e, true);
    });
    on("pointerup", e => {
      if (!this._interactive) return;
      e.preventDefault();
      this.pointer(e, true);
    });
    on("pointercancel", () => this.releaseAll());
    on("contextmenu", e => e.preventDefault());
    on("auxclick", e => e.preventDefault());
    on("wheel", e => {
      if (!this._interactive) return;
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 360 : 1.2;
      this.wheel.x += e.deltaX * unit; this.wheel.y -= e.deltaY * unit;
      const dx = Math.trunc(this.wheel.x), dy = Math.trunc(this.wheel.y);
      if (!dx && !dy) return;
      this.wheel.x -= dx; this.wheel.y -= dy;
      const clamp = (n: number) => Math.max(-32000, Math.min(32000, n));
      this.enqueue([{ type: "wheel", dx: clamp(dx), dy: clamp(dy) }], true);
    }, { passive: false });
    on("keydown", e => {
      if (!this._interactive) return;
      e.preventDefault(); e.stopPropagation();
      if (!e.code || e.code === "Unidentified") return;
      if (isLockShortcut(e)) {
        if (!e.repeat) {
          if (this.relative) this.release();
          else this.toggleLock();
        }
        this.swallowed.add(e.code);
        return;
      }
      // 锁定时的 Esc（全屏时浏览器不再用它解除锁定）：照常发给对方，按住够久就解除，松开之前不发自动重复
      if (e.code === "Escape" && (this.escHeld || this.locked)) {
        if (e.repeat) return;
        this.holdEsc();
      }
      if (IS_MAC && e.code === "CapsLock") return this.tapCaps();
      if (e.repeat && !this.pressed.has(e.code)) return;
      this.pressed.add(e.code);
      this.enqueue([{ type: "key", code: e.code, down: true }], true);
    });
    on("keyup", e => {
      if (!this._interactive) return;
      e.preventDefault(); e.stopPropagation();
      if (!e.code || e.code === "Unidentified") return;
      if (this.swallowed.delete(e.code)) return;
      if (e.code === "Escape") this.cancelEscHold();
      if (IS_MAC && e.code === "CapsLock") return this.tapCaps();
      this.pressed.delete(e.code);
      const events: InputEvent[] = [];
      if (IS_MAC && META_KEYS.has(e.code)) {
        for (const code of this.pressed) {
          if (MODIFIERS.has(code)) continue;
          this.pressed.delete(code);
          events.push({ type: "key", code, down: false });
        }
      }
      events.push({ type: "key", code: e.code, down: false });
      this.enqueue(events, true);
    });
    on("blur", () => { this.cancelEscHold(); this.releaseAll(); });
  }

  private tapCaps() {
    this.enqueue([{ type: "key", code: "CapsLock", down: true }, { type: "key", code: "CapsLock", down: false }], true);
  }
}
