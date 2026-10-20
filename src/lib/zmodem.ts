/**
 * lrzsz（ZMODEM）支持。
 *
 * 远端 `rz` / `sz` 会向终端发 ZMODEM 握手（`**B00...`）。xterm.js 只管渲染，
 * 协议由客户端用 zmodem.js 实现：
 *
 * - 远端字节先喂 `Sentry.consume()`：非 ZMODEM 数据原样进终端，
 *   嗅探到握手就触发 `on_detect`，会话接管整条字节流。
 * - `receive` → 远端 `sz`（下载）：accept 收完弹保存框落盘。
 * - `send` → 远端 `rz`（上传）：有拖拽进来的待传文件就直接用，否则弹文件框；
 *   之前「上传卡住」的根因：给 `Browser.send_files` 传的是 {name: File} 对象，而它按 `.length`
 *   遍历数组 —— 结果一个文件都没发、也没发 ZFIN，远端 rz 就一直挂着。现在自己实现发送循环：
 *   send_offer → 分片读文件 xfer.send → xfer.end → 全部完成后 close()（ZFIN/OO）。
 *
 * 出站字节走串行写队列：同一时刻只有一个 IPC 写在途，期间到达的字节合并成一批，
 * 既保证顺序，又避免 8KB 一个 invoke 打满 IPC。
 */
import Zmodem, {
  type Sentry as ZSentry,
  type ZSession,
  type ZTransfer,
} from "zmodem.js";

import { api } from "../api";

export interface ZmodemHooks {
  /** 写终端（用于显示传输提示） */
  termWrite: (s: string) => void;
  /** 写终端（透传非 ZMODEM 的原始字节流） */
  termWriteBytes: (b: Uint8Array) => void;
  /** 出站字节（SSH write 或本地 PTY write），返回 Promise 以便串行 */
  send: (bytes: Uint8Array) => Promise<unknown>;
  /** 轻提示 */
  notify: (msg: string) => void;
}

export interface ZmodemGate {
  consume: (bytes: Uint8Array) => void;
  /** 会话进行中时键盘输入要丢弃（出站被协议帧占用） */
  isActive: () => boolean;
  /** 预置待上传文件：下一次远端 rz 握手时直接发送，不再弹文件框（拖拽上传用） */
  queueUpload: (files: File[]) => void;
  dispose: () => void;
}

/** 拖拽后等待远端 rz 握手的最长时间；超时清掉，避免下次手动 rz 误用旧文件 */
const PENDING_TTL = 15_000;
/** 上传时每次从磁盘读的分片大小 */
const READ_SLICE = 256 * 1024;

const fmtSize = (n: number) =>
  n >= 1024 * 1024 * 1024
    ? `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`
    : n >= 1024 * 1024
      ? `${(n / 1024 / 1024).toFixed(1)} MB`
      : `${(n / 1024).toFixed(1)} KB`;

