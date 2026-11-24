/**
 * P2「决策依据」编辑区：偏好 / 意向栈 / 知识库 / 确定性剧本。
 *
 * 这些是用户**预先承载**给 Steward 的东西，不是终端输出。决策编译器每次只从这里取依据，
 * 屏幕文本永远只作为匹配输入（不可信数据），所以这里也不展示「从屏幕学来的规则」。
 *
 * 四条内容共用一张 `steward_ctx` 表，`extra` 的字段按 kind 不同而不同；这个组件负责
 * 把表单翻译成对应的 extra JSON，编辑时再反填回来。
 */

import { useCallback, useEffect, useState } from "react";
import {
  TIER_LABELS,
  splitList,
  stewardApi,
  type CtxInput,
  type CtxItem,
  type CtxKind,
} from "../lib/steward";
import { t, useLocale } from "../i18n";

export interface StewardContextProps {
  /** 当前选中的托管契约；意向栈按它过滤 */
  contractId: string | null;
  /** 依据变化后让上层刷新决策 / 状态 */
  onChanged?: () => void;
}

const kinds = (): Array<{ value: CtxKind; label: string; hint: string }> => [
  {
    value: "preference",
    label: t("偏好", "Preferences"),
    hint: t("硬约束 / 软偏好，决定能不能答、怎么答", "Hard / soft rules that decide whether and how to answer"),
  },
  {
    value: "intent",
    label: t("意向", "Intents"),
    hint: t("目标栈，栈顶那条决定本次回复", "Goal stack; the top entry drives the current reply"),
  },
  {
    value: "knowledge",
    label: t("知识", "Knowledge"),
    hint: t("项目背景 / 历史结论，标为不可信数据", "Project background / past findings, treated as untrusted data"),
  },
  {
    value: "playbook",
    label: t("剧本", "Playbooks"),
    hint: t("命中即确定性回答，不调模型", "Fixed answers on match, no model call"),
  },
];

/** 回复模板的占位提示：花括号是模板变量本身，原样显示（不传 vars，t() 不会替换） */
const TEMPLATE_HINT = () =>
  t(
    "可选：完整回复模板，支持 {goal} {constraints} {hard} {options} {topic} {knowledge}",
    "Optional: full reply template; supports {goal} {constraints} {hard} {options} {topic} {knowledge}"
  );

const extraSummary = (x: CtxItem): string => {
  const e = x.extra ?? {};
  if (x.kind === "knowledge") {
    const tags = Array.isArray(e.tags) ? (e.tags as unknown[]).join(" / ") : "";
    return tags ? t("标签：{tags}", "Tags: {tags}", { tags }) : "";
  }
  if (x.kind === "intent") {
    const c = Array.isArray(e.constraints) ? (e.constraints as unknown[]).length : 0;
    const tpl =
      typeof e.answerTemplate === "string" && e.answerTemplate ? t(" · 自定义回复模板", " · custom reply template") : "";
    return t("position {p} · 约束 {c} 条", "position {p} · {c} constraints", { p: x.position, c }) + tpl;
  }
  if (x.kind === "playbook") {
    const answer = typeof e.answer === "string" ? e.answer : "";
    const where = [e.matchProgram, e.matchKind, e.matchText]
      .filter((v): v is string => typeof v === "string" && !!v && v !== "*")
      .join("/");
    return (
      t("→ 「{answer}」", "→ \"{answer}\"", { answer }) +
      (where ? t(" · 匹配 {where}", " · matches {where}", { where }) : "") +
      (e.enabled === false ? t(" · 已停用", " · disabled") : "")
    );
  }
  return x.strength === "hard" ? t("硬约束", "Hard rule") : t("软偏好", "Soft preference");
};

