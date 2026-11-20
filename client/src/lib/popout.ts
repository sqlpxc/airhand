// 弹出窗口：把对方的一个显示器放到单独的窗口里。本机也有好几个显示器时，一个窗口放到一个显示器上、各自全屏。
//
// 弹出窗口是主窗口用 window.open 打开的同源空白页（about:blank），里面的东西全由主窗口建、主窗口管：
// * 画面的 canvas 照样交给主窗口的 Worker 绘制（console-worker.ts）：解码、连接都还是那一份，不用另外连一次对方。
//   哪一路画面放在哪个窗口里由会话决定（session.ts 的 placeCanvases）；
// * 鼠标键盘由主窗口挂在这个窗口的画面上（会话给每个弹出窗口一个 RemoteInput，见 input.ts）；
// * 工具栏、提示由主窗口的 React 用 portal 渲染进来（PopoutView.tsx），样式表从主窗口复制过来。
//
// 桌面版：Tauri 默认不让网页开新窗口，控制台窗口把 window.open 建成一个普通的 Tauri 窗口（src-tauri/src/popout.rs），
// 放到控制台不在的那个显示器上。它的 label 由这里定、写在网址的 # 后面：全屏、关闭都按它找到窗口（api.popouts）。
//
// 全屏和主窗口一样（actions.ts 的 applyFullscreen）：网页全屏 + 键盘锁定（Win、Alt+Tab 等发给对方）+ 窗口全屏；
// macOS 桌面版只有窗口全屏（弹出窗口用的是系统全屏）。
import { api } from "./api";
import { OS } from "./platform";

/** 等弹出窗口的空白页准备好最多这么久 */
const READY_TIMEOUT_MS = 5000;
const READY_POLL_MS = 20;
/** 隔这么久看一下窗口是不是被关掉了（关掉 Tauri 窗口时网页不一定收得到 pagehide） */
const CLOSED_CHECK_MS = 500;
/** 窗口大小停下来这么久再问它是不是还全屏（macOS 进出系统全屏有动画） */
const FULLSCREEN_CHECK_MS = 250;
/** 自己刚进 / 出全屏这么久之内不按窗口大小的变化改全屏的状态（窗口还在变） */
const FULLSCREEN_SETTLE_MS = 1500;

// macOS 桌面版只用窗口全屏（见 actions.ts 的 WEB_FULLSCREEN）
const WEB_FULLSCREEN = !(api.kind === "desktop" && OS === "mac");

type KeyboardLock = { lock?: (codes?: string[]) => Promise<void>; unlock?: () => void };

let seq = 0;
/** 开着的弹出窗口：主窗口关掉、刷新时一起关（网页版；桌面版控制台关掉时 Rust 那边也会关） */
const opened = new Set<Popout>();
if (typeof window !== "undefined") {
  window.addEventListener("pagehide", () => {
    for (const p of [...opened]) p.close();
  });
}

export class Popout {
  /** 窗口的名字（window.open 的 target）；桌面版也是 Tauri 窗口的 label */
  readonly label: string;
  readonly win: Window;
  /** 下面这些在准备好（onReady）之后才有 */
  doc!: Document;
  /** 铺满窗口：画面 + 工具栏。全屏时加上 immersive */
  root!: HTMLDivElement;
  /** 画面的显示区域 */
  host!: HTMLDivElement;
  /** 包着画面的 canvas：键盘焦点、鼠标事件、锁定指针都在它上面 */
  screen!: HTMLDivElement;
  /** 工具栏、提示：React 用 portal 渲染到这里 */
  ui!: HTMLDivElement;

  ready = false;
  closed = false;
  fullscreen = false;

  /** 准备好了：可以往里面放 canvas 了 */
  onReady: (() => void) | null = null;
  /** 窗口关掉了（用户关的，或者 close） */
  onClosed: (() => void) | null = null;
  /** 画面的显示区域变了（窗口大小、全屏、所在显示器的缩放） */
  onResize: (() => void) | null = null;
  /** 全屏变了（界面刷新） */
  onChange: (() => void) | null = null;

  private readonly abort = new AbortController();
  private closedTimer: ReturnType<typeof setInterval> | undefined;
  private fsTimer: ReturnType<typeof setTimeout> | undefined;
  /** 最近一次自己进 / 出全屏的时刻 */
  private fsAt = -Infinity;
  /** 最近一次量到的显示区域（窗口最小化时量不到） */
  private lastViewport: { w: number; h: number } | null = null;

  private constructor(label: string, win: Window, title: string) {
    this.label = label;
    this.win = win;
    opened.add(this);
    this.closedTimer = setInterval(() => this.checkClosed(), CLOSED_CHECK_MS);
    this.waitReady(title);
  }

