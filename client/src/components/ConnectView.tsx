// 新建连接标签：输入控制码连接；下面是"我的设备"（连接过的电脑，保存在 config.toml 里），点一下就连。
//
// 输入的内容放在 store 里（Draft），切到别的标签再回来时还在。
import { useEffect, useRef, useState } from "react";
import type { SavedDevice } from "../lib/api";
import { fmtCode, forgetDevice, openSession, renameDevice, resetDraft, restoreDevice } from "../lib/actions";
import { deviceTitle, matchDevice, relativeTime } from "../lib/devices";
import { getLang, useT } from "../lib/i18n";
import { problemText, sessionStateText } from "../lib/messages";
import { bestCodec } from "../lib/codec";
import { CODEC_LABEL, PRESETS, matchPreset, presetDesc, presetName, presetSettings } from "../lib/presets";
import type { RemoteSession } from "../lib/session";
import { store, useSession, useStore, type Draft } from "../lib/store";

/** 这些问题要用户输入配对码（对方的身份变了也是：重新配对才认新的身份） */
const PAIRING_PROBLEMS = new Set(["pairingRequired", "pairingFailed", "pairingUnavailable", "hostKeyChanged"]);
import { IconBolt, IconClose, IconMonitor, IconPencil, IconSearch, IconWarn } from "./Icons";

