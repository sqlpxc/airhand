// 标签栏右侧的两个按钮：
// * 服务器状态（ServerIndicator）：一直写着在线 / 连接中 / 离线，点开是服务器的详情，弹出层就在它下面；
// * 设置（SettingsButton）：本机名称、语言、配置文件、切换为被控端、快捷键说明。
//
// 以前主菜单在最左边的 Logo 上：macOS 上和系统的红绿灯按钮挤在一起，点服务器状态也会在最左边弹出菜单。
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { api } from "../lib/api";
import { focusActive, requestToHost } from "../lib/actions";
import { setLang, useLang, useT, type Lang } from "../lib/i18n";
import { problemText } from "../lib/messages";
import { serverStatus, useStore } from "../lib/store";
import { IconCheck, IconChevron, IconCopy, IconKeyboard, IconServer, IconSettings, IconSwap, IconWarn } from "./Icons";
import { Logo } from "./Logo";
import { Popover } from "./Popover";

/** 弹出层的开关：关掉后键盘回到远程画面 */
function useMenu() {
  const [open, setOpen] = useState(false);
  const close = useCallback(() => {
    setOpen(false);
    focusActive();
  }, []);
  const toggle = useCallback(() => setOpen((o) => !o), []);
  return { open, close, toggle };
}

// ───────────────────────── 设置 ─────────────────────────

export function SettingsButton() {
  const t = useT();
  const { open, close, toggle } = useMenu();
  return (
    <div className="menu-anchor">
      <button
        className={`title-btn ${open ? "on" : ""}`}
        title={t("menu.title")}
        aria-label={t("menu.title")}
        aria-expanded={open}
        onClick={toggle}
      >
        <IconSettings size={17} />
      </button>
      {open && <AppMenu onClose={close} />}
    </div>
  );
}

function AppMenu({ onClose }: { onClose: () => void }) {
  const t = useT();
  const app = useStore((s) => s.app);
  const web = api.kind === "web";
  const [keys, setKeys] = useState(false);

  return (
    <Popover className="app-menu" onClose={onClose}>
      <div className="am-head">
        <Logo size={38} />
        <div className="am-head-text">
          <div className="am-name">Airhand</div>
          <div className="muted small">{t(web ? "menu.versionWeb" : "menu.version", { version: app?.version ?? "—" })}</div>
        </div>
      </div>

      <div className="am-group">
        <div className="am-row">
          <span className="am-label">{t("menu.name")}</span>
          <span className="am-value" title={web ? t("menu.nameHintWeb") : (app?.hostname ?? undefined)}>
            {app?.hostname || t("common.thisComputer")}
          </span>
        </div>
        <div className="am-row">
          <span className="am-label">{t("menu.language")}</span>
          <LangSwitch />
        </div>
        {/* 设置、本机的控制码、我的设备都在这个文件里 */}
        {app?.configPath && (
          <div className="am-row am-stack" title={t("menu.configHint")}>
            <div className="am-row-line">
              <span className="am-label">{t("menu.config")}</span>
              <CopyButton text={app.configPath} />
            </div>
            <div className="am-path">{breakablePath(app.configPath)}</div>
          </div>
        )}
      </div>

      {/* 网页版没有被控端模式，浏览器也会拦下这些快捷键 */}
      {!web && (
        <div className="am-actions">
          <button
            className="am-item"
            onClick={() => {
              onClose();
              requestToHost();
            }}
          >
            <span className="am-item-icon">
              <IconSwap size={16} />
            </span>
            <span className="am-item-text">
              {t("menu.toHost")}
              <span className="muted small block">{t("menu.toHostHint")}</span>
            </span>
          </button>
          <button className={`am-item ${keys ? "open" : ""}`} aria-expanded={keys} onClick={() => setKeys((k) => !k)}>
            <span className="am-item-icon">
              <IconKeyboard size={16} />
            </span>
            <span className="am-item-text">
              {t("menu.shortcuts")}
              <span className="muted small block">{t("menu.shortcutsHint")}</span>
            </span>
            <IconChevron size={15} className="am-chevron" />
          </button>
          {keys && (
            <div className="am-keys">
              <Keys keys={["Ctrl", "T"]}>{t("shortcut.newTab")}</Keys>
              <Keys keys={["Ctrl", "W"]}>{t("shortcut.closeTab")}</Keys>
              <Keys keys={["Ctrl", "Tab"]}>{t("shortcut.nextTab")}</Keys>
              <Keys keys={["Ctrl", "1~9"]}>{t("shortcut.nthTab")}</Keys>
              <Keys keys={["Ctrl", "Shift", "T"]}>{t("shortcut.reopen")}</Keys>
            </div>
          )}
        </div>
      )}
    </Popover>
  );
}

