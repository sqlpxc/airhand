// 一个会话的声音解码（在 Worker 里，见 console-worker.ts）：WebCodecs 解码 Opus，PCM 经主线程给的端口
// 直接交给音频线程（AudioWorklet，见 audio.ts、audio-worklet.ts），不经过主线程。
import type { PlayerMessage } from "./audio-worklet";

/** 与 airhand_proto::session::AUDIO_* 一致 */
export const SAMPLE_RATE = 48000;
export const CHANNELS = 2;
const FRAME_SAMPLES = 960;
export const OPUS: AudioDecoderConfig = { codec: "opus", sampleRate: SAMPLE_RATE, numberOfChannels: CHANNELS };

export class AudioStream {
  /** 音频线程的端口；null = 不播放（收到的包丢掉） */
  private port: MessagePort | null = null;
  private decoder: AudioDecoder | null = null;
  /** 音画同步：让声音额外缓冲（换了端口也要带过去） */
  private extraMs = 0;
  /**
   * 已送去解码、还没出结果的包（按顺序一一对应）。不用解码输出的时间戳：
   * 浏览器可能按采样数自己推算输出时间戳，跳过静音段之后就对不上了。
   */
  private inflight: { hostUs: number; start: boolean }[] = [];

  /** 开始 / 停止播放（主线程打开、关闭了这个会话的声音输出） */
  setPort(port: MessagePort | null) {
    this.closeDecoder();
    this.port?.close();
    this.port = port;
    if (this.extraMs > 0) this.post({ type: "extra", ms: this.extraMs });
  }

  setExtraDelay(ms: number) {
    this.extraMs = ms;
    this.post({ type: "extra", ms });
  }

  /** 新的会话流：解码器与缓冲都从头开始 */
  reset() {
    this.closeDecoder();
    this.post({ type: "reset" });
  }

  push(start: boolean, hostUs: number, data: Uint8Array) {
    if (!this.port) return;
    if (!this.decoder || this.decoder.state === "closed") this.openDecoder();
    const dec = this.decoder;
    // 解码跟不上（几乎不会发生）：丢掉积压，重新开始
    if (!dec || dec.decodeQueueSize > 25) {
      this.closeDecoder();
      return;
    }
    this.inflight.push({ hostUs, start });
    try {
      dec.decode(new EncodedAudioChunk({ type: "key", timestamp: hostUs, data }));
    } catch {
      this.closeDecoder();
    }
  }

  private post(m: PlayerMessage, transfer: Transferable[] = []) {
    this.port?.postMessage(m, transfer);
  }

  private openDecoder() {
    const decoder = new AudioDecoder({
      output: (d) => this.output(decoder, d),
      error: (e) => {
        console.warn("audio decoder error", e);
        if (this.decoder === decoder) this.closeDecoder();
      },
    });
    try {
      decoder.configure(OPUS);
      this.decoder = decoder;
    } catch (e) {
      console.warn("can't configure the audio decoder", e);
    }
  }

  private closeDecoder() {
    if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    this.decoder = null;
    this.inflight = [];
  }

  private output(decoder: AudioDecoder, d: AudioData) {
    const info = this.decoder === decoder ? this.inflight.shift() : undefined;
    if (!this.port || !info) {
      d.close();
      return;
    }
    const n = d.numberOfFrames;
    const planes: Float32Array[] = [];
    for (let c = 0; c < Math.min(d.numberOfChannels, CHANNELS); c++) {
      const p = new Float32Array(n);
      d.copyTo(p, { planeIndex: c, format: "f32-planar" });
      planes.push(p);
    }
    d.close();
    // 解码器刚创建时会丢掉开头的编码器前瞻（输出比一包短）：丢的是包的前面部分
    const hostUs = info.hostUs + (Math.max(0, FRAME_SAMPLES - n) / SAMPLE_RATE) * 1e6;
    this.post({ type: "pcm", planes, start: info.start, hostUs }, planes.map((p) => p.buffer));
  }
}
