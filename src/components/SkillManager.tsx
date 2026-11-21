import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { api } from "../api";
import type { Skill, SkillInput, SkillTrustView } from "../types";
import {
  describeHostScope,
  exportFileName,
  formatLastRun,
  SOURCE_LABEL,
  splitProblems,
} from "../lib/skillForm";
import { RISK_LABEL, scopeBlock, trustBadge } from "../lib/skill";
import ConfirmDialog from "./ConfirmDialog";
import SkillEditor from "./SkillEditor";
import { Highlighted, RiskBadge, StepPreview } from "./SkillParts";
import "./SkillManager.css";

/**
 * 技能（Skill）管理器的入参。
 *
 * **`initialDraft` 契约**：传一份 `SkillInput`（没有 id / 计数 / 时间戳）就会在管理器打开时（以及之后
 * 每次传入一个新的对象引用时）直接弹出「新建技能」编辑器并预填它；保存走 `skill_create`，
 * 草稿里的 `source`（`recorded` / `migrated_macro` …）原样带上，缺省为 `manual`。
 * 用户取消则什么都不落库。草稿里 secret 参数的默认值会被丢弃并提示（见 `skillForm.toForm`）；
 * 草稿里的 `risk` 只是起点，后端保存时会把它抬到不低于自动评估值。
 * 3.2 录制器 / 3.3 LLM 草稿 / 3.5 旧宏迁移都通过它把产物交给用户确认后再保存。
 */
export interface SkillManagerProps {
  onClose: () => void;
  initialDraft?: SkillInput;
  /**
   * 「运行」回调（4.2）。由面板提供：它持有当前会话与事件订阅，运行进度 / 审批 / 密码卡片都显示在面板里。
   * 缺省（没有可用会话 / 正忙）时「运行」按钮置灰。`params` 只含非 secret 参数。
   */
  onRunSkill?: (skill: Skill, params: Record<string, string>) => void;
  /**
   * 当前会话（4.4）：给了就按它判断各技能的适用主机范围——超出的行显示「超出范围」并置灰「运行」。
   * 不给则不做范围判断（只显示信任档位）。
   */
  scopeTarget?: { sessionId: string; label: string };
}

/** 与后端 `MAX_IMPORT_BYTES` 一致；本地先拦一道，省得读一个巨大的文件 */
const MAX_IMPORT_BYTES = 256 * 1024;
const SEARCH_DEBOUNCE = 200;

// ——————————————————————————————— 列表行 ———————————————————————————————

const SkillRow = memo(function SkillRow({
  skill,
  onEdit,
  onExport,
  onDelete,
  onRun,
  trust,
}: {
  skill: Skill;
  /** 4.4：后端算出的信任结论（含范围判断）；还没加载到时为空 */
  trust?: SkillTrustView;
  onRun?: (s: Skill) => void;
  onEdit: (s: Skill) => void;
  onExport: (s: Skill) => void;
  onDelete: (s: Skill) => void;
}) {
  const badge = trustBadge(trust);
  const blocked = scopeBlock(trust);
  return (
    <div
      className="sk-row"
      role="button"
      tabIndex={0}
      onClick={() => onEdit(skill)}
      onKeyDown={(e) => {
        if (e.key === "Enter" && e.target === e.currentTarget) onEdit(skill);
      }}
    >
      <div className="sk-c-name">
        <div className="sk-name" title={skill.name}>
          {skill.name}
        </div>
        {skill.description && (
          <div className="sk-desc" title={skill.description}>
            {skill.description}
          </div>
        )}
      </div>
      <div className="sk-c-risk">
        <RiskBadge risk={skill.risk} />
      </div>
      <div className="sk-c-trust">
        <span className={`sk-trust ${badge.tone}`} title={badge.title}>
          {badge.label}
        </span>
        {blocked && (
          <span className="sk-trust scope" title={blocked}>
            超出范围
          </span>
        )}
      </div>
      <div className="sk-c-host" title={describeHostScope(skill.hostScope)}>
        {describeHostScope(skill.hostScope)}
      </div>
      <div className="sk-c-src">{SOURCE_LABEL[skill.source]}</div>
      <div className="sk-c-cnt" title="成功 / 失败次数">
        <span className="sk-ok">{skill.successCount}</span>
        {" / "}
        <span className={skill.failCount > 0 ? "sk-fail" : ""}>{skill.failCount}</span>
      </div>
      <div className="sk-c-last">{formatLastRun(skill.lastRunTs)}</div>
      <div className="sk-c-act" onClick={(e) => e.stopPropagation()}>
        <button
          className="sk-ibtn"
          title={blocked ? `不能在当前会话运行：${blocked}` : onRun ? "在当前会话运行" : "没有可用的会话，或 Agent 正忙"}
          disabled={!onRun || !!blocked}
          onClick={() => onRun?.(skill)}
        >
          ▶
        </button>
        <button className="sk-ibtn" title="导出为文件" onClick={() => onExport(skill)}>
          ⤒
        </button>
        <button className="sk-ibtn danger" title="删除" onClick={() => onDelete(skill)}>
          ✕
        </button>
      </div>
    </div>
  );
});

