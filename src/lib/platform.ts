/**
 * 宿主平台探测。
 *
 * 三处 UI 必须按平台分支，差一个像素都会显得很业余：
 * 1. 标题栏左侧 80px 交通灯留白 —— 只有 macOS 的 Overlay 标题栏需要；
 * 2. 窗口最小化 / 最大化 / 关闭按钮 —— macOS 由窗口服务器画，其余平台没有
 *    系统装饰可用（decorations 关掉后），必须自绘；
 * 3. 快捷键提示 —— ⌘ 与 Ctrl 的写法不同。
 *
 * 值来自 Rust 的 `std::env::consts::OS`，不靠 UA 嗅探：WebView 的 UA
 * 在各版本间会变，而编译期常量不会错。
 */

import { invoke } from "@tauri-apps/api/core";

export type Platform = "windows" | "macos" | "linux" | "unknown";

export interface HostInfo {
  platform: Platform;
  arch: string;
  osVersion: string;
}

let cache: HostInfo | null = null;

/** 在 main.tsx 首次渲染前调用一次，之后所有组件都能同步读到平台。 */
export async function initPlatform(): Promise<HostInfo> {
  if (cache) return cache;
  try {
    cache = await invoke<HostInfo>("host_info");
  } catch {
    // 后端不可达（例如浏览器里单独跑 vite）时退化为未知平台，界面按非 macOS 渲染
    cache = { platform: "unknown", arch: "", osVersion: "" };
  }
  document.documentElement.dataset.platform = cache.platform;
  return cache;
}

/** 同步读取。必须在 initPlatform() 之后调用。 */
export function platform(): Platform {
  return cache?.platform ?? "unknown";
}

export const isMacOS = () => platform() === "macos";
export const isWindows = () => platform() === "windows";

/** 主修饰键的显示名：macOS 是 ⌘，其余平台是 Ctrl。 */
export const modKeyLabel = () => (isMacOS() ? "⌘" : "Ctrl");

/** 密码存放处的名称。三端是三套不同的系统凭据服务，提示文案不能写死 macOS。 */
export const credentialServiceName = () => {
  if (isMacOS()) return "macOS Keychain";
  if (isWindows()) return "Windows 凭据管理器";
  return "Secret Service";
};

/** `~/.ssh` 在本平台的实际位置。Windows 没有 HOME，路径是 %USERPROFILE%。 */
export const sshDirLabel = () =>
  isWindows() ? "%USERPROFILE%\\.ssh" : "~/.ssh";
