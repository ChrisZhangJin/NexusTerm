import { useSyncExternalStore } from "react";
import { isMacOS } from "./platform";

/**
 * 应用级偏好 + 快捷键配置。
 * 全局单例 + localStorage 持久化 + useSyncExternalStore 订阅：
 * 设置窗口里一改，终端右键菜单、按键处理、状态栏同时生效，无需层层传 props。
 */

// ——— 偏好 ———

export interface Prefs {
  /** 底部栏是否展示 CPU / 内存 / 磁盘 */
  showResources: boolean;
  /** 实时监控：关闭后停止轮询、去掉一切动画 */
  liveMonitor: boolean;
  /** 鼠标选中文本即自动复制到剪贴板 */
  copyOnSelect: boolean;
  /** 关闭仍在连接的标签前弹确认 */
  confirmCloseTab: boolean;
  /**
   * 文件面板复用已登录的终端连接（在同一条连接上开 sftp 子系统）。
   *
   * 开着的好处：不用第二次认证 —— 对要 OTP / 堡垒机验证的服务器，等于
   * 每次打开文件面板省一次验证码。关掉则每次独立拨号（传输大文件时不会
   * 和交互式 shell 抢同一条 TCP 连接的带宽）。
   */
  shareSftpConnection: boolean;
}

const PREFS_KEY = "nx.prefs";
const DEFAULT_PREFS: Prefs = {
  showResources: true,
  liveMonitor: true,
  copyOnSelect: true,
  confirmCloseTab: true,
  shareSftpConnection: true,
};

// ——— 快捷键 ———

export type ShortcutAction =
  | "disconnect"
  | "copy"
  | "paste"
  | "softNewline"
  | "find"
  | "selectAll"
  | "clearScreen"
  | "exportBuffer";

export const SHORTCUT_LABELS: Record<ShortcutAction, string> = {
  disconnect: "断开",
  copy: "复制",
  paste: "粘贴",
  softNewline: "软换行（不执行）",
  find: "查找…",
  selectAll: "全选",
  clearScreen: "清屏",
  exportBuffer: "导出…",
};

export const SHORTCUT_ORDER: ShortcutAction[] = [
  "disconnect",
  "copy",
  "paste",
  "softNewline",
  "selectAll",
  "clearScreen",
  "find",
  "exportBuffer",
];

/** 绑定串格式："Ctrl+Shift+C"，空串 = 未绑定 */
export type Shortcuts = Record<ShortcutAction, string>;

const SC_KEY = "nx.shortcuts";
export const DEFAULT_SHORTCUTS: Shortcuts = isMacOS()
  ? {
      disconnect: "Alt+C",
      copy: "Meta+C",
      paste: "Meta+V",
      softNewline: "Ctrl+Enter",
      find: "Meta+F",
      selectAll: "",
      clearScreen: "",
      exportBuffer: "",
    }
  : {
      disconnect: "Alt+C",
      copy: "Ctrl+Shift+C",
      paste: "Ctrl+Shift+V",
      softNewline: "Ctrl+Enter",
      find: "Ctrl+Shift+F",
      selectAll: "",
      clearScreen: "",
      exportBuffer: "",
    };

// ——— store ———

function load<T extends object>(key: string, def: T): T {
  try {
    const raw = localStorage.getItem(key);
    if (!raw) return { ...def };
    return { ...def, ...(JSON.parse(raw) as Partial<T>) };
  } catch {
    return { ...def };
  }
}

let prefs: Prefs = load(PREFS_KEY, DEFAULT_PREFS);
// 旧版把「关闭确认 · 不再询问」单独存在 nx.confirmCloseTab，迁入 prefs 后删除
if (localStorage.getItem("nx.confirmCloseTab") === "off") {
  prefs = { ...prefs, confirmCloseTab: false };
  localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  localStorage.removeItem("nx.confirmCloseTab");
}

/** 非 React 代码里读当前偏好 */
export const getPrefs = (): Prefs => prefs;
let shortcuts: Shortcuts = load(SC_KEY, DEFAULT_SHORTCUTS);
const listeners = new Set<() => void>();
const emit = () => listeners.forEach((f) => f());
const subscribe = (f: () => void) => {
  listeners.add(f);
  return () => listeners.delete(f);
};

