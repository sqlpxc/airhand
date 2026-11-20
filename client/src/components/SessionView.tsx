import { type ReactNode, type RefObject, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { closeTab, fmtCode, toggleFullscreen } from "../lib/actions";
import { api, MOUSE_MODES, type MouseMode, type Platform } from "../lib/api";
import { type MsgKey, t, useT } from "../lib/i18n";
import { LocalFiles } from "../lib/files";
import { errorText, noticeText, problemText, sessionStateText } from "../lib/messages";
import { CODEC_LABEL } from "../lib/presets";
import type { RemoteSession } from "../lib/session";
import { store, useSession, useSessionTitle, useStore } from "../lib/store";
import {
  IconChevron,
  IconClipboard,
  IconCpu,
  IconExpand,
  IconFit,
  IconGamepad,
  IconKeyboard,
  IconMic,
  IconMicOff,
  IconMonitor,
  IconMouse,
  IconPointer,
  IconPopIn,
  IconPopOut,
  IconPower,
  IconRefresh,
  IconShrink,
  IconSliders,
  IconVolume,
  IconVolumeOff,
  IconWarn,
} from "./Icons";
import { Popover } from "./Popover";
import { QualityPanel, fmtKbps } from "./QualityPanel";

/** 把会话的 canvas 挂到当前位置（同一时刻只会挂在一个地方） */
export function CanvasHost({ s, interactive }: { s: RemoteSession; interactive: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const el = ref.current!;
    s.attach(el);
    s.setInteractive(interactive);
    if (interactive) s.focus();
    return () => {
      s.setInteractive(false);
      s.detach();
    };
  }, [s, interactive]);
  return <div ref={ref} className={`canvas-host ${interactive && s.scaleMode === "native" ? "native" : ""}`} />;
}

type Combo = { label: MsgKey; keys: string[] };

const WINDOWS_COMBOS: Combo[] = [
  { label: "combo.winStart", keys: ["MetaLeft"] },
  { label: "combo.altTab", keys: ["AltLeft", "Tab"] },
  { label: "combo.winD", keys: ["MetaLeft", "KeyD"] },
  { label: "combo.winE", keys: ["MetaLeft", "KeyE"] },
  { label: "combo.winR", keys: ["MetaLeft", "KeyR"] },
  { label: "combo.taskManager", keys: ["ControlLeft", "ShiftLeft", "Escape"] },
  { label: "combo.altF4", keys: ["AltLeft", "F4"] },
  { label: "combo.printScreen", keys: ["PrintScreen"] },
];

// Linux 桌面（GNOME / KDE 等）的常用快捷键
const LINUX_COMBOS: Combo[] = [
  { label: "combo.super", keys: ["MetaLeft"] },
  { label: "combo.altTab", keys: ["AltLeft", "Tab"] },
  { label: "combo.superD", keys: ["MetaLeft", "KeyD"] },
  { label: "combo.terminal", keys: ["ControlLeft", "AltLeft", "KeyT"] },
  { label: "combo.altF2", keys: ["AltLeft", "F2"] },
  { label: "combo.superL", keys: ["MetaLeft", "KeyL"] },
  { label: "combo.altF4", keys: ["AltLeft", "F4"] },
  { label: "combo.printScreen", keys: ["PrintScreen"] },
];

// macOS 的常用快捷键（Command 键在对方那边就是 MetaLeft）
const MAC_COMBOS: Combo[] = [
  { label: "combo.cmdTab", keys: ["MetaLeft", "Tab"] },
  { label: "combo.spotlight", keys: ["MetaLeft", "Space"] },
  { label: "combo.missionControl", keys: ["ControlLeft", "ArrowUp"] },
  { label: "combo.forceQuit", keys: ["MetaLeft", "AltLeft", "Escape"] },
  { label: "combo.cmdQ", keys: ["MetaLeft", "KeyQ"] },
  { label: "combo.lockMac", keys: ["ControlLeft", "MetaLeft", "KeyQ"] },
  { label: "combo.screenshotMac", keys: ["MetaLeft", "ShiftLeft", "Digit5"] },
];

