import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { api } from "../api";
import type { Skill, SkillInput, SkillOnUnexpected, SessionRecord } from "../types";
import { emptySkillInput, STEP_KIND_LABEL, undeclaredParams } from "../lib/skill";
import {
  cloneStep,
  declareMissing,
  formatLastRun,
  groupProblems,
  hasLocalProblems,
  hostsFromText,
  insertAt,
  isIdent,
  localProblems,
  moveItem,
  newParam,
  newStep,
  ON_UNEXPECTED_LABEL,
  problemCount,
  removeAt,
  removeParam,
  renameParam,
  replayChanged,
  respondRef,
  riskRaised,
  secretDefaultsIn,
  setParamSecret,
  setRespondSecret,
  skillToInput,
  SOURCE_LABEL,
  splitProblems,
  STEP_KINDS,
  toForm,
  toInput,
  type ParamForm,
  type SkillForm,
  type StepForm,
  type StepKind,
} from "../lib/skillForm";
import ConfirmDialog from "./ConfirmDialog";
import { PlaceholderChips, RiskBadge } from "./SkillParts";
import "./SkillManager.css";

/** 编辑器的入参。`skill` 与 `draft` 二选一：给 `skill` 是改已有的（走 skill_update），否则是新建（走 skill_create）。 */
export interface SkillEditorProps {
  /** 编辑已有 skill */
  skill?: Skill | null;
  /** 新建时的预填内容（录制器 / LLM 草稿 / 旧宏迁移的产物）；不给就是空白 */
  draft?: SkillInput;
  /** 保存成功。`raised` 为真表示后端把风险从提交值抬高了 */
  onSaved: (saved: Skill, raised: boolean) => void;
  onClose: () => void;
}

const VALIDATE_DEBOUNCE = 450;

type StepUpdate = (uid: number, fn: (s: StepForm) => StepForm) => void;

// ——————————————————————————————— 参数行 ———————————————————————————————