export function setPrefs(patch: Partial<Prefs>) {
  prefs = { ...prefs, ...patch };
  localStorage.setItem(PREFS_KEY, JSON.stringify(prefs));
  emit();
}

export function setShortcuts(next: Shortcuts) {
  shortcuts = { ...next };
  localStorage.setItem(SC_KEY, JSON.stringify(shortcuts));
  emit();
}

export function getShortcuts(): Shortcuts {
  return shortcuts;
}

export const usePrefs = () => useSyncExternalStore(subscribe, () => prefs);
export const useShortcuts = () => useSyncExternalStore(subscribe, () => shortcuts);

// ——— 按键 ⇄ 绑定串 ———

const MOD_KEYS = new Set(["Control", "Shift", "Alt", "Meta", "AltGraph", "CapsLock", "NumLock"]);

/** 物理键名：优先用 e.code（不受 Shift / 输入法 / 键盘布局影响） */
function keyName(e: KeyboardEvent): string | null {
  const c = e.code;
  if (/^Key[A-Z]$/.test(c)) return c.slice(3);
  if (/^Digit\d$/.test(c)) return c.slice(5);
  if (/^F\d{1,2}$/.test(c)) return c;
  const named: Record<string, string> = {
    Space: "Space", Enter: "Enter", Tab: "Tab", Backspace: "Backspace", Delete: "Delete",
    Insert: "Insert", Home: "Home", End: "End", PageUp: "PageUp", PageDown: "PageDown",
    ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left", ArrowRight: "Right",
    Minus: "-", Equal: "=", BracketLeft: "[", BracketRight: "]", Backslash: "\\",
    Semicolon: ";", Quote: "'", Comma: ",", Period: ".", Slash: "/", Backquote: "`",
  };
  if (named[c]) return named[c];
  if (MOD_KEYS.has(e.key)) return null;
  return e.key.length === 1 ? e.key.toUpperCase() : e.key;
}

/** 键盘事件 → 绑定串；只按了修饰键时返回 null */
export function comboFromEvent(e: KeyboardEvent): string | null {
  const k = keyName(e);
  if (!k) return null;
  const parts: string[] = [];
  if (e.ctrlKey) parts.push("Ctrl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (e.metaKey) parts.push("Meta");
  parts.push(k);
  return parts.join("+");
}

/** 事件命中的动作（未命中返回 null） */
export function matchShortcut(e: KeyboardEvent, sc: Shortcuts = shortcuts): ShortcutAction | null {
  const combo = comboFromEvent(e);
  if (!combo) return null;
  for (const a of SHORTCUT_ORDER) if (sc[a] && sc[a] === combo) return a;
  return null;
}

/** 显示用：macOS 用符号，其余原样 */
export function formatCombo(combo: string): string {
  if (!combo) return "";
  if (!isMacOS()) return combo;
  const sym: Record<string, string> = { Ctrl: "⌃", Alt: "⌥", Shift: "⇧", Meta: "⌘" };
  return combo
    .split("+")
    .map((p) => sym[p] ?? p)
    .join("");
}

/**
 * 绑定合法性：裸字母/数字/符号会吞掉正常输入，必须带 Ctrl/Alt/Meta；
 * 单独的功能键（F1-F12 等）允许。
 */
export function validateCombo(combo: string): string | null {
  const parts = combo.split("+");
  const key = parts[parts.length - 1];
  const mods = parts.slice(0, -1);
  const hasCmdMod = mods.some((m) => m === "Ctrl" || m === "Alt" || m === "Meta");
  const isFn = /^F\d{1,2}$/.test(key) || ["Insert", "PageUp", "PageDown"].includes(key);
  if (!hasCmdMod && !isFn) return "需包含 Ctrl / Alt" + (isMacOS() ? " / ⌘" : "") + " 修饰键，否则会占用正常输入";
  if (!isMacOS() && mods.length === 1 && mods[0] === "Ctrl" && /^[A-Z]$/.test(key)) {
    return `Ctrl+${key} 是终端控制字符（如 Ctrl+C 中断），建议加 Shift`;
  }
  return null;
}
