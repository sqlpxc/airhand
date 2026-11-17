// 剪贴板里的文件（见 crates/airhand-link/src/files、console.rs 的"剪贴板里的文件"）：复制时只发清单，粘贴时才传。
//
// * 这里的文件（只有网页版：在画面上粘贴、拖到画面上；桌面版的本机剪贴板由 Rust 端自己同步）：
//   LocalFiles 记着它们（File、目录的 FileSystemDirectoryEntry），公告给被控端；被控端粘贴时控制端服务来问
//   （fileList / fileRead，console.ts 转到这里），这里读出来答；
// * 被控端复制的文件（clipboardFiles 事件，只有网页版）：用户点"下载"时才一个个取下来（download）。
//   Chromium 写进用户选的文件夹 / 文件（showDirectoryPicker / showSaveFilePicker，边取边写），
//   别的浏览器在内存里拼成文件（几个文件、文件夹时打成 ZIP，不压缩）再交给浏览器下载。
import type { Backend, FileEntry, FileError } from "./api";

/** FileError 在线上的编号（二进制的文件内容里的错误，见 console.rs） */
export const FILE_ERRORS: FileError[] = [
  "notFound",
  "denied",
  "changed",
  "io",
  "tooLarge",
  "expired",
  "invalid",
  "busy",
  "tooMany",
  "closed",
];

/** 一次取多少 */
const CHUNK = 1 << 20;
/** 在内存里拼的（没有文件夹选择器的浏览器）最多这么大 */
const MAX_IN_MEMORY = 1 << 30;

type Node =
  | { kind: "file"; name: string; file: File | null; entry: FileSystemFileEntry | null; size: number; mtime: number | null }
  | { kind: "dir"; name: string; entry: FileSystemDirectoryEntry; children: Map<string, Node> | null };

/** 名字能不能用在协议里（见 airhand_proto::files::valid_name） */
function validName(n: string): boolean {
  return n !== "" && n !== "." && n !== ".." && !/[/\\\0]/.test(n);
}

function fileOf(entry: FileSystemFileEntry): Promise<File> {
  return new Promise((resolve, reject) => entry.file(resolve, reject));
}

/** 目录下面的一层（readEntries 一次只给一部分，读到空为止） */
async function readDir(dir: FileSystemDirectoryEntry): Promise<FileSystemEntry[]> {
  const reader = dir.createReader();
  const out: FileSystemEntry[] = [];
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) => reader.readEntries(resolve, reject));
    if (batch.length === 0) return out;
    out.push(...batch);
  }
}

function entryOf(n: Node): FileEntry {
  return n.kind === "file"
    ? { path: [n.name], kind: "file", size: n.size, mtime: n.mtime }
    : { path: [n.name], kind: "dir", size: 0, mtime: null };
}

/** 这里复制的一组文件（见文件开头） */
export class LocalFiles {
  private readonly roots: Map<string, Node>;

  private constructor(roots: Map<string, Node>) {
    this.roots = roots;
  }

  /**
   * 粘贴、拖放带来的文件（要在事件里同步地调用：事件结束后 DataTransfer 就空了）。目录只有拖放才拿得到。
   * 一个都没有时为 null
   */
  static fromDataTransfer(dt: DataTransfer | null): LocalFiles | null {
    if (!dt) return null;
    const roots = new Map<string, Node>();
    for (const item of Array.from(dt.items)) {
      if (item.kind !== "file") continue;
      const entry = item.webkitGetAsEntry?.() ?? null;
      if (entry?.isDirectory) {
        if (validName(entry.name) && !roots.has(entry.name)) {
          roots.set(entry.name, { kind: "dir", name: entry.name, entry: entry as FileSystemDirectoryEntry, children: null });
        }
        continue;
      }
      const file = item.getAsFile();
      if (file && validName(file.name) && !roots.has(file.name)) {
        roots.set(file.name, { kind: "file", name: file.name, file, entry: null, size: file.size, mtime: file.lastModified });
      }
    }
    return roots.size > 0 ? new LocalFiles(roots) : null;
  }

  /** 公告里的那几项 */
  items(): FileEntry[] {
    return [...this.roots.values()].map(entryOf);
  }

  /** 有没有这一项（控制端服务问的时候找是哪一次复制的） */
  has(name: string): boolean {
    return this.roots.has(name);
  }

  private async node(path: string[]): Promise<Node | null> {
    let n = this.roots.get(path[0] ?? "") ?? null;
    for (const part of path.slice(1)) {
      if (!n || n.kind !== "dir") return null;
      n = (await this.children(n)).get(part) ?? null;
    }
    return n;
  }

  private async children(dir: Extract<Node, { kind: "dir" }>): Promise<Map<string, Node>> {
    if (dir.children) return dir.children;
    const map = new Map<string, Node>();
    for (const e of await readDir(dir.entry)) {
      if (!validName(e.name)) continue;
      if (e.isDirectory) {
        map.set(e.name, { kind: "dir", name: e.name, entry: e as FileSystemDirectoryEntry, children: null });
      } else if (e.isFile) {
        const file = await fileOf(e as FileSystemFileEntry).catch(() => null);
        if (file) map.set(e.name, { kind: "file", name: e.name, file, entry: e as FileSystemFileEntry, size: file.size, mtime: file.lastModified });
      }
    }
    dir.children = map;
    return map;
  }

