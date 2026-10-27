/**
 * 把 `HTMLMediaElement.error` 翻成人能读的文案。
 * `MediaError.message` 是非标准字段，但 WebKitGTK（Linux 版用的内核）会把 GStreamer
 * 的原始报错（比如「缺少解码器插件」）塞进去——这段原文比笼统的错误码有用得多，能看出来。
 */
export function mediaErrorDetail(err: MediaError | null): string {
  if (!err) return "未知错误";
  const codeText: Record<number, string> = {
    1: "加载被中止",
    2: "网络错误",
    3: "解码失败（文件可能已损坏）",
    4: "格式或编码不受支持",
  };
  const base = codeText[err.code] ?? `错误码 ${err.code}`;
  const raw = err.message?.trim();
  return raw ? `${base}（${raw}）` : base;
}
