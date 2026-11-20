import { useEffect, useRef } from "react";
import { ConfirmDialog } from "./components/ConfirmDialog";
import { ConnectView } from "./components/ConnectView";
import { GridView } from "./components/GridView";
import { Popouts } from "./components/PopoutView";
import { SessionView } from "./components/SessionView";
import { TitleBar } from "./components/TitleBar";
import { WebUnsupported, webUnsupported } from "./components/WebUnsupported";
import { allowClose, applyFullscreen, closeTab, loadDevices, reopenLast } from "./lib/actions";
import { api } from "./lib/api";
import { supportedDecoders } from "./lib/codec";
import { setLang, useLang } from "./lib/i18n";
import { markPlatform } from "./lib/platform";
import { store, useStore } from "./lib/store";

markPlatform();

export function App() {
  // 语言切换时整个界面重新渲染
  useLang();
  // 网页版：不是安全页面（HTTPS / localhost）或浏览器太旧，没有视频解码可用
  const unsupported = webUnsupported();
  return unsupported ? <WebUnsupported reason={unsupported} /> : <Console />;
}

/**
 * 标签页的快捷键（桌面版；网页版里浏览器自己要用这些键）。
 * 正在看一台连着的电脑时一律不处理：键盘归对方（远程画面有焦点时按键根本到不了这里）。
 */
function shortcut(e: KeyboardEvent): boolean {
  if (api.kind !== "desktop" || !e.ctrlKey || e.altKey || e.metaKey) return false;
  const st = store.get();
  if (st.confirm || st.sessions.find((s) => s.id === st.view)?.state === "live") return false;
  const key = e.key.toLowerCase();
  if (e.key === "Tab" || e.key === "PageDown" || e.key === "PageUp") {
    store.cycle(e.shiftKey || e.key === "PageUp" ? -1 : 1);
  } else if (key === "t") {
    if (e.shiftKey) reopenLast();
    else store.newTab();
  } else if (key === "w" && !e.shiftKey) {
    closeTab(st.view);
  } else if (/^Digit[1-9]$/.test(e.code) && !e.shiftKey) {
    store.selectTab(Number(e.code.slice(5)));
  } else {
    return false;
  }
  return true;
}

function Console() {
  const view = useStore((s) => s.view);
  const sessions = useStore((s) => s.sessions);
  const immersive = useStore((s) => s.immersive);

  useEffect(() => {
    const unLang = api.onLangChanged(setLang);
    void api.appInfo().then((info) => {
      store.setApp(info);
      setLang(info.lang);
    });
    void api.serverStatus().then(store.setServer);
    void loadDevices();
    void supportedDecoders().then(store.setDecoders);
    const unStatus = api.onServerStatus(store.setServer);
    const unOnline = api.onHostOnline(store.hostOnline);
    // 关闭窗口（标题栏的关闭按钮、任务栏、Alt+F4、macOS 的红灯）：有会话时先确认
    const unClose = api.frame?.onCloseRequested(allowClose);
    const onKey = (e: KeyboardEvent) => {
      // 屏蔽 WebView 自带的刷新/查找/打印等快捷键，避免误触
      if (e.key === "F5" || ((e.ctrlKey || e.metaKey) && ["r", "p", "f", "g", "j", "u"].includes(e.key.toLowerCase()))) {
        e.preventDefault();
      } else if (shortcut(e)) {
        e.preventDefault();
      }
    };
    window.addEventListener("keydown", onKey);
    const noMenu = (e: MouseEvent) => {
      if (!(e.target instanceof HTMLInputElement)) e.preventDefault();
    };
    window.addEventListener("contextmenu", noMenu);
    // 网页版：关闭或刷新标签页会断开所有会话，先让浏览器确认一下
    const confirmLeave = (e: BeforeUnloadEvent) => {
      if (api.kind === "web" && store.get().sessions.length > 0) e.preventDefault();
    };
    window.addEventListener("beforeunload", confirmLeave);
    // 全屏被系统退出（长按 Esc、系统快捷键……）时同步状态
    const exitImmersive = () => {
      if (store.get().immersive) store.setImmersive(false);
    };
    const onFullscreenChange = () => {
      if (!document.fullscreenElement) exitImmersive();
    };
    document.addEventListener("fullscreenchange", onFullscreenChange);
    const unWinFs = api.onWindowFullscreenExit(exitImmersive);
    return () => {
      unStatus();
      unOnline();
      unLang();
      unClose?.();
      unWinFs();
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("contextmenu", noMenu);
      window.removeEventListener("beforeunload", confirmLeave);
      document.removeEventListener("fullscreenchange", onFullscreenChange);
    };
  }, []);

  // 退出沉浸模式（包括关掉了全屏中的会话、切到了别的页面）时，窗口也退出全屏
  const wasImmersive = useRef(immersive);
  useEffect(() => {
    if (wasImmersive.current && !immersive) void applyFullscreen(false);
    wasImmersive.current = immersive;
  }, [immersive]);

  const active = sessions.find((s) => s.id === view);

  return (
    <div className={`app ${immersive ? "immersive" : ""}`}>
      {!immersive && <TitleBar />}
      <main className="main">
        {view === "grid" && sessions.length > 0 ? (
          <GridView />
        ) : active ? (
          <SessionView key={active.id} s={active} />
        ) : (
          <ConnectView />
        )}
      </main>
      <ConfirmDialog />
      {/* 放在单独窗口里的显示器：界面渲染到那些窗口里（portal） */}
      <Popouts />
    </div>
  );
}