  /** 列出目录（第一段是公告里的一项）下面的一层 */
  async list(path: string[]): Promise<FileEntry[]> {
    const n = await this.node(path);
    if (!n || n.kind !== "dir") throw "notFound";
    return [...(await this.children(n)).values()].map(entryOf).sort((a, b) => (a.path[0] < b.path[0] ? -1 : 1));
  }

  /** 从 offset 开始最多读 len 字节（比 len 短就是到末尾了） */
  async read(path: string[], offset: number, len: number): Promise<Uint8Array> {
    const n = await this.node(path);
    if (!n || n.kind !== "file") throw "notFound";
    const file = n.file ?? (n.entry ? await fileOf(n.entry) : null);
    if (!file) throw "notFound";
    if (file.size !== n.size) throw "changed";
    return new Uint8Array(await file.slice(offset, offset + len).arrayBuffer());
  }
}

/** 粘贴键（Ctrl+V、Mac 上的 ⌘V、Shift+Insert） */
export function isPasteKey(e: KeyboardEvent): boolean {
  return (e.code === "KeyV" && (e.ctrlKey || e.metaKey) && !e.altKey) || (e.code === "Insert" && e.shiftKey);
}

// ───────────────────────── 下载被控端复制的文件 ─────────────────────────

/** 要取下来的一项：对方那里的路径，大小 */
interface Wanted {
  path: string[];
  kind: "file" | "dir";
  size: number;
  mtime: number | null;
}

let nextFile = 1;

/** 列出整棵树（文件夹里的链接这里表示不了，跳过） */
async function plan(api: Backend, sid: number, clip: number, items: FileEntry[]): Promise<Wanted[]> {
  const out: Wanted[] = [];
  for (const item of items) {
    if (item.kind === "file") out.push({ path: item.path, kind: "file", size: item.size, mtime: item.mtime });
    if (item.kind !== "dir") continue;
    out.push({ path: item.path, kind: "dir", size: 0, mtime: null });
    for (const e of await api.remoteList(sid, clip, item.path, true)) {
      if (e.kind !== "file" && e.kind !== "dir") continue;
      out.push({ path: [...item.path, ...e.path], kind: e.kind, size: e.size, mtime: e.mtime });
    }
  }
  return out;
}

/** 取一个文件，一块一块交给 sink */
async function fetchFile(
  api: Backend,
  sid: number,
  clip: number,
  w: Wanted,
  sink: (chunk: Uint8Array) => Promise<void>,
  progress: (n: number) => void,
) {
  const file = nextFile++;
  try {
    let offset = 0;
    while (offset < w.size) {
      const chunk = await api.remoteRead(sid, clip, file, w.path, w.size, w.mtime, offset, Math.min(CHUNK, w.size - offset));
      if (chunk.length === 0) throw "io";
      await sink(chunk);
      offset += chunk.length;
      progress(chunk.length);
    }
  } finally {
    api.remoteClose(sid, file);
  }
}

type Picker = {
  showDirectoryPicker?: (o?: { mode?: "readwrite" }) => Promise<FileSystemDirectoryHandle>;
  showSaveFilePicker?: (o?: { suggestedName?: string }) => Promise<FileSystemFileHandle>;
};

/**
 * 把被控端复制的文件（clipboardFiles 的 items）取下来（见文件开头）。`progress(已取, 一共)` 按字节。
 * 用户取消了选择框时什么也不做（返回 false）
 */
