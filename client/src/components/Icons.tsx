// 内联线性图标（24×24，currentColor），不依赖任何图标库
import type { SVGProps } from "react";

type P = SVGProps<SVGSVGElement> & { size?: number };

function base({ size = 18, ...rest }: P, children: React.ReactNode) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
      {...rest}
    >
      {children}
    </svg>
  );
}

export const IconPlus = (p: P) => base(p, <path d="M12 5v14M5 12h14" />);
/** 鼠标（锁定鼠标） */
export const IconMouse = (p: P) =>
  base(
    p,
    <>
      <rect x="6" y="3" width="12" height="18" rx="6" />
      <path d="M12 7v4" />
    </>,
  );
export const IconGrid = (p: P) =>
  base(
    p,
    <>
      <rect x="3.5" y="3.5" width="7" height="7" rx="1.6" />
      <rect x="13.5" y="3.5" width="7" height="7" rx="1.6" />
      <rect x="3.5" y="13.5" width="7" height="7" rx="1.6" />
      <rect x="13.5" y="13.5" width="7" height="7" rx="1.6" />
    </>,
  );
export const IconMonitor = (p: P) =>
  base(
    p,
    <>
      <rect x="2.5" y="3.5" width="19" height="13" rx="2.2" />
      <path d="M8 20.5h8M12 16.5v4" />
    </>,
  );
export const IconClose = (p: P) => base(p, <path d="M6 6l12 12M18 6L6 18" />);
export const IconSliders = (p: P) =>
  base(
    p,
    <>
      <path d="M4 6h10M18 6h2M4 12h4M12 12h8M4 18h12M20 18h0" />
      <circle cx="16" cy="6" r="2" />
      <circle cx="10" cy="12" r="2" />
      <circle cx="18" cy="18" r="2" />
    </>,
  );
export const IconKeyboard = (p: P) =>
  base(
    p,
    <>
      <rect x="2.5" y="5.5" width="19" height="13" rx="2.2" />
      <path d="M6.5 9.5h.01M10 9.5h.01M13.5 9.5h.01M17 9.5h.01M6.5 12.5h.01M17 12.5h.01M8 15.5h8M10 12.5h4" />
    </>,
  );
export const IconClipboard = (p: P) =>
  base(
    p,
    <>
      <rect x="5.5" y="4.5" width="13" height="16" rx="2" />
      <path d="M9 4.5V3.8c0-.7.5-1.3 1.2-1.3h3.6c.7 0 1.2.6 1.2 1.3v.7M9 10h6M9 14h4" />
    </>,
  );
export const IconVolume = (p: P) =>
  base(
    p,
    <>
      <path d="M4 9.5h3.2L12 5.5v13l-4.8-4H4z" />
      <path d="M15.5 9.2a4 4 0 0 1 0 5.6M18.3 6.6a7.6 7.6 0 0 1 0 10.8" />
    </>,
  );
export const IconVolumeOff = (p: P) =>
  base(
    p,
    <>
      <path d="M4 9.5h3.2L12 5.5v13l-4.8-4H4z" />
      <path d="M16 9.5l5 5M21 9.5l-5 5" />
    </>,
  );
