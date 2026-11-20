// 弹出窗口里的界面（对方的一个显示器放在单独的窗口里，见 lib/popout.ts）：主窗口的 React 用 portal 渲染进那个窗口，
// 事件、状态都和主窗口共用。平时是窗口顶上的一条工具栏；全屏时和主窗口一样悬浮在顶端，鼠标碰到屏幕顶端时出现
// （useToolbarReveal）。这里只有这个窗口自己的操作：全屏、放回主窗口；画质、声音、键盘这些还在主窗口的工具栏里。
import { useEffect, useRef } from "react";
import { createPortal } from "react-dom";
import { t, useT } from "../lib/i18n";
import { sessionStateText } from "../lib/messages";
import type { Popout } from "../lib/popout";
import type { RemoteSession } from "../lib/session";
import { useSession, useSessionTitle, useStore } from "../lib/store";
import { IconExpand, IconPopIn, IconShrink, IconWarn } from "./Icons";
import { useToolbarReveal } from "./SessionView";

/** 所有会话的弹出窗口里的界面 */
export function Popouts() {
  const sessions = useStore((st) => st.sessions);
  return (
    <>
      {sessions.map((s) => (
        <SessionPopouts key={s.id} s={s} />
      ))}
    </>
  );
}

function SessionPopouts({ s }: { s: RemoteSession }) {
  useSession(s);
  return (
    <>
      {s.popoutWindows.map(({ display, popout, gamePaused }) =>
        createPortal(
          <PopoutView s={s} p={popout} display={display} gamePaused={gamePaused} />,
          popout.ui,
          popout.label,
        ),
      )}
    </>
  );
}

function PopoutView({ s, p, display, gamePaused }: { s: RemoteSession; p: Popout; display: number; gamePaused: boolean }) {
  // 订阅语言切换（文案直接用导入的 t）
  useT();
  const host = useSessionTitle(s);
  const title = `${host} · ${t("session.display", { n: display + 1 })}`;
  // 窗口标题（桌面版由 Rust 同步到系统标题栏）
  useEffect(() => p.setTitle(title), [p, title]);
  const fullscreen = p.fullscreen;
  const bar = useRef<HTMLElement>(null);
  const revealed = useToolbarReveal(fullscreen, bar, p.win);
  const live = s.state === "live";
  const st = s.stats;
  const rtt = st.rttMs != null ? Math.round(st.rttMs) : null;

  return (
    <>
      {fullscreen && <div className="hot-edge" />}
      <header ref={bar} className={`popout-bar ${revealed ? "revealed" : ""}`}>
        <span className={`dot ${s.state}`} />
        <span className="popout-title ellipsis">{title}</span>
        <span className="popout-stats">
          {live ? `${Math.round(st.fps)} fps · ${rtt != null ? rtt : "—"} ms` : sessionStateText(s)}
        </span>
        <button
          className={`tb-btn icon ${fullscreen ? "on" : ""}`}
          title={fullscreen ? t("session.exitFullscreen") : t("session.fullscreen")}
          // 在这个窗口里的点击里进入全屏：浏览器只在用户操作里让网页全屏
          onClick={() => void p.setFullscreen(!fullscreen)}
        >
          {fullscreen ? <IconShrink size={17} /> : <IconExpand size={17} />}
        </button>
        <button
          className="tb-btn icon"
          title={t("session.popIn")}
          onClick={() => {
            s.dockDisplay(display);
            window.focus();
            s.focus();
          }}
        >
          <IconPopIn size={17} />
        </button>
      </header>
      {!live && (
        <div className="stage-overlay">
          <div className="overlay-card">
            {s.state === "closed" ? <IconWarn size={28} /> : <span className="spinner large" />}
            <div className="overlay-title">{sessionStateText(s)}</div>
            {s.state === "closed" && <div className="muted small">{t("session.popoutClosedHint")}</div>}
          </div>
        </div>
      )}
      {live && gamePaused && <div className="lock-hint">{t("session.clickToLock")}</div>}
    </>
  );
}
