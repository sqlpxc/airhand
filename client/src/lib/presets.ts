import type { Codec, QualitySettings } from "./api";
import { t } from "./i18n";

export type PresetId = "saver" | "balanced" | "smooth" | "hd" | "native";

export interface Preset {
  id: PresetId;
  settings: Omit<QualitySettings, "codec">;
}

/** 预设：从省流到原画。码率是上限，画面静止时几乎不占带宽。名称与说明见 i18n.ts 的 preset.* */
export const PRESETS: Preset[] = [
  { id: "saver", settings: { fps: 24, bitrateKbps: 1200, maxShort: 720, adaptive: true } },
  { id: "balanced", settings: { fps: 30, bitrateKbps: 4000, maxShort: 1080, adaptive: true } },
  { id: "smooth", settings: { fps: 60, bitrateKbps: 8000, maxShort: 1080, adaptive: true } },
  { id: "hd", settings: { fps: 60, bitrateKbps: 16000, maxShort: 1440, adaptive: true } },
  { id: "native", settings: { fps: 60, bitrateKbps: 30000, maxShort: 0, adaptive: true } },
];

export const DEFAULT_PRESET: PresetId = "balanced";

export const presetName = (id: PresetId) => t(`preset.${id}`);
export const presetDesc = (id: PresetId) => t(`preset.${id}.desc`);
export const codecHint = (c: Codec) => t(`codec.hint.${c}`);

export function presetSettings(id: PresetId, codec: Codec): QualitySettings {
  const p = PRESETS.find((p) => p.id === id) ?? PRESETS[1];
  return { ...p.settings, codec };
}

/** 判断当前设置是否与某个预设一致 */
export function matchPreset(s: QualitySettings): PresetId | null {
  const p = PRESETS.find(
    (p) =>
      p.settings.fps === s.fps &&
      p.settings.bitrateKbps === s.bitrateKbps &&
      p.settings.maxShort === s.maxShort,
  );
  return p?.id ?? null;
}

export const CODEC_LABEL: Record<Codec, string> = {
  h264: "H.264",
  h265: "H.265 / HEVC",
  av1: "AV1",
};
