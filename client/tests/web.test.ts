import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { test } from "node:test";

function harness(writeText: (text: string) => Promise<void>) {
  const source = fs.readFileSync(new URL("../src/lib/backend/web.ts", import.meta.url), "utf8")
    .replace(/^import[\s\S]*?from "[^"]+";\r?\n/gm, "");
  const js = stripTypeScriptTypes(source).replace(/\bexport\s+/g, "") + "\nglobalThis.WebConsole = WebConsole;";
  const sandbox: any = {
    HEADER_LEN: 24,
    document: { hasFocus: () => true },
    navigator: { clipboard: { writeText } },
  };
  vm.runInNewContext(js, sandbox);
  const backend = Object.create(sandbox.WebConsole.prototype);
  Object.assign(backend, {
    clipSeen: null, clipToWrite: null, clipBusy: false, clipRevision: 0, clipWriteTask: null,
    chans: new Map(),
  });
  return backend;
}

function deferred() {
  let resolve!: () => void;
  let reject!: (e: Error) => void;
  const promise = new Promise<void>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

test("new clipboard content replaces a failed retry", async () => {
  let clipboard = "";
  let first = true;
  const backend = harness(async (text) => {
    if (first) { first = false; throw new Error("not focused"); }
    clipboard = text;
  });
  await backend.writeClipboard("old A");
  await backend.writeClipboard("new B");
  await backend.syncClipboard(false);
  assert.equal(clipboard, "new B");
  assert.equal(backend.clipToWrite, null);
});

test("clipboard writes are serial and keep only the latest pending content", async () => {
  for (const fails of [false, true]) {
    const first = deferred();
    const writes: string[] = [];
    const backend = harness(async (text) => {
      writes.push(text);
      if (text === "A") await first.promise;
    });
    const a = backend.writeClipboard("A");
    const b = backend.writeClipboard("B");
    const c = backend.writeClipboard("C");
    assert.deepEqual(writes, ["A"]);
    if (fails) first.reject(new Error("old write failed")); else first.resolve();
    await Promise.all([a, b, c]);
    assert.deepEqual(writes, ["A", "C"]);
    assert.equal(backend.clipSeen, "C");
    assert.equal(backend.clipToWrite, null);
  }
});

test("a failed latest write is retried when clipboard polling resumes", async () => {
  const writes: string[] = [];
  const backend = harness(async (text) => {
    writes.push(text);
    if (writes.length === 1) throw new Error("not focused");
  });
  await backend.writeClipboard("A");
  assert.equal(backend.clipToWrite.text, "A");
  await backend.syncClipboard(false);
  assert.deepEqual(writes, ["A", "A"]);
  assert.equal(backend.clipToWrite, null);
});

test("a local read started before a remote update cannot echo stale content", async () => {
  const read = deferred();
  const started = deferred();
  const backend = harness(async () => {});
  const chan = { sid: 7, clipboard: true, synced: "initial" };
  backend.chans.set(1, chan);
  backend.readClipboard = async () => { started.resolve(); await read.promise; return "old local text"; };
  backend.notify = () => { assert.fail("stale local read must not be sent"); };
  const polling = backend.syncClipboard(false);
  await started.promise;
  await backend.writeClipboard("new remote text");
  read.resolve();
  await polling;
  assert.equal(backend.clipSeen, "new remote text");
});