export function attachZmodem(hooks: ZmodemHooks): ZmodemGate {
  let active = false;
  let disposed = false;
  let pending: { files: File[]; at: number } | null = null;

  // —— 串行写队列：按块存，单次 IPC 最多 BATCH 字节 ——
  const BATCH = 256 * 1024;
  let chunks: Uint8Array[] = [];
  let queued = 0;
  let inflight = false;
  const flush = () => {
    if (inflight || queued === 0 || disposed) return;
    let n = 0;
    let k = 0;
    while (k < chunks.length && (n === 0 || n + chunks[k].length <= BATCH)) {
      n += chunks[k].length;
      k++;
    }
    const out = new Uint8Array(n);
    let off = 0;
    for (let i = 0; i < k; i++) {
      out.set(chunks[i], off);
      off += chunks[i].length;
    }
    chunks = chunks.slice(k);
    queued -= n;
    inflight = true;
    hooks
      .send(out)
      .catch(() => {})
      .finally(() => {
        inflight = false;
        flush();
        wakeDrain();
      });
  };
  /** 等写队列回落到阈值以下（上传背压） */
  let drainWaiters: Array<() => void> = [];
  const LOW_WATER = 512 * 1024;
  const drain = () =>
    queued <= LOW_WATER && !inflight
      ? Promise.resolve()
      : new Promise<void>((r) => drainWaiters.push(r));
  const wakeDrain = () => {
    if (queued > LOW_WATER) return;
    const w = drainWaiters;
    drainWaiters = [];
    w.forEach((f) => f());
  };
  const enqueue = (octets: ArrayLike<number>) => {
    const u = octets instanceof Uint8Array ? octets : Uint8Array.from(octets);
    if (u.length === 0) return;
    chunks.push(u);
    queued += u.length;
    flush();
  };

  /** 进度行：\r 回到行首覆盖，避免刷屏 */
  const progress = (name: string, done: number, total: number, t0: number) => {
    const pct = total ? Math.floor((done / total) * 100) : 100;
    const sec = Math.max(0.001, (performance.now() - t0) / 1000);
    const speed = done / sec;
    const w = 24;
    const fill = Math.round((pct / 100) * w);
    hooks.termWrite(
      `\r\x1b[2K\x1b[36m  ${name} [${"#".repeat(fill)}${".".repeat(w - fill)}] ${pct}% ` +
        `${fmtSize(done)}/${fmtSize(total)} ${fmtSize(speed)}/s\x1b[0m`
    );
  };

  /** sz（下载）：accept 收完 → 弹保存框 → write_file 落盘 */
  const handleOffer = (xfer: ZTransfer) => {
    const { name, size } = xfer.get_details();
    const total = size || 0;
    hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 接收 ${name}（${fmtSize(total)}）\x1b[0m\r\n`);
    const t0 = performance.now();
    let got = 0;
    let last = 0;
    xfer.on("input", (octets) => {
      got += octets.length;
      const now = performance.now();
      if (now - last > 200) {
        last = now;
        progress(name, got, total, t0);
      }
    });
    void xfer
      .accept()
      .then(async () => {
        if (disposed) return;
        progress(name, total || got, total || got, t0);
        hooks.termWrite("\r\n");
        const payloads = xfer.get_payloads();
        const sum = payloads.reduce((n, p) => n + p.length, 0);
        const buf = new Uint8Array(sum);
        let off = 0;
        for (const p of payloads) {
          buf.set(p, off);
          off += p.length;
        }
        const { save } = await import("@tauri-apps/plugin-dialog");
        const path = await save({ defaultPath: name });
        if (!path) {
          hooks.termWrite(`\x1b[90m[ZMODEM] 已取消保存 ${name}\x1b[0m\r\n`);
          return;
        }
        await api.writeFile(path, Array.from(buf));
        hooks.termWrite(`\x1b[32m[ZMODEM] ${name} 已保存到 ${path}\x1b[0m\r\n`);
        hooks.notify(`${name} 下载完成（${fmtSize(sum)}）`);
      })
      .catch((e) => {
        hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 接收失败：${String(e)}\x1b[0m\r\n`);
      });
  };

  const safeAbort = (z: ZSession) => {
    try {
      z.abort();
    } catch {
      /* 会话可能已结束 */
    }
  };

  /** rz（上传）：拖拽预置的文件 / 文件框 → send_files → close */
  const handleUpload = async (zsession: ZSession) => {
    let files: File[];
    if (pending && Date.now() - pending.at < PENDING_TTL) {
      files = pending.files;
      pending = null;
    } else {
      pending = null;
      hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 请选择要上传的文件…\x1b[0m\r\n`);
      files = await pickFiles();
    }
    if (disposed) return;
    if (files.length === 0) {
      hooks.termWrite(`\r\n\x1b[90m[ZMODEM] 已取消，正在中止远端 rz…\x1b[0m\r\n`);
      safeAbort(zsession);
      return;
    }
    const total = files.reduce((n, f) => n + f.size, 0);
    hooks.termWrite(
      `\r\n\x1b[33m[ZMODEM] 上传 ${files.length} 个文件（${fmtSize(total)}）\x1b[0m\r\n`
    );
    let sent = 0;
    let skipped = 0;
    try {
      let remainingBytes = total;
      for (let i = 0; i < files.length; i++) {
        const f = files[i];
        const xfer = await zsession.send_offer({
          name: f.name,
          size: f.size,
          mtime: new Date(f.lastModified),
          files_remaining: files.length - i,
          bytes_remaining: remainingBytes,
        });
        remainingBytes -= f.size;
        if (disposed) return;
        if (!xfer) {
          skipped++;
          hooks.termWrite(`\x1b[90m  跳过 ${f.name}（远端拒收）\x1b[0m\r\n`);
          continue;
        }
        // 分片读文件、逐片发送；每片之后等写队列回落再读下一片 —— 内存占用恒定，
        // 不会像 Browser.send_files 那样把整个文件一次性压进队列
        const t0 = performance.now();
        let off = xfer.get_offset() || 0;
        let last = 0;
        while (off < f.size) {
          const end = Math.min(off + READ_SLICE, f.size);
          const piece = new Uint8Array(await f.slice(off, end).arrayBuffer());
          if (disposed) return;
          off = end;
          if (off < f.size) xfer.send(piece);
          else await Promise.all([xfer.end(piece), drain()]);
          await drain();
          const now = performance.now();
          if (now - last > 200 || off >= f.size) {
            last = now;
            progress(f.name, off, f.size, t0);
          }
        }
        if (f.size === 0) await xfer.end(new Uint8Array(0));
        sent++;
        progress(f.name, f.size, f.size, t0);
        hooks.termWrite("\r\n");
      }
      // 必须显式结束会话（ZFIN / OO），否则远端 rz 一直等待
      await zsession.close();
      const msg = `上传完成：${sent} 个文件` + (skipped ? `，跳过 ${skipped} 个` : "");
      hooks.termWrite(`\x1b[32m[ZMODEM] ${msg}\x1b[0m\r\n`);
      hooks.notify(msg);
    } catch (e) {
      hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 上传失败：${String(e)}\x1b[0m\r\n`);
      safeAbort(zsession);
    }
  };

  /** <input type="file"> 在 webview 里可用，直接拿到 File 对象 */
  function pickFiles(): Promise<File[]> {
    return new Promise((resolve) => {
      const input = document.createElement("input");
      input.type = "file";
      input.multiple = true;
      let settled = false;
      const done = (v: File[]) => {
        if (settled) return;
        settled = true;
        window.removeEventListener("focus", onFocus);
        resolve(v);
      };
      const onFocus = () =>
        window.setTimeout(() => done(input.files ? Array.from(input.files) : []), 400);
      input.onchange = () => done(input.files ? Array.from(input.files) : []);
      // 用户直接点取消：不会触发 change，靠窗口重新获焦兜底
      window.addEventListener("focus", onFocus);
      input.click();
    });
  }

  const sentry: ZSentry = new Zmodem.Sentry({
    to_terminal: (octets) => {
      hooks.termWriteBytes(new Uint8Array(Array.from(octets)));
    },
    sender: (octets) => enqueue(octets),
    on_detect: (detection) => {
      if (disposed) return;
      let zsession: ZSession;
      try {
        zsession = detection.confirm();
      } catch {
        return; // Detection 已失效
      }
      active = true;
      zsession.on("session_end", () => {
        active = false;
      });
      if (zsession.type === "receive") {
        hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 检测到远端发送（sz）\x1b[0m`);
        zsession.on("offer", handleOffer);
        zsession.start();
      } else {
        hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 检测到远端接收（rz）\x1b[0m`);
        void handleUpload(zsession);
      }
    },
    on_retract: () => {
      /* 被判为非 ZMODEM，无需处理 */
    },
  });

  return {
    consume: (bytes) => {
      try {
        sentry.consume(bytes);
      } catch (e) {
        // 协议异常时别把整个终端卡死：复位状态并提示
        active = false;
        hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 协议错误：${String(e)}\x1b[0m\r\n`);
      }
    },
    isActive: () => active,
    queueUpload: (files) => {
      pending = files.length ? { files, at: Date.now() } : null;
    },
    dispose: () => {
      disposed = true;
      pending = null;
      drainWaiters.forEach((f) => f());
      drainWaiters = [];
    },
  };
}
