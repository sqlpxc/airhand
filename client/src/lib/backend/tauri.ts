// 桌面版：控制台是 Tauri 窗口，控制端服务在同一个进程里（client/src-tauri）。
//
// 会话和网页版一样走 WebSocket（console.ts）：连的是本程序只监听 127.0.0.1 的服务，地址由 `console_url` 命令给出，
// 带着随机令牌（见 src-tauri/src/console.rs）。窗口、语言、我的设备这些还是 Tauri 的命令和事件。
// 剪贴板由 Rust 同步这台电脑的。
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { Backend, SavedDevice, WindowFrame } from "../api";
import { ConsoleLink } from "../console";
import type { Lang } from "../i18n";

/** 把异步注册的监听变成同步返回的取消函数 */
function unlistenLater(p: Promise<UnlistenFn>): () => void {
  return () => void p.then((f) => f());
}

function windowFrame(): WindowFrame {
  const w = getCurrentWindow();
  return {
    minimize: () => w.minimize(),
    toggleMaximize: () => w.toggleMaximize(),
    isMaximized: () => w.isMaximized(),
    onResized: (cb) => unlistenLater(w.onResized(() => cb())),
    close: () => w.close(),
    destroy: () => w.destroy(),
    onCloseRequested: (cb) =>
      unlistenLater(
        w.onCloseRequested((e) => {
          if (cb()) return;
          e.preventDefault();
          // 例如在任务栏上关闭最小化的窗口：先把窗口拿出来，用户才看得到询问
          void w.unminimize().then(() => w.setFocus());
        }),
      ),
  };
}

class DesktopConsole extends ConsoleLink implements Backend {
  readonly kind = "desktop" as const;
  readonly frame = windowFrame();

  constructor() {
    super(() => invoke<string>("console_url"));
  }

  localClipboardText() {
    return invoke<string>("clipboard_text");
  }

  switchToHost() {
    return invoke<void>("switch_to_host");
  }

  loadDevices() {
    return invoke<SavedDevice[]>("devices_load");
  }

  saveDevices(devices: SavedDevice[]) {
    return invoke<void>("devices_save", { devices });
  }

  setLanguage(lang: Lang) {
    return invoke<void>("set_language", { lang });
  }

  onLangChanged(cb: (lang: Lang) => void) {
    return unlistenLater(listen<Lang>("lang-changed", (e) => cb(e.payload)));
  }

  // 不直接用 setFullscreen：无边框窗口全屏时还要停用拖动边缘改大小（见 src-tauri/src/fullscreen.rs）
  setWindowFullscreen(on: boolean) {
    return invoke<void>("set_fullscreen", { on });
  }

  onWindowFullscreenExit(cb: () => void) {
    const w = getCurrentWindow();
    return unlistenLater(
      w.onResized(async () => {
        if (!(await w.isFullscreen())) cb();
      }),
    );
  }
}

export function tauriBackend(): Backend {
  return new DesktopConsole();
}
