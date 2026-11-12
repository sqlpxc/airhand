// 远程声音的抖动缓冲（运行在 AudioWorklet 里；纯逻辑、不依赖浏览器 API，便于单独测试）。
//
// * 开始播放前先攒够"目标延迟"的数据；
// * 每个包到达时量一下它比"最快的那些包"晚了多少（到达时刻 − 采集时刻，相对最小值），目标延迟马上加大到
//   能盖住它：包晚到不一定来自网络，WebKit（macOS / Linux 的 WebView）里 Worker 的 WebSocket 经页面主线程转交，
//   主线程忙一下声音就整批晚到。最近约一分钟里最大的晚到量也是目标延迟回落的下限，偶尔卡一下的不会一回落就又断粮；
// * 播放中途断粮：如果接下来的包不是"静音后第一包"，说明是网络抖动（或丢包），目标延迟再加大；
//   如果是（对方只是没声音了），则不加大；
// * 网络稳定一段时间、缓冲一直有余量时，目标延迟逐步回落；
// * 用 -1%~+2% 的播放速度微调把缓冲维持在目标附近（同时抵消两端声卡的时钟偏差），
//   积压太多（例如网络卡顿后数据一下子涌来）时直接跳到目标位置，不让声音越来越滞后；
// * 音画同步：记录每个包的采集时间，随时能回答"正在播放的是被控端哪一刻的声音"；
//   画面比声音还慢时，可以额外加大缓冲（extraMs）让声音等画面。

const INITIAL_TARGET_MS = 60;
const MIN_TARGET_MS = 40;
const MAX_TARGET_MS = 400;
/** 网络抖动导致断粮时目标延迟的增量 */
const UNDERRUN_STEP_MS = 20;
/** 每次回落的幅度 */
const DECREASE_STEP_MS = 5;
/**
 * 目标延迟至少比晚到量多这么多：包到达前缓冲里本来就少半个包（10 ms 的锯齿）、变速的死区（10 ms）、
 * 一个输出块，再留一点余量
 */
const LATE_MARGIN_MS = 30;
/** 晚到超过这么多算断线 / 卡死（缓冲盖不住），不按它加大目标延迟，只当一次断粮 */
const MAX_LATE_MS = 250;
/** 晚到量的基准（最小的"到达 − 采集"）每秒最多上浮这么多：跟上两端时钟的偏差、换了一条更慢的路 */
const BASE_LEAK_MS_PER_S = 10;
/** 记最近这么多个评估窗口（每个 WINDOW_MS）里的最大晚到量，约一分钟 */
const LATE_WINDOWS = 30;
/** 连续这么久没有断粮才允许回落 */
const CALM_MS = 20_000;
/** 评估窗口 */
const WINDOW_MS = 2_000;
/** 窗口内缓冲的最低点仍高于这个值，说明还有余量 */
const SLACK_MS = 25;
/** 比目标多出这么多就直接跳过 */
const SKIP_EXCESS_MS = 150;
/** 偏离目标不超过这个值时按原速播放 */
const DEADBAND_MS = 10;
/** 额外延迟一次加大超过这个值时，直接停顿补齐（否则靠减速要十几秒才能追上） */
const REBUFFER_MS = 60;
/** 开始播放 / 跳跃后的渐入，避免爆音 */
const FADE_MS = 3;
const CAPACITY_MS = 2_000;
const MAX_ANCHORS = 256;

export interface JitterStats {
  bufferedMs: number;
  targetMs: number;
  extraMs: number;
  underruns: number;
  skips: number;
}

/** 正在播放的声音对应被控端的哪一刻 */
export interface JitterClock {
  /** 本次输出第一个采样的采集时间（被控端时钟，微秒） */
  hostUs: number;
  /** 额外延迟（extraMs）中已经实际生效的部分 */
  appliedMs: number;
}

export class JitterBuffer {
  private readonly ring: Float32Array[];
  private readonly cap: number;
  /** 每毫秒的输入帧数 */
  private readonly perMs: number;
  /** 输入采样率 / 输出采样率 */
  private readonly baseStep: number;
  private readonly windowFrames: number;
  private readonly fadeLen: number;

  /** 已写入的帧数（绝对位置） */
  private w = 0;
  /** 读位置（带小数，用于变速） */
  private r = 0;
  private playing = false;
  /** 播放中途断粮，等下一包来判断原因 */
  private starved = false;
  /** 网络抖动需要的缓冲（帧） */
  private target: number;
  /** 音画同步要求的额外缓冲（帧） */
  private extra = 0;
  /** 额外延迟突然加大：当前输出块淡出后停顿补齐 */
  private rebuffer = false;
  private avg = 0;
  private low = Infinity;
  private windowLeft: number;
  /** 距离上次断粮已播放的输入帧数 */
  private calm = 0;
  private fade = 0;
  /** 每个包第一个采样的位置与采集时间 */
  private anchors: { pos: number; hostUs: number }[] = [];
  /** 晚到量的基准：最小的"到达时刻 − 采集时刻"（毫秒，两台电脑的时钟差也在里面）；还没有包时为 null */
  private lateBase: number | null = null;
  /** 上一个包的到达时刻（毫秒） */
  private lateAt = 0;
  /** 当前评估窗口里的最大晚到量（毫秒） */
  private lateNow = 0;
  /** 之前各评估窗口的最大晚到量（毫秒，环形） */
  private readonly lateWindows = new Float64Array(LATE_WINDOWS);
  private lateIndex = 0;

