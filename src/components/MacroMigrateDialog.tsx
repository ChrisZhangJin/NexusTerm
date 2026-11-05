import { useEffect, useState } from "react";
import { api } from "../api";
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
        <h2>{results ? "迁移结果" : ids ? "转为技能" : "全部迁移为技能"}</h2>

        {!previews && !error && <p className="macro-empty">正在转换…</p>}

        {previews && !results && (
          <>
            <p className="hint" style={{ margin: "0 0 10px" }}>
              预览：以下是转换出的技能草稿，点「迁移」才会保存。原宏不会被修改或删除；已迁移过的宏会被跳过。
              {warned > 0 && `有 ${warned} 条带警告，请逐条核对。`}
            </p>
            <div className="macro-mig-list">
              {previews.length === 0 && <p className="macro-empty">没有宏可迁移。</p>}
              {previews.map((p) => (
                <div className="macro-mig-item" key={p.macroId}>
                  <div className="macro-mig-head">
                    <strong>{p.macroName || p.macroId}</strong>
                    {p.draft && (
                      <span className="macro-mig-tag">
                        {p.draft.steps.length} 步 · {RISK_LABEL[p.draft.risk]}
                        {p.draft.name !== p.macroName && ` · 技能名「${p.draft.name}」`}
                      </span>
                    )}
                    {!p.draft && <span className="macro-mig-tag dim">不会迁移</span>}
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
                  {p.skipReason && <p className="macro-empty">跳过：{p.skipReason}</p>}
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
              {summarizeMigration(results)}。技能可在技能管理里查看和编辑；原宏仍保留。
            </p>
            <div className="macro-mig-list">
              {results.map((r) => (
                <div className="macro-mig-item" key={r.macroId}>
                  <div className="macro-mig-head">
                    <strong>{r.macroName || r.macroId}</strong>
                    <span className={`macro-mig-tag${r.outcome.kind === "migrated" ? "" : " dim"}`}>
                      {r.outcome.kind === "migrated"
                        ? "已迁移"
                        : r.outcome.kind === "skipped"
                          ? `已跳过：${r.outcome.reason}`
                          : "失败"}
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
                取消
              </button>
              <button
                className="btn primary"
                disabled={busy || migratable === 0}
                onClick={() => void run()}
              >
                {busy ? "迁移中…" : `迁移（${migratable} 条）`}
              </button>
            </>
          ) : (
            <button className="btn primary" onClick={onClose}>
              {results ? "完成" : "关闭"}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
