/**
 * 界面语言：中文 / English。
 *
 * ## 用法
 *
 * 文案就地写成中英一对：`t("连接失败", "Connection failed")`；带变量用 `{name}` 占位：
 * `t("已导入 {n} 个连接", "Imported {n} connections", { n })`。
 * 不搞集中的 key 字典：只有两种语言，英文就写在中文旁边最好维护，也不会让好几个人同时改一个大字典。
 *
 * 组件里渲染了翻译文案的，在开头调一次 `useLocale()`：切换语言时它会重渲染。
 * 模块加载时就算好的常量（`const LABELS = { … }`）切换语言不会变，要改成函数或在渲染时取。
 *
 * ## 选哪种语言
 *
 * 默认跟随系统（系统语言是中文就中文，否则英文），设置里可以改成固定中文 / 英文，记在 localStorage，
 * 同源的几个窗口（主窗口、浏览窗口、浮动 Agent 窗口）经 `storage` 事件同步。
 * 选定的语言同时告诉后端（`set_locale`）：后端直接显示给用户的提示、AI 回答的语言都跟着走。
 *
 * 非浏览器环境（单元测试）一律中文：现有用例按中文断言。
 */
import { useSyncExternalStore } from "react";

export type Locale = "zh" | "en";
export type LocalePref = Locale | "system";

const LS_KEY = "nx.locale";

function storedPref(): LocalePref {
  try {
    const v = localStorage.getItem(LS_KEY);
    return v === "zh" || v === "en" ? v : "system";
  } catch {
    return "system";
  }
}

/** 系统语言：中文开头的（zh-CN / zh-TW / zh-Hans…）用中文，其余英文 */
export function systemLocale(langs: readonly string[]): Locale {
  const first = langs.find((l) => !!l) ?? "";
  if (!first) return "zh";
  return /^zh\b/i.test(first) ? "zh" : "en";
}

function resolve(pref: LocalePref): Locale {
  if (pref !== "system") return pref;
  if (typeof window === "undefined" || typeof navigator === "undefined") return "zh";
  return systemLocale(navigator.languages?.length ? navigator.languages : [navigator.language]);
}

let pref: LocalePref = typeof window === "undefined" ? "system" : storedPref();
let current: Locale = resolve(pref);
const listeners = new Set<() => void>();

function emit() {
  if (typeof document !== "undefined") document.documentElement.lang = current === "zh" ? "zh-CN" : "en";
  listeners.forEach((f) => f());
  syncBackend();
}

/** 把当前语言告诉后端（后端提示、AI 回答语言跟着走）。后端未就绪 / 测试环境里静默失败 */
function syncBackend() {
  if (typeof window === "undefined") return;
  void import("@tauri-apps/api/core")
    .then(({ invoke }) => invoke("set_locale", { locale: current }))
    .catch(() => {});
}

/** 当前生效的语言 */
export const locale = (): Locale => current;
/** 用户的选择（system = 跟随系统） */
export const localePref = (): LocalePref => pref;

export function setLocalePref(next: LocalePref): void {
  pref = next;
  try {
    if (next === "system") localStorage.removeItem(LS_KEY);
    else localStorage.setItem(LS_KEY, next);
  } catch {
    /* 存不了就只在这次生效 */
  }
  current = resolve(pref);
  emit();
}

/** 启动时调一次：同步后端、设好 <html lang>，并跟随别的窗口里的切换 */
export function initLocale(): void {
  emit();
  if (typeof window === "undefined") return;
  window.addEventListener("storage", (e) => {
    if (e.key !== LS_KEY) return;
    pref = storedPref();
    const next = resolve(pref);
    if (next === current) return;
    current = next;
    emit();
  });
}

/** 渲染了翻译文案的组件调一次：切换语言时重渲染。返回当前语言 */
export function useLocale(): Locale {
  return useSyncExternalStore(
    (f) => {
      listeners.add(f);
      return () => listeners.delete(f);
    },
    () => current,
    () => current
  );
}

/** 中英一对里取当前语言那条；`vars` 替换 `{name}` 占位 */
export function t(zh: string, en: string, vars?: Record<string, string | number>): string {
  const s = current === "en" ? en : zh;
  if (!vars) return s;
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m));
}

/** 测试用：直接指定当前语言（不碰 localStorage、不通知后端） */
export function __setLocaleForTest(l: Locale): void {
  current = l;
  listeners.forEach((f) => f());
}
