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

import { api, bytesToBase64 } from "../api";

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
/**
 * 单次等待的上限。远端 rz 掉线 / 网络假死时，不要永远停在 99% ——
 * 超时报错总比无声挂住好排查。
 */
const STALL_MS = 60_000;

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
  /** 写失败（连接断了）时记下来，让 drain() 立刻报错而不是等超时 */
  let sendError: unknown = null;
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
      .catch((e) => {
        // 连接断了：清掉积压并记下错误，后续 drain() 立刻失败而不是等超时
        sendError = e;
        chunks = [];
        queued = 0;
      })
      .finally(() => {
        inflight = false;
        flush();
        wakeDrain();
      });
  };
  /** 等写队列回落到阈值以下（上传背压） */
  let drainWaiters: Array<() => void> = [];
  const LOW_WATER = 1024 * 1024;
  const drain = () =>
    sendError
      ? Promise.reject(sendError)
      : queued <= LOW_WATER && !inflight
        ? Promise.resolve()
        : new Promise<void>((r) => drainWaiters.push(r));
  const wakeDrain = () => {
    if (queued > LOW_WATER) return;
    const w = drainWaiters;
    drainWaiters = [];
    w.forEach((f) => f());
  };
  /** 给等待加上限：远端不响应时宁可报错，也不要永远挂在 99% */
  const deadline = <T,>(p: Promise<T>, ms: number, what: string): Promise<T> =>
    new Promise<T>((resolve, reject) => {
      const t = window.setTimeout(
        () => reject(new Error(`${what}超时（${Math.round(ms / 1000)} 秒无进展）`)),
        ms
      );
      p.then(
        (v) => {
          window.clearTimeout(t);
          resolve(v);
        },
        (e) => {
          window.clearTimeout(t);
          reject(e);
        }
      );
    });
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

  /**
   * sz（下载）：先问保存路径，再 accept，边收边落盘。
   *
   * 关键是 `accept({ on_input })` —— 用回调模式后 zmodem.js 不再把整个文件
   * 堆在 `_payloads` 里，收到的每块直接进「攒够 1MB 就写一次」的队列。
   * 老写法是「收完 → get_payloads() 拼成一个大 Uint8Array → Array.from 传给后端」，
   * 600MB 的文件会同时存在三份（spool + 拼接 + 六亿个 JSON 数字），必然崩。
   *
   * 顺序也讲究：远端发完 ZFILE 后一直在等我们的 ZRPOS，此时还没开始传数据，
   * 所以在 accept 之前弹保存框不会丢任何字节，也不会先灌进内存。
   */
  const handleOffer = (zsession: ZSession, xfer: ZTransfer) => {
    const { name, size } = xfer.get_details();
    const total = size || 0;
    const t0 = performance.now();
    let got = 0;
    let last = 0;

    // 攒批：zmodem 的数据块很小（几 KB～几十 KB），逐块 IPC 会把主线程打满
    const WRITE_CHUNK = 1024 * 1024;
    let parts: Uint8Array[] = [];
    let partsLen = 0;
    let first = true;
    let writeError: unknown = null;
    let chain: Promise<void> = Promise.resolve();
    const flushTo = (path: string) => {
      if (partsLen === 0) return;
      const buf = new Uint8Array(partsLen);
      let off = 0;
      for (const p of parts) {
        buf.set(p, off);
        off += p.length;
      }
      parts = [];
      partsLen = 0;
      const truncate = first;
      first = false;
      chain = chain.then(() =>
        api.appendFileB64(path, bytesToBase64(buf), truncate).catch((e) => {
          writeError = e;
          // 落盘失败就别再收了，否则远端会一直灌数据
          safeAbort(zsession);
          throw e;
        })
      );
    };

    void (async () => {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({ defaultPath: name });
      if (disposed) return;
      if (!path) {
        hooks.termWrite(`\r\n\x1b[90m[ZMODEM] 已取消保存 ${name}\x1b[0m\r\n`);
        safeAbort(zsession);
        return;
      }
      hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 接收 ${name}（${fmtSize(total)}）\x1b[0m\r\n`);

      await xfer.accept({
        on_input: (payload: ArrayLike<number>) => {
          const u = payload instanceof Uint8Array ? payload : Uint8Array.from(payload);
          parts.push(u);
          partsLen += u.length;
          got += u.length;
          if (partsLen >= WRITE_CHUNK) flushTo(path);
          const now = performance.now();
          if (now - last > 200) {
            last = now;
            progress(name, got, total, t0);
          }
        },
      });
      // accept 已 resolve，但最后不到 1MB 的尾巴还在队列里
      flushTo(path);
      await chain;
      if (writeError) throw writeError;
      if (disposed) return;
      progress(name, total || got, total || got, t0);
      hooks.termWrite("\r\n");
      hooks.termWrite(`\x1b[32m[ZMODEM] ${name} 已保存到 ${path}\x1b[0m\r\n`);
      hooks.notify(`${name} 下载完成（${fmtSize(total || got)}）`);
    })().catch((e) => {
      hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 接收失败：${String(e)}\x1b[0m\r\n`);
      safeAbort(zsession);
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
    // 上一次传输若因断线失败，错误标记要清掉，否则这一轮会一进来就报错
    sendError = null;
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
        // 不会像 Browser.send_files 那样把整个文件一次性压进队列。
        //
        // 这里的 drain() 是真的背压：后端写命令 await 一个有界队列，队列满时 IPC
        // 不返回，于是「读盘 → 编码 → 写」被网速卡住。没有它，600MB 文件会一秒内
        // 全部堆进后端内存，进度条直接冲到 99% 再「卡死」。
        const t0 = performance.now();
        let off = xfer.get_offset() || 0;
        let last = 0;
        while (off < f.size) {
          const end = Math.min(off + READ_SLICE, f.size);
          const piece = new Uint8Array(await f.slice(off, end).arrayBuffer());
          if (disposed) return;
          if (end >= f.size) {
            // 最后一片：先让积压落到低水位，再把 ZEOF 发出去。
            // 顺序反了的话 ZEOF 会排在几百 MB 数据后面，远端迟迟不确认。
            off = f.size;
            await deadline(drain(), STALL_MS, "等待发送队列回落");
            progress(f.name, off, f.size, t0);
            await deadline(xfer.end(piece), STALL_MS, "等待远端确认文件结束");
            break;
          }
          xfer.send(piece);
          off = end;
          await deadline(drain(), STALL_MS, "等待发送队列回落");
          const now = performance.now();
          if (now - last > 200) {
            last = now;
            progress(f.name, off, f.size, t0);
          }
        }
        if (f.size === 0) {
          await deadline(xfer.end(new Uint8Array(0)), STALL_MS, "等待远端确认文件结束");
        }
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
        zsession.on("offer", (x) => handleOffer(zsession, x));
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
