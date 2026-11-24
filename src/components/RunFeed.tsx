/**
 * Agent 运行过程的卡片：执行 / 按键 / 等待 / 盯屏 / 密码 / 审批……
 *
 * 从 AgentPanel 抽出，「对话」与「托管」两个视图共用同一套渲染。卡片只负责展示与把用户的
 * 选择回调出去；订阅事件、调后端的是 `useAgentRun`（全应用只有一份订阅，所以审批 / 密码
 * 只会被答一次）。
 */

import { DiffView } from "./DiffView";
import { needsIncompleteBanner } from "../lib/diff";
import { TodoList } from "./TodoList";
import { useEffect, useRef, useState } from "react";
import { MemoryStepCard } from "./HostMemory";
import { GateCard, type GateHost } from "./GateCard";
import { isHandEyeKind, maskReveal } from "../lib/agentRun";
import { revealNotSaved } from "../lib/agentHistory";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import { localized } from "../lib/localized";
import type {
  AgentApproveRequest,
  AgentMessage,
  AgentSecretRequest,
  AgentStep,
} from "../types";

/** 一条消息若是过程卡片（步骤 / 审批 / 密码）就渲染它，否则返回 null（由调用方画对话泡） */
export function RunCard({
  message: m,
  busy,
  onDecide,
  onAnswerSecret,
  host,
}: {
  message: AgentMessage;
  busy: boolean;
  onDecide: (req: AgentApproveRequest, approved: boolean) => void;
  onAnswerSecret: (req: AgentSecretRequest, value?: string) => Promise<string | null>;
  /** 托管上下文（门禁卡上的「这是误判 / 放开这一类」要它）；对话页不给 */
  host?: GateHost | null;
}) {
  // 托管门禁卡：被拒 / 要批准 / 执行后通知，带原因与屏幕分类
  if (m.step?.kind === "gate") return <GateCard step={m.step} host={host} />;
  // 过程卡片不是对话泡：整幅宽度更能排得下命令与输出
  if (m.step) return <StepCard step={m.step} />;
  if (m.approve) {
    const req = m.approve;
    return (
      <ApproveCard req={req} result={m.approveResult} busy={busy} onDecide={(ok) => onDecide(req, ok)} />
    );
  }
  if (m.secret) {
    const req = m.secret;
    return (
      <SecretCard
        req={req}
        result={m.secretResult}
        busy={busy}
        onAnswer={(value) => onAnswerSecret(req, value)}
      />
    );
  }
  return null;
}

/**
 * 托管视图里的运行实况：过程卡片 + 收尾的助手文字（结论 / 错误）+ 「还在跑 / 已暂停」提示。
 * 只渲染传进来的消息，不订阅任何事件。用户的提问气泡（托管里就是目标本身）不显示。
 */
