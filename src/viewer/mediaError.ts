import { t } from "../i18n";

/**
 * 把 `HTMLMediaElement.error` 翻成人能读的文案。
 * `MediaError.message` 是非标准字段，但 WebKitGTK（Linux 版用的内核）会把 GStreamer
 * 的原始报错（比如「缺少解码器插件」）塞进去——这段原文比笼统的错误码有用得多，能看出来。
 */
export function mediaErrorDetail(err: MediaError | null): string {
  if (!err) return t("未知错误", "Unknown error");
  const codeText: Record<number, string> = {
    1: t("加载被中止", "Loading was aborted"),
    2: t("网络错误", "Network error"),
    3: t("解码失败（文件可能已损坏）", "Decoding failed (the file may be corrupt)"),
    4: t("格式或编码不受支持", "Format or codec not supported"),
  };
  const base = codeText[err.code] ?? t("错误码 {code}", "Error code {code}", { code: err.code });
  const raw = err.message?.trim();
  return raw ? t("{base}（{raw}）", "{base} ({raw})", { base, raw }) : base;
}
