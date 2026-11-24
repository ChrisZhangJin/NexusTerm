import { setLocalePref, t, useLocale, useLocalePref, type LocalePref } from "../i18n";

export default function LanguageSelect({ compact = false }: { compact?: boolean }) {
  const locale = useLocale();
  const pref = useLocalePref();

  return (
    <select
      className={compact ? "sb-language" : undefined}
      value={pref}
      onChange={(e) => setLocalePref(e.target.value as LocalePref)}
      aria-label="语言 / Language"
      title={compact ? "语言 / Language" : undefined}
    >
      <option value="system">
        {compact
          ? t("系统 · {lang}", "System · {lang}", { lang: locale === "zh" ? "中文" : "EN" })
          : t("跟随系统", "Follow system")}
      </option>
      <option value="zh" lang="zh-CN">中文</option>
      <option value="en" lang="en">{compact ? "EN" : "English"}</option>
    </select>
  );
}
