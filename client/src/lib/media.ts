// 媒体包：画面、光标、声音。格式见 crates/airhand-proto/src/media.rs（被控端打包，控制端服务原样转交过来），
// 样例在 crates/airhand-link/tests/protocol.json（tests/protocol.test.ts 检查这里的解析和它一致）。
//
// 头（小端）+ 数据：
//   0 类型 u8（1 画面 / 2 光标 / 3 声音）  1 标志 u8  2 编码格式 u8（光标：槽位）  3 头的长度 u8（现在是 24）
//   4 序号 u32  8 宽 u16  10 高 u16  12 热点 x u16  14 热点 y u16  16 采集时间 u64（微秒，被控端时钟）
//
// 头可以变长：以后的版本把新字段加在第 24 字节之后，负载从"头的长度"处开始。所以负载一定从第 3 个字节给出的位置取
// （payload），不认识的标志位、保留字节、24 字节之后的部分都不管；头的长度不对的包丢掉（kind 为 0）。
//
// 光标形状按槽位缓存（动画光标反复用到的那几帧只传一次）：标志 bit1 = 新形状（RGBA 在头后面），存进槽位并换成它；
// bit2 = 换成槽位里已有的形状；都没有 = 形状没变。槽位只在一个会话里有效。
// bit0 = 可见；bit3 = 光标已经画在画面里了（被控端改用了软件光标，例如 Windows 拖动、缩放窗口时）：
// 不显示本地光标（不然有两个），鼠标照常用绝对坐标。
import type { Codec } from "./api";

/** 头最短的长度（本版本写的长度） */
export const HEADER_LEN = 24;
export const VIDEO = 1;
export const CURSOR = 2;
export const AUDIO = 3;

/** 头里的编码格式编号 → 格式（编号和 Rust 的 Codec 一样） */
const CODECS: Codec[] = ["h264", "h265", "av1"];

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
  /** 光标已经画在画面里了：不显示本地光标，鼠标照常用绝对坐标（只在 visible 时有意义） */
  inFrame: boolean;
  /** 形状放在哪个槽位里（shape 或 cached 时有意义） */
  slot: number;
  /** 带了新的形状（RGBA 在头后面）：存进 slot 并换成它；为 null 时没带 */
  shape: { width: number; height: number; hotX: number; hotY: number } | null;
  /** 换成 slot 里已有的形状 */
  cached: boolean;
}

/** 头的长度（第 3 个字节）；不到 24、或者比包还长时为 0（不是合法的包） */
export function headerLen(dv: DataView): number {
  if (dv.byteLength < HEADER_LEN) return 0;
  const n = dv.getUint8(3);
  return n >= HEADER_LEN && n <= dv.byteLength ? n : 0;
}

/** 包的类型；头不完整时为 0 */
export function kind(dv: DataView): number {
  return headerLen(dv) === 0 ? 0 : dv.getUint8(0);
}

/** 包的负载（头后面的部分）；`pkt` 和 `dv` 是同一段字节 */
export function payload(pkt: Uint8Array, dv: DataView): Uint8Array {
  return pkt.subarray(headerLen(dv) || pkt.byteLength);
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
    inFrame: (flags & 8) !== 0,
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
