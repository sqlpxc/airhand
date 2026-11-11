// 画面增强（控制台的 Worker 里）：画面要放大时用 Catmull-Rom 插值代替浏览器的双线性，再按亮度做自适应锐化。
// 都在本机的显卡上做，不增加带宽，也不经过 CPU。
//
// * 放大：Catmull-Rom（9 次双线性采样拼出 4×4 的三次插值），结果限制在最近 4 个像素的范围内，
//   放大不产生振铃（文字旁边的白边、黑边）；
// * 锐化：思路同 AMD FidelityFX CAS——周围反差已经很大的地方少锐化，不会过冲。只改亮度：
//   色度在 4:2:0 里本来就只有一半的分辨率，锐化它只会放大色边；
// * 用一个 WebGL2 的 OffscreenCanvas（Worker 里所有画面共用，同一时刻只有正在操作的那个会话用到）：
//   VideoFrame 上传成纹理 → 放大到显示大小（不需要放大时跳过）→ 锐化，再把这个 canvas 直接画到会话自己的
//   2D canvas 上（都在显卡上；先 transferToImageBitmap 再画要多等约 2 ms）。会话的 canvas 已经是 2D 的
//   （一个 canvas 只能有一种上下文），关掉增强时也完全是原来的路径；
// * 浏览器不支持 WebGL2、上下文丢了、连续出错：退回 2D canvas 的高质量缩放（不锐化），见 video.ts。
import { fitSize } from "./fit.ts";

export type EnhanceLevel = "off" | "low" | "medium" | "high";
export const ENHANCE_LEVELS: EnhanceLevel[] = ["off", "low", "medium", "high"];

/**
 * 锐化的强度：CAS 的 peak（反差很小的地方，中心像素比四邻平均多出的部分放大 4|p|/(1+4p) 倍）。
 * 弱 ≈ 0.3 倍、中 ≈ 0.7 倍、强 ≈ 1.3 倍；CAS 本身是 -0.125 ~ -0.2，对压缩过的画面太强
 */
const PEAK: Record<EnhanceLevel, number> = { off: 0, low: -0.06, medium: -0.1, high: -0.14 };

/** 显示区域（设备像素） */
export interface Area {
  w: number;
  h: number;
}

/**
 * 画面 `fw`×`fh` 显示在 `area`（设备像素；null = 1:1 显示）里时 canvas 画多大、要不要增强。
 * 增强时 canvas 就是显示的大小（浏览器不再缩放，见 session.ts 的 layout）；要缩小时（多半是窗口刚变小、
 * 新尺寸的画面还没到，或者多屏总览的缩略图）交给浏览器，不增强。与主线程的 fitSize 用同一套算法
 */
export function renderPlan(fw: number, fh: number, area: Area | null, level: EnhanceLevel): { w: number; h: number; enhance: boolean } {
  if (level === "off" || !(fw > 0 && fh > 0)) return { w: fw, h: fh, enhance: false };
  const size = area && area.w > 0 && area.h > 0 ? fitSize(fw, fh, area.w, area.h) : { w: fw, h: fh };
  if (size.w < fw || size.h < fh) return { w: fw, h: fh, enhance: false };
  return { w: size.w, h: size.h, enhance: true };
}