/** 长路径只在分隔符后面换行，不把文件名从中间断开 */
function breakablePath(path: string): ReactNode[] {
  return path.split(/(?<=[\\/])/).map((part, i) => (
    <span key={i}>
      {part}
      <wbr />
    </span>
  ));
}

/** 复制配置文件的路径（复制不了就什么也不做，路径本身也可以选中复制） */
function CopyButton({ text }: { text: string }) {
  const t = useT();
  const [done, setDone] = useState(false);
  useEffect(() => {
    if (!done) return;
    const timer = window.setTimeout(() => setDone(false), 1500);
    return () => clearTimeout(timer);
  }, [done]);
  return (
    <button
      className={`chip-btn ${done ? "done" : ""}`}
      onClick={() =>
        void navigator.clipboard?.writeText(text).then(
          () => setDone(true),
          () => {},
        )
      }
    >
      {done ? <IconCheck size={13} /> : <IconCopy size={13} />}
      <span>{t(done ? "menu.copied" : "menu.copy")}</span>
    </button>
  );
}

function Keys({ keys, children }: { keys: string[]; children: ReactNode }) {
  return (
    <>
      <span className="keys">
        {keys.map((k) => (
          <kbd key={k}>{k}</kbd>
        ))}
      </span>
      <span>{children}</span>
    </>
  );
}

/** 每个选项都用它自己的语言书写，看不懂当前语言的人也能找到 */
const LANGS: { id: Lang; label: string }[] = [
  { id: "zh", label: "中文" },
  { id: "en", label: "EN" },
];

export function LangSwitch() {
  const lang = useLang();
  const choose = (l: Lang) => {
    setLang(l);
    // 同步给 Rust 端：托盘菜单、窗口标题、之后产生的提示与错误
    void api.setLanguage(l);
  };
  return (
    <div className="segmented lang-switch" role="group" aria-label="Language / 语言" title="Language / 语言">
      {LANGS.map((l) => (
        <button key={l.id} className={lang === l.id ? "active" : ""} aria-pressed={lang === l.id} onClick={() => choose(l.id)}>
          {l.label}
        </button>
      ))}
    </div>
  );
}

// ───────────────────────── 服务器状态 ─────────────────────────

/** 标签栏右侧的服务器状态：一直写着是什么状态；点一下在它下面打开服务器的详情 */
export function ServerIndicator() {
  const t = useT();
  const status = useStore(serverStatus);
  const server = useStore((s) => s.server);
  const app = useStore((s) => s.app);
  const { open, close, toggle } = useMenu();
  const label = t(`server.${server.state}`);
  return (
    <div className="menu-anchor">
      <button
        className={`server-ind ${status} ${server.state === "connecting" ? "trying" : ""} ${open ? "on" : ""}`}
        title={`${t("server.title")} · ${label}${app && server.state === "connected" ? ` · ${app.server}` : ""}`}
        aria-expanded={open}
        onClick={toggle}
      >
        <span className="dot" />
        <span className="server-ind-text">{t(`server.short.${status}`)}</span>
      </button>
      {open && <ServerPanel onClose={close} />}
    </div>
  );
}

function ServerPanel({ onClose }: { onClose: () => void }) {
  const t = useT();
  const status = useStore(serverStatus);
  const server = useStore((s) => s.server);
  const problem = useStore((s) => s.serverProblem);
  const app = useStore((s) => s.app);
  return (
    <Popover className="server-panel" onClose={onClose}>
      <div className="sp-head">
        <span className={`sp-icon ${status}`}>
          <IconServer size={18} />
        </span>
        <div className="sp-head-text">
          <div className="pop-title">{t("server.title")}</div>
          <div className={`sp-state ${status}`}>
            <span className="dot" />
            {t(`server.${server.state}`)}
          </div>
        </div>
      </div>

      {problem && status !== "connected" && (
        <div className="error-box left" role="alert">
          <IconWarn size={15} />
          <span>
            {problemText(problem)}
            <span className="block sp-retry">{t(server.state === "connecting" ? "server.retrying" : "server.retryLater")}</span>
          </span>
        </div>
      )}

      {app && (
        <div className="sp-field">
          <div className="am-row-line">
            <span className="am-label">{t("server.address")}</span>
            {!app.pinned && (
              <span className="badge warn" title={t("server.unpinnedHint")}>
                {t("server.unpinned")}
              </span>
            )}
          </div>
          <div className="sp-addr mono">{app.server}</div>
        </div>
      )}

      <p className="sp-hint">{t("server.hint")}</p>
    </Popover>
  );
}
