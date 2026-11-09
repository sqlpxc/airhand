// 主菜单：标签栏最左边的 Logo。本机信息、服务器状态、语言、切换为被控端、快捷键说明。
import type { ReactNode } from "react";
import { api } from "../lib/api";
import { requestToHost } from "../lib/actions";
import { setLang, useLang, useT, type Lang } from "../lib/i18n";
import { problemText } from "../lib/messages";
import { useStore } from "../lib/store";
import { IconSwap } from "./Icons";
import { Logo } from "./Logo";
import { Popover } from "./Popover";

export function AppMenu({ onClose }: { onClose: () => void }) {
  const t = useT();
  const app = useStore((s) => s.app);
  const server = useStore((s) => s.server);
  const web = api.kind === "web";

  return (
    <Popover className="app-menu" onClose={onClose}>
      <div className="am-head">
        <Logo size={36} />
        <div className="am-head-text">
          <div className="am-name">Airhand</div>
          <div className="muted small">{t(web ? "menu.versionWeb" : "menu.version", { version: app?.version ?? "—" })}</div>
        </div>
      </div>

      <div className="am-rows">
        <div className="am-row">
          <span className="am-label">{t("menu.name")}</span>
          <span className="am-value" title={web ? t("menu.nameHintWeb") : undefined}>
            {app?.hostname || t("common.thisComputer")}
          </span>
        </div>
        <div className="am-row">
          <span className="am-label">{t("menu.server")}</span>
          <span className={`am-value server-state ${server.state}`}>
            <span className="dot" />
            {t(`server.${server.state}`)}
          </span>
        </div>
        {(server.state === "disconnected" || app) && (
          <div className="am-note">
            {server.state === "disconnected" && <div className="am-problem">{problemText(server.reason)}</div>}
            {app && (
              <div className="am-addr">
                <span>{app.server}</span>
                {!app.pinned && (
                  <span className="badge" title={t("server.unpinnedHint")}>
                    {t("server.unpinned")}
                  </span>
                )}
              </div>
            )}
          </div>
        )}
        <div className="am-row">
          <span className="am-label">{t("menu.language")}</span>
          <LangSwitch />
        </div>
        {/* 设置、本机的控制码、我的设备都在这个文件里 */}
        {app?.configPath && (
          <>
            <div className="am-row">
              <span className="am-label">{t("menu.config")}</span>
            </div>
            <div className="am-note am-path" title={app.configPath}>
              {app.configPath}
            </div>
          </>
        )}
      </div>

      {/* 网页版没有被控端模式，浏览器也会拦下这些快捷键 */}
      {!web && (
        <>
          <div className="am-sep" />
          <button
            className="dd-item with-icon"
            onClick={() => {
              onClose();
              requestToHost();
            }}
          >
            <IconSwap size={16} />
            <span>
              {t("menu.toHost")}
              <span className="muted small block">{t("menu.toHostHint")}</span>
            </span>
          </button>
          <div className="am-sep" />
          <div className="am-section">{t("menu.shortcuts")}</div>
          <div className="am-keys">
            <Keys keys={["Ctrl", "T"]}>{t("shortcut.newTab")}</Keys>
            <Keys keys={["Ctrl", "W"]}>{t("shortcut.closeTab")}</Keys>
            <Keys keys={["Ctrl", "Tab"]}>{t("shortcut.nextTab")}</Keys>
            <Keys keys={["Ctrl", "1~9"]}>{t("shortcut.nthTab")}</Keys>
            <Keys keys={["Ctrl", "Shift", "T"]}>{t("shortcut.reopen")}</Keys>
          </div>
        </>
      )}
    </Popover>
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

/** 标签栏右侧的服务器状态：连着时只是一个绿点，出问题时写出来；点一下打开主菜单看详情 */
export function ServerIndicator({ onClick }: { onClick: () => void }) {
  const t = useT();
  const server = useStore((s) => s.server);
  const app = useStore((s) => s.app);
  const label = t(`server.${server.state}`);
  const title =
    server.state === "disconnected" ? `${label} · ${problemText(server.reason)}` : app ? `${label} · ${app.server}` : label;
  return (
    <button className={`server-ind ${server.state}`} title={title} onClick={onClick}>
      <span className="dot" />
      {server.state !== "connected" && <span className="server-ind-text">{label}</span>}
    </button>
  );
}
