/**
 * lrzsz（ZMODEM）支持。
 *
 * 远端 `rz` / `sz` 会向终端发 ZMODEM 握手（截图里那串 `**B00...` 乱码）。
 * xterm.js 只管渲染，协议必须由客户端实现 —— 这里用 zmodem.js：
 *
 * - 所有来自远端的字节先喂 `Sentry.consume()`：非 ZMODEM 数据原样进终端，
 *   一旦嗅探到握手序列就触发 `on_detect`，会话接管整条字节流。
 * - `zsession.type === "receive"` → 远端 `sz`（下载）：offer 给出文件名，
 *   accept() 收完把 payloads 拼成 Uint8Array，弹保存框落盘。
 * - `zsession.type === "send"` → 远端 `rz`（上传）：弹本地文件选择框，
 *   Browser.send_files 发送；用户取消则 abort，远端 rz 会正常退出。
 *
 * 传输提示直接写在终端里（与 Xshell 行为一致），完成/失败再补 toast。
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
  /** 出站字节（SSH write 或本地 PTY write） */
  send: (bytes: number[]) => void;
  /** 轻提示 */
  notify: (msg: string) => void;
}

/** 会话进行中时，键盘输入要被丢弃（出站已被协议帧占用）。 */
export interface ZmodemGate {
  consume: (bytes: Uint8Array) => void;
  isActive: () => boolean;
  dispose: () => void;
}

export function attachZmodem(hooks: ZmodemHooks): ZmodemGate {
  let active = false;
  let disposed = false;

  const fmtSize = (n: number) =>
    n >= 1024 * 1024
      ? `${(n / 1024 / 1024).toFixed(1)} MB`
      : `${(n / 1024).toFixed(1)} KB`;

  /** sz（下载）：accept 收完 → 弹保存框 → invoke write_file 落盘 */
  const handleOffer = (xfer: ZTransfer) => {
    const { name, size } = xfer.get_details();
    hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 接收 ${name}（${fmtSize(size || 0)}）…\x1b[0m`);
    void xfer
      .accept()
      .then(async () => {
        if (disposed) return;
        const payloads = xfer.get_payloads();
        const total = payloads.reduce((n, p) => n + p.length, 0);
        const buf = new Uint8Array(total);
        let off = 0;
        for (const p of payloads) {
          buf.set(p, off);
          off += p.length;
        }
        const { save } = await import("@tauri-apps/plugin-dialog");
        const path = await save({ defaultPath: name });
        if (!path) {
          hooks.termWrite(`\r\n\x1b[90m[ZMODEM] 已取消保存（数据在剪贴板之外已丢弃）\x1b[0m\r\n`);
          return;
        }
        await api.writeFile(path, Array.from(buf));
        hooks.termWrite(`\r\n\x1b[32m[ZMODEM] ${name} 已保存到 ${path}\x1b[0m\r\n`);
        hooks.notify(`${name} 下载完成（${fmtSize(total)}）`);
      })
      .catch((e) => {
        hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 接收失败：${String(e)}\x1b[0m\r\n`);
      });
  };

  /** rz（上传）：弹本地文件框 → send_files */
  const handleUpload = (zsession: ZSession) => {
    hooks.termWrite(`\r\n\x1b[33m[ZMODEM] 请选择要上传的文件…\x1b[0m\r\n`);
    pickFiles().then((files) => {
      if (disposed) return;
      if (files.length === 0) {
        hooks.termWrite(`\r\n\x1b[90m[ZMODEM] 已取消，正在中止远端 rz…\x1b[0m\r\n`);
        try {
          zsession.abort();
        } catch {
          /* 会话可能已结束 */
        }
        return;
      }
      const obj: Record<string, File> = {};
      for (const f of files) obj[f.name] = f;
      const total = files.reduce((n, f) => n + f.size, 0);
      hooks.termWrite(`\x1b[33m[ZMODEM] 上传 ${files.length} 个文件（${fmtSize(total)}）…\x1b[0m\r\n`);
      Zmodem.Browser.send_files(zsession, obj)
        .then(() => {
          hooks.termWrite(`\r\n\x1b[32m[ZMODEM] 上传完成\x1b[0m\r\n`);
          hooks.notify(`${files.length} 个文件上传完成`);
        })
        .catch((e) => {
          hooks.termWrite(`\r\n\x1b[31m[ZMODEM] 上传失败：${String(e)}\x1b[0m\r\n`);
        });
    });
  };

  /** <input type="file"> 在 Tauri webview 里可用，比 dialog 插件多拿到 File 对象本身 */
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
      const onFocus = () => window.setTimeout(() => done(input.files ? Array.from(input.files) : []), 300);
      input.onchange = () => done(input.files ? Array.from(input.files) : []);
      // 用户直接点取消：输入框不会触发 change，靠窗口重新获焦兜底
      window.addEventListener("focus", onFocus);
      input.click();
    });
  }

  const sentry: ZSentry = new Zmodem.Sentry({
    to_terminal: (octets) => {
      hooks.termWriteBytes(new Uint8Array(Array.from(octets)));
    },
    sender: (octets) => hooks.send(octets),
    on_detect: (detection) => {
      if (disposed) return;
      let zsession: ZSession;
      try {
        zsession = detection.confirm();
      } catch {
        return; // Detection 已失效（被后续输入 retract）
      }
      active = true;
      if (zsession.type === "receive") {
        hooks.termWrite(`\x1b[33m[ZMODEM] 检测到远端发送（sz）\x1b[0m`);
        zsession.on("offer", handleOffer);
        zsession.on("session_end", () => {
          active = false;
        });
        zsession.start();
      } else {
        hooks.termWrite(`\x1b[33m[ZMODEM] 检测到远端接收（rz）\x1b[0m`);
        zsession.on("session_end", () => {
          active = false;
        });
        handleUpload(zsession);
      }
    },
    on_retract: () => {
      /* Detection 被后续输入判为非 ZMODEM，无需处理 */
    },
  });

  return {
    consume: (bytes) => sentry.consume(bytes),
    isActive: () => active,
    dispose: () => {
      disposed = true;
    },
  };
}