// ——————————————————————————————— 导入对话框 ———————————————————————————————

interface ImportState {
  fileName: string;
  text: string;
  preview: Skill | null;
  /** 解析 / 校验失败的逐条问题 */
  problems: string[];
}

function ImportDialog({
  state,
  existingNames,
  busy,
  onPick,
  onConfirm,
  onClose,
}: {
  state: ImportState;
  existingNames: ReadonlySet<string>;
  busy: boolean;
  onPick: () => void;
  onConfirm: () => void;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const p = state.preview;
  const declared = useMemo(() => new Set(p?.params.map((x) => x.name) ?? []), [p]);
  const secretNames = useMemo(() => new Set(p?.params.filter((x) => x.secret).map((x) => x.name) ?? []), [p]);

  return (
    <div className="modal-backdrop sk-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal sk-modal sk-import" onMouseDown={(e) => e.stopPropagation()}>
        <h2>导入技能</h2>
        <div className="sk-body">
          <div className="sk-import-file">
            <span className="sk-note">文件：{state.fileName}</span>
            <button className="btn sk-small" onClick={onPick} disabled={busy}>
              换一个文件…
            </button>
          </div>

          {state.problems.length > 0 && (
            <div className="sk-errbox">
              <div className="sk-errbox-title">无法导入（{state.problems.length} 个问题）</div>
              {state.problems.map((t, i) => (
                <div key={i}>{t}</div>
              ))}
            </div>
          )}

          {p && (
            <>
              <div className="sk-warn">
                导入的技能不带信任：来源标记为「导入」，成功 / 失败计数从 0 开始，风险会重新评估。
                先看一遍下面的步骤再确认。
              </div>
              {existingNames.has(p.name) && <div className="sk-warn">已有同名技能「{p.name}」，导入后会多出一个同名的。</div>}
              <div className="sk-prev-head">
                <strong>{p.name}</strong>
                <RiskBadge risk={p.risk} />
                <span className="sk-note">适用主机：{describeHostScope(p.hostScope)}</span>
              </div>
              {p.description && <div className="sk-note">{p.description}</div>}
              {p.params.length > 0 && (
                <div className="sk-prev-params">
                  参数：
                  {p.params.map((x) => (
                    <span className="sk-chip" key={x.name} title={x.description}>
                      {x.name}
                      {x.secret ? "（secret）" : x.default !== undefined ? ` = ${x.default}` : ""}
                      {x.required ? " *" : ""}
                    </span>
                  ))}
                </div>
              )}
              <StepPreview steps={p.steps} declared={declared} secretNames={secretNames} />
              {p.verify && (
                <div className="sk-note">
                  验证：屏幕应出现{" "}
                  <code className="sk-code">
                    <Highlighted text={p.verify.expect} declared={declared} secretNames={secretNames} />
                  </code>
                </div>
              )}
            </>
          )}
        </div>
        <div className="sk-foot">
          <div className="sk-foot-msg" />
          <button className="btn" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button className="btn primary" onClick={onConfirm} disabled={busy || !p}>
            {busy ? "导入中…" : "导入"}
          </button>
        </div>
      </div>
    </div>
  );
}

// ——————————————————————————————— 运行参数对话框 ———————————————————————————————

/** 运行前填参数：只列非 secret 参数（有默认值就预填）；secret 参数运行时由密码卡片向你索取，不在这里出现。 */
function RunDialog({
  skill,
  onRun,
  onClose,
}: {
  skill: Skill;
  onRun: (params: Record<string, string>) => void;
  onClose: () => void;
}) {
  const plain = skill.params.filter((p) => !p.secret);
  const secrets = skill.params.filter((p) => p.secret);
  const [vals, setVals] = useState<Record<string, string>>(() =>
    Object.fromEntries(plain.map((p) => [p.name, p.default ?? ""]))
  );
  const missing = plain.filter((p) => p.required && !(vals[p.name] ?? "").trim());

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const go = () => {
    if (missing.length > 0) return;
    // 空串视为没填：让后端用默认值 / 按 required 报错
    onRun(Object.fromEntries(Object.entries(vals).filter(([, v]) => v !== "")));
  };

  return (
    <div className="modal-backdrop sk-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className="modal sk-modal sk-import" onMouseDown={(e) => e.stopPropagation()}>
        <h2>运行「{skill.name}」</h2>
        <div className="sk-body">
          <div className="sk-warn">
            将在当前会话里按步骤执行。每一步仍会经过危险命令检查与审批；审批模式下每个写入步骤都要你确认。
          </div>
          {plain.map((p) => (
            <div className="sk-field" key={p.name}>
              <label className="sk-lbl">
                {p.name}
                {p.required ? " *" : ""}
                {p.description ? `　${p.description}` : ""}
              </label>
              <input
                className="sk-input mono"
                value={vals[p.name] ?? ""}
                onChange={(e) => setVals({ ...vals, [p.name]: e.target.value })}
                onKeyDown={(e) => e.key === "Enter" && go()}
              />
            </div>
          ))}
          {secrets.length > 0 && (
            <div className="sk-note">
              secret 参数（{secrets.map((p) => p.name).join("、")}）不在这里填：运行到需要时会弹出密码卡片，由你输入。
            </div>
          )}
        </div>
        <div className="sk-foot">
          <div className="sk-foot-msg sk-note">{missing.length > 0 ? `请填写：${missing.map((p) => p.name).join("、")}` : ""}</div>
          <button className="btn" onClick={onClose}>
            取消
          </button>
          <button className="btn primary" onClick={go} disabled={missing.length > 0}>
            运行
          </button>
        </div>
      </div>
    </div>
  );
}

// ——————————————————————————————— 管理器 ———————————————————————————————

export default function SkillManager({ onClose, initialDraft, onRunSkill, scopeTarget }: SkillManagerProps) {
  const [skills, setSkills] = useState<Skill[] | null>(null);
  // 4.4：id → 信任结论。失败 / 没加载到就是空表，徽标按最严的「需逐步确认」显示
  const [trust, setTrust] = useState<Record<string, SkillTrustView>>({});
  const scopeSid = scopeTarget?.sessionId;
  const scopeLabel = scopeTarget?.label;
  const [query, setQuery] = useState("");
  const [loadErr, setLoadErr] = useState<string | null>(null);
  const [notice, setNotice] = useState<{ text: string; kind: "ok" | "err" } | null>(null);
  // k：每次打开编辑器换一个，保证重建（连续两份草稿也不会串状态）
  const editSeq = useRef(0);
  const open = (e: { skill?: Skill; draft?: SkillInput }) => ({ ...e, k: ++editSeq.current });
  const [editing, setEditing] = useState<{ skill?: Skill; draft?: SkillInput; k: number } | null>(() =>
    initialDraft ? open({ draft: initialDraft }) : null
  );
  const [confirmDel, setConfirmDel] = useState<Skill | null>(null);
  const [imp, setImp] = useState<ImportState | null>(null);
  const [impBusy, setImpBusy] = useState(false);
  const [runTarget, setRunTarget] = useState<Skill | null>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  // 之后传入的新草稿也要弹出编辑器（首个草稿已在初始 state 里处理）
  const firstDraft = useRef(initialDraft);
  useEffect(() => {
    if (initialDraft && initialDraft !== firstDraft.current) setEditing(open({ draft: initialDraft }));
  }, [initialDraft]);

  // 列表：按关键字走后端搜索（名称 / 描述子串）。请求序号防止慢回包覆盖新结果。
  const seq = useRef(0);
  const load = useCallback(async (q: string) => {
    const my = ++seq.current;
    try {
      const [list, tv] = await Promise.all([
        api.skillList(q.trim() || undefined),
        api.skillTrust(scopeSid, scopeLabel).catch(() => [] as SkillTrustView[]),
      ]);
      if (my !== seq.current) return;
      setSkills(list);
      setTrust(Object.fromEntries(tv.map((v) => [v.id, v])));
      setLoadErr(null);
    } catch (e) {
      if (my === seq.current) setLoadErr(String(e));
    }
  }, [scopeSid, scopeLabel]);
  useEffect(() => {
    const t = window.setTimeout(() => void load(query), skills === null ? 0 : SEARCH_DEBOUNCE);
    return () => window.clearTimeout(t);
    // 只随关键字变化重查；skills 仅用来决定首次是否免防抖
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query, load]);

  // 提示条 4 秒后自动消失（错误不自动消失）
  useEffect(() => {
    if (!notice || notice.kind === "err") return;
    const t = window.setTimeout(() => setNotice(null), 4000);
    return () => window.clearTimeout(t);
  }, [notice]);

  // Esc：下层（编辑器 / 导入 / 确认）开着时由它们自己处理
  const childOpen = !!editing || !!imp || !!confirmDel || !!runTarget;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || childOpen) return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose, childOpen]);

  // 没有声明非 secret 参数就直接运行，不弹对话框
  const askRun = useCallback(
    (s: Skill) => {
      if (!onRunSkill) return;
      if (s.params.some((p) => !p.secret)) setRunTarget(s);
      else onRunSkill(s, {});
    },
    [onRunSkill]
  );

  const edit = useCallback((s: Skill) => setEditing(open({ skill: s })), []);

  const doExport = useCallback(async (s: Skill) => {
    try {
      const text = await api.skillExport(s.id);
      const { save } = await import("@tauri-apps/plugin-dialog");
      const path = await save({
        title: "导出技能",
        defaultPath: exportFileName(s.name),
        filters: [{ name: "技能文件", extensions: ["json"] }],
      });
      if (!path) return;
      await api.writeFile(path, Array.from(new TextEncoder().encode(text)));
      setNotice({ text: `已导出「${s.name}」到 ${path}（不含运行统计，也不含任何密码）`, kind: "ok" });
    } catch (e) {
      setNotice({ text: `导出失败：${String(e)}`, kind: "err" });
    }
  }, []);

  const doDelete = async () => {
    const s = confirmDel;
    setConfirmDel(null);
    if (!s) return;
    try {
      await api.skillDelete(s.id);
      setNotice({ text: `已删除「${s.name}」`, kind: "ok" });
      void load(query);
    } catch (e) {
      setNotice({ text: `删除失败：${String(e)}`, kind: "err" });
    }
  };

  // ——— 导入：文件 → skill_import_preview → 确认 → skill_import ———
  const onFile = async (file: File | undefined) => {
    if (!file) return;
    if (file.size > MAX_IMPORT_BYTES) {
      setImp({ fileName: file.name, text: "", preview: null, problems: [`文件超过 ${MAX_IMPORT_BYTES / 1024} KB，不像一份技能文件`] });
      return;
    }
    try {
      const text = await file.text();
      try {
        const preview = await api.skillImportPreview(text);
        setImp({ fileName: file.name, text, preview, problems: [] });
      } catch (e) {
        setImp({ fileName: file.name, text, preview: null, problems: splitProblems(String(e)) });
      }
    } catch (e) {
      setImp({ fileName: file.name, text: "", preview: null, problems: [`读取文件失败：${String(e)}`] });
    }
  };

  const pickFile = () => {
    // 同一个文件连选两次也要触发 onChange
    if (fileRef.current) fileRef.current.value = "";
    fileRef.current?.click();
  };

  const confirmImport = async () => {
    if (!imp?.preview) return;
    setImpBusy(true);
    try {
      const saved = await api.skillImport(imp.text);
      setImp(null);
      setNotice({
        text: `已导入「${saved.name}」（风险：${RISK_LABEL[saved.risk]}；计数从 0 开始）`,
        kind: "ok",
      });
      void load(query);
    } catch (e) {
      setImp({ ...imp, preview: null, problems: splitProblems(String(e)) });
    } finally {
      setImpBusy(false);
    }
  };

  const existingNames = useMemo(() => new Set((skills ?? []).map((s) => s.name)), [skills]);

  return createPortal(
    <>
      <div className="modal-backdrop sk-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
        <div className="modal sk-modal sk-manager" onMouseDown={(e) => e.stopPropagation()}>
          <div className="sk-title-row">
            <h2>技能</h2>
            <span className="sk-note sk-title-note">可回放的操作流程。步骤里永远没有密码值。</span>
            <span className="sk-spacer" />
            <button className="btn sk-small" onClick={pickFile} title="从文件导入技能">
              导入…
            </button>
            <button className="btn primary sk-small" onClick={() => setEditing(open({}))}>
              ＋ 新建
            </button>
            <button className="ap-x" onClick={onClose} title="关闭">
              ✕
            </button>
          </div>

          <input
            className="sk-input sk-search"
            value={query}
            placeholder="搜索名称或描述…"
            onChange={(e) => setQuery(e.target.value)}
          />

          {notice && <div className={notice.kind === "ok" ? "sk-notice" : "sk-errbox"}>{notice.text}</div>}
          {loadErr && <div className="sk-errbox">加载失败：{loadErr}</div>}

          <div className="sk-list">
            <div className="sk-row sk-head">
              <div className="sk-c-name">名称</div>
              <div className="sk-c-risk">风险</div>
              <div className="sk-c-trust">信任</div>
              <div className="sk-c-host">适用主机</div>
              <div className="sk-c-src">来源</div>
              <div className="sk-c-cnt">成功 / 失败</div>
              <div className="sk-c-last">上次运行</div>
              <div className="sk-c-act" />
            </div>
            {skills === null && !loadErr && <div className="sk-empty">加载中…</div>}
            {skills !== null && skills.length === 0 && (
              <div className="sk-empty">
                {query.trim() ? "没有匹配的技能" : "还没有技能。点「新建」手写一个，或「导入」别人导出的文件。"}
              </div>
            )}
            {skills?.map((s) => (
              <SkillRow key={s.id} skill={s} trust={trust[s.id]} onRun={onRunSkill ? askRun : undefined} onEdit={edit} onExport={doExport} onDelete={setConfirmDel} />
            ))}
          </div>
        </div>
      </div>

      <input
        ref={fileRef}
        type="file"
        accept=".json,.txt,application/json,text/plain"
        style={{ display: "none" }}
        onChange={(e) => void onFile(e.target.files?.[0])}
      />

      {editing && (
        <SkillEditor
          key={editing.k}
          skill={editing.skill}
          draft={editing.draft}
          onClose={() => setEditing(null)}
          onSaved={(saved, raised) => {
            setEditing(null);
            setNotice({
              text: raised
                ? `已保存「${saved.name}」。风险已被后端抬高为「${RISK_LABEL[saved.risk]}」（按步骤内容评估，不能手动降低）`
                : `已保存「${saved.name}」（风险：${RISK_LABEL[saved.risk]}）`,
              kind: "ok",
            });
            void load(query);
          }}
        />
      )}

      {runTarget && onRunSkill && (
        <RunDialog
          skill={runTarget}
          onClose={() => setRunTarget(null)}
          onRun={(p) => {
            const sk = runTarget;
            setRunTarget(null);
            onRunSkill(sk, p);
          }}
        />
      )}

      {imp && (
        <ImportDialog
          state={imp}
          existingNames={existingNames}
          busy={impBusy}
          onPick={pickFile}
          onConfirm={() => void confirmImport()}
          onClose={() => setImp(null)}
        />
      )}

      {confirmDel && (
        <ConfirmDialog
          opts={{
            title: "删除技能",
            message: `确定删除技能「${confirmDel.name}」？`,
            detail: "删除后无法恢复，运行统计一并丢失。想留底请先导出。",
            okText: "删除",
            danger: true,
          }}
          onResult={(ok) => (ok ? void doDelete() : setConfirmDel(null))}
        />
      )}
    </>,
    document.body
  );
}
