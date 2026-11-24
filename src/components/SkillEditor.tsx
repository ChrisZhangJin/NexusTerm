import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { api } from "../api";
import { t, useLocale } from "../i18n";
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
  useLocale();
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
          placeholder={t("参数名", "Parameter name")}
          title={t(
            "字母 / 数字 / 下划线，不以数字开头；改名会同步改掉所有 {{引用}}",
            "Letters, digits and underscores; can't start with a digit. Renaming updates every {{reference}}"
          )}
          onChange={(e) => setNameDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
        />
        <input
          className="sk-input sk-param-desc"
          value={p.description}
          placeholder={t("说明（给填参数的人看）", "Description (for whoever fills it in)")}
          onChange={(e) => onPatch(p.uid, (x) => ({ ...x, description: e.target.value }))}
        />
        <input
          className="sk-input sk-param-def"
          value={p.defaultValue}
          disabled={p.secret}
          spellCheck={false}
          placeholder={p.secret ? t("secret 参数不能有默认值", "Secret parameters can't have a default") : t("默认值（可空）", "Default (optional)")}
          title={p.secret ? t("secret 参数的值运行时走密码通道，不存进 skill", "Secret values go through the password channel at run time and are never stored in the skill") : ""}
          onChange={(e) => onPatch(p.uid, (x) => ({ ...x, defaultValue: e.target.value }))}
        />
        <label className="sk-check" title={t("没有默认值又没传时，回放会报错", "Replay fails if there's no default and no value is passed")}>
          <input
            type="checkbox"
            checked={p.required}
            onChange={(e) => onPatch(p.uid, (x) => ({ ...x, required: e.target.checked }))}
          />
          {t("必填", "Required")}
        </label>
        <label
          className="sk-check"
          title={t(
            "密码类参数：值运行时由用户在密码通道输入，不存库、不进模型上下文",
            "Password-type parameter: the user enters the value in the password channel at run time; it's never stored or shown to the model"
          )}
        >
          <input
            type="checkbox"
            checked={p.secret}
            onChange={(e) => onPatch(p.uid, (x) => setParamSecret(x, e.target.checked))}
          />
          secret
        </label>
        <button className="sk-ibtn" title={t("删除参数", "Remove parameter")} onClick={() => onRemove(p.uid)}>
          ✕
        </button>
      </div>
      {badName && (
        <div className="sk-err">
          {t("参数名只能是字母 / 数字 / 下划线，且不以数字开头。", "Parameter names may only use letters, digits and underscores, and can't start with a digit.")}
        </div>
      )}
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
  useLocale();
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
        <button className="sk-ibtn" title={t("上移", "Move up")} disabled={index === 0} onClick={() => onMove(s.uid, -1)}>
          ↑
        </button>
        <button className="sk-ibtn" title={t("下移", "Move down")} disabled={index === total - 1} onClick={() => onMove(s.uid, 1)}>
          ↓
        </button>
        <button className="sk-ibtn" title={t("复制这一步", "Duplicate step")} onClick={() => onDuplicate(s.uid)}>
          ⧉
        </button>
        <button className="sk-ibtn" title={t("删除这一步", "Delete step")} onClick={() => onRemove(s.uid)}>
          ✕
        </button>
      </div>

      {s.kind === "exec" && (
        <div className="sk-field">
          <input
            className="sk-input mono"
            value={s.command}
            spellCheck={false}
            placeholder={t("命令（单行，可含 {{参数}}）", "Command (single line, may use {{params}})")}
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
            placeholder={t("按键，如 <C-c>  <Enter>  \\x03（语法同 nx_send_keys）", "Keys, e.g. <C-c>  <Enter>  \\x03 (same syntax as nx_send_keys)")}
            onChange={(e) => set("keys", e.target.value)}
          />
          {chips(s.keys)}
        </div>
      )}

      {s.kind === "expect_respond" && (
        <>
          <div className="sk-field">
            <label className="sk-lbl">{t("等到屏幕出现", "Wait until the screen shows")}</label>
            <input
              className="sk-input mono"
              value={s.expect}
              spellCheck={false}
              placeholder={t("子串，或 /正则/i", "Substring, or /regex/i")}
              onChange={(e) => set("expect", e.target.value)}
            />
            {chips(s.expect)}
          </div>
          <div className="sk-field">
            <label className="sk-lbl sk-lbl-row">
              {t("应答", "Response")}
              <label
                className="sk-check"
                title={t(
                  "应答是密码：不能写字面值，只能引用 secret 参数或运行时现问",
                  "The response is a password: no literal values, only a secret parameter or asking at run time"
                )}
              >
                <input
                  type="checkbox"
                  checked={s.respondSecret}
                  onChange={(e) => onUpdate(s.uid, (x) => setRespondSecret(x, e.target.checked))}
                />
                {t("密码（secret）", "Password (secret)")}
              </label>
            </label>
            {s.respondSecret ? (
              <select
                className="sk-input"
                value={s.respond.trim() === "" ? "" : s.respond}
                onChange={(e) => set("respond", e.target.value)}
              >
                <option value="">{t("运行时现问（在审批卡里输入）", "Ask at run time (enter it in the approval card)")}</option>
                {secretList.map((n) => (
                  <option key={n} value={`{{${n}}}`}>
                    {t("引用 secret 参数 {n}", "Use secret parameter {n}", { n })}
                  </option>
                ))}
                {respondOdd && (
                  <option value={s.respond}>
                    {t("（无效）", "(invalid) ")}
                    {s.respond}
                  </option>
                )}
              </select>
            ) : (
              <>
                <input
                  className="sk-input mono"
                  value={s.respond}
                  spellCheck={false}
                  placeholder={t("要发送的按键，如 y<Enter>（可含 {{参数}}）", "Keys to send, e.g. y<Enter> (may use {{params}})")}
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
            <label className="sk-lbl">{t("输入理由（给用户看）", "Reason (shown to the user)")}</label>
            <input
              className="sk-input"
              value={s.reason}
              placeholder={t("如：sudo 密码", "e.g. sudo password")}
              onChange={(e) => set("reason", e.target.value)}
            />
          </div>
          <div className="sk-field">
            <label className="sk-lbl">{t("引用的 secret 参数（可选）", "Secret parameter (optional)")}</label>
            <select
              className="sk-input"
              value={s.param}
              onChange={(e) => set("param", e.target.value)}
            >
              <option value="">{t("不引用", "None")}</option>
              {secretList.map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
              {s.param && !secretList.includes(s.param) && (
                <option value={s.param}>
                  {t("（无效）", "(invalid) ")}
                  {s.param}
                </option>
              )}
            </select>
          </div>
          <div className="sk-note">
            {t(
              "skill 里永远不存密码值：回放到这一步时由用户在密码通道输入。",
              "Skills never store password values: on replay the user enters it in the password channel at this step."
            )}
          </div>
        </>
      )}

      {s.kind === "screen_check" && (
        <div className="sk-field">
          <label className="sk-lbl">{t("屏幕应出现", "Screen should show")}</label>
          <input
            className="sk-input mono"
            value={s.expect}
            spellCheck={false}
            placeholder={t("子串，或 /正则/i", "Substring, or /regex/i")}
            onChange={(e) => set("expect", e.target.value)}
          />
          {chips(s.expect)}
        </div>
      )}

      <div className="sk-step-opts">
        <label className="sk-opt">
          <span>{t("超时 ms", "Timeout ms")}</span>
          <input
            className={`sk-input sk-num${timeoutBad ? " bad" : ""}`}
            value={s.timeoutMs}
            inputMode="numeric"
            placeholder={t("默认", "Default")}
            title={t("100 ~ 600000；留空用默认", "100 – 600000; leave empty for the default")}
            onChange={(e) => set("timeoutMs", e.target.value)}
          />
        </label>
        <label className="sk-opt sk-opt-grow">
          <span>{t("做完后应出现", "Then expect")}</span>
          <input
            className="sk-input mono"
            value={s.expectAfter}
            spellCheck={false}
            placeholder={t("expectAfter：子串或 /正则/i（可空）", "expectAfter: substring or /regex/i (optional)")}
            onChange={(e) => set("expectAfter", e.target.value)}
          />
        </label>
        <label className="sk-opt">
          <span>{t("偏离期望时", "If unexpected")}</span>
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
      {timeoutBad && <div className="sk-err">{timeoutErr()}</div>}
      {problems?.map((t, i) => (
        <div className="sk-err" key={i}>
          {t}
        </div>
      ))}
    </div>
  );
});

const timeoutErr = () =>
  t("超时必须是 100 ~ 600000 之间的整数（毫秒），或留空。", "Timeout must be a whole number from 100 to 600000 (ms), or empty.");

// ——————————————————————————————— 编辑器 ———————————————————————————————

export default function SkillEditor({ skill, draft, onSaved, onClose }: SkillEditorProps) {
  useLocale();
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
      const n = list.length || 1;
      setSaveErr(t("保存失败（{n} 个问题）", n === 1 ? "Save failed (1 problem)" : "Save failed ({n} problems)", { n }));
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
            {isNew ? (draft ? t("新建技能（草稿）", "New skill (draft)") : t("新建技能", "New skill")) : t("编辑技能", "Edit skill")}
            {dirty && <span className="sk-dirty"> · {t("未保存", "Unsaved")}</span>}
          </h2>

          <div className="sk-body">
            {skill && (
              <div className="sk-meta">
                <span>{t("来源：{s}", "Source: {s}", { s: SOURCE_LABEL[skill.source] })}</span>
                <span>
                  {t("成功 {ok} / 失败 {fail}", "{ok} succeeded / {fail} failed", { ok: skill.successCount, fail: skill.failCount })}
                </span>
                <span>{t("上次运行：{w}", "Last run: {w}", { w: formatLastRun(skill.lastRunTs) })}</span>
              </div>
            )}
            {init.droppedSecret.length > 0 && (
              <div className="sk-warn">
                {t(
                  "草稿里 secret 参数「{names}」带有默认值，已丢弃（secret 参数不能有默认值）。",
                  "The draft had defaults for secret parameters “{names}”; they were dropped (secret parameters can't have defaults).",
                  { names: init.droppedSecret.join(t("、", ", ")) }
                )}
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
                <label className="sk-lbl">{t("名称", "Name")}</label>
                <input
                  className="sk-input"
                  value={form.name}
                  autoFocus
                  placeholder={t("如：部署 web", "e.g. Deploy web")}
                  onChange={(e) => patch({ name: e.target.value })}
                />
              </div>
              <div className="sk-field">
                <label className="sk-lbl">{t("描述", "Description")}</label>
                <textarea
                  className="sk-input sk-ta"
                  rows={2}
                  value={form.description}
                  placeholder={t(
                    "这个技能做什么、什么时候该用（AI 也会读它来决定是否调用）",
                    "What this skill does and when to use it (the AI reads this to decide whether to call it)"
                  )}
                  onChange={(e) => patch({ description: e.target.value })}
                />
              </div>
              <div className="sk-risk-row">
                <span className="sk-lbl-inline">{t("风险等级", "Risk level")}</span>
                <RiskBadge risk={form.risk} />
                <span className="sk-note">
                  {t(
                    "由后端按步骤内容评估，保存时只会被抬高、不能手动降低",
                    "Assessed by the backend from the steps; saving can only raise it, never lower it"
                  )}
                  {isNew
                    ? t(
                        "；这里显示的是草稿自带的值，保存后以后端评估为准",
                        ". This is the draft's own value; the backend's assessment applies after saving"
                      )
                    : ""}
                  {t("。", ".")}
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
                <h3>{t("参数", "Parameters")}</h3>
                <button className="btn sk-small" onClick={() => patch({ params: [...form.params, newParam()] })}>
                  {t("＋ 添加参数", "＋ Add parameter")}
                </button>
              </div>
              {form.params.length === 0 && (
                <div className="sk-note">
                  {t(
                    "没有参数。在命令里写 {{名字}} 引用参数；密码类参数请勾选 secret。",
                    "No parameters. Write {{name}} in a command to reference one; tick secret for passwords."
                  )}
                </div>
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
                  {t("引用了未声明的参数：", "Undeclared parameters referenced: ")}
                  {missing.map((n) => (
                    <span className="sk-ph bad" key={n}>{`{{${n}}}`}</span>
                  ))}
                  <button className="btn sk-small" onClick={() => setForm((f) => declareMissing(f).form)}>
                    {t("自动补全声明", "Declare them")}
                  </button>
                </div>
              )}
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>{t("步骤（{n}）", "Steps ({n})", { n: form.steps.length })}</h3>
              </div>
              {form.steps.length === 0 && <div className="sk-note">{t("还没有步骤，从下面添加一步。", "No steps yet. Add one below.")}</div>}
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
                <span className="sk-lbl-inline">{t("添加：", "Add: ")}</span>
                {STEP_KINDS.map((k) => (
                  <button key={k} className="btn sk-small" onClick={() => addStep(k)}>
                    {STEP_KIND_LABEL[k]}
                  </button>
                ))}
              </div>
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>{t("验证", "Verification")}</h3>
                <label className="sk-check">
                  <input
                    type="checkbox"
                    checked={form.verifyOn}
                    onChange={(e) => patch({ verifyOn: e.target.checked })}
                  />
                  {t("跑完后检查屏幕", "Check the screen when done")}
                </label>
              </div>
              {form.verifyOn && (
                <div className="sk-verify">
                  <div className="sk-field sk-grow">
                    <input
                      className="sk-input mono"
                      value={form.verifyExpect}
                      spellCheck={false}
                      placeholder={t("屏幕应出现：子串或 /正则/i", "Screen should show: substring or /regex/i")}
                      onChange={(e) => patch({ verifyExpect: e.target.value })}
                    />
                    <PlaceholderChips text={form.verifyExpect} declared={declared} secretNames={secretNames} />
                  </div>
                  <label className="sk-opt">
                    <span>{t("超时 ms", "Timeout ms")}</span>
                    <input
                      className={`sk-input sk-num${local.verifyTimeout ? " bad" : ""}`}
                      value={form.verifyTimeoutMs}
                      inputMode="numeric"
                      placeholder={t("默认", "Default")}
                      onChange={(e) => patch({ verifyTimeoutMs: e.target.value })}
                    />
                  </label>
                </div>
              )}
              {local.verifyTimeout && <div className="sk-err">{timeoutErr()}</div>}
              {grouped.verify.map((t, i) => (
                <div className="sk-err" key={i}>
                  {t}
                </div>
              ))}
            </section>

            <section className="sk-sec">
              <div className="sk-sec-head">
                <h3>{t("适用主机", "Hosts")}</h3>
                <div className="seg sk-seg">
                  <button
                    className={`seg-btn${form.hostMode === "any" ? " active" : ""}`}
                    onClick={() => patch({ hostMode: "any" })}
                  >
                    {t("不限", "Any")}
                  </button>
                  <button
                    className={`seg-btn${form.hostMode === "list" ? " active" : ""}`}
                    onClick={() => patch({ hostMode: "list" })}
                  >
                    {t("指定", "Specific")}
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
                    placeholder={t(
                      "每行一项：已保存连接的 ID，或主机名（* 通配，如 *.prod.example.com）",
                      "One per line: a saved connection ID, or a host name (* wildcard, e.g. *.prod.example.com)"
                    )}
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
                      <option value="">{t("从已保存连接添加…", "Add from saved connections…")}</option>
                      {sessions.map((s) => (
                        <option key={s.id} value={s.id}>
                          {t("{name}（{host}）", "{name} ({host})", { name: s.name, host: s.host })}
                        </option>
                      ))}
                    </select>
                    {hostEntries.map((h) => (
                      <span className="sk-chip" key={h} title={h}>
                        {sessionName.get(h) ?? h}
                      </span>
                    ))}
                  </div>
                  {hostEntries.length === 0 && (
                    <div className="sk-err">
                      {t(
                        "指定主机时至少要有一项（要不限主机请选「不限」）。",
                        "Add at least one host (choose Any to allow every host)."
                      )}
                    </div>
                  )}
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
                <span className="sk-err-inline">
                  {t("{n} 个问题待处理", nProblems === 1 ? "1 problem to fix" : "{n} problems to fix", { n: nProblems })}
                </span>
              ) : hasLocalProblems(local) ? (
                <span className="sk-err-inline">{t("有无效的超时设置", "Some timeouts are invalid")}</span>
              ) : resetsCounts ? (
                <span className="sk-warn-inline">
                  {t(
                    "保存后，步骤 / 参数 / 验证 / 主机范围的改动会让成功 / 失败计数清零",
                    "Saving changes to steps, parameters, verification or hosts resets the success / failure counts"
                  )}
                </span>
              ) : null}
            </div>
            <button className="btn" onClick={requestClose} disabled={saving}>
              {t("取消", "Cancel")}
            </button>
            <button
              className="btn primary"
              onClick={() => void save()}
              disabled={saving || hasLocalProblems(local) || (!isNew && !dirty)}
            >
              {saving ? t("保存中…", "Saving…") : t("保存", "Save")}
            </button>
          </div>
        </div>
      </div>

      {confirmClose && (
        <ConfirmDialog
          opts={{
            title: t("放弃修改？", "Discard changes?"),
            message: t("这个技能有未保存的修改，关闭后会丢失。", "This skill has unsaved changes that will be lost if you close it."),
            okText: t("放弃修改", "Discard"),
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
