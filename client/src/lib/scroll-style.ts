// 滚屏方式：原始大小（1:1）显示、画面比窗口大时，画面怎么跟着指针滚动（ScrollStyle，用户在显示方式菜单里选，这台电脑记住，所有会话通用）：
// * auto 自动滚动（默认）：指针在窗口里的位置对应画面上的位置。指针到窗口最左边看到画面最左边，到最右边看到最右边，一路跟着移；
// * edge 边缘滚动：指针进到窗口边上的一圈（EDGE_PX）才往那边移，到画面的边为止。
//   指针从窗口外面进来（例如从工具栏往下移）时先不算：要先进过里面一次，免得一进来就移。两种移法：
//   - 推（桌面版）：进去多深，画面就马上移多深，系统指针同时被推回那一圈的里沿。指针下面还是画面上的同一个地方，
//     像是推着画面走；窗口贴着屏幕边（最大化、全屏）时指针也有地方继续推；
//   - 持续移动（网页版不能移动系统指针；桌面版移不了时，例如 Linux 的 Wayland，第一次失败后一直这样）：
//     越靠外越快，停在边上不动也一直移；
// * bar 滚动条：只用滚动条，不跟随指针。
// 自动滚动、边缘滚动时不显示滚动条：位置由指针决定，拖滚动条会和它打架（styles.css 的 .canvas-host.auto-scroll）。
// 指针锁定、发相对移动时，本机指针在哪里没有意义，不跟随。
// 画面在没动的指针下面移动了（持续移动），指针指着的地方也变了，要按原来的指针位置再发一次（RemoteInput.repoint）。
// 做法参考 RustDesk 的 ScrollAuto / ScrollEdge。

export type ScrollStyle = "auto" | "bar" | "edge";
export const SCROLL_STYLES: ScrollStyle[] = ["auto", "bar", "edge"];

/** 边缘模式的那一圈有多宽（CSS 像素）；窗口太小时最多一半 */
export const EDGE_PX = 64;
/** 边缘模式的速度：每秒移动「进到边缘里的深度 × 这个值」CSS 像素（RustDesk：每帧 0.1 × 深度，按 60 帧算） */
const EDGE_SPEED = 6;

/** 自动滚动：指针在显示区域（长 `view`）的 `p` 处时，长 `content` 的画面滚到哪里 */
export function followScroll(p: number, view: number, content: number): number {
  const max = content - view;
  if (max <= 0 || view <= 0) return 0;
  return max * Math.min(1, Math.max(0, p / view));
}

/** 边缘：指针在显示区域（长 `view`）的 `p` 处时进到边缘里多深。负数往前（左、上）移，正数往后，0 不在边上 */
export function edgeDepth(p: number, view: number, edge = EDGE_PX): number {
  const e = Math.min(edge, view / 2);
  const q = Math.min(view, Math.max(0, p));
  if (q < e) return q - e;
  if (q > view - e) return q - (view - e);
  return 0;
}

/**
 * 一个可滚动的显示区域（原始大小显示时的 .canvas-host）上的滚屏方式。`mode`：现在怎么跟随，不跟随时返回 "bar"
 * （没有原始大小显示、指针锁定了……）；`moved`：画面在指针下面移动了
 */
export class ViewScroll {
  private host: HTMLElement | null = null;
  private abort: AbortController | null = null;
  /** 边缘模式：指针进过里面了（从外面进来时还没有） */
  private armed = false;
  /** 边缘模式：现在进到边缘里多深（CSS 像素），每一帧按它移 */
  private depth = { x: 0, y: 0 };
  /** 边缘模式要移到的位置（不取整：慢的时候每帧不到一个像素，也要攒着） */
  private pos = { x: 0, y: 0 };
  private frame = 0;
  private lastTick = 0;
  /** 边缘模式推着画面走时移动系统指针（见文件开头）；移不了时为 null */
  private warp: ((x: number, y: number) => Promise<void>) | null;
  /** 正在移动系统指针：等它到了再推下一下，不然一次推了两次 */
  private warping = false;
  private readonly mode: () => ScrollStyle;
  private readonly moved: () => void;

  /** `mode`：现在怎么跟随，不跟随时返回 "bar"；`moved`：画面在没动的指针下面移动了；`warp`：移动系统指针（只有桌面版） */
  constructor(mode: () => ScrollStyle, moved: () => void, warp?: (x: number, y: number) => Promise<void>) {
    this.mode = mode;
    this.moved = moved;
    this.warp = warp ?? null;
  }

  attach(host: HTMLElement) {
    this.detach();
    this.host = host;
    this.abort = new AbortController();
    const opts = { signal: this.abort.signal, capture: true };
    // 捕获阶段：先移好画面，画面上的 RemoteInput 再按移动之后的位置算指针在画面的哪里，不用再发一次
    host.addEventListener("pointermove", (e) => this.onMove(e), opts);
    host.addEventListener("pointerdown", (e) => this.onMove(e), opts);
    host.addEventListener("pointerleave", () => this.leave(), opts);
  }