/** 被控端平台对应的快捷键。hello 之前不知道平台，按系统名称猜（Windows 报告 "Windows 11 ..."，macOS 报告 "macOS 15.3"） */
const combosFor = (platform: Platform | null, os: string) => {
  if (platform === "mac") return MAC_COMBOS;
  if (platform === "linux") return LINUX_COMBOS;
  if (platform === "windows") return WINDOWS_COMBOS;
  return os === "" || /windows/i.test(os) ? WINDOWS_COMBOS : /^macos/i.test(os) ? MAC_COMBOS : LINUX_COMBOS;
};

type Menu = null | "quality" | "mouse" | "keys" | "display" | "clipboard" | "audio" | "mic" | "sessions";

/** 鼠标模式的图标（工具栏的鼠标按钮显示现在的） */
const MOUSE_ICONS: Record<MouseMode, typeof IconMouse> = { auto: IconMouse, desktop: IconPointer, game: IconGamepad };

/** 顶端热区的高度（和 .hot-edge 一致） */
const HOT_EDGE = 6;
/** 鼠标从这个高度以内移出网页，也算移到了顶端（例如 macOS 的菜单栏常驻、带刘海的屏幕，网页顶端上面还有一条） */
const LEAVE_TOP = 48;
/**
 * 工具栏周围这一圈内不收起：跨过热区和工具栏之间的空隙；macOS 上的网页版全屏时鼠标碰到顶端，
 * 系统菜单栏和浏览器的工具栏会滑下来盖住工具栏，鼠标往下移出它们时还在这一圈里
 * （桌面版全屏时菜单栏不出现，见 src-tauri/src/fullscreen.rs）
 */
const KEEP_NEAR = 32;

/**
 * 全屏时悬浮工具栏的显示 / 收起。不用 CSS :hover：macOS 的系统菜单栏盖住顶端时网页就不算悬停了，
 * 工具栏会马上收起。这里只在鼠标明确离开（在网页里移到了工具栏附近以外）时才收起。
 * `win`：工具栏在哪个窗口里（弹出窗口的工具栏也用它，见 PopoutView）
 */
export function useToolbarReveal(active: boolean, toolbar: RefObject<HTMLElement | null>, win: Window = window) {
  // 全屏中切换到别的连接时会重新挂载，这时鼠标还在工具栏上：先显示
  const [revealed, setRevealed] = useState(active);
  const shown = useRef(active);
  useEffect(() => {
    const set = (v: boolean) => {
      shown.current = v;
      setRevealed(v);
    };
    if (!active) {
      set(false);
      return;
    }
    const near = (x: number, y: number) => {
      const r = toolbar.current?.getBoundingClientRect();
      return !!r && x >= r.left - KEEP_NEAR && x <= r.right + KEEP_NEAR && y <= r.bottom + KEEP_NEAR;
    };
    const onPointer = (e: PointerEvent) => {
      // 按着键拖动时（例如把远程的窗口拖到顶端）不弹出；触摸屏点一下顶端也能弹出
      if (e.clientY <= HOT_EDGE && (e.buttons === 0 || e.type === "pointerdown")) set(true);
      else if (shown.current && !near(e.clientX, e.clientY)) set(false);
    };
    const onLeave = (e: MouseEvent) => {
      if (e.relatedTarget) return; // 还在网页里
      set(e.clientY <= LEAVE_TOP && e.buttons === 0);
    };
    const doc = win.document;
    win.addEventListener("pointermove", onPointer);
    win.addEventListener("pointerdown", onPointer);
    doc.addEventListener("mouseout", onLeave);
    return () => {
      win.removeEventListener("pointermove", onPointer);
      win.removeEventListener("pointerdown", onPointer);
      doc.removeEventListener("mouseout", onLeave);
    };
  }, [active, toolbar, win]);
  return revealed;
}