  underruns = 0;
  skips = 0;

  constructor(inRate: number, outRate: number, channels = 2) {
    this.perMs = inRate / 1000;
    this.baseStep = inRate / outRate;
    this.cap = Math.round(CAPACITY_MS * this.perMs);
    this.ring = Array.from({ length: channels }, () => new Float32Array(this.cap));
    this.target = INITIAL_TARGET_MS * this.perMs;
    this.windowFrames = Math.round((WINDOW_MS * outRate) / 1000);
    this.windowLeft = this.windowFrames;
    this.fadeLen = Math.max(1, Math.round((FADE_MS * outRate) / 1000));
  }

  get buffered(): number {
    return this.w - this.r;
  }

  /** 实际追求的缓冲量 */
  private get goal(): number {
    return this.target + this.extra;
  }

  stats(): JitterStats {
    return {
      bufferedMs: Math.max(0, this.buffered) / this.perMs,
      targetMs: this.target / this.perMs,
      extraMs: this.extra / this.perMs,
      underruns: this.underruns,
      skips: this.skips,
    };
  }

  /** 音画同步：让声音额外等一会儿（画面比声音慢时） */
  setExtraMs(ms: number) {
    const next = Math.max(0, Math.round(ms * this.perMs));
    if (this.playing && next - this.extra > REBUFFER_MS * this.perMs) this.rebuffer = true;
    this.extra = next;
  }

  /** 下一次 `pull` 输出的第一个采样对应的采集时间。没在播放时为 null。 */
  clock(): JitterClock | null {
    if (!this.playing || this.rebuffer) return null;
    const pos = this.r;
    let a: { pos: number; hostUs: number } | undefined;
    for (let i = this.anchors.length - 1; i >= 0; i--) {
      if (this.anchors[i].pos <= pos) {
        a = this.anchors[i];
        break;
      }
    }
    if (!a) return null;
    const applied = Math.min(this.extra, Math.max(0, this.avg - this.target)) / this.perMs;
    return { hostUs: a.hostUs + ((pos - a.pos) / this.perMs) * 1000, appliedMs: applied };
  }

  reset() {
    this.w = 0;
    this.r = 0;
    this.playing = false;
    this.starved = false;
    this.rebuffer = false;
    this.avg = 0;
    this.low = Infinity;
    this.windowLeft = this.windowFrames;
    this.calm = 0;
    this.target = INITIAL_TARGET_MS * this.perMs;
    this.anchors = [];
    this.lateBase = null;
    this.lateNow = 0;
    this.lateWindows.fill(0);
  }

  /** 最近约一分钟里最大的晚到量（毫秒） */
  private get recentLate(): number {
    let max = this.lateNow;
    for (const v of this.lateWindows) max = Math.max(max, v);
    return max;
  }

  /** 一个包在 `nowMs` 到达，第一个采样在 `hostMs` 采集（被控端时钟）：量晚到量，需要时马上加大目标延迟 */
  private measure(nowMs: number, hostMs: number) {
    const delay = nowMs - hostMs;
    if (this.lateBase !== null) this.lateBase += (Math.max(0, nowMs - this.lateAt) * BASE_LEAK_MS_PER_S) / 1000;
    this.lateAt = nowMs;
    if (this.lateBase === null || delay < this.lateBase) {
      this.lateBase = delay;
      return;
    }
    const late = delay - this.lateBase;
    // 断线 / 卡死：从这一包重新取基准，跟在它后面涌来的积压也就不算晚到
    if (late > MAX_LATE_MS) {
      this.lateBase = delay;
      return;
    }
    this.lateNow = Math.max(this.lateNow, late);
    this.target = Math.max(this.target, Math.min(MAX_TARGET_MS, late + LATE_MARGIN_MS) * this.perMs);
  }