  detach() {
    this.leave();
    this.abort?.abort();
    this.abort = null;
    this.host = null;
  }

  private onMove(e: PointerEvent) {
    const host = this.host;
    if (!host) return;
    const mode = this.mode();
    if (mode === "bar") return this.leave();
    const r = host.getBoundingClientRect();
    const x = e.clientX - r.left;
    const y = e.clientY - r.top;
    const w = host.clientWidth;
    const h = host.clientHeight;
    if (mode === "auto") {
      this.stop();
      this.scrollTo(followScroll(x, w, host.scrollWidth), followScroll(y, h, host.scrollHeight));
      return;
    }
    const depth = { x: edgeDepth(x, w), y: edgeDepth(y, h) };
    if (!this.armed) {
      if (depth.x || depth.y) return;
      this.armed = true;
    }
    if (this.warp) return this.push(e, depth);
    this.depth = depth;
    if (depth.x || depth.y) this.start();
    else this.stop();
  }

  /** 推着画面走：画面移 `depth`（到画面的边为止），系统指针退回同样多 */
  private push(e: PointerEvent, depth: { x: number; y: number }) {
    const host = this.host!;
    const warp = this.warp!;
    if (this.warping) return;
    const before = { x: host.scrollLeft, y: host.scrollTop };
    if (!this.scrollTo(before.x + depth.x, before.y + depth.y)) return;
    const dx = host.scrollLeft - before.x;
    const dy = host.scrollTop - before.y;
    // 这一下不发给对方：画面移了、指针还没退回来，指着的地方差了 depth。指针退回来之后的那次移动再发
    // （按着键时照常发：可能带着按键的变化）
    if (e.type === "pointermove" && e.buttons === 0) e.stopPropagation();
    this.warping = true;
    // 多退一个像素：换算成设备像素取整之后也落在那一圈外面
    warp(e.clientX - dx - Math.sign(dx), e.clientY - dy - Math.sign(dy)).then(
      () => { this.warping = false; },
      () => {
        // 移不了：以后都用持续移动；这一下已经移了画面，指着的地方变了
        this.warping = false;
        this.warp = null;
        this.moved();
      },
    );
  }

  /** 指针离开了显示区域：边缘模式要等指针再进到里面 */
  private leave() {
    this.armed = false;
    this.stop();
  }

  private start() {
    const host = this.host;
    const view = host?.ownerDocument.defaultView;
    if (this.frame || !host || !view) return;
    this.pos = { x: host.scrollLeft, y: host.scrollTop };
    this.lastTick = performance.now();
    const tick = (now: number) => {
      this.frame = 0;
      if (this.host !== host || this.mode() !== "edge") return;
      const k = (EDGE_SPEED * Math.min(100, now - this.lastTick)) / 1000;
      this.lastTick = now;
      if (this.scrollTo(this.pos.x + this.depth.x * k, this.pos.y + this.depth.y * k)) this.moved();
      // 往哪边都到了画面的边：停下，等指针再动
      const maxX = host.scrollWidth - host.clientWidth;
      const maxY = host.scrollHeight - host.clientHeight;
      const stuck = (d: number, p: number, max: number) => d === 0 || (d < 0 ? p <= 0 : p >= max);
      if (stuck(this.depth.x, this.pos.x, maxX) && stuck(this.depth.y, this.pos.y, maxY)) return;
      this.frame = view.requestAnimationFrame(tick);
    };
    this.frame = view.requestAnimationFrame(tick);
  }

  private stop() {
    if (!this.frame) return;
    this.host?.ownerDocument.defaultView?.cancelAnimationFrame(this.frame);
    this.frame = 0;
  }

  /** 滚到 (x, y)（CSS 像素），对齐到设备像素：差半个像素文字就糊了。返回真的移动了没有 */
  private scrollTo(x: number, y: number): boolean {
    const host = this.host!;
    const dpr = host.ownerDocument.defaultView?.devicePixelRatio || 1;
    const maxX = Math.max(0, host.scrollWidth - host.clientWidth);
    const maxY = Math.max(0, host.scrollHeight - host.clientHeight);
    this.pos = { x: Math.min(maxX, Math.max(0, x)), y: Math.min(maxY, Math.max(0, y)) };
    const left = Math.round(this.pos.x * dpr) / dpr;
    const top = Math.round(this.pos.y * dpr) / dpr;
    if (Math.abs(left - host.scrollLeft) < 0.01 && Math.abs(top - host.scrollTop) < 0.01) return false;
    host.scrollLeft = left;
    host.scrollTop = top;
    return true;
  }
}