export function RunFeed({
  messages,
  busy,
  paused,
  pauseText,
  onDecide,
  onAnswerSecret,
  host,
}: {
  messages: AgentMessage[];
  busy: boolean;
  paused: boolean;
  /** 暂停横幅里的说明（预算用尽 / 手动暂停），由调用方按暂停原因生成 */
  pauseText?: string;
  onDecide: (req: AgentApproveRequest, approved: boolean) => void;
  onAnswerSecret: (req: AgentSecretRequest, value?: string) => Promise<string | null>;
  /** 被托管的契约（门禁卡按钮用） */
  host?: GateHost | null;
}) {
  useLocale();
  const ref = useRef<HTMLDivElement>(null);
  // 贴底时才跟随：用户往上翻看历史就别把他拽回去
  const stick = useRef(true);
  useEffect(() => {
    const el = ref.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [messages, busy]);
  return (
    <div
      className="st-feed"
      ref={ref}
      onScroll={() => {
        const el = ref.current;
        if (el) stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
      }}
    >
      {messages.length === 0 && !busy && <div className="st-empty">{t("还没有运行记录。", "No runs yet.")}</div>}
      {messages.map((m) => {
        if (isRunCard(m)) {
          return (
            <RunCard key={m.id} message={m} busy={busy} onDecide={onDecide} onAnswerSecret={onAnswerSecret} host={host} />
          );
        }
        if (m.role === "user" || !m.text) return null;
        return (
          <div key={m.id} className="ap-msg assistant">
            <div className={`ap-bubble assistant${m.error ? " error" : ""}`}>
              <div className="ap-text">{m.text}</div>
            </div>
          </div>
        );
      })}
      {busy && !paused && (
        <div className="ap-running">
          <span className="ap-typing">
            <i />
            <i />
            <i />
          </span>
          {t("托管执行中…", "Autopilot running…")}
        </div>
      )}
      {busy && paused && pauseText && <div className="ap-note ap-paused">{pauseText}</div>}
    </div>
  );
}

/** 消息是不是过程卡片（RunCard 会渲染它） */
export const isRunCard = (m: AgentMessage): boolean => !!(m.step || m.approve || m.secret);

/**
 * 把工具的原始参数 JSON 排成一行人话：`{"op":"list"}` → `op=list`。
 * 刻意单行而非缩进块：参数通常只有两三个字段，缩进白白占掉对话区高度。
 */
function prettyToolArgs(raw: string): string {
  const trimmed = raw.trim();
  if (!trimmed || trimmed === "{}") return "";
  try {
    const obj = JSON.parse(trimmed) as Record<string, unknown>;
    if (typeof obj !== "object" || obj === null) return trimmed;
    const parts = Object.entries(obj).map(([k, v]) => {
      if (typeof v === "string") return `${k}=${v}`;
      return `${k}=${JSON.stringify(v)}`;
    });
    return parts.length ? parts.join("  ") : trimmed;
  } catch {
    // 模型偶尔吐出不是 JSON 的参数串。原样展示比丢掉强：
    // 用户至少能看到它到底想传什么。
    return trimmed;
  }
}

/** 终端停在交互提示上时（密码 / 确认 / 分页 / 菜单 / REPL）给步骤卡片挂的小标签；shell_prompt / unknown 不显示 */
const PROMPT_LABELS: Record<string, string> = localized({
  password: ["等待密码", "Awaiting password"],
  yes_no: ["等待确认", "Awaiting confirmation"],
  pager: ["分页器", "Pager"],
  menu: ["菜单", "Menu"],
  repl: ["REPL", "REPL"],
});

function PromptBadge({ kind }: { kind?: string }) {
  useLocale();
  const label = kind ? PROMPT_LABELS[kind] : undefined;
  if (!label) return null;
  return (
    <span className="ap-prompt-badge" title={t("终端当前停在：{kind}", "Terminal is waiting at: {kind}", { kind: kind ?? "" })}>
      {label}
    </span>
  );
}

/** 密码通道结局 → 卡片上的中文短语 */
const SECRET_STATUS_LABELS: Record<string, string> = localized({
  provided: ["已输入", "Entered"],
  declined: ["已跳过", "Skipped"],
  timeout: ["已超时", "Timed out"],
  cancelled: ["已取消", "Cancelled"],
  failed: ["写入失败", "Failed to write"],
});

/**
 * 一次执行的卡片：命令 + 结果。「在哪台机器上跑的」必须写清楚 —— Agent 是全局的，
 * 可以同时在好几个会话上下令，只显示命令的话分不清输出来自哪台机器。
 */
function StepCard({ step }: { step: AgentStep }) {
  useLocale();
  if (step.kind === "note" || step.kind === "resumed") {
    return <div className="ap-note">{step.text ?? ""}</div>;
  }
  // 运行暂停（预算用尽 / 手动暂停）：做了什么 / 还差什么 / 怎么继续，多行原样展示
  if (step.kind === "paused") {
    return <div className="ap-note ap-paused">{step.text ?? ""}</div>;
  }
  if (step.kind === "memory") return <MemoryStepCard step={step} />;
  // 交付给你（nx_reveal）：只给用户的内容，默认打码
  if (step.kind === "reveal") return <RevealCard step={step} />;
  // 计划清单：整表替换，每次都画最新一版
  if (step.kind === "todo") return <TodoList items={step.todo ?? []} />;
  // 文件工具：读 / 查找 / 搜索一行带过；写 / 改带差异预览
  if (step.kind === "file") {
    // 成功时后端给的是「完成」（英文界面可能是 Done）：两种都算成功
    const failed = step.text !== undefined && step.text !== "完成" && step.text !== "Done";
    return (
      <div className={`ap-step file${failed ? " danger" : ""}`}>
        <div className="ap-step-head">
          <span className="ap-step-tag">{t("文件", "File")}</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
        </div>
        <div className="ap-step-why">{step.command ?? ""}</div>
        {failed ? <div className="ap-step-why">{step.text}</div> : null}
        {step.output ? <DiffView diff={step.output} /> : null}
      </div>
    );
  }
  // 工具调用卡片：把「调了哪个工具、传了什么参数」原样摆出来，让调用过程看得见。
  // 没有它时，模型想开新标签页会写出 `new-tab` 这种不存在的命令，敲进去只换来
  // command not found —— 看起来就像它答应了却没动手。
  if (step.kind === "tool") {
    return (
      <div className="ap-step tool">
        <div className="ap-step-head">
          <span className="ap-step-tag">{t("工具调用", "Tool call")}</span>
          <span className="ap-step-tool">{step.toolLabel ?? t("工具", "Tool")}</span>
        </div>
        <div className="ap-step-why">
          <code>{step.tool ?? ""}</code>
          {step.toolArgs && step.toolArgs.trim() !== "{}" ? (
            <span className="ap-tool-args">{prettyToolArgs(step.toolArgs)}</span>
          ) : (
            <span className="ap-tool-args dim">{t("（无参数）", "(no arguments)")}</span>
          )}
        </div>
      </div>
    );
  }
  // `open` / `export` 没有 command / output：掉进下面那张执行卡会变成一个永远
  // 「执行中」的空框，看着像卡住了，其实标签页早就开出来了。
  if (step.kind === "open" || step.kind === "export" || step.kind === "tab") {
    const isOpen = step.kind === "open";
    const isTab = step.kind === "tab";
    return (
      <div className="ap-step">
        <div className="ap-step-head">
          <span className="ap-step-tag">
            {isTab ? t("标签页", "Tab") : isOpen ? t("新会话", "New session") : t("导记录", "Export log")}
          </span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
        </div>
        <div className="ap-step-why">{step.text ?? ""}</div>
      </div>
    );
  }
  // 密码通道的过程卡：start（secretStatus 为空）→ 收尾（带结局）。
  // 这张卡永远不含密码本身——只有理由、结局和写入后的输出尾部（后端已掩码）。
  if (step.kind === "secret") {
    const finished = step.secretStatus !== undefined;
    const out = step.output ?? "";
    const bad = step.secretStatus === "failed" || step.secretStatus === "timeout";
    return (
      <div className={`ap-step${finished ? "" : " pending"}${bad ? " danger" : ""}`}>
        <div className="ap-step-head">
          <span className="ap-step-tag">{t("密码", "Password")}</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
          <PromptBadge kind={step.promptKind} />
        </div>
        {step.command ? <div className="ap-step-why">{t("原因：", "Reason: ")}{step.command}</div> : null}
        <div className="ap-step-why">
          {finished ? (SECRET_STATUS_LABELS[step.secretStatus!] ?? step.secretStatus) : t("等待用户输入……", "Waiting for user input…")}
          {step.text && finished ? t("（{text}）", " ({text})", { text: step.text }) : ""}
        </div>
        {finished && out.trim() ? <pre className="ap-code ap-out-plain">{out}</pre> : null}
      </div>
    );
  }
  // 技能回放的一步：command = 「n/N 摘要」，skillStatus 是进度，text 是一句话说明
  if (step.kind === "skill") {
    const st = step.skillStatus ?? "running";
    const label: Record<string, string> = {
      running: t("进行中", "Running"),
      ok: t("完成", "Done"),
      deviated: t("偏离", "Deviated"),
      failed: t("失败", "Failed"),
      skipped: t("跳过", "Skipped"),
      declined: t("未获批", "Not approved"),
    };
    return (
      <div className={`ap-step${st === "running" ? " pending" : ""}`}>
        <div className="ap-step-head">
          <span className="ap-step-tag">{t("技能", "Skill")}</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
          <span className="ap-step-where">{label[st] ?? st}</span>
        </div>
        {step.command ? <pre className="ap-code">{step.command}</pre> : null}
        {step.text && st !== "running" ? <div className="ap-step-why">{step.text}</div> : null}
        {st === "running" ? <div className="ap-running">{step.text ?? t("执行中……", "Running…")}</div> : null}
      </div>
    );
  }
  // 手和眼：发按键 / 等输出 / 读屏幕 / 盯屏等待（托管）。output 为空 = 进行中，有值（含空串）= 已收尾。
  if (isHandEyeKind(step.kind)) {
    const tag =
      step.kind === "keys"
        ? t("按键", "Keys")
        : step.kind === "wait"
          ? t("等待", "Wait")
          : step.kind === "watch"
            ? t("盯屏等待", "Watch screen")
            : t("读屏", "Read screen");
    const finished = step.output !== undefined;
    const out = step.output ?? "";
    const lines = out.split("\n").length;
    return (
      <div className={`ap-step${finished ? "" : " pending"}`}>
        <div className="ap-step-head">
          <span className="ap-step-tag">{tag}</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
          {finished ? <PromptBadge kind={step.promptKind} /> : null}
        </div>
        {step.command ? <pre className="ap-code">{step.command}</pre> : null}
        {step.text ? <div className="ap-step-why">{step.text}</div> : null}
        {finished && out.trim() ? (
          lines > 10 ? (
            <details className="ap-out">
              <summary>
                {step.kind === "screen" || step.kind === "watch"
                  ? t("屏幕 {n} 行", "Screen: {n} lines", { n: lines })
                  : t("输出 {n} 行", "Output: {n} lines", { n: lines })}
              </summary>
              <pre className="ap-code">{out}</pre>
            </details>
          ) : (
            <pre className="ap-code ap-out-plain">{out}</pre>
          )
        ) : null}
        {!finished ? (
          <div className="ap-running">
            <span className="ap-typing">
              <i />
              <i />
              <i />
            </span>
            {step.kind === "watch" ? t("盯屏中…", "Watching…") : t("进行中…", "Running…")}
          </div>
        ) : null}
      </div>
    );
  }
  if (step.kind === "skip") {
    return (
      <div className="ap-step danger">
        <div className="ap-step-head">
          <span className="ap-step-tag">{t("未执行", "Not run")}</span>
          {/* 被拦下的伪命令（new-tab 之类）没有目标会话：写"未知会话"会让人以
              为它跑去了某台机器上，而实情是它一步都没出去。没有目标就别编一个。 */}
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
        </div>
        <pre className="ap-code">{step.command ?? ""}</pre>
        {step.reasons?.length ? (
          <div className="ap-step-why">{t("原因：", "Reason: ")}{step.reasons.join(t("；", "; "))}</div>
        ) : null}
      </div>
    );
  }

  // 后端新增、前端还不认识的步骤种类：给一张通用卡片，别画成一个永远「执行中」的空命令框。
  if (step.kind !== "exec" && step.kind !== "result" && step.kind !== "plan") {
    return (
      <div className="ap-step">
        <div className="ap-step-head">
          <span className="ap-step-tag">{step.kind}</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
        </div>
        {step.command ? <pre className="ap-code">{step.command}</pre> : null}
        {step.text ? <div className="ap-step-why">{step.text}</div> : null}
        {step.output?.trim() ? <pre className="ap-code ap-out-plain">{step.output}</pre> : null}
      </div>
    );
  }

  // 没有退出码这回事了：命令是否成功，由模型读下面这段真实输出自己判断，
  // 跟真人盯着终端看一样。这里只中性地标「执行中 / 已完成」，不再染色判对错。
  const done = step.kind === "result";
  const tag = done ? t("已完成", "Done") : t("执行中", "Running");
  const lines = (step.output ?? "").split("\n").length;
  return (
    <div className={`ap-step${done ? "" : " pending"}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{tag}</span>
        <span className="ap-step-where" title={step.target}>
          {step.targetLabel ?? t("未知会话", "Unknown session")}
        </span>
        {done ? <PromptBadge kind={step.promptKind} /> : null}
      </div>
      <pre className="ap-code">{step.command ?? ""}</pre>
      {done ? (
        (step.output ?? "").trim() ? (
          // 长输出折叠：一屏七八行的核对性输出最常见，全展开会把对话挤没
          lines > 10 ? (
            <details className="ap-out">
              <summary>{t("输出 {n} 行", "Output: {n} lines", { n: lines })}</summary>
              <pre className="ap-code">{step.output}</pre>
            </details>
          ) : (
            <pre className="ap-code ap-out-plain">{step.output}</pre>
          )
        ) : (
          <div className="ap-step-why">{t("（该命令没有输出）", "(no output)")}</div>
        )
      ) : (
        <div className="ap-running">
          <span className="ap-typing">
            <i />
            <i />
            <i />
          </span>
          {t("等待输出…", "Waiting for output…")}
          {step.elapsedSecs ? t("（已等待 {n} 秒）", " ({n}s elapsed)", { n: step.elapsedSecs }) : ""}
        </div>
      )}
    </div>
  );
}

/**
 * 待批卡片。危险命令在这里停一下等人点头 ——
 * 这是「人在回路」唯一发生的地方，所以命令原文必须完整可见，不许折叠。
 */
function ApproveCard({
  req,
  result,
  busy,
  onDecide,
}: {
  req: AgentApproveRequest;
  result?: boolean;
  busy: boolean;
  onDecide: (approved: boolean) => void;
}) {
  useLocale();
  const settled = result !== undefined;
  // 越界批准与危险命令批准含义不同：前者批的是「动这台不在范围里的会话」，
  // 标题与文案必须直说，别让人以为只是又一次命令风险提示。
  const scope = req.kind === "scope";
  // skill：技能回放的逐步确认 / 偏离选择，按钮文案按语义写成「确认 / 拒绝」
  const skill = req.kind === "skill";
  // file：写 / 改文件，卡上带差异预览；file_sensitive：路径在受保护清单里
  const file = req.kind === "file" || req.kind === "file_sensitive" || req.kind === "file_read_sensitive";
  return (
    <div className={`ap-approve${settled ? (result ? " ok" : " no") : ""}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">
          {scope
            ? t("超出作用范围 · 需要批准", "Out of scope · Approval needed")
            : skill
              ? t("技能 · 需要确认", "Skill · Confirmation needed")
              : req.kind === "file_sensitive"
                ? t("写受保护路径 · 需要批准", "Write to protected path · Approval needed")
                : req.kind === "file_read_sensitive"
                  ? t("读受保护路径 · 需要批准", "Read protected path · Approval needed")
                  : file
                  ? t("写文件 · 需要批准", "Write file · Approval needed")
                  : t("需要批准", "Approval needed")}
        </span>
        <span className="ap-step-where" title={req.target}>
          {req.targetLabel || t("未知会话", "Unknown session")}
        </span>
      </div>
      <pre className="ap-code">{req.command}</pre>
      {needsIncompleteBanner(req.previewComplete, req.diff) ? (
        <div className="ap-diff-incomplete" role="alert">
          {t(
            "⚠ 预览不完整：卡上看不到全部修改，批准写入的是完整内容",
            "⚠ Incomplete preview: not every change is shown here; approving writes the full content"
          )}
        </div>
      ) : null}
      {req.diff ? <DiffView diff={req.diff} open /> : null}
      {req.reasons.length > 0 && (
        <div className="ap-step-why">
          {scope ? t("越界原因：", "Out-of-scope reason: ") : skill || file ? t("说明：", "Note: ") : t("风险提示：", "Risk: ")}
          {req.reasons.join(t("；", "; "))}
        </div>
      )}
      {settled ? (
        <div className="ap-approve-done">
          {result
            ? scope
              ? t("已允许，该会话已并入本轮作用范围", "Allowed — this session is now in scope for this run")
              : skill
                ? t("已确认", "Confirmed")
                : t("已允许执行", "Allowed")
            : t("已拒绝 / 已超时", "Denied / timed out")}
        </div>
      ) : (
        <div className="ap-approve-row">
          <button className="btn primary" disabled={!busy} onClick={() => onDecide(true)}>
            {scope ? t("允许越界操作", "Allow out of scope") : skill ? t("确认", "Confirm") : file ? t("允许写入", "Allow write") : t("允许执行", "Allow")}
          </button>
          <button className="btn" disabled={!busy} onClick={() => onDecide(false)}>
            {t("拒绝", "Deny")}
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * 密码卡片（nx_secret）。模型只负责「请求」，值由人在这里输入、直送后端写进终端。
 *
 * - 输入值**只存在本组件的 useState 里**：提交 / 跳过后立刻清空，
 *   不写入会话 state、不进 `AgentMessage`、不进任何日志或事件。
 * - 用 `type="password"` + `autoComplete="off"`，避免浏览器 / WebView 记住或回填。
 * - `busy` 为 false（这一轮已结束 / 被取消）后输入框禁用：后端早就不再等了。
 */
function SecretCard({
  req,
  result,
  busy,
  onAnswer,
}: {
  req: AgentSecretRequest;
  result?: "submitted" | "skipped" | "expired";
  busy: boolean;
  /** `value` 为 undefined = 跳过。返回内联错误文案，或 null 表示已处理 */
  onAnswer: (value?: string) => Promise<string | null>;
}) {
  useLocale();
  const [value, setValue] = useState("");
  const [inlineErr, setInlineErr] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [left, setLeft] = useState(req.timeoutSecs);
  const settled = result !== undefined;

  // 倒计时只是提示：真正的超时在后端，到点后端会放弃等待并告诉模型「超时」
  useEffect(() => {
    if (settled) return;
    const timer = window.setInterval(() => setLeft((n) => (n > 0 ? n - 1 : 0)), 1000);
    return () => window.clearInterval(timer);
  }, [settled]);

  const submit = async (v?: string) => {
    if (sending) return;
    setSending(true);
    setInlineErr(null);
    const err = await onAnswer(v);
    setSending(false);
    if (err === null) {
      // 无论提交还是跳过，本地输入立刻清掉
      setValue("");
    } else {
      setInlineErr(err);
    }
  };

  const unexpected = req.promptKind !== undefined && req.promptKind !== "password";
  // timeoutSecs = 0：托管下的密码卡不限时
  const unlimited = req.timeoutSecs === 0;
  const dead = !busy || (!unlimited && left === 0);
  return (
    <div className={`ap-approve ap-secret${settled ? (result === "submitted" ? " ok" : " no") : ""}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{t("请求输入密码", "Password requested")}</span>
        <span className="ap-step-where" title={req.target}>
          {req.targetLabel || t("未知会话", "Unknown session")}
        </span>
      </div>
      <div className="ap-step-why">{t("原因：", "Reason: ")}{req.reason}</div>
      {settled ? (
        <div className="ap-approve-done">
          {result === "submitted"
            ? t("已提交（密码只写入了终端，助手看不到它）", "Submitted (sent to the terminal only; the assistant can't see it)")
            : result === "skipped"
              ? t("已跳过", "Skipped")
              : t("已失效（超时或这一轮已结束）", "Expired (timed out or the run has ended)")}
        </div>
      ) : (
        <form
          className="ap-secret-form"
          onSubmit={(e) => {
            e.preventDefault();
            if (value && !dead) void submit(value);
          }}
        >
          {unexpected && (
            <div className="ap-secret-warn">
              {t(
                "终端看起来并没有停在密码提示上，输入可能会被回显到屏幕。",
                "The terminal doesn't seem to be at a password prompt — your input may be echoed on screen."
              )}
            </div>
          )}
          <input
            className="ap-secret-input"
            type="password"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            placeholder={t("在此输入，回车提交", "Type here, press Enter to submit")}
            value={value}
            disabled={dead || sending}
            onChange={(e) => setValue(e.target.value)}
          />
          {inlineErr && <div className="ap-secret-err">{inlineErr}</div>}
          <div className="ap-approve-row">
            <button className="btn primary" type="submit" disabled={dead || sending || !value}>
              {t("提交", "Submit")}
            </button>
            <button className="btn" type="button" disabled={dead || sending} onClick={() => void submit(undefined)}>
              {t("跳过", "Skip")}
            </button>
            <span className="ap-secret-left">{dead ? t("已失效", "Expired") : unlimited ? t("不限时", "No time limit") : t("{n} 秒后超时", "Times out in {n}s", { n: left })}</span>
          </div>
        </form>
      )}
    </div>
  );
}

/**
 * 「交付给你」卡（`nx_reveal`）：Agent 生成、只交给用户的内容（密码 / 密钥 / 节点链接…），模型看不到。
 * 默认打码，点「显示」/「复制」才用到原文；原文只在内存里，重载后卡上只剩占位。
 */
function RevealCard({ step }: { step: AgentStep }) {
  useLocale();
  const [shown, setShown] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const content = step.revealContent;
  const copy = async () => {
    if (content === undefined) return;
    try {
      await api.clipboardWrite(content);
      setNote(t("已复制到剪贴板", "Copied to clipboard"));
    } catch (e) {
      setNote(t("复制失败：{e}", "Copy failed: {e}", { e: String(e) }));
    }
  };
  return (
    <div className="ap-step ap-reveal">
      <div className="ap-step-head">
        <span className="ap-step-tag">{t("交付给你", "Delivered to you")}</span>
        <span className="ap-reveal-title">{step.text || t("交付内容", "Delivered content")}</span>
        {step.targetLabel ? (
          <span className="ap-step-where" title={step.target}>
            {step.targetLabel}
          </span>
        ) : null}
      </div>
      {step.command ? (
        <div className="ap-step-why">
          {t("来源：", "Source: ")}
          {step.command}
          {step.output ? t("（{v}）", " ({v})", { v: step.output }) : ""}
        </div>
      ) : null}
      {content === undefined ? (
        <div className="ap-step-why">{revealNotSaved()}</div>
      ) : (
        <>
          <pre className={`ap-code ap-out-plain ap-reveal-body${shown ? "" : " masked"}`}>
            {shown ? content : maskReveal(content)}
          </pre>
          <div className="ap-approve-row">
            {shown ? (
              <button className="btn" type="button" onClick={() => setShown(false)}>
                {t("隐藏", "Hide")}
              </button>
            ) : (
              <button className="btn primary" type="button" onClick={() => setShown(true)}>
                {t("显示", "Show")}
              </button>
            )}
            <button className="btn" type="button" onClick={() => void copy()}>
              {t("复制", "Copy")}
            </button>
            {note ? <span className="ap-reveal-note">{note}</span> : null}
          </div>
          <div className="ap-step-why">
            {t(
              "内容只给你，没有经过助手；不会写进聊天记录，重启后就看不到了，请及时妥善保存。",
              "This is for you only and never passed through the assistant. It isn't saved to the chat history and will be gone after a restart, so store it somewhere safe now."
            )}
          </div>
        </>
      )}
    </div>
  );
}
