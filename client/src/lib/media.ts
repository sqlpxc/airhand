// 媒体包：画面、光标、声音。格式见 crates/airhand-proto/src/media.rs（被控端打包，控制端服务原样转交过来），
// 样例在 crates/airhand-link/tests/protocol.json（tests/protocol.test.ts 检查这里的解析和它一致）。
//
// 24 字节的头（小端）+ 数据：
//   0 类型 u8（1 画面 / 2 光标 / 3 声音）  1 标志 u8  2 编码格式 u8（光标：槽位）  4 序号 u32
//   8 宽 u16  10 高 u16  12 热点 x u16  14 热点 y u16  16 采集时间 u64（微秒，被控端时钟）
//
// 光标形状按槽位缓存（动画光标反复用到的那几帧只传一次）：标志 bit1 = 新形状（RGBA 在头后面），存进槽位并换成它；
// bit2 = 换成槽位里已有的形状；都没有 = 形状没变。槽位只在一个会话里有效。
import type { Codec } from "./api";

export const HEADER_LEN = 24;
export const VIDEO = 1;
export const CURSOR = 2;
export const AUDIO = 3;

const CODECS: Codec[] = ["H264", "H265", "AV1"];

export interface VideoHeader {
  seq: number;
  key: boolean;
  codec: Codec;
  width: number;
  height: number;
  /** 采集时间（被控端时钟，微秒）；在 2^53 以内，转成 number 不丢精度 */
  captureUs: number;
}

export interface AudioHeader {
  seq: number;
  /** 停顿之后的第一包 */
  start: boolean;
  captureUs: number;
}

export interface CursorHeader {
  visible: boolean;
  /** 形状放在哪个槽位里（shape 或 cached 时有意义） */
  slot: number;
  /** 带了新的形状（RGBA 在头后面）：存进 slot 并换成它；为 null 时没带 */
  shape: { width: number; height: number; hotX: number; hotY: number } | null;
  /** 换成 slot 里已有的形状 */
  cached: boolean;
}

/** 包的类型；不到一个头长时为 0 */
export function kind(dv: DataView): number {
  return dv.byteLength < HEADER_LEN ? 0 : dv.getUint8(0);
}

/** 画面包的头；编码格式不认识（对方的版本更新）时为 null：和 Rust、Android 一样丢掉这一帧，不猜成 H.264 */
export function videoHeader(dv: DataView): VideoHeader | null {
  const codec = CODECS[dv.getUint8(2)];
  if (codec === undefined) return null;
  return {
    seq: dv.getUint32(4, true),
    key: (dv.getUint8(1) & 1) !== 0,
    codec,
    width: dv.getUint16(8, true),
    height: dv.getUint16(10, true),
    captureUs: Number(dv.getBigUint64(16, true)),
  };
}

export function audioHeader(dv: DataView): AudioHeader {
  return {
    seq: dv.getUint32(4, true),
    start: (dv.getUint8(1) & 1) !== 0,
    captureUs: Number(dv.getBigUint64(16, true)),
  };
}

export function cursorHeader(dv: DataView): CursorHeader {
  const flags = dv.getUint8(1);
  return {
    visible: (flags & 1) !== 0,
    slot: dv.getUint8(2),
    shape: flags & 2
      ? { width: dv.getUint16(8, true), height: dv.getUint16(10, true), hotX: dv.getUint16(12, true), hotY: dv.getUint16(14, true) }
      : null,
    cached: (flags & 4) !== 0,
  };
}

/** 画面包的序号（收到就要确认）；不是画面包时为 null */
export function videoSeq(dv: DataView): number | null {
  return kind(dv) === VIDEO ? dv.getUint32(4, true) : null;
}
