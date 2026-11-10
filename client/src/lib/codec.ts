// WebCodecs 解码配置：根据码流生成 codec 字符串，并探测本机支持的硬件解码格式。
import type { Codec } from "./api";

/** 在 Annex-B 码流中找到第一个指定类型的 NAL，返回 NAL 头之后的字节 */
function findNal(data: Uint8Array, want: (header: number) => boolean): Uint8Array | null {
  for (let i = 0; i + 3 < data.length; i++) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      const h = data[i + 3];
      if (want(h)) return data.subarray(i + 3);
      i += 2;
    }
  }
  return null;
}

const hex2 = (v: number) => v.toString(16).padStart(2, "0");

/** 从关键帧推导 codec 字符串（H.264 读取 SPS 的 profile/level，其余使用足够高的 level） */
export function codecString(codec: Codec, keyframe: Uint8Array): string {
  switch (codec) {
    case "h264": {
      const sps = findNal(keyframe, (h) => (h & 0x1f) === 7);
      if (sps && sps.length >= 4) return `avc1.${hex2(sps[1])}${hex2(sps[2])}${hex2(sps[3])}`;
      return "avc1.640033"; // High@5.1
    }
    case "h265":
      return "hvc1.1.6.L153.B0"; // Main@5.1
    case "av1":
      return "av01.0.13M.08"; // Main@5.1, 8bit
  }
}

const PROBE: Record<Codec, string> = {
  h264: "avc1.640033",
  h265: "hvc1.1.6.L153.B0",
  av1: "av01.0.13M.08",
};

// ───────────────────── Annex-B → AVC（长度前缀）─────────────────────
//
// WebCodecs 规定：H.264 不带 description 时按 Annex-B（起始码）解析。Chromium/WebView2 都支持，
// 但 WebKitGTK（GStreamer 后端）不支持，只认"AVC 格式 + avcC 描述"。
// 遇到这种 WebView 时，在控制端把码流转换成 AVC 格式再解码（只是重新打包，开销很小）。

/** 拆分 Annex-B 码流里的 NAL 单元（不含起始码） */
export function splitAnnexB(data: Uint8Array): Uint8Array[] {
  const nals: Uint8Array[] = [];
  let start = -1;
  let i = 0;
  while (i + 2 < data.length) {
    if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
      if (start >= 0) {
        // 4 字节起始码前面多出来的 0 属于起始码，不属于上一个 NAL
        let end = i;
        while (end > start && data[end - 1] === 0) end--;
        if (end > start) nals.push(data.subarray(start, end));
      }
      i += 3;
      start = i;
    } else {
      i++;
    }
  }
  if (start >= 0 && start < data.length) nals.push(data.subarray(start));
  return nals;
}

/** 由 SPS/PPS 生成 avcC（AVCDecoderConfigurationRecord），NAL 长度字段 4 字节 */
export function avcC(sps: Uint8Array, pps: Uint8Array): Uint8Array {
  const out = new Uint8Array(11 + sps.length + pps.length);
  out.set([1, sps[1], sps[2], sps[3], 0xff, 0xe1, sps.length >> 8, sps.length & 0xff]);
  out.set(sps, 8);
  let o = 8 + sps.length;
  out.set([1, pps.length >> 8, pps.length & 0xff], o);
  out.set(pps, o + 3);
  return out;
}

/** Annex-B H.264 访问单元 → AVC 格式：参数集单独拿出来（用于 avcC），其余 NAL 改为 4 字节长度前缀 */
export function annexBToAvc(data: Uint8Array): { sps?: Uint8Array; pps?: Uint8Array; data: Uint8Array } {
  let sps: Uint8Array | undefined;
  let pps: Uint8Array | undefined;
  const keep: Uint8Array[] = [];
  for (const nal of splitAnnexB(data)) {
    const type = nal[0] & 0x1f;
    if (type === 7) sps ??= nal;
    else if (type === 8) pps ??= nal;
    // 访问单元分隔符在 AVC 格式里没有意义
    else if (type !== 9) keep.push(nal);
  }
  const out = new Uint8Array(keep.reduce((n, k) => n + 4 + k.length, 0));
  let o = 0;
  for (const k of keep) {
    out.set([k.length >>> 24, (k.length >> 16) & 0xff, (k.length >> 8) & 0xff, k.length & 0xff], o);
    out.set(k, o + 4);
    o += 4 + k.length;
  }
  return { sps, pps, data: out };
}

// Linux 上的 WebKitGTK：Annex-B 的 H.264 不能直接解码，一开始就用 AVC 格式。
// 其它 WebView 默认用 Annex-B；如果实际解码失败，会自动改用 AVC 格式（见 markH264NeedsAvc）。
const ua = typeof navigator === "undefined" ? "" : navigator.userAgent;
const isWebKitGtk = /Linux/.test(ua) && /AppleWebKit/.test(ua) && !/Chrome|Chromium|Edg\//.test(ua);
let needsAvc = isWebKitGtk;

/** H.264 是否需要转换为 AVC 格式再解码 */
export function h264NeedsAvc(): boolean {
  return needsAvc;
}

/** Annex-B 格式的 H.264 解码失败：之后都改用 AVC 格式 */
export function markH264NeedsAvc() {
  if (!needsAvc) console.info("Annex-B H.264 isn't supported here, decoding as AVC");
  needsAvc = true;
}

/** 编码格式的优先级：同画质下压缩率从高到低（界面上也按这个顺序列出） */
export const CODEC_PRIORITY: readonly Codec[] = ["av1", "h265", "h264"];

/**
 * 这种格式能不能用：本机能解码，并且被控端能编码。H.264 双方都一定支持。
 * `hostCodecs` 为 undefined 表示还不知道被控端支持什么（还没连上），这时只看本机。
 */
export function codecUsable(c: Codec, decoders: Codec[], hostCodecs?: Codec[]): boolean {
  return c === "h264" || (decoders.includes(c) && (hostCodecs?.includes(c) ?? true));
}

/** 按 {@link CODEC_PRIORITY} 选出能用的格式里压缩率最高的一个 */
export function bestCodec(decoders: Codec[], hostCodecs?: Codec[]): Codec {
  return CODEC_PRIORITY.find((c) => codecUsable(c, decoders, hostCodecs)) ?? "h264";
}

let probeCache: Promise<Codec[]> | null = null;

/** 探测本机可以（最好是硬件）解码的格式，H.264 必定可用 */
export function supportedDecoders(): Promise<Codec[]> {
  probeCache ??= (async () => {
    const out: Codec[] = [];
    for (const c of ["h264", "h265", "av1"] as Codec[]) {
      try {
        const r = await VideoDecoder.isConfigSupported({
          codec: PROBE[c],
          codedWidth: 1920,
          codedHeight: 1080,
          hardwareAcceleration: c === "h264" ? "no-preference" : "prefer-hardware",
          optimizeForLatency: true,
        });
        if (r.supported) out.push(c);
      } catch {
        /* 不支持 */
      }
    }
    if (!out.includes("h264")) out.unshift("h264");
    return out;
  })();
  return probeCache;
}
