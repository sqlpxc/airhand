import assert from "node:assert/strict";
import fs from "node:fs";
import vm from "node:vm";
import { stripTypeScriptTypes } from "node:module";
import { test } from "node:test";

/** 网页版的剪贴板同步（会话部分在 console.ts，这里换成记录调用的假的） */
function harness(writeText: (text: string) => Promise<void>) {
  const source = fs.readFileSync(new URL("../src/lib/backend/web.ts", import.meta.url), "utf8")
    .replace(/^import[\s\S]*?from "[^"]+";\r?\n/gm, "");
  const js = stripTypeScriptTypes(source).replace(/\bexport\s+/g, "") + "\nglobalThis.WebConsole = WebConsole;";
  const commands: any[] = [];
  const link: { events: ((ev: any) => void) | null; reply: Promise<any> } = { events: null, reply: Promise.resolve({ sid: 7 }) };
  const sandbox: any = {
    document: { hasFocus: () => true },
    navigator: { clipboard: { writeText } },
    ConsoleLink: class {
      command(c: any) { commands.push(c); return Promise.resolve(); }
      connect(_code: string, _opts: unknown, _view: unknown, events: (ev: any) => void) {
        link.events = events;
        return link.reply;
      }
    },
  };
  vm.runInNewContext(js, sandbox);
  const backend = Object.create(sandbox.WebConsole.prototype);
  Object.assign(backend, {
    clipSeen: null, clipToWrite: null, clipBusy: false, clipRevision: 0, clipWriteTask: null,
    clips: new Set(),
  });
  return { backend, commands, link };
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
  const { backend } = harness(async (text) => {
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
    const { backend } = harness(async (text) => {
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
  const { backend } = harness(async (text) => {
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
  const { backend, commands } = harness(async () => {});
  backend.clips.add({ sid: 7, enabled: true, synced: "initial" });
  backend.readClipboard = async () => { started.resolve(); await read.promise; return "old local text"; };
  const polling = backend.syncClipboard(false);
  await started.promise;
  await backend.writeClipboard("new remote text");
  read.resolve();
  await polling;
  assert.equal(backend.clipSeen, "new remote text");
  assert.deepEqual(commands, [], "stale local read must not be sent");
});

test("the remote clipboard goes to the browser, local changes go to sessions that sync", async () => {
  const writes: string[] = [];
  const { backend, commands, link } = harness(async (text) => { writes.push(text); });
  const events: any[] = [];
  const info = await backend.connect("123456", {}, {}, (ev: any) => events.push(ev), true);
  assert.equal(info.sid, 7);
  // 被控端复制的文本：写进浏览器的剪贴板，不交给会话
  link.events!({ type: "clipboard", text: "from host" });
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(writes, ["from host"]);
  assert.deepEqual(events, []);

  // 本机复制了别的：发给这个会话
  backend.readClipboard = async () => "from browser";
  await backend.syncClipboard(false);
  assert.deepEqual(commands.map((c) => JSON.parse(JSON.stringify(c))), [{ cmd: "clipboard", sid: 7, text: "from browser" }]);

  // 关掉同步：之后的变化不发
  await backend.command({ cmd: "clipboardSync", sid: 7, enabled: false });
  backend.readClipboard = async () => "again";
  await backend.syncClipboard(false);
  assert.equal(commands.length, 1);

  // 会话结束：不再管它
  link.events!({ type: "closed", reason: { kind: "hostEnded" }, retry: false });
  assert.equal(events.at(-1).type, "closed");
  assert.equal(backend.clips.size, 0);
});
