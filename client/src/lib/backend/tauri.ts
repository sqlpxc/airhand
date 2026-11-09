// 桌面版：控制台是 Tauri 窗口，控制端服务在同一个进程里（client/src-tauri/src/lib.rs 的命令）。
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { AppInfo, Backend, Command, SavedDevice, ServerState, SessionEvent, SessionInfo, WindowFrame } from "../api";
import type { Lang } from "../i18n";
import { AckCoalescer } from "../ack";
import { videoSeq } from "../media";

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

/** 对会话的操作：Rust 的 `ctrl` 命令（和网页版、Android 一样的 Command） */
function ctrl(command: Command): Promise<void> {
  return invoke("ctrl", { command });
}

export function tauriBackend(): Backend {
  return {
    frame: windowFrame(),
    kind: "desktop",
    appInfo: () => invoke<AppInfo>("app_info"),
    serverStatus: () => invoke<ServerState>("server_status"),
    onServerStatus: (cb) => unlistenLater(listen<ServerState>("server-status", (e) => cb(e.payload))),
    connect: async (code, resume, settings, sink, clipboard) => {
      let sid: number | null = null;
      let unacked: number | null = null;
      let closed = false;
      // 确认是累积的：连着到的几帧只确认最后一帧，少几次 IPC（见 ack.ts）
      const acks = new AckCoalescer((seq) => {
        if (sid !== null) void ctrl({ cmd: "ack", sid, seq }).catch(() => {});
        else unacked = seq;
      });
      const video = new Channel<ArrayBuffer>();
      video.onmessage = (buf) => {
        if (closed) return;
        // 画面到了 WebView 才确认（把 IPC 这一段也算进在途帧控制）
        const seq = videoSeq(new DataView(buf));
        if (seq !== null) acks.push(seq);
        sink.packet(new Uint8Array(buf));
      };
      const events = new Channel<SessionEvent>();
      events.onmessage = (ev) => {
        if (ev.type === "closed") {
          closed = true;
          acks.dispose();
        }
        sink.event(ev);
      };
      let info: SessionInfo;
      try {
        info = await invoke<SessionInfo>("ctrl_connect", { code, resume, settings, video, events, clipboard });
      } catch (e) {
        acks.dispose();
        throw e;
      }
      sid = info.sid;
      // 连上之前就收到的画面：现在知道 sid 了，马上补发确认（不等合并）
      if (!closed && unacked !== null) void ctrl({ cmd: "ack", sid, seq: unacked }).catch(() => {});
      return info;
    },
    onHostOnline: (cb) => unlistenLater(listen<number>("host-online", (e) => cb(e.payload))),
    command: ctrl,
    localClipboardText: () => invoke<string>("clipboard_text"),
    switchToHost: () => invoke("switch_to_host"),
    loadDevices: () => invoke<SavedDevice[]>("devices_load"),
    saveDevices: (devices) => invoke("devices_save", { devices }),
    setLanguage: (lang) => invoke("set_language", { lang }),
    onLangChanged: (cb) => unlistenLater(listen<Lang>("lang-changed", (e) => cb(e.payload))),
    // 不直接用 setFullscreen：无边框窗口全屏时还要停用拖动边缘改大小（见 src-tauri/src/fullscreen.rs）
    setWindowFullscreen: (on) => invoke("set_fullscreen", { on }),
    onWindowFullscreenExit: (cb) => {
      const w = getCurrentWindow();
      return unlistenLater(
        w.onResized(async () => {
          if (!(await w.isFullscreen())) cb();
        }),
      );
    },
  };
}
