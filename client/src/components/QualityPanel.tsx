import { useEffect, useId, useRef, useState } from "react";
import type { Codec, QualitySettings, VideoPriority } from "../lib/api";
import { useT } from "../lib/i18n";
import { CODEC_PRIORITY, codecUsable } from "../lib/codec";
import { ENHANCE_LEVELS } from "../lib/enhance";
import { CODEC_LABEL, PRESETS, codecHint, matchPreset, presetDesc, presetName, presetSettings } from "../lib/presets";
import type { RemoteSession } from "../lib/session";
import { store, useSession, useStore } from "../lib/store";
import { IconInfo } from "./Icons";
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

/**
 * 左边一列的名称。有说明的带一个 ⓘ，鼠标停在名称上显示说明：说明文字不再平铺在面板里，窗口矮时也放得下。
 * `htmlFor`：点名称就切换右边的开关
 */
function Label({ text, hint, htmlFor }: { text: string; hint?: string; htmlFor?: string }) {
  return (
    <label className="qp-label" title={hint} htmlFor={htmlFor}>
      {text}
      {hint && <IconInfo size={12} />}
    </label>
  );
}

/**
 * 画质面板：预设一行，下面是发给对方的各项设置（左名称 / 右控件，一项一行），最后是只影响本机显示的画面增强。
 * 标题与底部的编码器信息固定，中间放不下时滚动（高度上限见 Popover 的 --pop-max-h）
 */
export function QualityPanel({ s, onClose }: { s: RemoteSession; onClose: () => void }) {
  const t = useT();
  useSession(s);
  const decoders = useStore((st) => st.decoders);
  const enhance = useStore((st) => st.enhance);
  const [draftKbps, setDraftKbps] = useState(s.settings.bitrateKbps);
  const timer = useRef<number | undefined>(undefined);
  const adaptiveId = useId();
  const cur = s.settings;
  const preset = matchPreset(cur);
  const priority = cur.priority ?? "latency";

  useEffect(() => setDraftKbps(cur.bitrateKbps), [cur.bitrateKbps]);

  const apply = (patch: Partial<QualitySettings>) => s.updateSettings({ ...s.settings, ...patch });
  const codecOk = (c: Codec) => codecUsable(c, decoders, s.hostCodecs);
  // 高亮对方实际在用的格式（与工具栏一致）：请求的格式对方建不起来时会回退到 H.264
  const activeCodec = s.encoder?.codec ?? cur.codec;
  const priorityHint = PRIORITIES.map((p) => `${t(`quality.priority.${p}`)} · ${t(`quality.priority.${p}.hint`)}`).join("\n\n");
  const encoderLine =
    s.encoder &&
    t("quality.encoder", {
      name: s.encoder.name,
      kind: s.encoder.hardware ? t("quality.hardware") : t("quality.software"),
      size: `${s.encoder.width}×${s.encoder.height}`,
    });

  return (
    <Popover className="quality-panel" onClose={onClose}>
      <div className="qp-head pop-title">{t("quality.title")}</div>

      <div className="qp-body">
        <div className="segmented wrap" role="group" aria-label={t("quality.preset")}>
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

        <div className="qp-grid">
          <Label text={t("quality.resolution")} />
          <div className="segmented">
            {RES.map((r) => (
              <button key={r.v} className={cur.maxShort === r.v ? "active" : ""} onClick={() => apply({ maxShort: r.v })}>
                {r.label ?? t("quality.native")}
              </button>
            ))}
          </div>

          <Label text={t("quality.fps")} />
          <div className="segmented">
            {FPS.map((f) => (
              <button key={f} className={cur.fps === f ? "active" : ""} onClick={() => apply({ fps: f })}>
                {f}
              </button>
            ))}
          </div>

          <Label text={t("quality.bitrate")} hint={`0.5 ~ 50 Mbps · ${t("quality.idleHint")}`} />
          <div className="qp-bitrate">
            <input
              type="range"
              min={0}
              max={100}
              step={0.5}
              value={toSlider(draftKbps)}
              aria-label={t("quality.bitrate")}
              onChange={(e) => {
                const k = fromSlider(Number(e.target.value));
                setDraftKbps(k);
                clearTimeout(timer.current);
                timer.current = window.setTimeout(() => apply({ bitrateKbps: k }), 250);
              }}
            />
            <span className="qp-value">{fmtKbps(draftKbps)}</span>
          </div>

          <Label text={t("quality.adaptive")} hint={t("quality.adaptiveHint")} htmlFor={adaptiveId} />
          <input
            id={adaptiveId}
            type="checkbox"
            className="switch"
            checked={cur.adaptive}
            onChange={(e) => apply({ adaptive: e.target.checked })}
          />

          <Label text={t("quality.codec")} />
          <div className="segmented">
            {CODEC_PRIORITY.map((c) => {
              const ok = codecOk(c);
              const why = !decoders.includes(c) ? t("quality.noDecoder") : t("quality.noEncoder");
              return (
                <button
                  key={c}
                  className={activeCodec === c ? "active" : ""}
                  disabled={!ok}
                  onClick={() => apply({ codec: c })}
                  title={ok ? codecHint(c) : why}
                >
                  {CODEC_LABEL[c]}
                </button>
              );
            })}
          </div>

          <Label text={t("quality.priority")} hint={priorityHint} />
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

          {/* 本机显示的偏好，不发给对方；所有连接通用（见 enhance.ts） */}
          <div className="qp-section">{t("quality.local")}</div>
          <Label text={t("quality.enhance")} hint={t("quality.enhance.hint")} />
          <div className="segmented">
            {ENHANCE_LEVELS.map((l) => (
              <button key={l} className={enhance === l ? "active" : ""} onClick={() => store.setEnhance(l)}>
                {t(`quality.enhance.${l}`)}
              </button>
            ))}
          </div>
        </div>
      </div>

      {encoderLine && (
        <div className="qp-foot" title={encoderLine}>
          {encoderLine}
        </div>
      )}
    </Popover>
  );
}
