import assert from "node:assert/strict";
import { test } from "node:test";
import { ACK_INTERVAL_MS, AckCoalescer, ackMessage } from "../src/lib/ack.ts";

test("网页版的二进制确认与 web/src/console.rs 的 parse_ack 一致", () => {
  assert.deepEqual([...new Uint8Array(ackMessage(3, 298))], [1, 3, 0, 0, 0, 0x2a, 1, 0, 0]);
});

/** 手动拨动的时钟 */
function fakeClock() {
  let now = 0;
  let timers: { at: number; fn: () => void; id: number }[] = [];
  let next = 1;
  return {
    clock: {
      now: () => now,
      setTimeout: (fn: () => void, ms: number) => {
        const id = next++;
        timers.push({ at: now + ms, fn, id });
        return id;
      },
      clearTimeout: (id: unknown) => {
        timers = timers.filter((t) => t.id !== id);
      },
    },
    advance(ms: number) {
      now += ms;
      const due = timers.filter((t) => t.at <= now);
      timers = timers.filter((t) => t.at > now);
      for (const t of due) t.fn();
    },
    pending: () => timers.length,
  };
}

test("确认：隔得够久就马上发，连着到的几帧只确认最后一帧", () => {
  const c = fakeClock();
  const sent: number[] = [];
  const acks = new AckCoalescer((s) => sent.push(s), c.clock);
  acks.push(1);
  assert.deepEqual(sent, [1]);
  // 紧跟着到的几帧：等到间隔满了只发最新的
  acks.push(2);
  acks.push(3);
  acks.push(4);
  assert.deepEqual(sent, [1]);
  c.advance(ACK_INTERVAL_MS);
  assert.deepEqual(sent, [1, 4]);
  // 帧率不高时（60 fps）每帧都马上确认
  for (let seq = 5; seq < 10; seq++) {
    c.advance(16);
    acks.push(seq);
  }
  assert.deepEqual(sent, [1, 4, 5, 6, 7, 8, 9]);
  assert.equal(c.pending(), 0);
});

test("确认：会话结束后不再发", () => {
  const c = fakeClock();
  const sent: number[] = [];
  const acks = new AckCoalescer((s) => sent.push(s), c.clock);
  acks.push(1);
  acks.push(2);
  acks.dispose();
  c.advance(100);
  assert.deepEqual(sent, [1]);
});