export function SessionView({ s }: { s: RemoteSession }) {
  // 订阅语言切换（文案直接用导入的 t）
  useT();
  useSession(s);
  const immersive = useStore((st) => st.immersive);
  const multi = useStore((st) => st.sessions.length > 1);
  const title = useSessionTitle(s);
  const [menu, setMenu] = useState<Menu>(null);
  const [toast, setToast] = useState<string | null>(null);
  /** 正在往画面上拖文件 */
  const [dropping, setDropping] = useState(false);
  // 对方要求配对码、或者对方的身份变了（要重新配对）时输入的配对码
  const [pairing, setPairing] = useState("");
  const needsPairing =
    s.state === "closed" &&
    (s.closedReason?.kind === "pairingRequired" ||
      s.closedReason?.kind === "pairingFailed" ||
      s.closedReason?.kind === "pairingUnavailable" ||
      s.closedReason?.kind === "hostKeyChanged");
  const toolbarRef = useRef<HTMLElement>(null);
  const revealed = useToolbarReveal(immersive, toolbarRef);
  // 菜单关掉后键盘回到远程画面（例如按 Esc 关掉画质面板后接着打字）
  const closeMenu = useCallback(() => {
    setMenu(null);
    s.focus();
  }, [s]);
  const toggleMenu = (m: Menu) => setMenu((cur) => (cur === m ? null : m));

  useEffect(() => {
    if (!toast) return;
    const t = window.setTimeout(() => setToast(null), 2400);
    return () => clearTimeout(t);
  }, [toast]);

  // 第一次锁定鼠标时（自动模式里被控端隐藏光标、进入游戏模式）提示怎么放开：
  // 全屏时 Esc 发给了对方，指针也碰不到顶端叫出工具栏
  const pointerLocked = s.pointerLocked;
  const lockHinted = useRef(false);
  useEffect(() => {
    if (!pointerLocked || lockHinted.current) return;
    lockHinted.current = true;
    setToast(t("session.mouseLocked"));
  }, [pointerLocked]);

  const fullscreen = async () => {
    setMenu(null);
    await toggleFullscreen();
    s.focus();
  };

  const typeClipboard = async () => {
    try {
      const text = await api.localClipboardText();
      if (!text) return setToast(t("session.noClipboard"));
      s.sendText(text);
      setToast(t("session.typed", { n: text.length }));
    } catch (e) {
      setToast(errorText(e));
    }
    s.focus();
  };

  const st = s.stats;
  const live = s.state === "live";
  const MouseIcon = MOUSE_ICONS[s.mouseMode];
  // 同时看着好几个显示器（多屏）
  const showingAll = s.allDisplays && s.shown.length > 1;
  const rtt = st.rttMs != null ? Math.round(st.rttMs) : null;
  const rttClass = rtt == null ? "" : rtt < 60 ? "good" : rtt < 150 ? "ok" : "bad";

  return (
    <div className={`session-view ${immersive ? "immersive" : ""}`}>
      {immersive && <div className="hot-edge" />}
      <header ref={toolbarRef} className={`toolbar ${revealed ? "revealed" : ""}`}>
        {/* 像浏览器的地址栏：连的是哪台、现在怎么样 */}
        <div className="addr">
          {immersive && multi ? (
            // 全屏时看不到标签栏：在这里切换到其它连接
            <div className="menu-anchor addr-switch">
              <button
                className={`addr-title switcher ${menu === "sessions" ? "on" : ""}`}
                title={t("session.switchSession")}
                onClick={() => toggleMenu("sessions")}
              >
                <span className={`dot ${s.state}`} />
                <span className="host">{title}</span>
                <IconChevron size={14} />
              </button>
              {menu === "sessions" && <SessionSwitcher current={s} onClose={closeMenu} />}
            </div>
          ) : (
            <div className="addr-title">
              <span className={`dot ${s.state}`} />
              <span className="host">{title}</span>
            </div>
          )}
          <span
            className="addr-code"
            title={s.safety ? `${t("session.safety", { code: s.safety })} · ${t("session.safetyHint")}` : undefined}
          >
            {fmtCode(s.code)}
          </span>

          <div className="addr-stats">
            {live ? (
              <>
                <span>{Math.round(st.fps)} fps</span>
                <span>{fmtKbps(st.kbps)}</span>
                <span className={`rtt ${rttClass}`}>{rtt != null ? `${rtt} ms` : "— ms"}</span>
                {st.width > 0 && (
                  <span className="muted" title={t("session.pathTitle")}>
                    {st.direct ? t("session.direct") : t("session.relayed")}
                  </span>
                )}
                {s.syncActive && s.syncStats.videoDelayMs >= 5 && (
                  <span className="muted" title={t("session.syncTitle")}>
                    {t("session.sync", { ms: Math.round(s.syncStats.videoDelayMs) })}
                  </span>
                )}
                {st.width > 0 && (
                  <span className="muted stat-res">
                    {st.width}×{st.height}
                  </span>
                )}
              </>
            ) : (
              <span className="muted">{sessionStateText(s)}</span>
            )}
          </div>
          {s.encoder && live && (
            <span className={`tag ${s.encoder.hardware ? "hw" : ""}`} title={s.encoder.name}>
              <IconCpu size={13} />
              <span className="tag-text">
                {CODEC_LABEL[s.encoder.codec]} · {s.encoder.hardware ? t("session.hwEncoding") : t("session.swEncoding")}
              </span>
            </span>
          )}
        </div>

        <div className="tb-right">
          {s.displays.length > 1 && (
            <div className="menu-anchor">
              <button className="tb-btn" onClick={() => toggleMenu("display")} title={t("session.switchDisplay")}>
                <IconMonitor size={17} />
                <span>
                  {showingAll ? t("session.allDisplays") : t("session.display", { n: s.currentDisplay + 1 })}
                </span>
                <IconChevron size={14} />
              </button>
              {menu === "display" && (
                <Dropdown onClose={closeMenu} className="display-menu">
                  {s.displays.map((d) => {
                    const popped = s.isPopped(d.index);
                    const canPop = s.canPopOut(d.index);
                    return (
                      <div key={d.index} className="dd-row">
                        <button
                          className={`dd-item ${(showingAll ? s.shown.includes(d.index) : d.index === s.currentDisplay) ? "active" : ""}`}
                          onClick={() => {
                            // 点一个显示器：只看它；它在单独的窗口里时切到那个窗口
                            if (popped) {
                              setMenu(null);
                              s.focusPopout(d.index);
                              return;
                            }
                            s.selectDisplay(d.index);
                            closeMenu();
                          }}
                        >
                          <span>
                            {t("session.display", { n: d.index + 1 })}
                            {d.primary ? t("session.primary") : ""}
                            {popped && <span className="muted small block">{t("session.inWindow")}</span>}
                          </span>
                          <span className="muted small">
                            {d.width}×{d.height}
                          </span>
                        </button>
                        {(popped || canPop) && (
                          // 放到单独的窗口里 / 放回来（要在点击里开窗口：浏览器只在用户操作里让开）
                          <button
                            className="dd-icon"
                            title={popped ? t("session.popIn") : t("session.popOut")}
                            onClick={() => {
                              if (popped) {
                                s.dockDisplay(d.index);
                                closeMenu();
                              } else {
                                setMenu(null);
                                if (!s.popOut(d.index)) setToast(t("session.popOutFailed"));
                              }
                            }}
                          >
                            {popped ? <IconPopIn size={16} /> : <IconPopOut size={16} />}
                          </button>
                        )}
                      </div>
                    );
                  })}
                  {s.canShowAll && (
                    <>
                      <div className="dd-sep" />
                      <label className="dd-item toggle">
                        <span>
                          {t("session.showAll")}
                          <span className="muted small block">{t("session.showAllHint")}</span>
                          {s.displays.length > s.maxStreams && (
                            <span className="muted small block">{t("session.showAllLimit", { n: s.maxStreams })}</span>
                          )}
                        </span>
                        <input
                          type="checkbox"
                          className="switch"
                          checked={s.allDisplays}
                          onChange={(e) => {
                            s.setAllDisplays(e.target.checked);
                            s.focus();
                          }}
                        />
                      </label>
                      <div className="dd-foot muted small">{t("session.popOutHint")}</div>
                    </>
                  )}
                </Dropdown>
              )}
            </div>
          )}

          <div className="menu-anchor">
            <button className={`tb-btn ${menu === "quality" ? "on" : ""}`} onClick={() => toggleMenu("quality")}>
              <IconSliders size={17} />
              <span>{t("session.quality")}</span>
            </button>
            {menu === "quality" && <QualityPanel s={s} onClose={closeMenu} />}
          </div>

          <div className="menu-anchor">
            <button className={`tb-btn icon ${menu === "keys" ? "on" : ""}`} title={t("session.shortcuts")} onClick={() => toggleMenu("keys")}>
              <IconKeyboard size={17} />
            </button>
            {menu === "keys" && (
              <Dropdown onClose={closeMenu}>
                {s.hostActions.length > 0
                  ? // 手机：系统操作（返回、主页……），对方列出了哪些就显示哪些
                    s.hostActions.map((a) => (
                      <button
                        key={a}
                        className="dd-item"
                        onClick={() => {
                          s.sendSystem(a);
                          closeMenu();
                          s.focus();
                        }}
                      >
                        {t(`action.${a}` as const)}
                      </button>
                    ))
                  : combosFor(s.hostPlatform, s.hostOs).map((c) => (
                      <button
                        key={c.label}
                        className="dd-item"
                        onClick={() => {
                          s.sendCombo(c.keys);
                          closeMenu();
                          s.focus();
                        }}
                      >
                        {t(c.label)}
                      </button>
                    ))}
                {s.hostPlatform === "mac" && (
                  <>
                    <div className="dd-sep" />
                    <label className="dd-item toggle">
                      <span>{t("session.swapCtrlCommand")}</span>
                      <input
                        type="checkbox"
                        className="switch"
                        checked={s.swapCtrlCommand}
                        onChange={(e) => {
                          s.setSwapCtrlCommand(e.target.checked);
                          closeMenu();
                          s.focus();
                        }}
                      />
                    </label>
                  </>
                )}
              </Dropdown>
            )}
          </div>

          <div className="menu-anchor">
            <button
              className={`tb-btn icon ${menu === "clipboard" ? "on" : ""}`}
              title={t("session.clipboard")}
              onClick={() => toggleMenu("clipboard")}
            >
              <IconClipboard size={17} />
              {s.clipboardSync && <span className="tb-dot" />}
            </button>
            {menu === "clipboard" && (
              <Dropdown onClose={closeMenu}>
                <label className="dd-item toggle">
                  <span>
                    {t("session.clipSync")}
                    <span className="muted small block">
                      {t(api.kind === "web" ? "session.clipSyncHintWeb" : "session.clipSyncHint")}
                    </span>
                    <span className="muted small block">
                      {t(api.kind === "web" ? "session.clipFilesHintWeb" : "session.clipFilesHint")}
                    </span>
                  </span>
                  <input
                    type="checkbox"
                    className="switch"
                    checked={s.clipboardSync}
                    onChange={(e) => s.setClipboardSync(e.target.checked)}
                  />
                </label>
                <button
                  className="dd-item"
                  onClick={() => {
                    closeMenu();
                    void typeClipboard();
                  }}
                >
                  <span>
                    {t("session.clipType")}
                    <span className="muted small block">{t("session.clipTypeHint")}</span>
                  </span>
                </button>
              </Dropdown>
            )}
          </div>
          <div className="menu-anchor">
            <button
              className={`tb-btn icon ${menu === "audio" ? "on" : ""}`}
              title={s.audioEnabled ? t("session.sound") : t("session.soundMuted")}
              onClick={() => toggleMenu("audio")}
            >
              {s.audioEnabled ? <IconVolume size={17} /> : <IconVolumeOff size={17} />}
              {s.audioEnabled && s.audioError !== null ? (
                <span className="tb-dot warn" />
              ) : (
                s.audioEnabled && s.avSync && <span className="tb-dot" />
              )}
            </button>
            {menu === "audio" && (
              <Dropdown onClose={closeMenu} className="audio-menu">
                <label className="dd-item toggle">
                  <span>
                    {t("session.playSound")}
                    <span className="muted small block">{audioHint(s)}</span>
                  </span>
                  <input
                    type="checkbox"
                    className="switch"
                    checked={s.audioEnabled}
                    onChange={(e) => s.setAudioEnabled(e.target.checked)}
                  />
                </label>
                <label className={`dd-item toggle ${s.audioEnabled ? "" : "disabled"}`}>
                  <span>
                    {t("session.avSync")}
                    <span className="muted small block">{t("session.avSyncHint")}</span>
                    <span className="muted small block">{syncHint(s)}</span>
                  </span>
                  <input
                    type="checkbox"
                    className="switch"
                    checked={s.avSync}
                    disabled={!s.audioEnabled}
                    onChange={(e) => s.setAvSync(e.target.checked)}
                  />
                </label>
              </Dropdown>
            )}
          </div>
          {s.micState !== null && s.micState !== "unsupported" && (
            // 支持麦克风转发时才显示，Windows 缺驱动时也能打开开关触发安装
            <div className="menu-anchor">
              <button
                className={`tb-btn icon ${menu === "mic" ? "on" : ""}`}
                title={s.micLive ? t("session.micOn") : t("session.mic")}
                onClick={() => toggleMenu("mic")}
              >
                {s.micEnabled ? <IconMic size={17} /> : <IconMicOff size={17} />}
                {micProblem(s) ? <span className="tb-dot warn" /> : s.micLive && <span className="tb-dot" />}
              </button>
              {menu === "mic" && (
                <Dropdown onClose={closeMenu} className="audio-menu">
                  <label className="dd-item toggle">
                    <span>
                      {t("mic.send")}
                      <span className="muted small block">{micHint(s)}</span>
                    </span>
                    <input
                      type="checkbox"
                      className="switch"
                      checked={s.micEnabled}
                      onChange={(e) => s.setMicEnabled(e.target.checked)}
                    />
                  </label>
                </Dropdown>
              )}
            </div>
          )}
          {s.hostPlatform !== "android" && (
            // 鼠标：自动 / 桌面 / 游戏（见 input.ts），还有画面里有光标时显不显示本机光标。手机是点触屏的，没有这些
            <div className="menu-anchor">
              <button
                className={`tb-btn icon ${menu === "mouse" || s.mouseMode === "game" ? "on" : ""}`}
                title={t("session.mouse", { mode: t(`mouse.${s.mouseMode}`) })}
                onClick={() => toggleMenu("mouse")}
              >
                <MouseIcon size={17} />
              </button>
              {menu === "mouse" && <MouseMenu s={s} onClose={closeMenu} />}
            </div>
          )}
          <button
            className={`tb-btn icon ${s.scaleMode === "native" ? "on" : ""}`}
            title={s.scaleMode === "fit" ? t("session.toNative") : t("session.toFit")}
            onClick={() => {
              s.setScaleMode(s.scaleMode === "fit" ? "native" : "fit");
              s.focus();
            }}
          >
            <IconFit size={17} />
          </button>
          <button
            className={`tb-btn icon ${immersive ? "on" : ""}`}
            title={immersive ? t("session.exitFullscreen") : t("session.fullscreen")}
            onClick={fullscreen}
          >
            {immersive ? <IconShrink size={17} /> : <IconExpand size={17} />}
          </button>
          <span className="tb-sep" />
          <button className="tb-btn icon danger" title={t("session.disconnect")} onClick={() => closeTab(s.id)}>
            <IconPower size={17} />
          </button>
        </div>
      </header>

      <div
        className={`stage ${dropping ? "dropping" : ""}`}
        // 拖到画面上的文件（文件夹也行）：放进对方的剪贴板，在对方那里粘贴（见 files.ts）。桌面版的窗口里拖放由 Tauri 接管
        onDragOver={(e) => {
          if (api.kind !== "web" || !live || !e.dataTransfer.types.includes("Files")) return;
          e.preventDefault();
          e.dataTransfer.dropEffect = "copy";
          if (!dropping) setDropping(true);
        }}
        onDragLeave={(e) => {
          if (e.currentTarget === e.target) setDropping(false);
        }}
        onDrop={(e) => {
          if (api.kind !== "web" || !live) return;
          e.preventDefault();
          setDropping(false);
          const files = LocalFiles.fromDataTransfer(e.dataTransfer);
          if (files && s.offerFiles(files)) setToast(t("session.filesOffered"));
        }}
      >
        <CanvasHost s={s} interactive={s.state === "live"} />
        {dropping && <div className="drop-hint">{t("session.dropFiles")}</div>}

        {s.state !== "live" && (
          <div className="stage-overlay">
            {s.state === "closed" ? (
              <div className="overlay-card">
                <IconWarn size={28} />
                <div className="overlay-title">{s.retries > 0 ? t("session.reconnectFailed") : t("session.lost")}</div>
                <div className="muted">{s.closedReason ? problemText(s.closedReason) : t("session.ended")}</div>
                {needsPairing && (
                  <input
                    className="pairing-input"
                    value={pairing}
                    inputMode="numeric"
                    autoComplete="off"
                    placeholder="000 000"
                    aria-label={t("connect.pairing")}
                    autoFocus
                    onChange={(e) => setPairing(e.target.value.replace(/\D/g, "").slice(0, 6))}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" && pairing.length === 6) s.pairWith(pairing);
                    }}
                  />
                )}
                <div className="overlay-actions">
                  {needsPairing ? (
                    <button className="primary-btn small" disabled={pairing.length !== 6} onClick={() => s.pairWith(pairing)}>
                      <IconRefresh size={16} />
                      {t("session.pairAndReconnect")}
                    </button>
                  ) : (
                    <button className="primary-btn small" onClick={() => s.reconnect()}>
                      <IconRefresh size={16} />
                      {t("session.reconnect")}
                    </button>
                  )}
                  <button className="ghost-btn" onClick={() => closeTab(s.id)}>
                    {t("session.close")}
                  </button>
                </div>
              </div>
            ) : s.state === "waiting" ? (
              <div className="overlay-card">
                <span className="spinner large" />
                <div className="overlay-title">{t("session.waiting")}</div>
                <div className="muted small">{t("session.waitingHint")}</div>
                <div className="overlay-actions">
                  <button className="ghost-btn" onClick={() => closeTab(s.id)}>
                    {t("session.close")}
                  </button>
                </div>
              </div>
            ) : (
              <div className="overlay-card">
                <span className="spinner large" />
                <div className="overlay-title">
                  {s.state === "reconnecting" ? t("session.reconnecting") : t("session.connecting")}
                </div>
                {s.closedReason && <div className="muted small">{problemText(s.closedReason)}</div>}
                {s.retries > 0 && <div className="muted small">{t("session.attempt", { n: s.retries })}</div>}
              </div>
            )}
          </div>
        )}

        {s.notice && (
          <div className="notice" onClick={() => s.dismissNotice()}>
            <IconWarn size={16} />
            <span>{noticeText(s.notice)}</span>
          </div>
        )}
        {live && s.gamePaused && <div className="lock-hint">{t("session.clickToLock")}</div>}
        {s.remoteFiles && (
          <div className="files-bar">
            <span className="files-bar-text">
              {s.download
                ? t("session.downloading", {
                    pct: s.download.total > 0 ? Math.floor((s.download.done * 100) / s.download.total) : 0,
                  })
                : t("session.remoteFiles", {
                    n: s.remoteFiles.items.length,
                    names: s.remoteFiles.items
                      .slice(0, 3)
                      .map((i) => i.path[0])
                      .join("、")
                      .concat(s.remoteFiles.items.length > 3 ? "…" : ""),
                  })}
            </span>
            <button className="primary-btn small" disabled={s.download !== null} onClick={() => void s.downloadRemoteFiles()}>
              {t("session.download")}
            </button>
            <button className="ghost-btn small" disabled={s.download !== null} onClick={() => s.dismissRemoteFiles()}>
              ✕
            </button>
          </div>
        )}
        {toast && <div className="toast">{toast}</div>}
      </div>
    </div>
  );
}