  /**
   * 打开一个弹出窗口（要在用户操作里调用：浏览器只在点击这类操作里让开新窗口）。打不开（被拦住了）时为 null。
   * 返回时还没准备好，准备好了调用 onReady
   */
  static open(title: string): Popout | null {
    const label = `popout-${Date.now().toString(36)}-${(seq++).toString(36)}`;
    // 桌面版：位置大小由 Rust 定（放到控制台不在的那个显示器上）；网页版：和主窗口差不多大
    const features =
      api.kind === "desktop"
        ? "popup"
        : `popup,width=${Math.round(window.outerWidth * 0.8)},height=${Math.round(window.outerHeight * 0.8)}`;
    let win: Window | null = null;
    try {
      win = window.open(`about:blank#${label}`, label, features);
    } catch (e) {
      console.warn("can't open a popout window", e);
    }
    return win ? new Popout(label, win, title) : null;
  }

  /** 等新窗口的空白页加载完（WebView 新建的窗口可能还要再导航一次到 about:blank#label，在那之前放进去的东西会被冲掉） */
  private waitReady(title: string) {
    const start = performance.now();
    const tick = () => {
      if (this.closed) return;
      let ok = false;
      try {
        ok = this.win.location.href.endsWith(`#${this.label}`) && this.win.document.readyState === "complete";
      } catch {
        // 还没好
      }
      if (ok) return this.build(title);
      if (performance.now() - start > READY_TIMEOUT_MS) {
        console.warn("the popout window didn't load");
        this.close();
        return;
      }
      setTimeout(tick, READY_POLL_MS);
    };
    // 第一次也不在这里马上看：空白页可能已经好了，调用的地方还没设好 onReady
    setTimeout(tick, 0);
  }

  private build(title: string) {
    const win = this.win;
    const doc = win.document;
    this.doc = doc;
    doc.title = title;
    const html = doc.documentElement;
    html.lang = document.documentElement.lang;
    // 系统的 class（样式里有区分）；主窗口别的状态（最大化等）和这里无关
    for (const c of document.documentElement.classList) if (c.startsWith("os-")) html.classList.add(c);
    // 样式表：开发时是 <style>，打包后是 <link>（相对地址：换成绝对的，空白页的基准地址不一定是主窗口的）
    for (const el of document.head.querySelectorAll('style, link[rel="stylesheet"]')) {
      const copy = doc.importNode(el, true);
      if (el instanceof HTMLLinkElement) (copy as HTMLLinkElement).href = el.href;
      doc.head.appendChild(copy);
    }
    const div = (cls: string) => {
      const d = doc.createElement("div");
      d.className = cls;
      return d;
    };
    this.root = div("popout");
    const stage = div("stage popout-stage");
    this.host = div("canvas-host");
    this.screen = div("remote-screen");
    this.screen.tabIndex = 0;
    this.ui = div("popout-ui");
    this.host.appendChild(this.screen);
    stage.appendChild(this.host);
    this.root.append(stage, this.ui);
    doc.body.appendChild(this.root);

    const signal = this.abort.signal;
    // 显示区域变了：用这个窗口自己的 ResizeObserver（观察的是它的网页）
    const Observer = (win as unknown as typeof globalThis).ResizeObserver;
    const observer = new Observer(() => this.onResize?.());
    observer.observe(this.host);
    signal.addEventListener("abort", () => observer.disconnect());
    this.watchDpr();
    win.addEventListener("resize", () => this.scheduleFullscreenCheck(), { signal });
    // 网页全屏被浏览器退出了（例如长按 Esc）：窗口也退出
    doc.addEventListener(
      "fullscreenchange",
      () => {
        if (WEB_FULLSCREEN && this.fullscreen && !doc.fullscreenElement) void this.setFullscreen(false);
      },
      { signal },
    );
    // 和主窗口一样屏蔽 WebView 自带的刷新 / 查找 / 打印等快捷键（画面有焦点时按键都发给对方，到不了这里）
    win.addEventListener(
      "keydown",
      (e) => {
        if (e.key === "F5" || ((e.ctrlKey || e.metaKey) && ["r", "p", "f", "g", "j", "u"].includes(e.key.toLowerCase()))) {
          e.preventDefault();
        }
      },
      { signal },
    );
    win.addEventListener("contextmenu", (e) => e.preventDefault(), { signal });
    // 拖进来的文件不让浏览器打开（会把这一页换掉）
    win.addEventListener("dragover", (e) => e.preventDefault(), { signal });
    win.addEventListener("drop", (e) => e.preventDefault(), { signal });
    win.addEventListener("pagehide", () => this.checkClosed(true), { signal });

    this.ready = true;
    this.onReady?.();
  }

