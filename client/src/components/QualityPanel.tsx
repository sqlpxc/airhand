import { useEffect, useRef, useState } from "react";
import type { Codec, QualitySettings, VideoPriority } from "../lib/api";
import { useT } from "../lib/i18n";
import { CODEC_PRIORITY, codecUsable } from "../lib/codec";
import { CODEC_LABEL, PRESETS, codecHint, matchPreset, presetDesc, presetName, presetSettings } from "../lib/presets";
import type { RemoteSession } from "../lib/session";
import { useSession, useStore } from "../lib/store";
import { Popover } from "./Popover";

const FPS = [15, 24, 30, 60, 90, 120];
/** 没有 label 的是"原始分辨率"（文字随界面语言） */
const RES: { v: number; label?: string }[] = [
  { v: 720, label: "720p" },
  { v: 1080, label: "1080p" },
  { v: 1440, label: "1440p" },
  { v: 2160, label: "4K" },
  { v: 0 },
];

/** 缩放画面时优先什么：低延迟（显卡缩放，默认）/ 清楚（CPU 缩放） */
const PRIORITIES: VideoPriority[] = ["latency", "clarity"];

// 码率滑块使用对数刻度：0.5 ~ 50 Mbps
const MIN_KBPS = 500;
const MAX_KBPS = 50000;
const toSlider = (kbps: number) => (Math.log(kbps / MIN_KBPS) / Math.log(MAX_KBPS / MIN_KBPS)) * 100;
const fromSlider = (v: number) => {
  const k = MIN_KBPS * Math.pow(MAX_KBPS / MIN_KBPS, v / 100);
  return k >= 10000 ? Math.round(k / 1000) * 1000 : Math.round(k / 100) * 100;
};
export const fmtKbps = (k: number) =>
  k >= 1000 ? `${(k / 1000).toFixed(k >= 10000 ? 0 : 1)} Mbps` : `${Math.round(k)} kbps`;

export function QualityPanel({ s, onClose }: { s: RemoteSession; onClose: () => void }) {
  const t = useT();
  useSession(s);
  const decoders = useStore((st) => st.decoders);
  const [draftKbps, setDraftKbps] = useState(s.settings.bitrateKbps);
  const timer = useRef<number | undefined>(undefined);
  const cur = s.settings;
  const preset = matchPreset(cur);
  const priority = cur.priority ?? "latency";

  useEffect(() => setDraftKbps(cur.bitrateKbps), [cur.bitrateKbps]);

  const apply = (patch: Partial<QualitySettings>) => s.updateSettings({ ...s.settings, ...patch });
  const codecOk = (c: Codec) => codecUsable(c, decoders, s.hostCodecs);
  // 高亮对方实际在用的格式（与工具栏一致）：请求的格式对方建不起来时会回退到 H.264
  const activeCodec = s.encoder?.codec ?? cur.codec;

  return (
    <Popover className="quality-panel" onClose={onClose}>
      <div className="pop-title">{t("quality.title")}</div>

      <div className="field">
        <label>{t("quality.preset")}</label>
        <div className="segmented wrap">
          {PRESETS.map((p) => (
            <button
              key={p.id}
              className={preset === p.id ? "active" : ""}
              title={presetDesc(p.id)}
              onClick={() => s.updateSettings({ ...presetSettings(p.id, cur.codec), priority: cur.priority })}
            >
              {presetName(p.id)}
            </button>
          ))}
          <button className={preset === null ? "active" : ""} disabled>
            {t("quality.custom")}
          </button>
        </div>
      </div>

      <div className="field">
        <label>{t("quality.resolution")}</label>
        <div className="segmented">
          {RES.map((r) => (
            <button key={r.v} className={cur.maxShort === r.v ? "active" : ""} onClick={() => apply({ maxShort: r.v })}>
              {r.label ?? t("quality.native")}
            </button>
          ))}
        </div>
      </div>

      <div className="field">
        <label>{t("quality.priority")}</label>
        <div className="segmented">
          {PRIORITIES.map((p) => (
            <button
              key={p}
              className={priority === p ? "active" : ""}
              title={t(`quality.priority.${p}.hint`)}
              onClick={() => apply({ priority: p })}
            >
              {t(`quality.priority.${p}`)}
            </button>
          ))}
        </div>
        <div className="muted small">{t(`quality.priority.${priority}.hint`)}</div>
      </div>

      <div className="field">
        <label>{t("quality.fps")}</label>
        <div className="segmented">
          {FPS.map((f) => (
            <button key={f} className={cur.fps === f ? "active" : ""} onClick={() => apply({ fps: f })}>
              {f}
            </button>
          ))}
        </div>
      </div>

      <div className="field">
        <label>
          {t("quality.bitrate")} <span className="value">{fmtKbps(draftKbps)}</span>
        </label>
        <input
          type="range"
          min={0}
          max={100}
          step={0.5}
          value={toSlider(draftKbps)}
          onChange={(e) => {
            const k = fromSlider(Number(e.target.value));
            setDraftKbps(k);
            clearTimeout(timer.current);
            timer.current = window.setTimeout(() => apply({ bitrateKbps: k }), 250);
          }}
        />
        <div className="range-legend muted small">
          <span>0.5 Mbps</span>
          <span>{t("quality.idleHint")}</span>
          <span>50 Mbps</span>
        </div>
      </div>

      <div className="field">
        <label>{t("quality.codec")}</label>
        <div className="codec-list">
          {CODEC_PRIORITY.map((c) => {
            const ok = codecOk(c);
            const why = !decoders.includes(c) ? t("quality.noDecoder") : t("quality.noEncoder");
            return (
              <button
                key={c}
                className={`codec-opt ${activeCodec === c ? "active" : ""}`}
                disabled={!ok}
                onClick={() => apply({ codec: c })}
                title={ok ? codecHint(c) : why}
              >
                <span className="codec-name">{CODEC_LABEL[c]}</span>
                <span className="codec-hint">{ok ? codecHint(c) : t("quality.unavailable")}</span>
              </button>
            );
          })}
        </div>
      </div>

      <label className="toggle-row">
        <span>
          <b>{t("quality.adaptive")}</b>
          <span className="muted small">{t("quality.adaptiveHint")}</span>
        </span>
        <input
          type="checkbox"
          className="switch"
          checked={cur.adaptive}
          onChange={(e) => apply({ adaptive: e.target.checked })}
        />
      </label>

      {s.encoder && (
        <div className="encoder-line muted small">
          {t("quality.encoder", {
            name: s.encoder.name,
            kind: s.encoder.hardware ? t("quality.hardware") : t("quality.software"),
            size: `${s.encoder.width}×${s.encoder.height}`,
          })}
        </div>
      )}
    </Popover>
  );
}
