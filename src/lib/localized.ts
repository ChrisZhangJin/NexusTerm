import { t } from "../i18n";

/**
 * 中英对照表 → 按当前语言取文案的对象：调用处 `LABELS[k]` 的写法不变，
 * 每次读都现取（getter），切换语言后自然跟着变，不会停在模块加载时的语言。
 */
export function localized<K extends string>(pairs: Record<K, readonly [string, string]>): Record<K, string> {
  const out = {} as Record<K, string>;
  for (const k of Object.keys(pairs) as K[]) {
    const [zh, en] = pairs[k];
    Object.defineProperty(out, k, { get: () => t(zh, en), enumerable: true });
  }
  return out;
}
