// 标签栏：也是无边框窗口的标题栏。
//
// [Logo] [多屏总览] [会话标签…] [新建连接] [+]  ……空白（拖动窗口 / 双击最大化）……  [服务器状态] [设置] [— □ ×]
//
// macOS 上最左边是系统的红绿灯按钮（没有 Logo），右边没有窗口按钮。能点的按钮都在右边，不和红绿灯挤在一起。
//
// 和浏览器一样：按下就切换、拖动排序、中键关闭、关掉当前标签后显示右边的。
// 空白处靠 Tauri 的 data-tauri-drag-region="deep" 拖动窗口：按钮、标签（role="tab"）不会触发拖动。
import { useEffect, useRef, useState, type PointerEvent as ReactPointerEvent } from "react";
import { api } from "../lib/api";
import { closeTab, fmtCode } from "../lib/actions";
import { useT } from "../lib/i18n";
import { sessionStateText } from "../lib/messages";
import { NATIVE_WINDOW_BUTTONS } from "../lib/platform";
import type { RemoteSession } from "../lib/session";
import { showGrid, store, useSession, useSessionTitle, useStore } from "../lib/store";
import { ServerIndicator, SettingsButton } from "./AppMenu";
import {
  IconClose,
  IconGrid,
  IconMonitor,
  IconPlus,
  IconWarn,
  IconWinClose,
  IconWinMaximize,
  IconWinMinimize,
  IconWinRestore,
} from "./Icons";
import { Logo } from "./Logo";

export function TitleBar() {
  return (
    <header className={`titlebar ${NATIVE_WINDOW_BUTTONS ? "native-buttons" : ""}`} data-tauri-drag-region="deep">
      {/* 只是标识，不能点（也是拖动窗口的地方）；macOS 这里是红绿灯 */}
      {!NATIVE_WINDOW_BUTTONS && (
        <div className="brand" aria-hidden>
          <Logo size={18} />
        </div>
      )}
      <TabStrip />
      <div className="title-actions">
        <ServerIndicator />
        <SettingsButton />
      </div>
      {api.frame && !NATIVE_WINDOW_BUTTONS && <WindowButtons />}
    </header>
  );
}

// ───────────────────────── 标签 ─────────────────────────

/** 拖动多远才算拖动（不然就只是点一下） */
const DRAG_THRESHOLD = 4;