  /** 所在显示器的缩放变了（拖到另一个显示器上）：显示区域的设备像素跟着变，CSS 大小可能没变 */
  private watchDpr() {
    if (this.closed) return;
    const mq = this.win.matchMedia(`(resolution: ${this.win.devicePixelRatio}dppx)`);
    mq.addEventListener(
      "change",
      () => {
        this.onResize?.();
        this.watchDpr();
      },
      { once: true, signal: this.abort.signal },
    );
  }

  /** 窗口关掉了、或者网页被换掉了（放进去的东西都没了）就算关掉了。`gone`：网页正在被卸载 */
  private checkClosed(gone = false) {
    if (this.closed) return;
    let replaced = false;
    try {
      replaced = this.ready && this.win.document !== this.doc;
    } catch {
      replaced = true;
    }
    if (gone || this.win.closed || replaced) this.close();
  }

  /** 显示区域（设备像素）；量不到（例如最小化了）时是上一次的 */
  get viewport(): { w: number; h: number } | null {
    if (!this.ready || this.closed) return this.lastViewport;
    const r = this.host.getBoundingClientRect();
    const dpr = this.dpr;
    if (r.width > 0 && r.height > 0) this.lastViewport = { w: Math.floor(r.width * dpr), h: Math.floor(r.height * dpr) };
    return this.lastViewport;
  }

  /** 这个窗口的设备像素比例（所在显示器的缩放，可能和主窗口不一样） */
  get dpr(): number {
    return this.win.devicePixelRatio || 1;
  }

  setTitle(title: string) {
    if (this.ready && !this.closed && this.doc.title !== title) this.doc.title = title;
  }

  /** 切到这个窗口（桌面版由 Rust 把窗口拿到前面：网页里的 window.focus() 不一定管用） */
  focus() {
    if (this.closed) return;
    if (api.popouts) void api.popouts.focus(this.label).catch(() => this.win.focus());
    else this.win.focus();
    if (this.ready) this.screen.focus({ preventScroll: true });
  }

  /** 进入 / 退出全屏（网页全屏 + 键盘锁定 + 窗口全屏）。要进入时在这个窗口里的点击里调用 */
  async setFullscreen(on: boolean) {
    if (!this.ready || this.closed) return;
    this.fsAt = performance.now();
    this.applyFullscreenFlag(on);
    const doc = this.doc;
    const keyboard = (this.win.navigator as Navigator & { keyboard?: KeyboardLock }).keyboard;
    if (WEB_FULLSCREEN && on) {
      // 先进入网页全屏（键盘锁定的前提），再让窗口全屏
      if (!doc.fullscreenElement) await doc.documentElement.requestFullscreen?.().catch(() => {});
      await keyboard?.lock?.().catch(() => {});
    } else if (WEB_FULLSCREEN) {
      keyboard?.unlock?.();
      if (doc.fullscreenElement) await doc.exitFullscreen().catch(() => {});
    }
    if (this.closed) return;
    await api.popouts?.setFullscreen(this.label, on).catch(() => {});
    this.fsAt = performance.now();
    this.screen.focus({ preventScroll: true });
  }

  private applyFullscreenFlag(on: boolean) {
    if (this.fullscreen === on) return;
    this.fullscreen = on;
    this.root.classList.toggle("immersive", on);
    this.onChange?.();
  }

  /**
   * 窗口大小变了：可能被系统退出了全屏（macOS 的绿色按钮、菜单），或者被系统放进了全屏，跟着改。
   * 网页版只有网页全屏，看 fullscreenchange
   */
  private scheduleFullscreenCheck() {
    const frames = api.popouts;
    if (!frames) return;
    clearTimeout(this.fsTimer);
    this.fsTimer = setTimeout(() => {
      if (this.closed || performance.now() - this.fsAt < FULLSCREEN_SETTLE_MS) return;
      void frames.isFullscreen(this.label).then((on) => {
        if (this.closed || on === this.fullscreen || performance.now() - this.fsAt < FULLSCREEN_SETTLE_MS) return;
        if (on) this.applyFullscreenFlag(true);
        else void this.setFullscreen(false);
      });
    }, FULLSCREEN_CHECK_MS);
  }

  /** 关掉窗口（之后调用 onClosed）。可以重复调用 */
  close() {
    if (this.closed) return;
    this.closed = true;
    opened.delete(this);
    clearInterval(this.closedTimer);
    clearTimeout(this.fsTimer);
    this.abort.abort();
    if (!this.win.closed) {
      if (api.popouts) void api.popouts.close(this.label).catch(() => this.win.close());
      else this.win.close();
    }
    this.onClosed?.();
  }
}
