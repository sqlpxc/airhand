// 剪贴板里的文件（src/lib/files.ts）：粘贴键、内存里拼的 ZIP
import assert from "node:assert/strict";
import { test } from "node:test";
import { crc32, isPasteKey, ZipWriter } from "../src/lib/files.ts";

test("粘贴键：Ctrl+V、⌘V、Shift+Insert，别的组合不算", () => {
  const key = (code: string, mods: Partial<KeyboardEvent> = {}) =>
    ({ code, ctrlKey: false, metaKey: false, altKey: false, shiftKey: false, ...mods }) as KeyboardEvent;
  assert.ok(isPasteKey(key("KeyV", { ctrlKey: true })));
  assert.ok(isPasteKey(key("KeyV", { metaKey: true })));
  assert.ok(isPasteKey(key("Insert", { shiftKey: true })));
  assert.ok(!isPasteKey(key("KeyV")));
  assert.ok(!isPasteKey(key("KeyV", { ctrlKey: true, altKey: true })));
  assert.ok(!isPasteKey(key("KeyC", { ctrlKey: true })));
});

test("CRC-32 和标准的一样", () => {
  const enc = new TextEncoder();
  assert.equal(crc32([enc.encode("123456789")]), 0xcbf43926);
  // 分成几段算的和整段一样
  assert.equal(crc32([enc.encode("1234"), enc.encode("56789")]), 0xcbf43926);
  assert.equal(crc32([]), 0);
});

test("ZIP：本地头、中央目录、结尾的偏移和个数对得上，名字是 UTF-8", async () => {
  const zip = new ZipWriter();
  zip.dir("文件夹");
  zip.file("文件夹/a.txt", [new TextEncoder().encode("hello")], Date.UTC(2024, 0, 2, 3, 4, 6));
  const bytes = new Uint8Array(await zip.finish().arrayBuffer());
  const dv = new DataView(bytes.buffer);
  assert.equal(dv.getUint32(0, true), 0x04034b50);
  // 第 11 位：UTF-8
  assert.equal(dv.getUint16(6, true) & 0x0800, 0x0800);
  const end = bytes.length - 22;
  assert.equal(dv.getUint32(end, true), 0x06054b50);
  assert.equal(dv.getUint16(end + 10, true), 2);
  const cdSize = dv.getUint32(end + 12, true);
  const cdOffset = dv.getUint32(end + 16, true);
  assert.equal(cdOffset + cdSize, end);
  assert.equal(dv.getUint32(cdOffset, true), 0x02014b50);
  // 第二个条目（文件）：中央目录里记着它的本地头在哪、CRC
  const nameLen = dv.getUint16(cdOffset + 28, true);
  const second = cdOffset + 46 + nameLen;
  assert.equal(dv.getUint32(second + 16, true), crc32([new TextEncoder().encode("hello")]));
  const local = dv.getUint32(second + 42, true);
  assert.equal(dv.getUint32(local, true), 0x04034b50);
  const n = dv.getUint16(local + 26, true);
  assert.equal(new TextDecoder().decode(bytes.subarray(local + 30, local + 30 + n)), "文件夹/a.txt");
  assert.equal(new TextDecoder().decode(bytes.subarray(local + 30 + n, local + 30 + n + 5)), "hello");
});