export const IconExpand = (p: P) => base(p, <path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5" />);
export const IconShrink = (p: P) => base(p, <path d="M9 4v5H4M15 4v5h5M9 20v-5H4M15 20v-5h5" />);
export const IconPower = (p: P) => base(p, <path d="M12 3v8M6.4 6.6a8 8 0 1 0 11.2 0" />);
export const IconRefresh = (p: P) =>
  base(p, <path d="M20 11a8 8 0 0 0-14.6-4.5M4 4v4h4M4 13a8 8 0 0 0 14.6 4.5M20 20v-4h-4" />);
export const IconChevron = (p: P) => base(p, <path d="M6 9l6 6 6-6" />);
export const IconSwap = (p: P) => base(p, <path d="M7 4L3 8l4 4M3 8h14M17 20l4-4-4-4M21 16H7" />);
export const IconFit = (p: P) =>
  base(
    p,
    <>
      <rect x="3" y="5" width="18" height="14" rx="2" />
      <path d="M8 12h8M8 12l2-2M8 12l2 2M16 12l-2-2M16 12l-2 2" />
    </>,
  );
export const IconBolt = (p: P) => base(p, <path d="M13 2.5L4.5 13.5H12l-1 8 8.5-11H12l1-8z" />);
export const IconCpu = (p: P) =>
  base(
    p,
    <>
      <rect x="6" y="6" width="12" height="12" rx="2" />
      <path d="M9 2.5v3M15 2.5v3M9 18.5v3M15 18.5v3M2.5 9h3M2.5 15h3M18.5 9h3M18.5 15h3" />
    </>,
  );
/** 说明（ⓘ）：鼠标停在上面看详细说明 */
export const IconInfo = (p: P) =>
  base(
    p,
    <>
      <circle cx="12" cy="12" r="9" />
      <path d="M12 11v5.5M12 7.5h.01" />
    </>,
  );
export const IconWarn = (p: P) =>
  base(p, <path d="M12 3.5l9.5 16.5h-19L12 3.5zM12 10v4.5M12 17.5h.01" />);
export const IconHistory = (p: P) =>
  base(p, <path d="M3.5 12a8.5 8.5 0 1 0 2.5-6M3.5 4v4h4M12 7.5V12l3 2" />);
export const IconArrowRight = (p: P) => base(p, <path d="M5 12h14M13 6l6 6-6 6" />);
export const IconPencil = (p: P) => base(p, <path d="M4 20h4L19 9l-4-4L4 16v4zM13.5 6.5l4 4" />);
/** 设置（标签栏右侧的主菜单） */
export const IconSettings = (p: P) =>
  base(
    p,
    <>
      <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
      <circle cx="12" cy="12" r="3" />
    </>,
  );
/** 中转服务器 */
export const IconServer = (p: P) =>
  base(
    p,
    <>
      <rect x="3.5" y="4" width="17" height="6.5" rx="2" />
      <rect x="3.5" y="13.5" width="17" height="6.5" rx="2" />
      <path d="M7.5 7.25h.01M7.5 16.75h.01M11 7.25h5.5M11 16.75h5.5" />
    </>,
  );
export const IconCopy = (p: P) =>
  base(
    p,
    <>
      <rect x="8.5" y="8.5" width="12" height="12" rx="2.2" />
      <path d="M15.5 8.5V5.7c0-1.2-1-2.2-2.2-2.2H5.7c-1.2 0-2.2 1-2.2 2.2v7.6c0 1.2 1 2.2 2.2 2.2h2.8" />
    </>,
  );
export const IconCheck = (p: P) => base(p, <path d="M5 12.5l4.5 4.5L19 7.5" />);
export const IconSearch = (p: P) =>
  base(
    p,
    <>
      <circle cx="11" cy="11" r="6.5" />
      <path d="M16 16l4.5 4.5" />
    </>,
  );

// 窗口按钮：细线图标（10×10），与 Windows 11 的标题栏按钮一致
function win(children: React.ReactNode) {
  return (
    <svg width={10} height={10} viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth={1} aria-hidden>
      {children}
    </svg>
  );
}
export const IconWinMinimize = () => win(<path d="M0 5h10" />);
export const IconWinMaximize = () => win(<rect x="0.5" y="0.5" width="9" height="9" rx="1.2" />);
export const IconWinRestore = () =>
  win(
    <>
      <rect x="0.5" y="2.5" width="7" height="7" rx="1.2" />
      <path d="M2.5 2.5v-.8c0-.66.54-1.2 1.2-1.2h4.6c.66 0 1.2.54 1.2 1.2v4.6c0 .66-.54 1.2-1.2 1.2h-.8" />
    </>,
  );
export const IconWinClose = () => win(<path d="M.5.5l9 9M9.5.5l-9 9" />);
