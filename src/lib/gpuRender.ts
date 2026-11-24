/**
 * 终端 GPU 渲染（xterm WebGL 渲染器）的调度逻辑。纯逻辑部分可单测，见 tests/fe/gpuRender.test.ts。
 *
 * - Mac 的 WKWebView 里 WebGL 2 底层走 Metal：由 GPU 画字，大量输出 / 滚动 / Retina 下 CPU 占用明显更低；
 * - WebKit 对同时存在的 WebGL 上下文个数有上限（约 16 个），超了会悄悄丢掉最老的上下文。所以给 GPU 渲染
 *   发「名额」：最多 `GPU_SLOTS` 个终端同时用 GPU，按最近使用挤掉最久没看的那个（它退回 Canvas）。
 *   不在每次切标签时来回换渲染器：换一次要重建字形纹理，来回切反而更慢、还会闪；
 * - 软件模拟的 WebGL（Linux 上常见的 llvmpipe / SwiftShader）比 Canvas 还慢，识别出来就不用；
 * - 上下文丢失（休眠唤醒、切换显卡）会自动退回 Canvas；同一个终端丢失太多次就不再尝试。
 */

/** 同时用 GPU 渲染的终端数上限（留足余量，低于 WebKit 的上下文上限） */
export const GPU_SLOTS = 8;
/** 同一个终端丢失 GPU 上下文几次后不再尝试 */
export const MAX_CONTEXT_LOSSES = 3;

/** 显卡名像不像软件模拟（这种 WebGL 比 Canvas 2D 还慢） */
export function isSoftwareRenderer(name: string): boolean {
  return /swiftshader|llvmpipe|softpipe|software|basic render|mesa offscreen/i.test(name);
}

export interface GpuInfo {
  /** 能用 GPU 渲染（有 WebGL 2，且不是软件模拟） */
  ok: boolean;
  /** 显卡 / 渲染器名（拿不到就是空串） */
  renderer: string;
  /** 不能用时的原因 */
  reason?: string;
}

let cached: GpuInfo | null = null;

/** 探测一次当前 WebView 的 WebGL 2（结果缓存）。在非浏览器环境（测试）里返回不可用。 */
export function detectGpu(): GpuInfo {
  if (cached) return cached;
  cached = probe();
  return cached;
}

function probe(): GpuInfo {
  if (typeof document === "undefined") return { ok: false, renderer: "", reason: "非浏览器环境" };
  try {
    const canvas = document.createElement("canvas");
    const gl = canvas.getContext("webgl2");
    if (!gl) return { ok: false, renderer: "", reason: "这个 WebView 不支持 WebGL 2" };
    const ext = gl.getExtension("WEBGL_debug_renderer_info");
    const renderer = String(
      (ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER)) ?? ""
    );
    // 用完立即释放，别占一个上下文名额
    gl.getExtension("WEBGL_lose_context")?.loseContext();
    if (isSoftwareRenderer(renderer)) {
      return { ok: false, renderer, reason: "只有软件模拟的 WebGL（比 CPU 渲染还慢）" };
    }
    return { ok: true, renderer };
  } catch {
    return { ok: false, renderer: "", reason: "探测 WebGL 失败" };
  }
}

/**
 * GPU 名额：最多 `max` 个终端同时用 GPU。`claim` 时满了就挤掉最久没用的那个（调它的 `evict`，
 * 让它退回 Canvas）。已经有名额的再 `claim` 只是把它挪到「最近使用」。
 */
export class GpuSlots {
  /** 按最近使用排序：数组末尾是最近的 */
  private order: { id: string; evict: () => void }[] = [];

  constructor(private readonly max = GPU_SLOTS) {}

  claim(id: string, evict: () => void): void {
    const at = this.order.findIndex((s) => s.id === id);
    if (at >= 0) {
      const [slot] = this.order.splice(at, 1);
      this.order.push({ ...slot, evict });
      return;
    }
    while (this.order.length >= this.max) {
      const oldest = this.order.shift();
      oldest?.evict();
    }
    this.order.push({ id, evict });
  }

  release(id: string): void {
    this.order = this.order.filter((s) => s.id !== id);
  }

  has(id: string): boolean {
    return this.order.some((s) => s.id === id);
  }

  /** 当前持有名额的终端（最久没用的在前），测试 / 诊断用 */
  holders(): string[] {
    return this.order.map((s) => s.id);
  }
}

/** 全应用共用一份名额表 */
export const gpuSlots = new GpuSlots();

/** 这个终端现在要不要用 GPU */
export function wantsGpu(o: { pref: boolean; visible: boolean; gpu: GpuInfo; losses: number }): boolean {
  return o.pref && o.visible && o.gpu.ok && o.losses < MAX_CONTEXT_LOSSES;
}
