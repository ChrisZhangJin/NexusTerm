import { useEffect, useState } from "react";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import { RISK_LABEL, STEP_KIND_LABEL, describeSkillStep } from "../lib/skill";
import { countMigratable, countWarned, summarizeMigration } from "../lib/macroMigrate";
import type { MacroMigrationPreview, MacroMigrationResult } from "../types";

interface Props {
  /** 要迁移的宏 id；null = 全部 */
  ids: string[] | null;
  onClose: () => void;
}

/**
 * 旧宏 → 技能：先预览（不写库），确认后再迁移。
 * 迁移不会改动或删除原宏；已迁移过的宏会被跳过。
 */
export default function MacroMigrateDialog({ ids, onClose }: Props) {
  useLocale();
  const [previews, setPreviews] = useState<MacroMigrationPreview[] | null>(null);
  const [results, setResults] = useState<MacroMigrationResult[] | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    api
      .macrosMigratePreview(ids ?? undefined)
      .then((p) => alive && setPreviews(p))
      .catch((e) => alive && setError(String(e)));
    return () => {
      alive = false;
    };
  }, [ids]);

  // Esc 只关这一层（宏管理器那边在本窗口打开时不响应 Esc）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const run = async () => {
    setBusy(true);
    setError(null);
    try {
      setResults(await api.macrosMigrate(ids ?? undefined));
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const migratable = previews ? countMigratable(previews) : 0;
  const warned = previews ? countWarned(previews) : 0;

  return (
    // 本窗口渲染在宏管理器的遮罩里面：点遮罩只关这一层，不能冒泡把宏管理器也关了
    <div
      className="modal-backdrop"
      onClick={(e) => {
        e.stopPropagation();
        onClose();
      }}
    >
      <div className="modal macro-modal macro-mig" onClick={(e) => e.stopPropagation()}>
        <h2>
          {results
            ? t("迁移结果", "Migration results")
            : ids
              ? t("转为技能", "Convert to skill")
              : t("全部迁移为技能", "Migrate all to skills")}
        </h2>

        {!previews && !error && <p className="macro-empty">{t("正在转换…", "Converting…")}</p>}

        {previews && !results && (
          <>
            <p className="hint" style={{ margin: "0 0 10px" }}>
              {t(
                "预览：以下是转换出的技能草稿，点「迁移」才会保存。原宏不会被修改或删除；已迁移过的宏会被跳过。",
                "Preview: these are the converted skill drafts. Nothing is saved until you click Migrate. The original macros aren't changed or deleted; macros already migrated are skipped."
              )}
              {warned > 0 &&
                t("有 {n} 条带警告，请逐条核对。", " {n} have warnings; please review each one.", { n: warned })}
            </p>
            <div className="macro-mig-list">
              {previews.length === 0 && <p className="macro-empty">{t("没有宏可迁移。", "No macros to migrate.")}</p>}
              {previews.map((p) => (
                <div className="macro-mig-item" key={p.macroId}>
                  <div className="macro-mig-head">
                    <strong>{p.macroName || p.macroId}</strong>
                    {p.draft && (
                      <span className="macro-mig-tag">
                        {t("{n} 步", p.draft.steps.length === 1 ? "{n} step" : "{n} steps", { n: p.draft.steps.length })} ·{" "}
                        {RISK_LABEL[p.draft.risk]}
                        {p.draft.name !== p.macroName && t(" · 技能名「{name}」", " · Skill name “{name}”", { name: p.draft.name })}
                      </span>
                    )}
                    {!p.draft && <span className="macro-mig-tag dim">{t("不会迁移", "Won't migrate")}</span>}
                  </div>
                  {p.draft && (
                    <ol className="macro-mig-steps">
                      {p.draft.steps.map((s, i) => (
                        <li key={i}>
                          <span className="macro-mig-kind">{STEP_KIND_LABEL[s.kind]}</span>
                          <code>{describeSkillStep(s)}</code>
                        </li>
                      ))}
                    </ol>
                  )}
                  {p.skipReason && <p className="macro-empty">{t("跳过：", "Skipped: ")}{p.skipReason}</p>}
                  {p.errors.map((e, i) => (
                    <p className="error" key={i}>
                      {e}
                    </p>
                  ))}
                  {p.warnings.map((w, i) => (
                    <p className="macro-mig-warn" key={i}>
                      ⚠ {w}
                    </p>
                  ))}
                </div>
              ))}
            </div>
          </>
        )}

        {results && (
          <>
            <p className="hint" style={{ margin: "0 0 10px" }}>
              {summarizeMigration(results)}
              {t("。技能可在技能管理里查看和编辑；原宏仍保留。", ". You can view and edit the skills in Skill manager; the original macros are kept.")}
            </p>
            <div className="macro-mig-list">
              {results.map((r) => (
                <div className="macro-mig-item" key={r.macroId}>
                  <div className="macro-mig-head">
                    <strong>{r.macroName || r.macroId}</strong>
                    <span className={`macro-mig-tag${r.outcome.kind === "migrated" ? "" : " dim"}`}>
                      {r.outcome.kind === "migrated"
                        ? t("已迁移", "Migrated")
                        : r.outcome.kind === "skipped"
                          ? t("已跳过：{r}", "Skipped: {r}", { r: r.outcome.reason })
                          : t("失败", "Failed")}
                    </span>
                  </div>
                  {r.outcome.kind === "failed" &&
                    r.outcome.errors.map((e, i) => (
                      <p className="error" key={i}>
                        {e}
                      </p>
                    ))}
                  {r.warnings.map((w, i) => (
                    <p className="macro-mig-warn" key={i}>
                      ⚠ {w}
                    </p>
                  ))}
                </div>
              ))}
            </div>
          </>
        )}

        {error && <p className="error">{error}</p>}

        <div className="modal-actions">
          {previews && !results ? (
            <>
              <button className="btn" onClick={onClose}>
                {t("取消", "Cancel")}
              </button>
              <button
                className="btn primary"
                disabled={busy || migratable === 0}
                onClick={() => void run()}
              >
                {busy ? t("迁移中…", "Migrating…") : t("迁移（{n} 条）", "Migrate ({n})", { n: migratable })}
              </button>
            </>
          ) : (
            <button className="btn primary" onClick={onClose}>
              {results ? t("完成", "Done") : t("关闭", "Close")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