/** 鼠标菜单：三种模式（点了马上生效，游戏模式马上锁定指针），画面里有光标时显不显示本机光标，怎么放开鼠标 */
function MouseMenu({ s, onClose }: { s: RemoteSession; onClose: () => void }) {
  const localCursor = useStore((st) => st.localCursor);
  const game = s.mouseMode === "game";
  return (
    <Dropdown onClose={onClose} className="mouse-menu">
      <div className="dd-title pop-title">{t("mouse.title")}</div>
      {MOUSE_MODES.map((m) => {
        const Icon = MOUSE_ICONS[m];
        return (
          <button
            key={m}
            className={`dd-item mode ${s.mouseMode === m ? "active" : ""}`}
            onClick={() => {
              // 在点击里换：游戏模式要马上锁定指针，浏览器只在用户操作里让锁定
              s.setMouseMode(m);
              onClose();
            }}
          >
            <Icon size={18} />
            <span>
              {t(`mouse.${m}`)}
              <span className="muted small block">{t(`mouse.${m}.hint`)}</span>
            </span>
          </button>
        );
      })}
      <div className="dd-sep" />
      <label className={`dd-item toggle ${game ? "disabled" : ""}`}>
        <span>
          {t("mouse.localCursor")}
          <span className="muted small block">{t(game ? "mouse.localCursor.game" : "mouse.localCursor.hint")}</span>
        </span>
        <input
          type="checkbox"
          className="switch"
          checked={localCursor}
          disabled={game}
          onChange={(e) => store.setLocalCursor(e.target.checked)}
        />
      </label>
      <div className="dd-sep" />
      <div className="dd-foot muted small">{t("mouse.footer")}</div>
    </Dropdown>
  );
}

