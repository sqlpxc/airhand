// H.264 Annex-B → AVC 转换（WebKitGTK 只能解码 AVC 格式）：`npm test`
import assert from "node:assert/strict";
import { test } from "node:test";
import { annexBToAvc, avcC, bestCodec, codecUsable, splitAnnexB } from "../src/lib/codec.ts";

const SPS = [0x67, 0x42, 0xc0, 0x28, 0xda, 0x01];
const PPS = [0x68, 0xce, 0x3c, 0x80];
const IDR = [0x65, 0x88, 0x84, 0x00, 0x00, 0x03, 0x00, 0x10];
const AUD = [0x09, 0xf0];

test("拆分 3 字节和 4 字节起始码，NAL 内部的 00 00 03 不受影响", () => {
  const data = new Uint8Array([0, 0, 0, 1, ...AUD, 0, 0, 1, ...SPS, 0, 0, 0, 1, ...PPS, 0, 0, 1, ...IDR]);
  const nals = splitAnnexB(data).map((n) => Array.from(n));
  assert.deepEqual(nals, [AUD, SPS, PPS, IDR]);
  assert.deepEqual(splitAnnexB(new Uint8Array([1, 2, 3])), []);
});

test("关键帧：参数集单独拿出来，其余改为 4 字节长度前缀，去掉访问单元分隔符", () => {
  const data = new Uint8Array([0, 0, 0, 1, ...AUD, 0, 0, 0, 1, ...SPS, 0, 0, 0, 1, ...PPS, 0, 0, 0, 1, ...IDR]);
  const r = annexBToAvc(data);
  assert.deepEqual(Array.from(r.sps!), SPS);
  assert.deepEqual(Array.from(r.pps!), PPS);
  assert.deepEqual(Array.from(r.data), [0, 0, 0, IDR.length, ...IDR]);
});

test("非关键帧：多个 slice 都保留", () => {
  const p1 = [0x41, 0x9a, 0x01];
  const p2 = [0x41, 0x9b, 0x02, 0x03];
  const r = annexBToAvc(new Uint8Array([0, 0, 1, ...p1, 0, 0, 1, ...p2]));
  assert.equal(r.sps, undefined);
  assert.deepEqual(Array.from(r.data), [0, 0, 0, 3, ...p1, 0, 0, 0, 4, ...p2]);
});

test("avcC：profile/level 来自 SPS，NAL 长度字段 4 字节", () => {
  const c = Array.from(avcC(new Uint8Array(SPS), new Uint8Array(PPS)));
  assert.deepEqual(c, [1, 0x42, 0xc0, 0x28, 0xff, 0xe1, 0, SPS.length, ...SPS, 1, 0, PPS.length, ...PPS]);
});

test("编码格式优先级：AV1 > HEVC > H.264，取双方都支持的里最好的", () => {
  // 还不知道对方支持什么：只看本机能解码的
  assert.equal(bestCodec(["h264", "h265", "av1"]), "av1");
  assert.equal(bestCodec(["h264", "h265"]), "h265");
  assert.equal(bestCodec(["h264"]), "h264");
  // Mac 被控端没有 AV1 编码器：改用 HEVC，而不是让对方回退到 H.264
  assert.equal(bestCodec(["h264", "h265", "av1"], ["h264", "h265"]), "h265");
  assert.equal(bestCodec(["h264", "av1"], ["h264", "h265", "av1"]), "av1");
  assert.equal(bestCodec(["h264", "av1"], ["h264", "h265"]), "h264");
  // H.264 双方都一定支持
  assert.equal(bestCodec([], []), "h264");
  assert.ok(codecUsable("h264", [], []));
  assert.ok(!codecUsable("av1", ["h264", "av1"], ["h264", "h265"]));
  assert.ok(codecUsable("h265", ["h264", "h265"], ["h264", "h265"]));
});