function TabStrip() {
  const t = useT();
  const sessions = useStore((s) => s.sessions);
  const view = useStore((s) => s.view);
  const draft = useStore((s) => s.draft);
  const grid = useStore(showGrid);
  const strip = useRef<HTMLDivElement>(null);

  // 标签放不下时可以用滚轮横向滚动；切换后把当前标签滚到看得见的地方
  useEffect(() => {
    const el = strip.current!;
    const wheel = (e: WheelEvent) => {
      if (el.scrollWidth <= el.clientWidth || Math.abs(e.deltaX) > Math.abs(e.deltaY)) return;
      e.preventDefault();
      el.scrollLeft += e.deltaY;
    };
    el.addEventListener("wheel", wheel, { passive: false });
    return () => el.removeEventListener("wheel", wheel);
  }, []);
  useEffect(() => {
    strip.current?.querySelector(".tab.active")?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [view, sessions.length]);

  /** 按下会话标签：切过去，按住拖动就排序 */
  const press = (e: ReactPointerEvent<HTMLElement>, id: string) => {
    if (e.button !== 0) return;
    store.setView(id);
    const el = e.currentTarget;
    let startX = e.clientX;
    let dragging = false;
    const move = (ev: PointerEvent) => {
      let dx = ev.clientX - startX;
      if (!dragging) {
        if (Math.abs(dx) < DRAG_THRESHOLD) return;
        dragging = true;
        el.classList.add("dragging");
      }
      // 越过相邻标签的一半就交换位置。offsetLeft 不受 transform 影响，是标签本来的位置
      const tabs = [...el.parentElement!.querySelectorAll<HTMLElement>(".tab[data-session]")];
      const i = tabs.indexOf(el);
      const next = tabs[i + 1];
      const prev = tabs[i - 1];
      if (next) {
        const shift = next.offsetLeft + next.offsetWidth - (el.offsetLeft + el.offsetWidth);
        if (dx > shift / 2) {
          store.move(id, i + 1);
          startX += shift;
          dx -= shift;
        }
      }
      if (prev) {
        const shift = el.offsetLeft - prev.offsetLeft;
        if (dx < -shift / 2) {
          store.move(id, i - 1);
          startX -= shift;
          dx += shift;
        }
      }
      // 不拖出标签栏
      const first = tabs[0];
      const last = tabs[tabs.length - 1];
      const min = first.offsetLeft - el.offsetLeft;
      const max = last.offsetLeft + last.offsetWidth - (el.offsetLeft + el.offsetWidth);
      el.style.transform = `translateX(${Math.max(min, Math.min(max, dx))}px)`;
    };
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
      window.removeEventListener("pointercancel", up);
      el.style.transform = "";
      el.classList.remove("dragging");
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    window.addEventListener("pointercancel", up);
  };

  return (
    <div className="tabstrip" ref={strip} role="tablist">
      {grid && (
        <div
          role="tab"
          aria-selected={view === "grid"}
          className={`tab pinned ${view === "grid" ? "active" : ""}`}
          title={t("tabs.grid")}
          onPointerDown={(e) => e.button === 0 && store.setView("grid")}
          onMouseDown={keepFocus}
        >
          <IconGrid size={15} />
          <span className="tab-count">{sessions.length}</span>
        </div>
      )}
      {sessions.map((s) => (
        <SessionTab key={s.id} s={s} active={view === s.id} onPress={press} />
      ))}
      {draft && (
        <div
          role="tab"
          aria-selected={view === "new"}
          className={`tab draft ${view === "new" ? "active" : ""}`}
          title={t("tabs.new")}
          onPointerDown={(e) => e.button === 0 && store.setView("new")}
          {...middleClickCloses("new")}
        >
          <span className="tab-icon">{draft.busy ? <span className="spinner tiny" /> : <IconMonitor size={15} />}</span>
          <span className="tab-title">{t("tabs.new")}</span>
          {/* 没有会话时它是唯一的标签，不能关 */}
          {sessions.length > 0 && <CloseButton view="new" title={t("tabs.closeDraft")} />}
        </div>
      )}
      <button className="tab-add" title={t("tabs.newTip")} onMouseDown={keepFocus} onClick={store.newTab}>
        <IconPlus size={16} />
      </button>
    </div>
  );
}

const BUSY = new Set(["connecting", "reconnecting", "approving"]);

function SessionTab({
  s,
  active,
  onPress,
}: {
  s: RemoteSession;
  active: boolean;
  onPress: (e: ReactPointerEvent<HTMLElement>, id: string) => void;
}) {
  const t = useT();
  useSession(s);
  const name = useSessionTitle(s);
  return (
    <div
      role="tab"
      aria-selected={active}
      data-session={s.id}
      className={`tab ${active ? "active" : ""} ${s.state}`}
      title={`${name} · ${fmtCode(s.code)}\n${sessionStateText(s)}`}
      onPointerDown={(e) => onPress(e, s.id)}
      {...middleClickCloses(s.id)}
    >
      <span className="tab-icon">
        {BUSY.has(s.state) ? (
          <span className="spinner tiny" />
        ) : s.state === "closed" ? (
          <IconWarn size={14} className="tab-warn" />
        ) : (
          <span className={`avatar tiny ${s.state}`}>{name.slice(0, 1).toUpperCase()}</span>
        )}
      </span>
      <span className="tab-title">{name}</span>
      <CloseButton view={s.id} title={t("tabs.closeSession")} />
    </div>
  );
}

function CloseButton({ view, title }: { view: string; title: string }) {
  return (
    <button
      className="tab-close"
      title={title}
      // 不要触发标签的切换和拖动
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={keepFocus}
      onClick={(e) => {
        e.stopPropagation();
        closeTab(view);
      }}
    >
      <IconClose size={12} />
    </button>
  );
}

/**
 * 按下标签栏里的东西时不要改变焦点。标签在 pointerdown 时就切换了，新页面会把焦点放到远程画面 / 控制码输入框，
 * 之后 mousedown 的默认行为又会把焦点移走（移到 body），按键就到不了远程电脑了
 */
const keepFocus = (e: React.MouseEvent) => e.preventDefault();

/** 中键点标签 = 关闭（并且不出现中键自动滚动） */
const middleClickCloses = (view: string) => ({
  onMouseDown: keepFocus,
  onAuxClick: (e: React.MouseEvent) => {
    if (e.button === 1) closeTab(view);
  },
});

// ───────────────────────── 窗口按钮 ─────────────────────────

function WindowButtons() {
  const t = useT();
  const frame = api.frame!;
  const [maximized, setMaximized] = useState(false);
  useEffect(() => {
    const update = () =>
      void frame.isMaximized().then((m) => {
        setMaximized(m);
        // 没有最大化时 Linux 上画一圈细边框（系统不给无边框窗口画阴影）
        document.documentElement.classList.toggle("maximized", m);
      });
    update();
    return frame.onResized(update);
  }, [frame]);
  return (
    <div className="win-buttons">
      <button className="win-btn" title={t("window.minimize")} onMouseDown={keepFocus} onClick={() => void frame.minimize()}>
        <IconWinMinimize />
      </button>
      <button
        className="win-btn"
        title={maximized ? t("window.restore") : t("window.maximize")}
        onMouseDown={keepFocus}
        onClick={() => void frame.toggleMaximize()}
      >
        {maximized ? <IconWinRestore /> : <IconWinMaximize />}
      </button>
      {/* 和系统的关闭按钮一样：有会话时先确认（见 App 里的 onCloseRequested） */}
      <button className="win-btn close" title={t("window.close")} onClick={() => void frame.close()}>
        <IconWinClose />
      </button>
    </div>
  );
}
