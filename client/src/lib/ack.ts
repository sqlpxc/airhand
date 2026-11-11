// 画面确认（ack）的合并。
//
// 确认是累积的（确认第 n 帧 = 前面的都收到了），所以短时间里连着到的几帧只需要确认最后一帧：
// 离上次确认已经超过 ACK_INTERVAL_MS 就马上发，否则等到那个时候再发最新的序号。
// 帧率不超过 1000 / ACK_INTERVAL_MS 时和每帧确认一样；更高的帧率、或者一下子到了好几帧（网络抖了一下）时
// 确认少发很多（每次确认是一条 WebSocket 消息），被控端测到的往返最多多 ACK_INTERVAL_MS。

/** 两次确认之间至少隔这么久（毫秒） */
export const ACK_INTERVAL_MS = 8;

/**
 * 发给控制端服务的二进制确认：`1 | sid u32 LE | seq u32 LE`（crates/airhand-link/src/console.rs 的 parse_ack），
 * 每帧都有的消息不走 JSON
 */
export function ackMessage(sid: number, seq: number): ArrayBuffer {
  const buf = new ArrayBuffer(9);
  const dv = new DataView(buf);
  dv.setUint8(0, 1);
  dv.setUint32(1, sid, true);
  dv.setUint32(5, seq, true);
  return buf;
}

type Clock = { now(): number; setTimeout(fn: () => void, ms: number): unknown; clearTimeout(t: unknown): void };

const realClock: Clock = {
  now: () => performance.now(),
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (t) => clearTimeout(t as ReturnType<typeof setTimeout>),
};

export class AckCoalescer {
  private latest: number | null = null;
  private last = -Infinity;
  private timer: unknown = null;
  private readonly send: (seq: number) => void;
  private readonly clock: Clock;

  constructor(send: (seq: number) => void, clock: Clock = realClock) {
    this.send = send;
    this.clock = clock;
  }

  /** 收到了第 `seq` 帧 */
  push(seq: number) {
    this.latest = seq;
    if (this.timer !== null) return;
    const wait = this.last + ACK_INTERVAL_MS - this.clock.now();
    if (wait <= 0) {
      this.flush();
    } else {
      this.timer = this.clock.setTimeout(() => {
        this.timer = null;
        this.flush();
      }, wait);
    }
  }

  /** 会话结束：不再发 */
  dispose() {
    if (this.timer !== null) this.clock.clearTimeout(this.timer);
    this.timer = null;
    this.latest = null;
  }

  private flush() {
    const seq = this.latest;
    if (seq === null) return;
    this.latest = null;
    this.last = this.clock.now();
    this.send(seq);
  }
}