const VERTEX = `#version 300 es
void main() {
  // 盖住整个画面的一个三角形
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

// 纹理的第 0 行是画面的最上面一行（上传时不翻转），画进中间纹理时也一样：都按"y 向下"的画面坐标算
const UPSCALE = `#version 300 es
precision highp float;
uniform sampler2D src;
uniform vec2 srcSize;
uniform vec2 ratio;
out vec4 color;
void main() {
  vec2 pos = gl_FragCoord.xy * ratio;
  vec2 t1 = floor(pos - 0.5) + 0.5;
  vec2 f = pos - t1;
  vec2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
  vec2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
  vec2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
  vec2 w3 = f * f * (-0.5 + 0.5 * f);
  vec2 w12 = w1 + w2;
  vec2 t0 = (t1 - 1.0) / srcSize;
  vec2 t3 = (t1 + 2.0) / srcSize;
  vec2 t12 = (t1 + w2 / w12) / srcSize;
  vec3 c =
      (texture(src, vec2(t0.x, t0.y)).rgb * w0.x + texture(src, vec2(t12.x, t0.y)).rgb * w12.x
        + texture(src, vec2(t3.x, t0.y)).rgb * w3.x) * w0.y
    + (texture(src, vec2(t0.x, t12.y)).rgb * w0.x + texture(src, vec2(t12.x, t12.y)).rgb * w12.x
        + texture(src, vec2(t3.x, t12.y)).rgb * w3.x) * w12.y
    + (texture(src, vec2(t0.x, t3.y)).rgb * w0.x + texture(src, vec2(t12.x, t3.y)).rgb * w12.x
        + texture(src, vec2(t3.x, t3.y)).rgb * w3.x) * w3.y;
  // 不超出最近 4 个像素的范围：不产生振铃
  ivec2 i = ivec2(floor(t1));
  ivec2 m = ivec2(srcSize) - 1;
  vec3 a = texelFetch(src, clamp(i, ivec2(0), m), 0).rgb;
  vec3 b = texelFetch(src, clamp(i + ivec2(1, 0), ivec2(0), m), 0).rgb;
  vec3 d = texelFetch(src, clamp(i + ivec2(0, 1), ivec2(0), m), 0).rgb;
  vec3 e = texelFetch(src, clamp(i + ivec2(1, 1), ivec2(0), m), 0).rgb;
  color = vec4(clamp(c, min(min(a, b), min(d, e)), max(max(a, b), max(d, e))), 1.0);
}`;

// 画到 canvas 上（canvas 的 y 向上）
const SHARPEN = `#version 300 es
precision highp float;
uniform sampler2D img;
uniform float peak;
out vec4 color;
const vec3 Y = vec3(0.2126, 0.7152, 0.0722);
ivec2 m;
float luma(ivec2 p) { return dot(texelFetch(img, clamp(p, ivec2(0), m), 0).rgb, Y); }
void main() {
  ivec2 size = textureSize(img, 0);
  m = size - 1;
  ivec2 p = ivec2(int(gl_FragCoord.x), size.y - 1 - int(gl_FragCoord.y));
  vec3 rgb = texelFetch(img, p, 0).rgb;
  float e = dot(rgb, Y);
  float a = luma(p + ivec2(-1, -1)), b = luma(p + ivec2(0, -1)), c = luma(p + ivec2(1, -1));
  float d = luma(p + ivec2(-1, 0)), f = luma(p + ivec2(1, 0));
  float g = luma(p + ivec2(-1, 1)), h = luma(p + ivec2(0, 1)), i = luma(p + ivec2(1, 1));
  // 十字和 3×3 的最小、最大值加在一起（0 ~ 2）：离 0 或 2 越近，反差已经越大，越少锐化
  float mn = min(min(min(d, e), min(f, b)), h);
  mn += min(mn, min(min(a, c), min(g, i)));
  float mx = max(max(max(d, e), max(f, b)), h);
  mx += max(mx, max(max(a, c), max(g, i)));
  float amp = sqrt(clamp(min(mn, 2.0 - mx) / max(mx, 1e-5), 0.0, 1.0));
  float w = amp * peak;
  float l = (e + w * (b + d + f + h)) / (1.0 + 4.0 * w);
  color = vec4(clamp(rgb + (l - e), 0.0, 1.0), 1.0);
}`;

/** 连续出错这么多次就不再用（退回 2D canvas） */
const MAX_FAILURES = 3;

class Enhancer {
  private readonly canvas: OffscreenCanvas;
  private readonly gl: WebGL2RenderingContext;
  private readonly upscale: WebGLProgram;
  private readonly sharpen: WebGLProgram;
  private readonly src: WebGLTexture;
  private readonly mid: WebGLTexture;
  private readonly fbo: WebGLFramebuffer;
  private readonly maxSize: number;
  private midW = 0;
  private midH = 0;
  private u: {
    src: WebGLUniformLocation | null;
    srcSize: WebGLUniformLocation | null;
    ratio: WebGLUniformLocation | null;
    img: WebGLUniformLocation | null;
    peak: WebGLUniformLocation | null;
  };

  constructor() {
    this.canvas = new OffscreenCanvas(16, 16);
    const gl = this.canvas.getContext("webgl2", {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      premultipliedAlpha: false,
      preserveDrawingBuffer: false,
    });
    if (!gl) throw new Error("no WebGL2");
    this.gl = gl;
    this.upscale = program(gl, UPSCALE);
    this.sharpen = program(gl, SHARPEN);
    this.u = {
      src: gl.getUniformLocation(this.upscale, "src"),
      srcSize: gl.getUniformLocation(this.upscale, "srcSize"),
      ratio: gl.getUniformLocation(this.upscale, "ratio"),
      img: gl.getUniformLocation(this.sharpen, "img"),
      peak: gl.getUniformLocation(this.sharpen, "peak"),
    };
    this.src = texture(gl, gl.LINEAR);
    this.mid = texture(gl, gl.NEAREST);
    this.fbo = gl.createFramebuffer()!;
    this.maxSize = Math.min(gl.getParameter(gl.MAX_TEXTURE_SIZE) as number, gl.getParameter(gl.MAX_RENDERBUFFER_SIZE) as number);
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
    gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
    // 不需要顶点数据（见 VERTEX），但有的实现要求绑着一个顶点数组
    gl.bindVertexArray(gl.createVertexArray());
  }

  get lost(): boolean {
    return this.gl.isContextLost();
  }

  /** 把 `frame` 放大到 `w`×`h` 并锐化，画在自己的 canvas 上。放不下（超过显卡的纹理上限）返回 null；出错时抛出 */
  render(frame: VideoFrame, w: number, h: number, level: EnhanceLevel): OffscreenCanvas | null {
    const gl = this.gl;
    const fw = frame.displayWidth;
    const fh = frame.displayHeight;
    if (w > this.maxSize || h > this.maxSize || fw > this.maxSize || fh > this.maxSize) return null;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.src);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, gl.RGBA, gl.UNSIGNED_BYTE, frame);

    let img = this.src;
    if (w !== fw || h !== fh) {
      // 放大到中间纹理（显示大小）
      if (this.midW !== w || this.midH !== h) {
        gl.bindTexture(gl.TEXTURE_2D, this.mid);
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA8, w, h, 0, gl.RGBA, gl.UNSIGNED_BYTE, null);
        gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.mid, 0);
        this.midW = w;
        this.midH = h;
        gl.bindTexture(gl.TEXTURE_2D, this.src);
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
      gl.viewport(0, 0, w, h);
      gl.useProgram(this.upscale);
      gl.uniform1i(this.u.src, 0);
      gl.uniform2f(this.u.srcSize, fw, fh);
      gl.uniform2f(this.u.ratio, fw / w, fh / h);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      img = this.mid;
    }

    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, w, h);
    gl.bindTexture(gl.TEXTURE_2D, img);
    gl.useProgram(this.sharpen);
    gl.uniform1i(this.u.img, 0);
    gl.uniform1f(this.u.peak, PEAK[level]);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    if (gl.isContextLost()) throw new Error("WebGL context lost");
    return this.canvas;
  }
}

function program(gl: WebGL2RenderingContext, fragment: string): WebGLProgram {
  const p = gl.createProgram()!;
  for (const [type, src] of [[gl.VERTEX_SHADER, VERTEX], [gl.FRAGMENT_SHADER, fragment]] as const) {
    const s = gl.createShader(type)!;
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS) && !gl.isContextLost()) {
      throw new Error(`shader: ${gl.getShaderInfoLog(s)}`);
    }
    gl.attachShader(p, s);
  }
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS) && !gl.isContextLost()) {
    throw new Error(`program: ${gl.getProgramInfoLog(p)}`);
  }
  return p;
}

function texture(gl: WebGL2RenderingContext, filter: number): WebGLTexture {
  const t = gl.createTexture()!;
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  return t;
}

// Worker 里共用一个（WebGL 上下文的个数有限）
let shared: Enhancer | null = null;
let failures = 0;

/**
 * 增强：把 `frame` 放大到 `w`×`h` 并锐化，返回画好的 canvas（共用的：马上画走，下一次调用会覆盖它）。
 * 不支持、放不下、出错时返回 null，调用的一方照原来的方法画
 */
export function enhance(frame: VideoFrame, w: number, h: number, level: EnhanceLevel): OffscreenCanvas | null {
  if (failures >= MAX_FAILURES || typeof OffscreenCanvas === "undefined") return null;
  try {
    if (!shared || shared.lost) shared = new Enhancer();
    const out = shared.render(frame, w, h, level);
    failures = 0;
    return out;
  } catch (e) {
    shared = null;
    if (++failures >= MAX_FAILURES) console.warn("picture enhancement unavailable, using the browser's scaling", e);
    return null;
  }
}