function audioHint(s: RemoteSession): string {
  if (!s.audioEnabled) return t("audio.muted");
  if (s.audioError !== null) return t("notice.audioFailed", { detail: s.audioError });
  const st = s.audio.stats;
  return st ? t("audio.jitter", { ms: Math.round(st.targetMs) }) : t("audio.remote");
}

/** 麦克风打开了、但声音没发过去（本机采集不了，或者对方现在接不了） */
function micProblem(s: RemoteSession): boolean {
  return s.micError !== null || (s.micEnabled && s.micState !== "ready");
}

function micHint(s: RemoteSession): string {
  if (s.micError !== null) return t(`mic.local.${s.micError}` as const);
  const device = s.hostPlatform === "windows" ? "Steam Streaming Microphone" : "Airhand Microphone";
  switch (s.micState) {
    case "noDevice":
      return t("mic.noDevice");
    case "steamMissing":
      return t("mic.steamMissing");
    case "preparing":
      return s.hostPlatform === "windows" ? t("mic.preparingSteam") : t("mic.preparing");
    case "setupFailed":
      return t("mic.setupFailed", { detail: s.micDetail ?? "—" });
    case "busy":
      return t("mic.busy");
    case "failed":
      return t("mic.failed", { device, detail: s.micDetail ?? "—" });
    default: {
      const hint = t(s.micLive ? "mic.live" : "mic.ready", { device });
      return s.hostPlatform === "mac" ? `${hint} ${t("mic.macPermission")}` : hint;
    }
  }
}

