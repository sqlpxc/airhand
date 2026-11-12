import { useEffect, useLayoutEffect, useRef, type ReactNode } from "react";

/** 弹出层底边离窗口底边至少留这么多 */
const BOTTOM_MARGIN = 12;

/**
 * 弹出层（工具栏菜单、画质面板、主菜单）：点击外面或按 Esc 时关闭。
 *
 * 放在 `.menu-anchor` 里，和打开它的按钮在一起：点那个按钮不算"点击外面"，
 * 由按钮自己切换开 / 关（否则这里先关掉，按钮的 click 又把它打开）。
 *
 * 弹出层下面还剩多高写在 CSS 变量 `--pop-max-h` 里（窗口变大变小时更新）：内容多的弹出层（画质面板）
 * 用它做 max-height、放不下就滚动，窗口很矮时也不会有一截跑到窗口外面。
 */
export function Popover({ className, onClose, children }: { className: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const close = (e: PointerEvent) => {
      const anchor = ref.current?.parentElement;
      if (anchor && !anchor.contains(e.target as Node)) onClose();
    };
    const esc = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // 推迟到下一轮再开始监听：打开弹出层的那次点击不算"点击外面"
    const timer = window.setTimeout(() => window.addEventListener("pointerdown", close), 0);
    window.addEventListener("keydown", esc);
    return () => {
      clearTimeout(timer);
      window.removeEventListener("pointerdown", close);
      window.removeEventListener("keydown", esc);
    };
  }, [onClose]);
  useLayoutEffect(() => {
    const el = ref.current;
    const anchor = el?.parentElement;
    if (!el || !anchor) return;
    const fit = () => {
      // offsetTop 是相对 .menu-anchor 的布局位置，不受弹出动画（translateY）影响；
      // 锚点用 getBoundingClientRect：全屏时悬浮工具栏是用 transform 放下来的
      const top = anchor.getBoundingClientRect().top + el.offsetTop;
      el.style.setProperty("--pop-max-h", `${Math.max(160, window.innerHeight - top - BOTTOM_MARGIN)}px`);
    };
    fit();
    window.addEventListener("resize", fit);
    return () => window.removeEventListener("resize", fit);
  }, []);
  return (
    // 标签栏整个是窗口的拖动区域：弹出层里的空白处不能拖动窗口
    <div className={`popover ${className}`} ref={ref} data-tauri-drag-region="false">
      {children}
    </div>
  );
}