export function ConnectView() {
  const t = useT();
  const draft = useStore((s) => s.draft);
  const server = useStore((s) => s.server);
  const decoders = useStore((s) => s.decoders);
  const devices = useStore((s) => s.devices);
  const inputs = useRef<(HTMLInputElement | null)[]>([]);

  // 新建连接标签总是有的时候才显示这一页；万一没有（例如状态不一致），补一个
  useEffect(() => {
    if (!draft) store.newTab();
  }, [draft]);

  const busy = draft?.busy ?? false;
  const error = draft?.error ?? null;

  // 打开这一页时光标放在第一个空格里；连接失败后放在最后一格，方便改
  useEffect(() => {
    const digits = store.get().draft?.digits ?? [];
    const i = digits.findIndex((d) => !d);
    inputs.current[i < 0 ? 5 : i]?.focus();
  }, []);
  useEffect(() => {
    // 要配对码时光标留在配对码里
    if (!busy && error && !PAIRING_PROBLEMS.has(error.kind)) inputs.current[5]?.focus();
  }, [busy, error]);

  if (!draft) return null;
  const { digits, preset, pairing } = draft;
  // 还不知道对方支持什么：先按本机能解码的选，连上后会按双方都支持的重新选（见 RemoteSession.fitCodec）
  const codec = bestCodec(decoders);
  const code = digits.join("");
  const ready = /^\d{6}$/.test(code) && code[0] !== "0";
  const online = server.state === "connected";
  // 输入的是我的设备里的：告诉用户是哪台
  const known = ready ? devices.find((d) => d.code === code) : undefined;

  // 对方要配对码（或者已经粘贴 / 输入了配对码）：显示配对码一栏
  const pairingProblem = error && PAIRING_PROBLEMS.has(error.kind);
  const showPairing = pairingProblem || pairing.length > 0;
  const pairingReady = /^\d{6}$/.test(pairing);

  const setFrom = (start: number, text: string) => {
    const only = text.replace(/\D/g, "");
    if (!only) return;
    const next = [...digits];
    let i = start;
    let used = 0;
    for (const ch of only) {
      if (i > 5) break;
      next[i++] = ch;
      used++;
    }
    // 一次粘贴了控制码和配对码（12 位）：后面 6 位是配对码
    const rest = only.slice(used);
    const patch: Partial<Draft> = { digits: next, error: null };
    if (start === 0 && rest.length === 6) patch.pairing = rest;
    store.patchDraft(patch);
    inputs.current[Math.min(i, 5)]?.focus();
  };

  const connect = () => {
    if (!ready || busy || !online) return;
    void openSession(code, presetSettings(preset, codec), { pairing: pairingReady ? pairing : null });
  };

  /** 我的设备：用上次的画质连（没记下画质的用默认预设） */
  const openDevice = (d: SavedDevice) => void openSession(d.code, d.quality ?? presetSettings(preset, codec));

  return (
    <div className="newtab">
      <div className="nt-wrap">
        <section className="nt-card">
          <div className="nt-head">
            <div className="hero-icon">
              <IconMonitor size={26} />
            </div>
            <h1>{t("connect.title")}</h1>
            <p className="muted">{t("connect.hint")}</p>
          </div>

          <div className="nt-field">
            <div className="nt-label">
              <span>
                {t("connect.code")}
                {known && <span className="nt-known">{t("devices.known", { name: deviceTitle(known) })}</span>}
              </span>
              {code.length > 0 && !busy && (
                <button className="text-btn" onClick={() => (resetDraft(), inputs.current[0]?.focus())}>
                  {t("connect.clear")}
                </button>
              )}
            </div>
            <div
              className={`code-inputs ${error ? "invalid" : ""}`}
              onPaste={(e) => (e.preventDefault(), setFrom(0, e.clipboardData.getData("text")))}
            >
              {digits.map((d, i) => (
                <input
                  key={i}
                  ref={(el) => {
                    inputs.current[i] = el;
                  }}
                  className={`code-box ${d ? "filled" : ""} ${i === 3 ? "gap" : ""}`}
                  value={d}
                  inputMode="numeric"
                  autoComplete="off"
                  maxLength={1}
                  disabled={busy}
                  aria-label={`${t("connect.code")} ${i + 1}`}
                  onFocus={(e) => e.target.select()}
                  onChange={(e) => {
                    const v = e.target.value;
                    if (v.length > 1) return setFrom(i, v);
                    if (/^\d$/.test(v)) setFrom(i, v);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Backspace") {
                      e.preventDefault();
                      const next = [...digits];
                      if (next[i]) next[i] = "";
                      else if (i > 0) {
                        next[i - 1] = "";
                        inputs.current[i - 1]?.focus();
                      }
                      store.patchDraft({ digits: next, error: null });
                    } else if (e.key === "Delete") {
                      e.preventDefault();
                      const next = [...digits];
                      next[i] = "";
                      store.patchDraft({ digits: next, error: null });
                    } else if (e.key === "ArrowLeft" && i > 0) inputs.current[i - 1]?.focus();
                    else if (e.key === "ArrowRight" && i < 5) inputs.current[i + 1]?.focus();
                    else if (e.key === "Home") inputs.current[0]?.focus();
                    else if (e.key === "End") inputs.current[5]?.focus();
                    else if (e.key === "Enter") connect();
                  }}
                />
              ))}
            </div>
          </div>

          {showPairing && (
            <div className="nt-field">
              <div className="nt-label">
                <span>{t("connect.pairing")}</span>
              </div>
              <input
                className={`pairing-input ${pairingProblem && !pairingReady ? "invalid" : ""}`}
                value={pairing}
                inputMode="numeric"
                autoComplete="off"
                maxLength={7}
                placeholder="000 000"
                disabled={busy}
                aria-label={t("connect.pairing")}
                autoFocus={!!pairingProblem}
                onChange={(e) => store.patchDraft({ pairing: e.target.value.replace(/\D/g, "").slice(0, 6) })}
                onKeyDown={(e) => {
                  if (e.key === "Enter") connect();
                }}
              />
              <div className="preset-desc muted small">{t("connect.pairingHint")}</div>
            </div>
          )}

          <div className="nt-field">
            <div className="nt-label">
              <span>{t("connect.quality")}</span>
            </div>
            <div className="segmented presets">
              {PRESETS.map((p) => (
                <button
                  key={p.id}
                  className={preset === p.id ? "active" : ""}
                  title={presetDesc(p.id)}
                  onClick={() => store.patchDraft({ preset: p.id })}
                  disabled={busy}
                >
                  {presetName(p.id)}
                </button>
              ))}
            </div>
            <div className="preset-desc muted small">
              {presetDesc(preset)} · {t("connect.prefer", { codec: CODEC_LABEL[codec] })}
            </div>
          </div>

          {error && (
            <div className="error-box" role="alert">
              <IconWarn size={15} />
              <span>{problemText(error)}</span>
            </div>
          )}
          {server.state === "disconnected" && (
            <div className="error-box soft">
              <span className="dot closed" />
              <span>
                {t("server.disconnected")} · {problemText(server.reason)}
              </span>
            </div>
          )}

          <button className="primary-btn" disabled={!ready || busy || !online} onClick={connect}>
            {busy ? <span className="spinner" /> : <IconBolt size={18} />}
            <span>{busy ? t("connect.busy") : !online ? t("connect.waitServer") : t("connect.go")}</span>
          </button>
        </section>

        <MyDevices onOpen={openDevice} disabled={busy || !online} />
      </div>
    </div>
  );
}

/** 超过这么多台时显示搜索框 */
const SEARCH_FROM = 7;