function syncHint(s: RemoteSession): string {
  if (!s.audioEnabled) return t("sync.needSound");
  const { skewMs, videoDelayMs, extraMs } = s.syncStats;
  if (skewMs === null) return t("sync.waiting");
  const skew = Math.round(skewMs);
  if (!s.avSync) return skew >= 0 ? t("sync.audioLate", { ms: skew }) : t("sync.videoLate", { ms: -skew });
  const before = skew >= 0 ? t("sync.wasAudioLate", { ms: skew }) : t("sync.wasVideoLate", { ms: -skew });
  if (extraMs > 0) return t("sync.audioDelayed", { ms: Math.round(extraMs), before });
  return t("sync.videoDelayed", { ms: Math.round(videoDelayMs), before });
}

/** 全屏时切换到其它连接（标签栏藏起来了），仍然保持全屏 */
function SessionSwitcher({ current, onClose }: { current: RemoteSession; onClose: () => void }) {
  const sessions = useStore((st) => st.sessions);
  return (
    <Dropdown onClose={onClose} className="left session-menu">
      {sessions.map((x) => (
        <SwitcherItem
          key={x.id}
          s={x}
          active={x === current}
          onPick={() => {
            onClose();
            store.setView(x.id);
          }}
        />
      ))}
    </Dropdown>
  );
}

function SwitcherItem({ s, active, onPick }: { s: RemoteSession; active: boolean; onPick: () => void }) {
  useSession(s);
  const title = useSessionTitle(s);
  return (
    <button className={`dd-item ${active ? "active" : ""}`} onClick={onPick}>
      <span className="dd-main">
        <span className={`dot ${s.state}`} />
        <span className="ellipsis">{title}</span>
      </span>
      <span className="muted small">{s.state === "live" ? fmtCode(s.code) : sessionStateText(s)}</span>
    </button>
  );
}

function Dropdown({ children, onClose, className = "" }: { children: ReactNode; onClose: () => void; className?: string }) {
  return (
    <Popover className={`dropdown ${className}`} onClose={onClose}>
      {children}
    </Popover>
  );
}
