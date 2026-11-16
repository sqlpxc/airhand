// 同时看对方的好几个显示器（多屏，见 airhand_proto::session 的"多屏"）时画面怎么摆：
// 按对方的显示器在桌面上的位置排（鼠标从一块移到相邻的一块时，在这边也是从一块画面移到旁边那一块），
// 整体等比缩放到显示区域里，每一块的边对齐到整数设备像素、相邻的两块正好接上。
// 每一路画面在自己那一块里和只看一个显示器时一样显示（fit.ts：差不多大时 1:1，否则等比缩放），
// 向被控端要的分辨率也按那一块的大小算（viewMaxShort）。
import type { DisplayInfo } from "./api";

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

type Placed = Pick<DisplayInfo, "x" | "y" | "width" | "height">;

/** 两块有没有重叠（只碰到边不算） */
function overlaps(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/**
 * 几个显示器在桌面上怎么摆（对方的坐标，物理像素），原点挪到左上角。位置对不上（有重叠，例如对方报的坐标和大小
 * 不是一种单位）时从左到右排成一排、顶端对齐
 */
export function placeDisplays(ds: Placed[]): Rect[] {
  const rects = ds.map((d) => ({ x: d.x, y: d.y, w: Math.max(1, d.width), h: Math.max(1, d.height) }));
  const clash = rects.some((a, i) => rects.some((b, j) => j > i && overlaps(a, b)));
  if (clash) {
    let x = 0;
    return rects.map((r) => {
      const placed = { x, y: 0, w: r.w, h: r.h };
      x += r.w;
      return placed;
    });
  }
  const left = Math.min(...rects.map((r) => r.x));
  const top = Math.min(...rects.map((r) => r.y));
  return rects.map((r) => ({ x: r.x - left, y: r.y - top, w: r.w, h: r.h }));
}

/**
 * 摆好的几块（`placed`，对方的像素）放进 `area`（设备像素；null = 原始大小：对方的一个像素是一个设备像素）里：
 * 整体等比缩放，每一块的边按缩放后的位置四舍五入到整数设备像素，相邻的两块正好接上、不留缝也不重叠。
 * 返回每一块（相对整体的左上角，设备像素）、整体的大小和缩放比例
 */
export function layoutTiles(placed: Rect[], area: { w: number; h: number } | null): { tiles: Rect[]; w: number; h: number; scale: number } {
  if (placed.length === 0) return { tiles: [], w: 0, h: 0, scale: 1 };
  const bw = Math.max(...placed.map((r) => r.x + r.w));
  const bh = Math.max(...placed.map((r) => r.y + r.h));
  const scale = area && area.w > 0 && area.h > 0 ? Math.min(area.w / bw, area.h / bh) : 1;
  const at = (v: number) => Math.round(v * scale);
  const tiles = placed.map((r) => {
    const x = at(r.x);
    const y = at(r.y);
    return { x, y, w: Math.max(1, at(r.x + r.w) - x), h: Math.max(1, at(r.y + r.h) - y) };
  });
  return { tiles, w: Math.max(1, at(bw)), h: Math.max(1, at(bh)), scale };
}

/**
 * 在 `rects`（屏幕坐标）里找点 (`x`, `y`) 落在哪一块上：在哪一块里面就是哪一块，落在缝里、外面时找最近的一块。
 * 返回第几块和在那一块里的归一化坐标（0..1）；没有一块时为 null
 */
export function hitTest(
  rects: { left: number; top: number; width: number; height: number }[],
  x: number,
  y: number,
): { index: number; x: number; y: number } | null {
  let best = -1;
  let bestDist = Infinity;
  for (let i = 0; i < rects.length; i++) {
    const r = rects[i];
    const dx = Math.max(r.left - x, 0, x - (r.left + r.width));
    const dy = Math.max(r.top - y, 0, y - (r.top + r.height));
    const d = dx * dx + dy * dy;
    if (d < bestDist) {
      bestDist = d;
      best = i;
    }
  }
  if (best < 0) return null;
  const r = rects[best];
  const clamp = (v: number) => Math.min(1, Math.max(0, v));
  return {
    index: best,
    x: clamp((x - r.left) / Math.max(1, r.width)),
    y: clamp((y - r.top) / Math.max(1, r.height)),
  };
}