export default function StewardContext({ contractId, onChanged }: StewardContextProps) {
  useLocale();
  const KINDS = kinds();
  const [items, setItems] = useState<CtxItem[]>([]);
  const [kind, setKind] = useState<CtxKind>("preference");
  const [editing, setEditing] = useState<string | null>(null);
  const [text, setText] = useState("");
  const [strength, setStrength] = useState("soft");
  const [scope, setScope] = useState("global");
  const [tags, setTags] = useState("");
  const [constraints, setConstraints] = useState("");
  const [answerTemplate, setAnswerTemplate] = useState("");
  const [matchProgram, setMatchProgram] = useState("*");
  const [matchKind, setMatchKind] = useState("*");
  const [matchText, setMatchText] = useState("");
  const [answer, setAnswer] = useState("");
  const [optionIndex, setOptionIndex] = useState("");
  const [tier, setTier] = useState("auto");
  const [err, setErr] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    try {
      const v = await stewardApi.brain();
      setItems(v.items);
      setErr(null);
    } catch (e) {
      setErr(t("读取依据失败：{e}", "Failed to load decision basis: {e}", { e: String(e) }));
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const reset = () => {
    setEditing(null);
    setText("");
    setStrength("soft");
    setScope("global");
    setTags("");
    setConstraints("");
    setAnswerTemplate("");
    setMatchProgram("*");
    setMatchKind("*");
    setMatchText("");
    setAnswer("");
    setOptionIndex("");
    setTier("auto");
  };

  const edit = (x: CtxItem) => {
    setKind(x.kind);
    setEditing(x.id);
    setText(x.text);
    setStrength(x.strength);
    setScope(x.scope);
    const e = x.extra ?? {};
    setTags(Array.isArray(e.tags) ? (e.tags as unknown[]).join(", ") : "");
    setConstraints(
      Array.isArray(e.constraints) ? (e.constraints as unknown[]).join(", ") : ""
    );
    setAnswerTemplate(typeof e.answerTemplate === "string" ? e.answerTemplate : "");
    setMatchProgram(typeof e.matchProgram === "string" ? e.matchProgram : "*");
    setMatchKind(typeof e.matchKind === "string" ? e.matchKind : "*");
    setMatchText(typeof e.matchText === "string" ? e.matchText : "");
    setAnswer(typeof e.answer === "string" ? e.answer : "");
    setOptionIndex(typeof e.optionIndex === "string" ? e.optionIndex : "");
    setTier(typeof e.tier === "string" ? e.tier : "auto");
  };

  const submit = async () => {
    if (!text.trim() || busy) return;
    if (kind === "intent" && !contractId) {
      setErr(t("先开始托管，意向栈要挂在托管契约上", "Start autopilot first — intents attach to the autopilot contract"));
      return;
    }
    let extra: Record<string, unknown> = {};
    if (kind === "intent") {
      extra = {
        successCriteria: [],
        constraints: splitList(constraints),
        answerTemplate: answerTemplate.trim() || null,
        status: "active",
        priority: 0,
      };
    } else if (kind === "knowledge") {
      extra = { tags: splitList(tags) };
    } else if (kind === "playbook") {
      if (!answer.trim()) {
        setErr(t("剧本必须给出要发出的回复文本", "A playbook needs the reply text to send"));
        return;
      }
      extra = {
        matchProgram: matchProgram || "*",
        matchKind: matchKind || "*",
        matchText: matchText.trim() || null,
        answer: answer.trim(),
        optionIndex: optionIndex.trim() || null,
        tier,
        enabled: true,
      };
    }
    const input: CtxInput = {
      id: editing,
      kind,
      text: text.trim(),
      strength: kind === "preference" ? strength : "soft",
      scope,
      source: null,
      evidence: null,
      extra,
      contractId: kind === "intent" ? contractId : null,
      position: null,
    };
    setBusy(true);
    try {
      if (kind === "intent" && !editing && contractId) {
        await stewardApi.intentPush(
          contractId,
          text.trim(),
          splitList(constraints),
          answerTemplate.trim() || undefined
        );
      } else {
        await stewardApi.ctxUpsert(input);
      }
      reset();
      await load();
      onChanged?.();
    } catch (e) {
      setErr(t("保存失败：{e}", "Save failed: {e}", { e: String(e) }));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (id: string) => {
    try {
      await stewardApi.ctxDelete(id);
      if (editing === id) reset();
      await load();
      onChanged?.();
    } catch (e) {
      setErr(t("删除失败：{e}", "Delete failed: {e}", { e: String(e) }));
    }
  };

  const shown = items.filter(
    (x) => x.kind === kind && (kind !== "intent" || x.contractId === contractId)
  );

  return (
    <div className="st-ctx">
      <div className="st-sit-head">
        <span>
          {t("决策依据", "Decision basis")} · {KINDS.find((k) => k.value === kind)?.hint}
        </span>
        <button className="st-btn" onClick={() => void load()}>
          {t("刷新", "Refresh")}
        </button>
      </div>

      <div className="st-ctx-tabs">
        {KINDS.map((k) => (
          <button
            key={k.value}
            className={`st-btn${kind === k.value ? " primary" : ""}`}
            onClick={() => {
              setKind(k.value);
              reset();
            }}
          >
            {k.label} {items.filter((x) => x.kind === k.value).length}
          </button>
        ))}
      </div>

      {err && <div className="st-err">{err}</div>}

      <div className="st-ctx-list">
        {shown.length === 0 && (
          <div className="st-empty">
            {kind === "intent" && !contractId
              ? t(
                  "先在上面开始托管，这里会出现由目标编译出的意向栈 I0",
                  "Start autopilot above; the intent stack I0 compiled from your goal will appear here"
                )
              : t("这一类还没有条目", "No entries of this kind yet")}
          </div>
        )}
        {shown.map((x) => (
          <div key={x.id} className="st-ctx-row">
            <div className="st-ctx-text">
              <span className={`st-badge${x.strength === "hard" ? " hard" : ""}`}>
                {x.strength === "hard" ? t("硬", "Hard") : t("软", "Soft")}
              </span>
              {x.source !== "user" && <span className="st-badge">{x.source}</span>}
              {x.scope !== "global" && <span className="st-badge">{x.scope}</span>}
              {x.text}
            </div>
            <div className="st-evidence">{extraSummary(x) || "—"}</div>
            <div className="st-ctx-acts">
              <button className="st-btn" onClick={() => edit(x)}>
                {t("编辑", "Edit")}
              </button>
              <button className="st-btn danger" onClick={() => void remove(x.id)}>
                {t("删除", "Delete")}
              </button>
            </div>
          </div>
        ))}
      </div>

      <div className="st-ctx-form">
        <div className="st-form-title">
          {editing ? t("编辑条目", "Edit entry") : t("新增条目", "New entry")} · {KINDS.find((k) => k.value === kind)?.label}
        </div>
        <input
          className="st-input"
          value={text}
          onChange={(e) => setText(e.target.value)}
          placeholder={
            kind === "preference"
              ? t("例如：不要改设置界面的记住开关", "e.g. Don't change the \"remember\" toggle in Settings")
              : kind === "intent"
                ? t("例如：修复钥匙串二次弹窗并冷启动验证", "e.g. Fix the duplicate keychain prompt and verify on cold start")
                : kind === "knowledge"
                  ? t("例如：vault_try_remembered 被调用两次是根因", "e.g. Root cause: vault_try_remembered is called twice")
                  : t("例如：SSH 首次连接确认回答 yes", "e.g. Answer yes to the SSH first-connection prompt")
          }
        />
        {kind === "preference" && (
          <div className="st-form-row">
            <select className="st-select" value={strength} onChange={(e) => setStrength(e.target.value)}>
              <option value="hard">{t("硬约束（不可越过）", "Hard rule (never crossed)")}</option>
              <option value="soft">{t("软偏好", "Soft preference")}</option>
            </select>
            <input className="st-input" value={scope} onChange={(e) => setScope(e.target.value)} placeholder={t("作用域 global / claude_code / hostKey", "Scope: global / claude_code / hostKey")} />
          </div>
        )}
        {kind === "knowledge" && (
          <input className="st-input" value={tags} onChange={(e) => setTags(e.target.value)} placeholder={t("标签，逗号分隔：keychain, vault, 钥匙串", "Tags, comma-separated: keychain, vault")} />
        )}
        {kind === "intent" && (
          <>
            <input className="st-input" value={constraints} onChange={(e) => setConstraints(e.target.value)} placeholder={t("本次约束，逗号分隔：别改设置 UI, 最小改动", "Constraints, comma-separated: don't touch Settings UI, minimal change")} />
            <input
              className="st-input"
              value={answerTemplate}
              onChange={(e) => setAnswerTemplate(e.target.value)}
              placeholder={TEMPLATE_HINT()}
            />
          </>
        )}
        {kind === "playbook" && (
          <>
            <div className="st-form-row">
              <input className="st-input" value={matchProgram} onChange={(e) => setMatchProgram(e.target.value)} placeholder={t("程序 * / claude_code / ssh", "Program: * / claude_code / ssh")} />
              <input className="st-input" value={matchKind} onChange={(e) => setMatchKind(e.target.value)} placeholder={t("局面 * / question / pager", "Situation: * / question / pager")} />
            </div>
            <input className="st-input" value={matchText} onChange={(e) => setMatchText(e.target.value)} placeholder={t("屏幕文本包含（不区分大小写）", "Screen text contains (case-insensitive)")} />
            <div className="st-form-row">
              <input className="st-input" value={answer} onChange={(e) => setAnswer(e.target.value)} placeholder={t("要发出的回复（单行）", "Reply to send (single line)")} />
              <input className="st-input short" value={optionIndex} onChange={(e) => setOptionIndex(e.target.value)} placeholder={t("选项号", "Option #")} />
              <select className="st-select" value={tier} onChange={(e) => setTier(e.target.value)}>
                <option value="auto">{TIER_LABELS.auto}</option>
                <option value="notify">{TIER_LABELS.notify}</option>
                <option value="approve">{TIER_LABELS.approve}</option>
              </select>
            </div>
          </>
        )}
        <div className="st-form-row">
          <button className="st-btn primary" disabled={busy || !text.trim()} onClick={() => void submit()}>
            {editing ? t("保存", "Save") : t("添加", "Add")}
          </button>
          {editing && (
            <button className="st-btn" onClick={reset}>
              {t("取消", "Cancel")}
            </button>
          )}
          <span className="st-meta">
            {t(
              "seed 条目可编辑 / 删除；依据写错会直接影响自动答复",
              "Seed entries can be edited or deleted; a wrong entry directly affects automatic replies"
            )}
          </span>
        </div>
      </div>
    </div>
  );
}