/** 我的设备：连接过的电脑，最近连过的在前面。点一下就连；可以重命名、移除（能撤销） */
function MyDevices({ onOpen, disabled }: { onOpen: (d: SavedDevice) => void; disabled: boolean }) {
  const t = useT();
  const devices = useStore((s) => s.devices);
  const saved = useStore((s) => s.devicesSaved);
  const sessions = useStore((s) => s.sessions);
  const [query, setQuery] = useState("");
  const [editing, setEditing] = useState<string | null>(null);
  const [removed, setRemoved] = useState<SavedDevice | null>(null);

  // 撤销的机会只保留一会儿
  useEffect(() => {
    if (!removed) return;
    const timer = window.setTimeout(() => setRemoved(null), 6000);
    return () => clearTimeout(timer);
  }, [removed]);

  if (devices.length === 0 && !removed) {
    // 第一次用：告诉用户连过的电脑会出现在这里
    return saved ? <p className="nt-empty muted small">{t("devices.empty")}</p> : null;
  }
  const open = new Map(sessions.map((s) => [s.code, s]));
  const shown = devices.filter((d) => matchDevice(d, query));

  return (
    <section className="nt-devices">
      <div className="section-title">
        <span>
          <IconMonitor size={14} />
          {t("devices.title")}
          <span className="count">{devices.length}</span>
        </span>
        {devices.length >= SEARCH_FROM ? (
          <label className="nt-search">
            <IconSearch size={14} />
            <input
              value={query}
              placeholder={t("devices.search")}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => e.key === "Escape" && setQuery("")}
            />
          </label>
        ) : (
          <span className="small">{t(saved ? "devices.hint" : "devices.hintWeb")}</span>
        )}
      </div>

      {removed && (
        <div className="undo-bar" role="status">
          <span>{t("devices.removed", { name: deviceTitle(removed) })}</span>
          <button
            className="text-btn"
            onClick={() => {
              restoreDevice(removed);
              setRemoved(null);
            }}
          >
            {t("devices.undo")}
          </button>
        </div>
      )}

      <div className="device-list">
        {shown.map((d) => (
          <DeviceItem
            key={d.code}
            d={d}
            session={open.get(d.code)}
            disabled={disabled}
            editing={editing === d.code}
            onOpen={() => onOpen(d)}
            onEdit={(on) => setEditing(on ? d.code : null)}
            onForget={() => {
              forgetDevice(d.code);
              setRemoved(d);
            }}
          />
        ))}
      </div>
      {shown.length === 0 && <p className="nt-empty muted small">{t("devices.noMatch")}</p>}
    </section>
  );
}

function DeviceItem({
  d,
  session,
  disabled,
  editing,
  onOpen,
  onEdit,
  onForget,
}: {
  d: SavedDevice;
  /** 已经打开的标签（点一下就切过去） */
  session: RemoteSession | undefined;
  disabled: boolean;
  editing: boolean;
  onOpen: () => void;
  onEdit: (on: boolean) => void;
  onForget: () => void;
}) {
  const t = useT();
  useSession(session);
  const title = deviceTitle(d);
  // 打开着的可以随时切过去；其余的要等服务器连上、没有正在进行的连接
  const off = disabled && !session;
  const status = session
    ? session.state === "live"
      ? t("devices.open")
      : sessionStateText(session)
    : relativeTime(d.lastUsed, Date.now(), getLang() === "zh" ? "zh-CN" : "en");
  const p = d.quality ? matchPreset(d.quality) : null;
  const finish = (value: string | null) => {
    if (value !== null) renameDevice(d.code, value);
    onEdit(false);
  };

  return (
    <div
      role="button"
      tabIndex={0}
      className={`device-item ${session ? "open" : ""} ${off ? "disabled" : ""} ${editing ? "editing" : ""}`}
      title={[title !== d.name && d.name, d.os, fmtCode(d.code)].filter(Boolean).join(" · ")}
      onClick={() => !off && !editing && onOpen()}
      onKeyDown={(e) => e.key === "Enter" && !off && !editing && onOpen()}
    >
      <span className="avatar">{title.slice(0, 1).toUpperCase()}</span>
      <span className="device-meta">
        {editing ? (
          <input
            className="device-rename"
            autoFocus
            defaultValue={title}
            maxLength={60}
            onClick={(e) => e.stopPropagation()}
            onFocus={(e) => e.target.select()}
            onKeyDown={(e) => {
              e.stopPropagation();
              if (e.key === "Enter") finish(e.currentTarget.value);
              else if (e.key === "Escape") finish(null);
            }}
            onBlur={(e) => finish(e.currentTarget.value)}
          />
        ) : (
          <span className="device-name">{title}</span>
        )}
        <span className="device-sub">
          {session && <span className={`dot ${session.state}`} />}
          {fmtCode(d.code)}
          {status && ` · ${status}`}
          {!session && d.quality && ` · ${p ? presetName(p) : t("connect.customQuality")}`}
        </span>
      </span>
      {!editing && (
        <span className="device-actions">
          <button
            className="icon-btn subtle"
            title={t("devices.rename")}
            onClick={(e) => {
              e.stopPropagation();
              onEdit(true);
            }}
          >
            <IconPencil size={13} />
          </button>
          <button
            className="icon-btn subtle"
            title={t("devices.forget")}
            onClick={(e) => {
              e.stopPropagation();
              onForget();
            }}
          >
            <IconClose size={13} />
          </button>
        </span>
      )}
    </div>
  );
}
