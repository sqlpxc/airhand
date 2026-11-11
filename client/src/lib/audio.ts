// 远程声音的输出：Worker 解码 Opus（audio-decoder.ts）→ AudioWorklet 抖动缓冲（audio-worklet.ts）→ 扬声器。
//
// * 所有会话共用一个 AudioContext，每个正在播放的会话一个 AudioWorkletNode；
// * 解码后的 PCM 由 Worker 经 MessageChannel 直接交给音频线程，主线程不经手；
// * 音频线程定期报告"正在播放的是被控端哪一刻的声音"，这里换算成本地时间后交给音画同步（在 Worker 里）。
import type { ControlMessage, WorkletMessage } from "./audio-worklet";
import workletUrl from "./audio-worklet?worker&url";
import { CHANNELS, OPUS, SAMPLE_RATE } from "./audio-decoder";
import type { JitterStats } from "./jitter";

let shared: { ctx: AudioContext; ready: Promise<void> } | null = null;

function audioContext() {
  if (shared) return shared;
  let ctx: AudioContext;
  try {
    ctx = new AudioContext({ latencyHint: "interactive", sampleRate: SAMPLE_RATE });
  } catch {
    ctx = new AudioContext({ latencyHint: "interactive" });
  }
  const ready = ctx.audioWorklet.addModule(workletUrl);
  // 自动播放策略：没有用户操作时 AudioContext 可能处于 suspended，在第一次点击/按键时恢复
  const resume = () => {
    if (ctx.state === "suspended") void ctx.resume().catch(() => {});
  };
  window.addEventListener("pointerdown", resume, { capture: true });
  window.addEventListener("keydown", resume, { capture: true });
  resume();
  shared = { ctx, ready };
  return shared;
}

let support: Promise<boolean> | null = null;

/** 本机能否解码 Opus（WebView2 自带软件解码器，正常都支持） */
export function opusSupported(): Promise<boolean> {
  support ??= (async () => {
    if (typeof AudioDecoder === "undefined") return false;
    try {
      return (await AudioDecoder.isConfigSupported(OPUS)).supported === true;
    } catch {
      return false;
    }
  })();
  return support;
}

export class AudioOutput {
  /** 最近一次统计（约每秒更新） */
  stats: JitterStats | null = null;
  onUnsupported: (() => void) | null = null;
  /** 被控端 `hostUs`（微秒）采集的声音在本地 `localMs`（performance.now() 时基）被听到 */
  onClock: ((hostUs: number, localMs: number, appliedMs: number) => void) | null = null;

  private enabled = false;
  private gen = 0;
  private node: AudioWorkletNode | null = null;
  private ctx: AudioContext | null = null;
  /** 把解码那边（Worker）接到音频线程的端口上；null = 停止播放 */
  private readonly attach: (port: MessagePort | null) => void;

  constructor(attach: (port: MessagePort | null) => void) {
    this.attach = attach;
  }

  enable() {
    if (this.enabled) return;
    this.enabled = true;
    const gen = ++this.gen;
    void (async () => {
      if (!(await opusSupported())) {
        if (gen === this.gen) this.onUnsupported?.();
        return;
      }
      const { ctx, ready } = audioContext();
      try {
        await ready;
      } catch (e) {
        console.warn("can't load the audio worklet", e);
        return;
      }
      if (gen !== this.gen) return;
      const node = new AudioWorkletNode(ctx, "airhand-audio", {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [CHANNELS],
        processorOptions: { inputRate: SAMPLE_RATE },
      });
      node.port.onmessage = (e: MessageEvent<WorkletMessage>) => this.onWorklet(e.data);
      node.connect(ctx.destination);
      this.node = node;
      this.ctx = ctx;
      const pipe = new MessageChannel();
      this.post({ type: "port", port: pipe.port2 }, [pipe.port2]);
      this.attach(pipe.port1);
    })();
  }

  disable() {
    if (!this.enabled) return;
    this.enabled = false;
    this.gen++;
    if (this.node) {
      this.attach(null);
      this.post({ type: "close" });
      this.node.disconnect();
      this.node = null;
    }
    this.stats = null;
  }

  private onWorklet(m: WorkletMessage) {
    if (m.type === "stats") {
      this.stats = m;
    } else if (m.type === "clock" && this.ctx && this.onClock) {
      // 音频上下文时间 → 本地时间：getOutputTimestamp 给出"此刻扬声器正在播放的位置"，已含输出延迟
      const ts = this.ctx.getOutputTimestamp();
      if (!ts.performanceTime || ts.contextTime === undefined) return;
      this.onClock(m.hostUs, ts.performanceTime + (m.ctxTime - ts.contextTime) * 1000, m.appliedMs);
    }
  }

  private post(m: ControlMessage, transfer: Transferable[] = []) {
    this.node?.port.postMessage(m, transfer);
  }
}
