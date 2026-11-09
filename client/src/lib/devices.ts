// "我的设备"的列表操作（纯函数）。保存在 config.toml 的 [[device]] 里（见 Rust 端 devices.rs），
// 每次变化都把整个列表交给 api.saveDevices（见 actions.ts）。
import type { QualitySettings, ResumeToken, SavedDevice } from "./api";

/** 最多保存多少台（与 Rust 端 devices::MAX 相同） */
export const MAX_DEVICES = 50;

/** 控制码显示成 "123 456" */
export const fmtCode = (code: string) => (code.length === 6 ? `${code.slice(0, 3)} ${code.slice(3)}` : code);

/** 显示的名字：自己起的名字 > 对方的设备名 > 控制码 */
export const deviceTitle = (d: SavedDevice) => d.alias?.trim() || d.name.trim() || fmtCode(d.code);

/** 最近连过的在前面 */
export const sortDevices = (list: SavedDevice[]) => [...list].sort((a, b) => b.lastUsed - a.lastUsed);

/** 连上了：记下（或更新）这台电脑。自己起的名字保留，对方的名字、系统、凭证、画质用这次的 */
export function remember(
  list: SavedDevice[],
  seen: {
    code: string;
    name: string;
    os: string;
    resume: ResumeToken | null;
    quality: QualitySettings | null;
    hostKey?: string | null;
  },
  now: number,
): SavedDevice[] {
  const old = list.find((d) => d.code === seen.code);
  const d: SavedDevice = {
    code: seen.code,
    alias: old?.alias ?? null,
    name: seen.name.trim() || old?.name || "",
    os: seen.os.trim() || old?.os || "",
    resume: seen.resume ?? old?.resume ?? null,
    lastUsed: now,
    quality: seen.quality ?? old?.quality ?? null,
    // 对方的公钥：连上时确认过的（第一次连上、或者用户信任了新的身份）代替旧的
    hostKey: seen.hostKey ?? old?.hostKey ?? null,
  };
  return sortDevices([d, ...list.filter((x) => x.code !== seen.code)]).slice(0, MAX_DEVICES);
}

/** 改一台的名字或画质（列表里没有就什么也不变） */
export function patchDevice(list: SavedDevice[], code: string, patch: Partial<Pick<SavedDevice, "alias" | "quality">>) {
  if (!list.some((d) => d.code === code)) return list;
  return list.map((d) => (d.code === code ? { ...d, ...patch } : d));
}

/** 重命名：和对方的设备名一样、或者清空，就是不另起名字 */
export function renameDevice(list: SavedDevice[], code: string, name: string) {
  const d = list.find((x) => x.code === code);
  if (!d) return list;
  const n = name.trim();
  return patchDevice(list, code, { alias: n && n !== d.name ? n : null });
}

export const forgetDevice = (list: SavedDevice[], code: string) => list.filter((d) => d.code !== code);

/** 撤销移除：放回原来的位置（按最后连接时间） */
export const restoreDevice = (list: SavedDevice[], d: SavedDevice) =>
  sortDevices([d, ...list.filter((x) => x.code !== d.code)]).slice(0, MAX_DEVICES);

/** 搜索：名字（不分大小写）或控制码（可以带空格） */
export function matchDevice(d: SavedDevice, query: string): boolean {
  const q = query.trim().toLowerCase();
  if (!q) return true;
  const digits = q.replace(/\s/g, "");
  return (
    deviceTitle(d).toLowerCase().includes(q) ||
    d.name.toLowerCase().includes(q) ||
    (/^\d+$/.test(digits) && d.code.includes(digits))
  );
}

/** "刚刚" / "5 分钟前" / "3 天前" / 很久以前的写日期 */
export function relativeTime(ms: number, now: number, locale: string): string {
  if (!ms) return "";
  const s = Math.max(0, (now - ms) / 1000);
  const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  if (s < 60) return rtf.format(0, "second");
  if (s < 3600) return rtf.format(-Math.floor(s / 60), "minute");
  if (s < 86400) return rtf.format(-Math.floor(s / 3600), "hour");
  if (s < 86400 * 30) return rtf.format(-Math.floor(s / 86400), "day");
  return new Date(ms).toLocaleDateString(locale);
}