const ParamRow = memo(function ParamRow({
  p,
  problems,
  onRename,
  onPatch,
  onRemove,
}: {
  p: ParamForm;
  problems?: string[];
  onRename: (uid: number, name: string) => void;
  onPatch: (uid: number, fn: (p: ParamForm) => ParamForm) => void;
  onRemove: (uid: number) => void;
}) {
  // 名字用本地草稿、失焦 / 回车才提交：改名会同步改掉所有步骤里的 {{引用}}，
  // 逐字符提交的话敲到一半的名字会把引用改到一个半截的名字上。
  const [nameDraft, setNameDraft] = useState(p.name);
  useEffect(() => setNameDraft(p.name), [p.name]);
  const commit = () => {
    if (nameDraft !== p.name) onRename(p.uid, nameDraft);
  };
  const badName = nameDraft !== "" && !isIdent(nameDraft);

  return (
    <div className="sk-param">
      <div className="sk-param-row">
        <input
          className={`sk-input sk-param-name${badName ? " bad" : ""}`}
          value={nameDraft}
          spellCheck={false}
          placeholder="参数名"
          title="字母 / 数字 / 下划线，不以数字开头；改名会同步改掉所有 {{引用}}"
          onChange={(e) => setNameDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
        />
        <input
          className="sk-input sk-param-desc"
          value={p.description}
          placeholder="说明（给填参数的人看）"
          onChange={(e) => onPatch(p.uid, (x) => ({ ...x, description: e.target.value }))}
        />
        <input
          className="sk-input sk-param-def"
          value={p.defaultValue}
          disabled={p.secret}
          spellCheck={false}
          placeholder={p.secret ? "secret 参数不能有默认值" : "默认值（可空）"}
          title={p.secret ? "secret 参数的值运行时走密码通道，不存进 skill" : ""}
          onChange={(e) => onPatch(p.uid, (x) => ({ ...x, defaultValue: e.target.value }))}
        />
        <label className="sk-check" title="没有默认值又没传时，回放会报错">
          <input
            type="checkbox"
            checked={p.required}
            onChange={(e) => onPatch(p.uid, (x) => ({ ...x, required: e.target.checked }))}
          />
          必填
        </label>
        <label className="sk-check" title="密码类参数：值运行时由用户在密码通道输入，不存库、不进模型上下文">
          <input
            type="checkbox"
            checked={p.secret}
            onChange={(e) => onPatch(p.uid, (x) => setParamSecret(x, e.target.checked))}
          />
          secret
        </label>
        <button className="sk-ibtn" title="删除参数" onClick={() => onRemove(p.uid)}>
          ✕
        </button>
      </div>
      {badName && <div className="sk-err">参数名只能是字母 / 数字 / 下划线，且不以数字开头。</div>}
      {problems?.map((t, i) => (
        <div className="sk-err" key={i}>
          {t}
        </div>
      ))}
    </div>
  );
});

// ——————————————————————————————— 步骤行 ———————————————————————————————

interface StepRowProps {
  s: StepForm;
  index: number;
  total: number;
  declared: ReadonlySet<string>;
  secretNames: ReadonlySet<string>;
  /** 已声明的 secret 参数名（下拉选项） */
  secretList: readonly string[];
  problems?: string[];
  timeoutBad: boolean;
  onUpdate: StepUpdate;
  onMove: (uid: number, delta: number) => void;
  onRemove: (uid: number) => void;
  onDuplicate: (uid: number) => void;
}

const StepRow = memo(function StepRow(props: StepRowProps) {
  const { s, index, total, declared, secretNames, secretList, problems, timeoutBad, onUpdate, onMove, onRemove, onDuplicate } = props;
  const set = <K extends keyof StepForm>(k: K, v: StepForm[K]) => onUpdate(s.uid, (x) => ({ ...x, [k]: v }));
  const chips = (text: string, allowSecret = false) => (
    <PlaceholderChips text={text} declared={declared} secretNames={secretNames} allowSecret={allowSecret} />
  );

  // secret 应答的下拉：空 = 运行时现问；引用某个已声明 secret 参数；原文不是单一引用时（后端会拒）保留为一个「无效」选项
  const rr = respondRef(s.respond);
  const respondOdd = s.respond.trim() !== "" && !(rr && secretList.includes(rr));

  return (
    <div className="sk-step">
      <div className="sk-step-head">
        <span className="sk-step-no">{index + 1}</span>
        <select
          className="sk-input sk-kind-sel"
          value={s.kind}
          onChange={(e) => set("kind", e.target.value as StepKind)}
        >
          {STEP_KINDS.map((k) => (
            <option key={k} value={k}>
              {STEP_KIND_LABEL[k]}
            </option>
          ))}
        </select>
        <span className="sk-spacer" />
        <button className="sk-ibtn" title="上移" disabled={index === 0} onClick={() => onMove(s.uid, -1)}>
          ↑
        </button>
        <button className="sk-ibtn" title="下移" disabled={index === total - 1} onClick={() => onMove(s.uid, 1)}>
          ↓
        </button>
        <button className="sk-ibtn" title="复制这一步" onClick={() => onDuplicate(s.uid)}>
          ⧉
        </button>
        <button className="sk-ibtn" title="删除这一步" onClick={() => onRemove(s.uid)}>
          ✕
        </button>
      </div>

      {s.kind === "exec" && (
        <div className="sk-field">
          <input
            className="sk-input mono"
            value={s.command}
            spellCheck={false}
            placeholder="命令（单行，可含 {{参数}}）"
            onChange={(e) => set("command", e.target.value)}
          />
          {chips(s.command)}
        </div>
      )}

      {s.kind === "keys" && (
        <div className="sk-field">
          <input
            className="sk-input mono"
            value={s.keys}
            spellCheck={false}
            placeholder="按键，如 <C-c>  <Enter>  \x03（语法同 nx_send_keys）"
            onChange={(e) => set("keys", e.target.value)}
          />
          {chips(s.keys)}
        </div>
      )}

      {s.kind === "expect_respond" && (
        <>
          <div className="sk-field">
            <label className="sk-lbl">等到屏幕出现</label>
            <input
              className="sk-input mono"
              value={s.expect}
              spellCheck={false}
              placeholder="子串，或 /正则/i"
              onChange={(e) => set("expect", e.target.value)}
            />
            {chips(s.expect)}
          </div>
          <div className="sk-field">
            <label className="sk-lbl sk-lbl-row">
              应答
              <label className="sk-check" title="应答是密码：不能写字面值，只能引用 secret 参数或运行时现问">
                <input
                  type="checkbox"
                  checked={s.respondSecret}
                  onChange={(e) => onUpdate(s.uid, (x) => setRespondSecret(x, e.target.checked))}
                />
                密码（secret）
              </label>
            </label>
            {s.respondSecret ? (
              <select
                className="sk-input"
                value={s.respond.trim() === "" ? "" : s.respond}
                onChange={(e) => set("respond", e.target.value)}
              >
                <option value="">运行时现问（在审批卡里输入）</option>
                {secretList.map((n) => (
                  <option key={n} value={`{{${n}}}`}>
                    引用 secret 参数 {n}
                  </option>
                ))}
                {respondOdd && <option value={s.respond}>（无效）{s.respond}</option>}
              </select>
            ) : (
              <>
                <input
                  className="sk-input mono"
                  value={s.respond}
                  spellCheck={false}
                  placeholder="要发送的按键，如 y<Enter>（可含 {{参数}}）"
                  onChange={(e) => set("respond", e.target.value)}
                />
                {chips(s.respond)}
              </>
            )}
          </div>
        </>
      )}

      {s.kind === "secret" && (
        <>
          <div className="sk-field">
            <label className="sk-lbl">输入理由（给用户看）</label>
            <input
              className="sk-input"
              value={s.reason}
              placeholder="如：sudo 密码"
              onChange={(e) => set("reason", e.target.value)}
            />
          </div>
          <div className="sk-field">
            <label className="sk-lbl">引用的 secret 参数（可选）</label>
            <select
              className="sk-input"
              value={s.param}
              onChange={(e) => set("param", e.target.value)}
            >
              <option value="">不引用</option>
              {secretList.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
              {s.param && !secretList.includes(s.param) && <option value={s.param}>（无效）{s.param}</option>}
            </select>
          </div>
          <div className="sk-note">skill 里永远不存密码值：回放到这一步时由用户在密码通道输入。</div>
        </>
      )}

      {s.kind === "screen_check" && (
        <div className="sk-field">
          <label className="sk-lbl">屏幕应出现</label>
          <input
            className="sk-input mono"
            value={s.expect}
            spellCheck={false}
            placeholder="子串，或 /正则/i"
            onChange={(e) => set("expect", e.target.value)}
          />
          {chips(s.expect)}
        </div>
      )}

      <div className="sk-step-opts">
        <label className="sk-opt">
          <span>超时 ms</span>
          <input
            className={`sk-input sk-num${timeoutBad ? " bad" : ""}`}
            value={s.timeoutMs}
            inputMode="numeric"
            placeholder="默认"
            title="100 ~ 600000；留空用默认"
            onChange={(e) => set("timeoutMs", e.target.value)}
          />
        </label>
        <label className="sk-opt sk-opt-grow">
          <span>做完后应出现</span>
          <input
            className="sk-input mono"
            value={s.expectAfter}
            spellCheck={false}
            placeholder="expectAfter：子串或 /正则/i（可空）"
            onChange={(e) => set("expectAfter", e.target.value)}
          />
        </label>
        <label className="sk-opt">
          <span>偏离期望时</span>
          <select
            className="sk-input"
            value={s.onUnexpected}
            onChange={(e) => set("onUnexpected", e.target.value as SkillOnUnexpected)}
          >
            {(Object.keys(ON_UNEXPECTED_LABEL) as SkillOnUnexpected[]).map((k) => (
              <option key={k} value={k}>
                {ON_UNEXPECTED_LABEL[k]}
              </option>
            ))}
          </select>
        </label>
      </div>
      {s.expectAfter && chips(s.expectAfter)}
      {timeoutBad && <div className="sk-err">超时必须是 100 ~ 600000 之间的整数（毫秒），或留空。</div>}
      {problems?.map((t, i) => (
        <div className="sk-err" key={i}>
          {t}
        </div>
      ))}
    </div>
  );
});

// ——————————————————————————————— 编辑器 ———————————————————————————————

export default function SkillEditor({ skill, draft, onSaved, onClose }: SkillEditorProps) {
  const isNew = !skill;
  // 基线：打开时的内容（走一遍 toForm→toInput 归一化，这样「没动过」就不会被判成有改动）
  const [init] = useState(() => {
    const src = skill ? skillToInput(skill) : (draft ?? emptySkillInput());
    const form = toForm(src);
    return { form, json: JSON.stringify(toInput(form)), droppedSecret: secretDefaultsIn(src) };
  });
  const [form, setForm] = useState<SkillForm>(init.form);
  const [problems, setProblems] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [saveErr, setSaveErr] = useState<string | null>(null);
  const [confirmClose, setConfirmClose] = useState(false);
  const [sessions, setSessions] = useState<SessionRecord[]>([]);

  const input = useMemo(() => toInput(form), [form]);
  const json = useMemo(() => JSON.stringify(input), [input]);
  const dirty = json !== init.json;
  const local = useMemo(() => localProblems(form), [form]);
  const grouped = useMemo(() => groupProblems(problems), [problems]);

  const declared = useMemo(() => new Set(form.params.map((p) => p.name)), [form.params]);
  const secretNames = useMemo(
    () => new Set(form.params.filter((p) => p.secret).map((p) => p.name)),
    [form.params]
  );
  const secretList = useMemo(() => [...secretNames], [secretNames]);
  // 引用了却没声明的参数（只用于提示 + 一键补全）
  const missing = useMemo(() => undeclaredParams(input), [input]);

  // 已保存连接：给主机范围做名字解析 / 快速添加
  useEffect(() => {
    if (form.hostMode !== "list" || sessions.length > 0) return;
    let alive = true;
    void api
      .listSessions()
      .then((l) => alive && setSessions(l))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [form.hostMode, sessions.length]);

  // 实时校验（后端为准）。新建且没动过时不校验，免得一打开就一屏「不能为空」。
  const reqSeq = useRef(0);
  useEffect(() => {
    if (isNew && !draft && !dirty) {
      setProblems([]);
      return;
    }
    const seq = ++reqSeq.current;
    const t = window.setTimeout(() => {
      api
        .skillValidate(input)
        .then((list) => {
          if (seq === reqSeq.current) setProblems(list);
        })
        .catch(() => {});
    }, VALIDATE_DEBOUNCE);
    return () => window.clearTimeout(t);
    // json 代表 input 的内容；input 对象本身每次表单变化都是新的
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [json]);

  const requestClose = useCallback(() => {
    if (saving) return;
    if (dirty) setConfirmClose(true);
    else onClose();
  }, [saving, dirty, onClose]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || confirmClose) return;
      e.stopPropagation();
      requestClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [requestClose, confirmClose]);

  // ——— 表单操作（回调保持稳定，memo 行才有意义） ———
  const patch = useCallback((p: Partial<SkillForm>) => setForm((f) => ({ ...f, ...p })), []);
  const updateStep: StepUpdate = useCallback(
    (uid, fn) => setForm((f) => ({ ...f, steps: f.steps.map((s) => (s.uid === uid ? fn(s) : s)) })),
    []
  );
  const moveStep = useCallback(
    (uid: number, d: number) =>
      setForm((f) => {
        const i = f.steps.findIndex((s) => s.uid === uid);
        const steps = moveItem(f.steps, i, d);
        return steps === f.steps ? f : { ...f, steps };
      }),
    []
  );
  const removeStep = useCallback(
    (uid: number) =>
      setForm((f) => ({ ...f, steps: removeAt(f.steps, f.steps.findIndex((s) => s.uid === uid)) })),
    []
  );
  const dupStep = useCallback(
    (uid: number) =>
      setForm((f) => {
        const i = f.steps.findIndex((s) => s.uid === uid);
        return i < 0 ? f : { ...f, steps: insertAt(f.steps, i + 1, cloneStep(f.steps[i])) };
      }),
    []
  );
  const addStep = (kind: StepKind) => setForm((f) => ({ ...f, steps: [...f.steps, newStep(kind)] }));
  const renameP = useCallback((uid: number, name: string) => setForm((f) => renameParam(f, uid, name)), []);
  const patchParam = useCallback(
    (uid: number, fn: (p: ParamForm) => ParamForm) =>
      setForm((f) => ({ ...f, params: f.params.map((p) => (p.uid === uid ? fn(p) : p)) })),
    []
  );
  const removeP = useCallback((uid: number) => setForm((f) => removeParam(f, uid)), []);

  const save = async () => {
    if (saving || hasLocalProblems(local)) return;
    setSaving(true);
    setSaveErr(null);
    try {
      const saved = skill ? await api.skillUpdate(skill.id, input) : await api.skillCreate(input);
      onSaved(saved, riskRaised(input.risk, saved.risk));
    } catch (e) {
      const list = splitProblems(String(e));
      setProblems(list.length ? list : [String(e)]);
      setSaveErr(`保存失败（${list.length || 1} 个问题）`);
    } finally {
      setSaving(false);
    }
  };

  const resetsCounts = !!skill && replayChanged(skillToInput(skill), input) && skill.successCount + skill.failCount > 0;
  const hostEntries = useMemo(() => hostsFromText(form.hostsText), [form.hostsText]);
  const sessionName = useMemo(() => new Map(sessions.map((s) => [s.id, s.name])), [sessions]);
  const nProblems = problemCount(grouped);

  return createPortal(
    <>
      <div
        className="modal-backdrop sk-backdrop"
        onMouseDown={(e) => e.target === e.currentTarget && requestClose()}
      >
        <div className="modal sk-modal sk-editor" onMouseDown={(e) => e.stopPropagation()}>
          <h2>
            {isNew ? (draft ? "新建技能（草稿）" : "新建技能") : "编辑技能"}
            {dirty && <span className="sk-dirty"> · 未保存</span>}
          </h2>

          <div className="sk-body">
            {skill && (
              <div className="sk-meta">
                <span>来源：{SOURCE_LABEL[skill.source]}</span>
                <span>
                  成功 {skill.successCount} / 失败 {skill.failCount}
                </span>
                <span>上次运行：{formatLastRun(skill.lastRunTs)}</span>
              </div>
            )}
            {init.droppedSecret.length > 0 && (
              <div className="sk-warn">
                草稿里 secret 参数「{init.droppedSecret.join("、")}」带有默认值，已丢弃（secret 参数不能有默认值）。
              </div>
            )}
            {grouped.general.length > 0 && (
              <div className="sk-errbox">
                {grouped.general.map((t, i) => (
                  <div key={i}>{t}</div>
                ))}
              </div>
            )}

            <section className="sk-sec">
              <div className="sk-field">
                <label className="sk-lbl">名称</label>
                <input
                  className="sk-input"
                  value={form.name}
                  autoFocus
                  placeholder="如：部署 web"
                  onChange={(e) => patch({ name: e.target.value })}
                />
              </div>
              <div className="sk-field">
                <label className="sk-lbl">描述</label>
                <textarea
                  className="sk-input sk-ta"
                  rows={2}
                  value={form.description}
                  placeholder="这个技能做什么、什么时候该用（AI 也会读它来决定是否调用）"
                  onChange={(e) => patch({ description: e.target.value })}
                />
              </div>
              <div className="sk-risk-row">
                <span className="sk-lbl-inline">风险等级</span>
                <RiskBadge risk={form.risk} />
                <span className="sk-note">
                  由后端按步骤内容评估，保存时只会被抬高、不能手动降低
                  {isNew ? "；这里显示的是草稿自带的值，保存后以后端评估为准" : ""}。
                </span>
              </div>
              {grouped.risk.map((t, i) => (
                <div className="sk-err" key={i}>
                  {t}
                </div>
              ))}
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>参数</h3>
                <button className="btn sk-small" onClick={() => patch({ params: [...form.params, newParam()] })}>
                  ＋ 添加参数
                </button>
              </div>
              {form.params.length === 0 && (
                <div className="sk-note">没有参数。在命令里写 {"{{名字}}"} 引用参数；密码类参数请勾选 secret。</div>
              )}
              {form.params.map((p) => (
                <ParamRow
                  key={p.uid}
                  p={p}
                  problems={grouped.params.get(p.name)}
                  onRename={renameP}
                  onPatch={patchParam}
                  onRemove={removeP}
                />
              ))}
              {missing.length > 0 && (
                <div className="sk-warn sk-missing">
                  引用了未声明的参数：
                  {missing.map((n) => (
                    <span className="sk-ph bad" key={n}>{`{{${n}}}`}</span>
                  ))}
                  <button className="btn sk-small" onClick={() => setForm((f) => declareMissing(f).form)}>
                    自动补全声明
                  </button>
                </div>
              )}
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>步骤（{form.steps.length}）</h3>
              </div>
              {form.steps.length === 0 && <div className="sk-note">还没有步骤，从下面添加一步。</div>}
              {form.steps.map((s, i) => (
                <StepRow
                  key={s.uid}
                  s={s}
                  index={i}
                  total={form.steps.length}
                  declared={declared}
                  secretNames={secretNames}
                  secretList={secretList}
                  problems={grouped.steps.get(i)}
                  timeoutBad={local.stepTimeout.has(s.uid)}
                  onUpdate={updateStep}
                  onMove={moveStep}
                  onRemove={removeStep}
                  onDuplicate={dupStep}
                />
              ))}
              <div className="sk-addbar">
                <span className="sk-lbl-inline">添加：</span>
                {STEP_KINDS.map((k) => (
                  <button key={k} className="btn sk-small" onClick={() => addStep(k)}>
                    {STEP_KIND_LABEL[k]}
                  </button>
                ))}
              </div>
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>验证</h3>
                <label className="sk-check">
                  <input
                    type="checkbox"
                    checked={form.verifyOn}
                    onChange={(e) => patch({ verifyOn: e.target.checked })}
                  />
                  跑完后检查屏幕
                </label>
              </div>
              {form.verifyOn && (
                <div className="sk-verify">
                  <div className="sk-field sk-grow">
                    <input
                      className="sk-input mono"
                      value={form.verifyExpect}
                      spellCheck={false}
                      placeholder="屏幕应出现：子串或 /正则/i"
                      onChange={(e) => patch({ verifyExpect: e.target.value })}
                    />
                    <PlaceholderChips text={form.verifyExpect} declared={declared} secretNames={secretNames} />
                  </div>
                  <label className="sk-opt">
                    <span>超时 ms</span>
                    <input
                      className={`sk-input sk-num${local.verifyTimeout ? " bad" : ""}`}
                      value={form.verifyTimeoutMs}
                      inputMode="numeric"
                      placeholder="默认"
                      onChange={(e) => patch({ verifyTimeoutMs: e.target.value })}
                    />
                  </label>
                </div>
              )}
              {local.verifyTimeout && <div className="sk-err">超时必须是 100 ~ 600000 之间的整数（毫秒），或留空。</div>}
              {grouped.verify.map((t, i) => (
                <div className="sk-err" key={i}>
                  {t}
                </div>
              ))}
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>适用主机</h3>
                <div className="seg sk-seg">
                  <button
                    className={`seg-btn${form.hostMode === "any" ? " active" : ""}`}
                    onClick={() => patch({ hostMode: "any" })}
                  >
                    不限
                  </button>
                  <button
                    className={`seg-btn${form.hostMode === "list" ? " active" : ""}`}
                    onClick={() => patch({ hostMode: "list" })}
                  >
                    指定
                  </button>
                </div>
              </div>
              {form.hostMode === "list" && (
                <>
                  <textarea
                    className="sk-input sk-ta mono"
                    rows={3}
                    value={form.hostsText}
                    spellCheck={false}
                    placeholder={"每行一项：已保存连接的 ID，或主机名（* 通配，如 *.prod.example.com）"}
                    onChange={(e) => patch({ hostsText: e.target.value })}
                  />
                  <div className="sk-hosts">
                    <select
                      className="sk-input sk-host-pick"
                      value=""
                      onChange={(e) => {
                        const id = e.target.value;
                        if (id && !hostEntries.includes(id)) {
                          patch({ hostsText: [...hostEntries, id].join("\n") });
                        }
                      }}
                    >
                      <option value="">从已保存连接添加…</option>
                      {sessions.map((s) => (
                        <option key={s.id} value={s.id}>
                          {s.name}（{s.host}）
                        </option>
                      ))}
                    </select>
                    {hostEntries.map((h) => (
                      <span className="sk-chip" key={h} title={h}>
                        {sessionName.get(h) ?? h}
                      </span>
                    ))}
                  </div>
                  {hostEntries.length === 0 && <div className="sk-err">指定主机时至少要有一项（要不限主机请选「不限」）。</div>}
                </>
              )}
              {grouped.hosts.map((t, i) => (
                <div className="sk-err" key={i}>
                  {t}
                </div>
              ))}
            </section>
          </div>

          <div className="sk-foot">
            <div className="sk-foot-msg">
              {saveErr ? (
                <span className="sk-err-inline">{saveErr}</span>
              ) : nProblems > 0 ? (
                <span className="sk-err-inline">{nProblems} 个问题待处理</span>
              ) : hasLocalProblems(local) ? (
                <span className="sk-err-inline">有无效的超时设置</span>
              ) : resetsCounts ? (
                <span className="sk-warn-inline">保存后，步骤 / 参数 / 验证 / 主机范围的改动会让成功 / 失败计数清零</span>
              ) : null}
            </div>
            <button className="btn" onClick={requestClose} disabled={saving}>
              取消
            </button>
            <button
              className="btn primary"
              onClick={() => void save()}
              disabled={saving || hasLocalProblems(local) || (!isNew && !dirty)}
            >
              {saving ? "保存中…" : "保存"}
            </button>
          </div>
        </div>
      </div>

      {confirmClose && (
        <ConfirmDialog
          opts={{
            title: "放弃修改？",
            message: "这个技能有未保存的修改，关闭后会丢失。",
            okText: "放弃修改",
            danger: true,
          }}
          onResult={(ok) => {
            setConfirmClose(false);
            if (ok) onClose();
          }}
        />
      )}
    </>,
    document.body
  );
}