export async function download(
  api: Backend,
  sid: number,
  clip: number,
  items: FileEntry[],
  progress: (done: number, total: number) => void,
): Promise<boolean> {
  const wanted = await plan(api, sid, clip, items);
  const total = wanted.reduce((n, w) => n + w.size, 0);
  let done = 0;
  const step = (n: number) => progress((done += n), total);
  progress(0, total);
  const picker = window as unknown as Picker;
  const single = wanted.length === 1 && wanted[0].kind === "file" ? wanted[0] : null;
  try {
    if (single && picker.showSaveFilePicker) {
      const handle = await picker.showSaveFilePicker({ suggestedName: single.path[single.path.length - 1] });
      const w = await handle.createWritable();
      try {
        await fetchFile(api, sid, clip, single, (c) => w.write(c as unknown as BufferSource), step);
        await w.close();
      } catch (e) {
        await w.abort();
        throw e;
      }
      return true;
    }
    if (!single && picker.showDirectoryPicker) {
      const root = await picker.showDirectoryPicker({ mode: "readwrite" });
      for (const want of wanted) {
        let dir = root;
        for (const part of want.path.slice(0, -1)) dir = await dir.getDirectoryHandle(part, { create: true });
        const name = want.path[want.path.length - 1];
        if (want.kind === "dir") {
          await dir.getDirectoryHandle(name, { create: true });
          continue;
        }
        const w = await (await dir.getFileHandle(name, { create: true })).createWritable();
        try {
          await fetchFile(api, sid, clip, want, (c) => w.write(c as unknown as BufferSource), step);
          await w.close();
        } catch (e) {
          await w.abort();
          throw e;
        }
      }
      return true;
    }
  } catch (e) {
    // 用户关掉了选择框
    if (e instanceof DOMException && e.name === "AbortError") return false;
    throw e;
  }
  // 没有选择器：在内存里拼
  if (total > MAX_IN_MEMORY) throw "tooLarge";
  if (single) {
    const parts: Uint8Array[] = [];
    await fetchFile(api, sid, clip, single, async (c) => void parts.push(c), step);
    save(new Blob(parts as BlobPart[]), single.path[single.path.length - 1]);
    return true;
  }
  const zip = new ZipWriter();
  for (const want of wanted) {
    const name = want.path.join("/");
    if (want.kind === "dir") {
      zip.dir(name);
      continue;
    }
    const parts: Uint8Array[] = [];
    await fetchFile(api, sid, clip, want, async (c) => void parts.push(c), step);
    zip.file(name, parts, want.mtime);
  }
  const first = items[0]?.path[0] ?? "files";
  save(zip.finish(), `${items.length === 1 ? first : "airhand-files"}.zip`);
  return true;
}

function save(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

// ───────────────────────── ZIP（不压缩） ─────────────────────────

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

export function crc32(parts: Uint8Array[]): number {
  let c = 0xffffffff;
  for (const p of parts) for (let i = 0; i < p.length; i++) c = CRC_TABLE[(c ^ p[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** DOS 时间、日期（ZIP 里的修改时间，本地时间，精确到 2 秒） */
function dosTime(ms: number | null): [number, number] {
  const d = new Date(ms ?? Date.now());
  const time = (d.getHours() << 11) | (d.getMinutes() << 5) | (d.getSeconds() >> 1);
  const date = ((Math.max(d.getFullYear(), 1980) - 1980) << 9) | ((d.getMonth() + 1) << 5) | d.getDate();
  return [time, date];
}

/** 不压缩的 ZIP（名字是 UTF-8），在内存里拼（见 download） */
export class ZipWriter {
  private parts: BlobPart[] = [];
  private central: Uint8Array[] = [];
  private offset = 0;
  private count = 0;

  dir(name: string) {
    this.entry(`${name}/`, [], null, 0x10);
  }

  file(name: string, data: Uint8Array[], mtime: number | null) {
    this.entry(name, data, mtime, 0);
  }

  private entry(name: string, data: Uint8Array[], mtime: number | null, attrs: number) {
    const nameBytes = new TextEncoder().encode(name);
    const size = data.reduce((n, p) => n + p.length, 0);
    const crc = crc32(data);
    const [time, date] = dosTime(mtime);
    const local = new DataView(new ArrayBuffer(30));
    local.setUint32(0, 0x04034b50, true);
    local.setUint16(4, 20, true);
    // 第 11 位：名字是 UTF-8
    local.setUint16(6, 0x0800, true);
    local.setUint16(8, 0, true);
    local.setUint16(10, time, true);
    local.setUint16(12, date, true);
    local.setUint32(14, crc, true);
    local.setUint32(18, size, true);
    local.setUint32(22, size, true);
    local.setUint16(26, nameBytes.length, true);
    local.setUint16(28, 0, true);
    this.parts.push(local.buffer, nameBytes as BlobPart, ...(data as BlobPart[]));
    const cd = new DataView(new ArrayBuffer(46));
    cd.setUint32(0, 0x02014b50, true);
    cd.setUint16(4, 20, true);
    cd.setUint16(6, 20, true);
    cd.setUint16(8, 0x0800, true);
    cd.setUint16(10, 0, true);
    cd.setUint16(12, time, true);
    cd.setUint16(14, date, true);
    cd.setUint32(16, crc, true);
    cd.setUint32(20, size, true);
    cd.setUint32(24, size, true);
    cd.setUint16(28, nameBytes.length, true);
    cd.setUint32(38, attrs, true);
    cd.setUint32(42, this.offset, true);
    this.central.push(new Uint8Array(cd.buffer), nameBytes);
    this.offset += 30 + nameBytes.length + size;
    this.count++;
  }

  finish(): Blob {
    const cdSize = this.central.reduce((n, p) => n + p.length, 0);
    const end = new DataView(new ArrayBuffer(22));
    end.setUint32(0, 0x06054b50, true);
    end.setUint16(8, this.count, true);
    end.setUint16(10, this.count, true);
    end.setUint32(12, cdSize, true);
    end.setUint32(16, this.offset, true);
    return new Blob([...this.parts, ...(this.central as BlobPart[]), end.buffer], { type: "application/zip" });
  }
}
