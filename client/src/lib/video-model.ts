// Shared shape of airhand_proto::video; generated fixtures verify these types.
import type { Codec, QualitySettings } from "./api";

export interface DecodeCapability {
  codec: Codec;
  maxWidth: number | null;
  maxHeight: number | null;
  maxFps: number | null;
}
export interface ReceiverCapabilities { codecs: DecodeCapability[] }
export interface ViewRequest { requestId: number; preference: QualitySettings }
export interface MediaInstance { streamId: number; generation: number }
export interface VideoContext { configId: number; instance: MediaInstance }
export interface VideoConfig {
  context: VideoContext;
  requestId: number;
  /** Unknown future codecs retain their wire number. */
  codec: Codec | number;
  width: number;
  height: number;
  fps: number;
  bitrateKbps: number;
}
export interface VideoFeedback {
  context: VideoContext;
  sampleId: number;
  receivedSeq: number;
  decodedSeq: number | null;
  presentedSeq: number | null;
  pendingFrames: number;
  oldestPendingMs: number;
  decodeTimeUs: number;
  droppedFrames: number;
  decoderErrors: number;
}
export function sameContext(a: VideoContext | null | undefined, b: VideoContext | null | undefined): boolean {
  return a != null && b != null && a.configId === b.configId
    && a.instance.streamId === b.instance.streamId && a.instance.generation === b.instance.generation;
}

/** Unknown limits stay null: a successful probe at 1080p is not a maximum-size capability. */
export function receiverCapabilities(codecs: readonly Codec[]): ReceiverCapabilities {
  return { codecs: [...new Set(codecs)].map(codec => ({ codec, maxWidth: null, maxHeight: null, maxFps: null })) };
}

/** Cumulative per-context telemetry. Receiving/decoding/presenting are deliberately independent. */
export class VideoFeedbackCollector {
  private value: VideoFeedback | null = null;
  private dirty = false;

  reset() { this.value = null; this.dirty = false; }
  received(seq: number, context: VideoContext | null | undefined) {
    if (!context) return;
    if (!sameContext(context, this.value?.context)) {
      this.value = { context, sampleId: 0, receivedSeq: seq, decodedSeq: null, presentedSeq: null,
        pendingFrames: 0, oldestPendingMs: 0, decodeTimeUs: 0, droppedFrames: 0, decoderErrors: 0 };
    } else this.value!.receivedSeq = seq;
    this.dirty = true;
  }
  decoded(seq: number, context: VideoContext | null | undefined, ms: number) {
    if (!sameContext(context, this.value?.context)) return;
    const v = this.value!;
    if (v.decodedSeq !== null && ((seq - v.decodedSeq) | 0) < 0) return;
    v.decodedSeq = seq;
    const us = Math.round(Math.max(0, Math.min(60_000, ms)) * 1000);
    v.decodeTimeUs = v.decodeTimeUs === 0 ? us : Math.round(v.decodeTimeUs * 0.9 + us * 0.1);
    this.dirty = true;
  }
  presented(seq: number, context: VideoContext | null | undefined) {
    if (!sameContext(context, this.value?.context)) return;
    const v = this.value!;
    if (v.presentedSeq === null || ((seq - v.presentedSeq) | 0) >= 0) v.presentedSeq = seq;
    this.dirty = true;
  }
  dropped(count = 1, context?: VideoContext) {
    if (context && !sameContext(context, this.value?.context)) return;
    if (this.value) { this.value.droppedFrames = Math.min(0xffffffff, this.value.droppedFrames + count); this.dirty = true; }
  }
  error() { if (this.value) { this.value.decoderErrors = Math.min(0xffffffff, this.value.decoderErrors + 1); this.dirty = true; } }
  sample(queueFrames: number, queueMs: number): VideoFeedback | null {
    if (!this.value || (!this.dirty && queueFrames === 0)) return null;
    if (this.value.sampleId === 0xffffffff) return null;
    this.value.sampleId++;
    this.dirty = false;
    return { ...this.value, pendingFrames: Math.min(1024, Math.max(0, Math.round(queueFrames))),
      oldestPendingMs: Math.min(60_000, Math.max(0, Math.round(queueMs))) };
  }
}