  /**
   * 写入一个解码后的包（每声道一个数组）。`start` = 静音后的第一包；
   * `hostUs` = 第一个采样的采集时间（被控端时钟，微秒）；`nowMs` = 到达时刻（本地任意时基，毫秒，不随声音输出暂停）。
   * 两个都给了才量晚到量。
   */
  push(planes: Float32Array[], start: boolean, hostUs?: number, nowMs?: number) {
    if (planes.length === 0) return;
    let n = planes[0].length;
    if (n === 0) return;
    if (this.starved) {
      this.starved = false;
      if (!start) {
        this.target = Math.min(MAX_TARGET_MS * this.perMs, this.target + UNDERRUN_STEP_MS * this.perMs);
        this.underruns++;
        this.calm = 0;
      }
    }
    if (hostUs !== undefined && nowMs !== undefined) this.measure(nowMs, hostUs / 1000);
    let offset = 0;
    if (n > this.cap - 2) {
      offset = n - (this.cap - 2);
      n = this.cap - 2;
    }
    // 极端积压：丢掉最旧的数据（保留 2 帧给插值）
    const over = this.w + n - this.r - (this.cap - 2);
    if (over > 0) this.r += over;
    if (hostUs !== undefined) {
      this.anchors.push({ pos: this.w, hostUs: hostUs + (offset / this.perMs) * 1000 });
      // 只保留还没播完的包
      while (this.anchors.length > 1 && (this.anchors[1].pos <= this.r || this.anchors.length > MAX_ANCHORS)) {
        this.anchors.shift();
      }
    }
    const pos = this.w % this.cap;
    const first = Math.min(n, this.cap - pos);
    for (let c = 0; c < this.ring.length; c++) {
      const src = planes[Math.min(c, planes.length - 1)].subarray(offset, offset + n);
      const ring = this.ring[c];
      ring.set(src.subarray(0, first), pos);
      if (first < n) ring.set(src.subarray(first), 0);
    }
    this.w += n;
  }

  /** 填满一个输出块（每声道一个数组，长度 `frames`）。 */
  pull(out: Float32Array[], frames: number) {
    if (!this.playing) {
      if (this.buffered < this.goal) {
        for (const ch of out) ch.fill(0, 0, frames);
        return;
      }
      this.playing = true;
      this.fade = 0;
      this.avg = this.buffered;
    }
    if (this.buffered > this.goal + SKIP_EXCESS_MS * this.perMs) {
      this.r = this.w - this.goal;
      this.avg = this.goal;
      this.fade = 0;
      this.skips++;
    }
    this.low = Math.min(this.low, this.buffered);
    this.avg += (this.buffered - this.avg) * 0.02;

    const errMs = (this.avg - this.goal) / this.perMs;
    let adj = 0;
    if (errMs > DEADBAND_MS) adj = Math.min(0.02, (errMs - DEADBAND_MS) / 2000);
    else if (errMs < -DEADBAND_MS) adj = -Math.min(0.01, (-errMs - DEADBAND_MS) / 2000);
    const step = this.baseStep * (1 + adj);
    // 停顿补齐：这一块淡出，然后重新攒够缓冲（不算断粮）
    const fadeOut = this.rebuffer;

    const cap = this.cap;
    const chs = Math.min(out.length, this.ring.length);
    let i = 0;
    for (; i < frames; i++) {
      const idx = Math.floor(this.r);
      // 线性插值需要下一帧
      if (idx + 1 >= this.w) break;
      const f = this.r - idx;
      const a = idx % cap;
      const b = a + 1 === cap ? 0 : a + 1;
      let g = 1;
      if (this.fade < this.fadeLen) g = this.fade++ / this.fadeLen;
      if (fadeOut) g *= 1 - i / frames;
      for (let c = 0; c < chs; c++) {
        const ring = this.ring[c];
        out[c][i] = (ring[a] + (ring[b] - ring[a]) * f) * g;
      }
      this.r += step;
    }
    for (let c = chs; c < out.length; c++) out[c].set(out[chs - 1].subarray(0, i));
    if (i < frames) {
      for (const ch of out) ch.fill(0, i, frames);
      this.playing = false;
      this.starved = !fadeOut;
    }
    if (fadeOut) {
      this.rebuffer = false;
      this.playing = false;
    }
    this.calm += i * step;
    this.adapt(frames);
  }

  private adapt(frames: number) {
    this.windowLeft -= frames;
    if (this.windowLeft > 0) return;
    this.windowLeft = this.windowFrames;
    const low = this.low;
    this.low = Infinity;
    // 回落的下限：最近约一分钟里最大的晚到量也要盖得住
    const floor = Math.max(MIN_TARGET_MS, Math.min(MAX_TARGET_MS, this.recentLate + LATE_MARGIN_MS)) * this.perMs;
    this.lateWindows[this.lateIndex] = this.lateNow;
    this.lateIndex = (this.lateIndex + 1) % LATE_WINDOWS;
    this.lateNow = 0;
    if (!this.playing || low === Infinity) return;
    if (this.calm >= CALM_MS * this.perMs && low > (SLACK_MS * this.perMs) + this.extra && this.target > floor) {
      this.target = Math.max(floor, this.target - DECREASE_STEP_MS * this.perMs);
    }
  }
}
