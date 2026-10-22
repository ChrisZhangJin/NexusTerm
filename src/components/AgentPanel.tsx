import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";
import {
  api,
  onAgentApprove,
  onAgentDone,
  onAgentError,
  onAgentStep,
  onAgentToken,
} from "../api";
import { formatBytes, isAgentReady, useAgentConfig } from "../lib/agent";
import { toFriendlyError } from "../lib/errors";
import { matchSessions, scanMention } from "../lib/mention";
import type {
  AgentApproveRequest,
  AgentBufferStats,
  AgentDoneReason,
  AgentMessage,
  AgentRunMode,
  AgentStep,
  AgentTarget,
} from "../types";

export interface AgentPanelProps {
  /** 当前会话 id（绑定哪个终端） */
  sessionId: string;
  /** 面板是否可见 */
  visible: boolean;
  onClose: () => void;
  /** 把文本写入终端 pty。由 App 传入，组件不直接碰后端 */
  onSendToTerminal: (sessionId: string, text: string) => Promise<void>;
  /** 打开设置（AI 助手那一项）。未配置时的引导按钮走这里 */
  onOpenSettings: () => void;
  /** 浮出为独立窗口。独立窗口里不传（没有再浮一次的意义） */
  onFloatOut?: () => void;
  /**
   * 当前所有可操作的会话（**作为 Agentic 模式的靶机候选**）。
   * 由 App 从打开的标签算出来 —— 面板自己不认识标签树。
   */
  sessions?: AgentTarget[];
}

/** 单个会话的对话状态。按 sessionId 分桶，切会话时彼此完全隔离。 */
interface Conversation {
  messages: AgentMessage[];
  /** 这个会话正在等一次回答的流结束 */
  busy: boolean;
  /** 上一次是被「切走会话 / 收起面板」打断的 —— 半截答案不可信，要提示 */
  interrupted: boolean;
}

const EMPTY_CONV: Conversation = { messages: [], busy: false, interrupted: false };

/** 撞到 max_tokens 时追加在答案末尾的说明 */
const TRUNCATED_HINT = "\n\n— 回答被长度限制截断，可以让它继续，或缩小问题范围再问一次。";
/** 一个字都没收到就 done 了（被过滤 / 空回复）：不能给用户留一个空气泡 */
const EMPTY_ANSWER = "（本次没有返回内容）";
const CANCELLED_ANSWER = "（已取消）";
/** 一轮跑完却没等到结论（多半是撞到步数上限）时补的那句话 */
const NO_CONCLUSION = "（这一轮没有给出结论：上面的步骤就是它做过的全部事情。）";

let seq = 0;
/** 纯前端的消息 id。不用 crypto.randomUUID：Tauri 的 WebView 不保证是安全上下文。 */
const uid = () => `m${Date.now().toString(36)}-${(seq++).toString(36)}`;

type Segment =
  | { kind: "text"; text: string }
  | { kind: "code"; lang: string; text: string };

/**
 * 把回答切成「普通文本 / 代码块」序列。
 * 流式过程中代码块按普通 <pre> 渲染 —— 半个围栏不该被当成一条命令；
 * 流结束后才把代码块升级成可插入终端的建议卡片，同一份命令也只显示一次。
 */
function splitSegments(text: string): Segment[] {
  const out: Segment[] = [];
  const re = /```([^\n`]*)\r?\n?([\s\S]*?)```/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    if (m.index > last) out.push({ kind: "text", text: text.slice(last, m.index) });
    const body = m[2].replace(/\s+$/, "");
    if (body) out.push({ kind: "code", lang: m[1].trim(), text: body });
    last = re.lastIndex;
  }
  if (last < text.length) out.push({ kind: "text", text: text.slice(last) });
  return out;
}

/** 收到一个增量文本块：追加到当前正在流的那条助手消息上 */
function withToken(c: Conversation, chunk: string): Conversation {
  const msgs = c.messages.slice();
  const last = msgs[msgs.length - 1];
  if (last && last.role === "assistant" && last.streaming) {
    msgs[msgs.length - 1] = { ...last, text: last.text + chunk };
    return { ...c, messages: msgs };
  }
  // 正常路径下提问时已经先建好占位消息；真走到这儿说明后端漏了顺序，补一条别丢字
  msgs.push({ id: uid(), role: "assistant", text: chunk, ts: Date.now(), streaming: true });
  return { ...c, messages: msgs };
}

/**
 * 收到 done：这是唯一的正常收尾信号（后端保证一定发，取消时 reason=cancelled）。
 * 前端不做任何超时兜底 —— 猜出来的结束会和后端状态打架。
 */
function withDone(c: Conversation, reason: AgentDoneReason): Conversation {
  return {
    messages: c.messages.map((m) => {
      if (!m.streaming) return m;
      const text = reason === "length" ? m.text + TRUNCATED_HINT : m.text;
      return {
        ...m,
        streaming: false,
        text: text.trim()
          ? text
          : reason === "cancelled"
            ? CANCELLED_ANSWER
            : EMPTY_ANSWER,
      };
    }),
    busy: false,
    interrupted: false,
  };
}

/** 收到 error：终态，其后不会再有 done。把当前助手消息整条换成错误文案。 */
function withError(c: Conversation, msg: string): Conversation {
  const msgs = c.messages.slice();
  const idx = msgs.findIndex((m) => m.role === "assistant" && m.streaming);
  if (idx >= 0) {
    msgs[idx] = { ...msgs[idx], streaming: false, error: true, text: msg };
  } else {
    msgs.push({ id: uid(), role: "assistant", text: msg, ts: Date.now(), error: true });
  }
  return { messages: msgs, busy: false, interrupted: false };
}

/**
 * 追加一个过程步骤。
 *
 * `result` 会回填到它对应的那条 `exec` 上合成**一张**卡片 ——
 * 「下令 + 结果」本来就是一件事，拆成两张卡会把对话拉长一倍，
 * 人也很难看出哪个结果属于哪条命令。
 */
function withStep(c: Conversation, step: AgentStep): Conversation {
  const msgs = c.messages.slice();
  if (step.kind === "result") {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "exec" &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step: { ...s, kind: "result", output: step.output, exitCode: step.exitCode } };
        return { ...c, messages: msgs };
      }
    }
  }
  msgs.push({ id: uid(), role: "assistant", text: "", ts: Date.now(), step });
  return { ...c, messages: msgs };
}

/** 追加一张待批卡片。真正的执行由用户在卡片上点头后才继续。 */
function withApprove(c: Conversation, req: AgentApproveRequest): Conversation {
  return {
    ...c,
    messages: [...c.messages, { id: uid(), role: "assistant", text: "", ts: Date.now(), approve: req }],
  };
}

/** 记录用户对某张待批卡片的答复（卡片自己据此变成「已允许 / 已拒绝」的终态）。 */
function withDecision(
  c: Conversation,
  requestId: string,
  approved: boolean
): Conversation {
  return {
    ...c,
    messages: c.messages.map((m) =>
      m.approve?.requestId === requestId ? { ...m, approveResult: approved } : m
    ),
  };
}

/** Agentic 一轮的结束语落地：既不置也不用补 markup，直接作为一条完整助手消息。 */
function withConclusion(c: Conversation, text: string): Conversation {
  const trimmed = text.trim();
  return {
    busy: false,
    interrupted: false,
    messages: [
      ...c.messages,
      { id: uid(), role: "assistant", text: trimmed || EMPTY_ANSWER, ts: Date.now() },
    ],
  };
}

/**
 * 一轮结束却**没等到**任何结论时兜的那一步（撞到步数上限就是这种情形）。
 *
 * 判定依据是「调用方看到 busy 仍为 true」：有结论来的话后端会发 kind=answer，
 * 那条事件走的是 `withConclusion`，早就把 busy 置回 false 了 —— 还站着 true
 * 就说明这一轮没有收尾，必须由这里补上，否则界面永远在转圈。
 */
function finishWithoutAnswer(c: Conversation): Conversation {
  return withConclusion(c, NO_CONCLUSION);
}

/** 清空记忆的确认框文案是固定的，提到组件外避免每次渲染新建对象 */
const CLEAR_OPTS: ConfirmOptions = {
  title: "清空 AI 记忆",
  message: "将丢弃该会话已旁路缓存的终端输出，助手会忘掉之前看到的内容。聊天记录保留。",
  okText: "清空",
  danger: true,
};

export function AgentPanel({
  sessionId,
  visible,
  onClose,
  onSendToTerminal,
  onOpenSettings,
  onFloatOut,
  sessions = [],
}: AgentPanelProps) {
  const cfg = useAgentConfig();
  const ready = isAgentReady(cfg);

  /** 按会话分桶的聊天记录。key = sessionId，切会话互不可见。 */
  const [convs, setConvs] = useState<Record<string, Conversation>>({});
  const [input, setInput] = useState("");
  /** 输入框。`@` 联想要在选中一项之后把光标放回原处，所以得拿得到它 */
  const taRef = useRef<HTMLTextAreaElement>(null);
  const [attached, setAttached] = useState(false);
  const [stats, setStats] = useState<AgentBufferStats | null>(null);
  const [err, setErr] = useState<string | null>(null);
  /** ask = 只问不做；act = 允许它在指定的会话里真跑命令 */
  const [mode, setMode] = useState<AgentRunMode>("ask");
  const [pending, setPending] = useState<
    { kind: "clear" } | { kind: "cmd"; cmd: string } | null
  >(null);

  /**
   * Agentic 模式的靶机。为空表示「只当前会话」（后端会退回 [sessionId]）。
   * 用 Set 之外的数组是为了让渲染顺序与 tags 一致 —— 勾选拉直这个 useMemo 很绕。
   */
  const [scoped, setScoped] = useState<string[] | null>(null);

  /**
   * 输入框里的 `@` 联想。
   *
   * 作用范围那一排 chips 是「勾选机器」，而 `@` 是另一条更快的路：
   * 正写着「把 @host2 的 nginx 重启一下」的时候顺手就把机器定下来了，
   * 不用先抬头去点 chip 再回来打字。
   *
   * 选完之后 `@xxx` 这一段会被吃掉 —— 它只是给人选机器用的记号，
   * 真正的作用范围在上面的 chips 里，发给模型的也是 chips 的那份。
   */
  const [mention, setMention] = useState<{
    /** `@` 在输入里的下标 */
    start: number;
    query: string;
    active: number;
  } | null>(null);

  const conv = convs[sessionId] ?? EMPTY_CONV;
  const messages = conv.messages;
  const busy = conv.busy;
  /** 本轮的作用范围：scoped 为空时就是「仅当前会话」 */
  const activeScope = scoped && scoped.length > 0 ? scoped : [sessionId];

  const patch = useCallback((sid: string, fn: (c: Conversation) => Conversation) => {
    setConvs((prev) => ({ ...prev, [sid]: fn(prev[sid] ?? EMPTY_CONV) }));
  }, []);

  // ——— 会话切换 / 收起时的收尾 ———

  /**
   * 切走会话或收起面板都会 unlisten，此后后端的流就算跑完我们也收不到了。
   * 这里把那条消息从 streaming 摘下来并标记 interrupted，
   * 否则切回来会看到一个永远在转圈的「正在回答」。
   */
  const markInterrupted = useCallback((sid: string) => {
    setConvs((prev) => {
      const c = prev[sid];
      if (!c) return prev;
      const streaming = c.messages.some((m) => m.streaming);
      if (!c.busy && !streaming) return prev;
      return {
        ...prev,
        [sid]: {
          busy: false,
          interrupted: streaming,
          messages: streaming
            ? c.messages.map((m) => (m.streaming ? { ...m, streaming: false } : m))
            : c.messages,
        },
      };
    });
  }, []);

  const watchingRef = useRef<string | null>(null);
  useEffect(() => {
    const prev = watchingRef.current;
    const next = visible && sessionId ? sessionId : null;
    watchingRef.current = next;
    if (prev && prev !== next) markInterrupted(prev);
  }, [sessionId, visible, markInterrupted]);

  // ——— 事件订阅：按 sessionId 订阅，切会话即整体换掉 ———

  useEffect(() => {
    if (!visible || !sessionId) return;
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];

    void (async () => {
      try {
        const [offToken, offDone, offError, offStep, offApprove] = await Promise.all([
          onAgentToken(sessionId, (chunk) => {
            if (!disposed) patch(sessionId, (c) => withToken(c, chunk));
          }),
          onAgentDone(sessionId, (p) => {
            if (!disposed) patch(sessionId, (c) => withDone(c, p.reason));
          }),
          onAgentError(sessionId, (msg) => {
            if (!disposed) patch(sessionId, (c) => withError(c, msg));
          }),
          onAgentStep(sessionId, (step) => {
            if (disposed) return;
            // kind=answer 是这一轮的总结，作为一条完整的助手消息落地并收掉 busy；
            // 其余都当作过程卡片逐条追加。
            patch(sessionId, (c) =>
              step.kind === "answer"
                ? withConclusion(c, step.text ?? "")
                : withStep(c, step)
            );
          }),
          onAgentApprove(sessionId, (req) => {
            if (!disposed) patch(sessionId, (c) => withApprove(c, req));
          }),
        ]);
        // 竞态：listen 是异步的，等它 resolve 时会话可能已经切走了 —— 这时要立刻退订，
        // 否则旧会话的 token 会被 patch 到新会话的 key 上（unlisten 还没进 unlisteners）
        if (disposed) {
          offToken();
          offDone();
          offError();
          offStep();
          offApprove();
          return;
        }
        unlisteners.push(offToken, offDone, offError, offStep, offApprove);
      } catch {
        // 订阅失败：提问时会在 agentAsk 上抛出真错误，这里不必再弹一次
      }
    })();

    return () => {
      disposed = true;
      unlisteners.forEach((f) => f());
    };
  }, [sessionId, visible, patch]);

  // ——— 状态栏：旁路开关 + 缓冲体积 ———

  const sidRef = useRef(sessionId);
  useEffect(() => {
    sidRef.current = sessionId;
  }, [sessionId]);

  /** 拉一次旁路开关与缓冲体积。会话切走后回来的旧结果直接丢弃，避免张冠李戴。 */
  const refreshStats = useCallback(async () => {
    const sid = sessionId;
    try {
      const [on, st] = await Promise.all([api.agentGetSession(sid), api.agentStats(sid)]);
      if (sidRef.current !== sid) return;
      setAttached(on);
      setStats(st);
    } catch {
      // 会话已关闭 / 旁路未登记：状态栏保留上一次的值，不打扰用户
    }
  }, [sessionId]);

  useEffect(() => {
    if (!visible || !sessionId) return;
    void refreshStats();
    const timer = window.setInterval(() => void refreshStats(), 3000);
    return () => window.clearInterval(timer);
  }, [sessionId, visible, refreshStats]);

  // 面板收起时清掉可能残留的确认框，免得下次打开凭空弹一个
  useEffect(() => {
    if (!visible) setPending(null);
  }, [visible]);

  // ——— 自动滚动：用户往上翻看历史时不要把他拽回底部 ———

  const listRef = useRef<HTMLDivElement>(null);
  /** true = 视口贴着底部，新内容来了就跟着滚；用户手动往上翻后置 false */
  const stickRef = useRef(true);

  const onListScroll = () => {
    const el = listRef.current;
    if (!el) return;
    stickRef.current = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
  };

  // 换会话等于换了一份对话，必须回到贴底模式
  useEffect(() => {
    stickRef.current = true;
  }, [sessionId]);

  useEffect(() => {
    const el = listRef.current;
    if (!el || !stickRef.current) return;
    el.scrollTop = el.scrollHeight;
  }, [messages, visible]);

  // ——— 发送 / 取消 ———

  const send = async () => {
    const q = input.trim();
    setMention(null);
    if (!q || busy || !sessionId) return;
    if (!ready.ok) {
      // 引导态会盖住输入框，正常到不了这里；真到了就把原因说清楚而不是发一个注定失败的请求
      patch(sessionId, (c) => withError(c, ready.reason ?? "Nexus AI Agent 尚未配置完成"));
      return;
    }
    if (mode === "act") await runAgentic(q);
    else await ask(q);
  };

  /**
   * 起手：清空输入框、贴底、把用户消息挂上去。
   *
   * `withPlaceholder` 决定是否先立一个空的助手泡：纯问答是流式的，必须有它接 token；
   * Agentic 的「过程」走一张张 step 卡片，结论最后才来，
   * 提前立一个空泡只会让对话里多一条空气泡。
   */
  const startRun = (sid: string, q: string, withPlaceholder: boolean) => {
    setInput("");
    setErr(null);
    stickRef.current = true;
    const placeholder: AgentMessage = {
      id: uid(),
      role: "assistant",
      text: "",
      ts: Date.now(),
      streaming: true,
    };
    patch(sid, (c) => ({
      busy: true,
      interrupted: false,
      messages: [
        ...c.messages,
        { id: uid(), role: "user" as const, text: q, ts: Date.now() },
        ...(withPlaceholder ? [placeholder] : []),
      ],
    }));
  };

  // ——— `@会话名` 联想 ———

  const mentionHits = (query: string): AgentTarget[] =>
    matchSessions(sessions, activeScope, query);

  const refreshMention = (text: string, caret: number) => {
    const found = scanMention(text, caret);
    if (!found) {
      setMention(null);
      return;
    }
    const hits = mentionHits(found.query);
    setMention(hits.length > 0 ? { start: found.start, query: found.query, active: 0 } : null);
  };

  /** 选中某一项：把 `@xxx` 从输入里吃掉，并把这台机器加进作用范围 */
  const pickMention = (target: AgentTarget) => {
    const m = mention;
    if (!m) return;
    const el = taRef.current;
    const caret = el?.selectionStart ?? input.length;
    const next = input.slice(0, m.start) + input.slice(caret).replace(/^\s*/, "");
    setInput(next);
    setMention(null);
    setScoped([...new Set([...activeScope, target.id])]);
    // 敲 `@` 就是在点机器，那这一句自然是「执行」而不是「问答」
    setMode("act");
    requestAnimationFrame(() => {
      el?.focus();
      const at = m.start;
      el?.setSelectionRange(at, at);
    });
  };

  /** 本轮要交给后端的靶机。 */
  const targetsForRun = (): AgentTarget[] => {
    // 标签可能已经关了而 scoped 还留着它的 id：过滤掉，
    // 否则后端回一句「会话不存在」，用户完全不知道该去修什么。
    const alive = new Map(sessions.map((s) => [s.id, s.label]));
    return activeScope
      .filter((id) => id === sessionId || alive.has(id))
      .map((id) => ({ id, label: alive.get(id) ?? "当前会话" }));
  };

  /** 纯问答：流式，答案走 token/done/error。 */
  const ask = async (q: string) => {
    const sid = sessionId;
    startRun(sid, q, true);
    try {
      // resolve 只代表流已启动；答案走 token/done/error 事件收
      await api.agentAsk(sid, q);
    } catch (e) {
      patch(sid, (c) => withError(c, toFriendlyError(e).message));
    }
  };

  /**
   * Agentic：这一条会真的在终端里跑命令。
   *
   * `await` 到的是**整轮跑完**（不是「已启动」）：后端把「思考 / 执行 / 读输出」
   * 串成一条时间线跑到底才返回。模型那边的失败后端已经发过 error 事件了，
   * 这里的 catch 只兜 IPC / 网络级的意外；finally 负责无论如何把 busy 摘掉 ——
   * 撞到步数上限时不会有结论 step，少了它界面就会永远转圈。
   */
  const runAgentic = async (q: string) => {
    const sid = sessionId;
    startRun(sid, q, false);
    try {
      await api.agentRun({ sessionId: sid, question: q, targets: targetsForRun() });
    } catch (e) {
      patch(sid, (c) => withError(c, toFriendlyError(e).message));
    } finally {
      patch(sid, (c) => (c.busy ? finishWithoutAnswer(c) : c));
    }
  };

  const cancel = async () => {
    if (!sessionId) return;
    try {
      // 幂等：流式问答后端会补发一条 reason=cancelled 的 done，busy 由那条事件解除
      await (mode === "act" ? api.agentRunCancel(sessionId) : api.agentCancel(sessionId));
    } catch (e) {
      setErr(`取消失败：${toFriendlyError(e).message}`);
    }
  };

  /** 用户在审批卡片上的选择。后端收不到（超时）也照样把卡片置成终态。 */
  const decide = async (req: AgentApproveRequest, approved: boolean) => {
    try {
      await api.agentApprove(req.requestId, approved);
    } catch (e) {
      setErr(`提交审批结果失败：${toFriendlyError(e).message}`);
      return;
    }
    patch(sessionId, (c) => withDecision(c, req.requestId, approved));
  };

  // ——— 状态栏动作 ———

  const toggleAttach = async () => {
    if (!sessionId) return;
    const next = !attached;
    try {
      await api.agentSetSession(sessionId, next);
      setAttached(next);
      setErr(null);
      void refreshStats();
    } catch (e) {
      setErr(`${next ? "开启" : "关闭"}旁路失败：${toFriendlyError(e).message}`);
    }
  };

  const clearMemory = async () => {
    if (!sessionId) return;
    try {
      await api.agentClear(sessionId);
      setErr(null);
      void refreshStats();
    } catch (e) {
      setErr(`清空失败：${toFriendlyError(e).message}`);
    }
  };

  /**
   * 把命令写进终端。**这是全组件唯一会碰到 pty 的出口** ——
   * 调用链上必然已经经过：用户点「插入到终端」→ 确认框里点「插入」。
   * 此处刻意不加换行：只做插入，回车执行留给用户在终端里自己按。
   */
  const insertCommand = async (cmd: string) => {
    if (!sessionId) return;
    try {
      await onSendToTerminal(sessionId, cmd);
      setErr(null);
    } catch (e) {
      setErr(`写入终端失败：${toFriendlyError(e).message}`);
    }
  };

  const onConfirm = (ok: boolean) => {
    const p = pending;
    setPending(null);
    if (!ok || !p) return;
    if (p.kind === "clear") void clearMemory();
    else void insertCommand(p.cmd);
  };

  if (!visible || !sessionId) return null;

  return (
    <div className="ap-root">
      <div className="ap-head">
        <span className="ap-title">Nexus AI Agent</span>
        {onFloatOut && (
          <button className="ap-x ap-float" onClick={onFloatOut} title="浮出为独立窗口">
            ⧉
          </button>
        )}
        <button className="ap-x" onClick={onClose} title="关闭面板">
          ✕
        </button>
      </div>

      <div className="ap-status">
        <button
          className={`ap-toggle${attached ? " on" : ""}`}
          onClick={() => void toggleAttach()}
          title="开关本会话的终端输出旁路"
        >
          旁路 {attached ? "开启" : "关闭"}
        </button>
        <span className="ap-stat">
          {stats ? `已缓存 ${formatBytes(stats.bytes)}` : "无缓存"}
        </span>
        {stats?.truncated && (
          <span
            className="ap-warn"
            title={`早期内容已丢弃（累计 ${formatBytes(stats.dropped)}）`}
          >
            早期内容已丢弃
          </span>
        )}
        <button
          className="ap-mini"
          onClick={() => setPending({ kind: "clear" })}
          title="丢弃该会话已缓存的终端输出"
        >
          清空记忆
        </button>
      </div>

      {err && <div className="ap-err">{err}</div>}

      {ready.ok ? (
        <>
          <div className="ap-body" ref={listRef} onScroll={onListScroll}>
            {messages.length === 0 && (
              <div className="ap-empty">
                <p>助手能看到本会话终端的最近输出，可以直接问「刚才那条命令为什么报错」。</p>
                <p>
                  {mode === "act"
                    ? "「执行」模式下它会在你勾选的会话里真的跑命令；危险操作会先问过你。"
                    : "回答里的命令会变成建议卡片，需要你确认后才会写进终端。"}
                </p>
              </div>
            )}
            {messages.map((m) => {
              // 过程卡片不是对话泡：整幅宽度更能排得下命令与输出
              if (m.step) return <StepCard key={m.id} step={m.step} />;
              if (m.approve) {
                return (
                  <ApproveCard
                    key={m.id}
                    req={m.approve}
                    result={m.approveResult}
                    busy={busy}
                    onDecide={(ok) => void decide(m.approve!, ok)}
                  />
                );
              }
              return (
                <div key={m.id} className={`ap-msg ${m.role}`}>
                  <div className={`ap-bubble ${m.role}${m.error ? " error" : ""}`}>
                    {splitSegments(m.text).map((s, i) =>
                      s.kind === "text" ? (
                        <div className="ap-text" key={i}>
                          {s.text}
                        </div>
                      ) : m.streaming ? (
                        <pre className="ap-code" key={i}>
                          {s.text}
                        </pre>
                      ) : (
                        <SuggestionCard
                          key={i}
                          lang={s.lang}
                          cmd={s.text}
                          onInsert={(cmd) => setPending({ kind: "cmd", cmd })}
                        />
                      )
                    )}
                    {m.streaming && (
                      <span className="ap-typing" title="正在回答">
                        <i />
                        <i />
                        <i />
                      </span>
                    )}
                  </div>
                </div>
              );
            })}
            {/* Agentic 没有占位泡，过程中的「还在跑」要靠这一行说出来 */}
            {busy && mode === "act" && (
              <div className="ap-running">
                <span className="ap-typing">
                  <i />
                  <i />
                  <i />
                </span>
                正在执行…
              </div>
            )}
            {conv.interrupted && (
              <div className="ap-note">
                上次离开该会话时回答还没结束，上面的内容可能不完整。
              </div>
            )}
          </div>

          {/* 「执行」模式的作用范围。放在输入框上方而不是藏在卡片里：
              勾选了哪些机器，是下一次发送前**必须**看得见的东西。 */}
          {mode === "act" && (
            <div className="ap-scope">
              <span className="ap-scope-label">作用范围</span>
              <div className="ap-chips">
                {activeScope.map((id) => {
                  const label = sessions.find((s) => s.id === id)?.label ?? "当前会话";
                  return (
                    <button
                      key={id}
                      className="ap-chip on"
                      disabled={busy}
                      title={`点击移出作用范围 · ${id}`}
                      onClick={() => {
                        const next = activeScope.filter((x) => x !== id);
                        // 空了就退回「仅当前会话」—— 一个目标都不给，模型会在个 nullptr 上干活
                        setScoped(next.length > 0 ? next : [sessionId]);
                      }}
                    >
                      {label} ✕
                    </button>
                  );
                })}
                {sessions
                  .filter((s) => !activeScope.includes(s.id))
                  .map((s) => (
                    <button
                      key={s.id}
                      className="ap-chip"
                      disabled={busy}
                      title={`加入作用范围 · ${s.id}`}
                      onClick={() => setScoped([...activeScope, s.id])}
                    >
                      + {s.label}
                    </button>
                  ))}
                {activeScope.length > 1 && (
                  <button className="ap-mini" disabled={busy} onClick={() => setScoped(null)}>
                    重置
                  </button>
                )}
              </div>
            </div>
          )}

          <div className="ap-input">
            <div className="ap-modes">
              <button
                className={`ap-mode${mode === "ask" ? " on" : ""}`}
                disabled={busy}
                onClick={() => setMode("ask")}
                title="只回答不给建议以外的动作：答案里的命令需要你自己插入终端"
              >
                问答
              </button>
              <button
                className={`ap-mode${mode === "act" ? " on" : ""}`}
                disabled={busy}
                onClick={() => setMode("act")}
                title={
                  cfg.execMode === "agentic"
                    ? "全自动：它会直接在终端里执行，不再逐条问你"
                    : "人在回路：危险命令会先弹卡片等你点头"
                }
              >
                执行
                <em>{cfg.execMode === "agentic" ? "自动" : "需确认"}</em>
              </button>
            </div>
            <textarea
              ref={taRef}
              className="ap-ta"
              rows={3}
              value={input}
              disabled={busy}
              placeholder={
                busy
                  ? mode === "act"
                    ? "正在执行…"
                    : "正在回答…"
                  : mode === "act"
                    ? "描述你要它完成的事，Enter 开始（打 @ 可以快速指定某台机器）"
                    : "问点什么，Enter 发送"
              }
              onChange={(e) => {
                setInput(e.target.value);
                refreshMention(e.target.value, e.target.selectionStart ?? 0);
              }}
              // 光标被方向键挪动后片段也会变，keyup / select 都要重新扫一次
              onKeyUp={(e) => refreshMention(input, e.currentTarget.selectionStart ?? 0)}
              onClick={(e) => refreshMention(input, e.currentTarget.selectionStart ?? 0)}
              onBlur={() => setMention(null)}
              onKeyDown={(e) => {
                const m = mention;
                if (m && !e.nativeEvent.isComposing) {
                  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                    e.preventDefault();
                    setMention({
                      ...m,
                      active:
                        (m.active + (e.key === "ArrowDown" ? 1 : -1) + mentionHits(m.query).length) %
                        mentionHits(m.query).length,
                    });
                    return;
                  }
                  // Tab / Enter 选中当前项：Enter 在这里必须让位，
                  // 否则「打 @ 选机器」永远撞上「Enter 发送」
                  if (e.key === "Tab" || e.key === "Enter") {
                    e.preventDefault();
                    pickMention(mentionHits(m.query)[Math.min(m.active, mentionHits(m.query).length - 1)]);
                    return;
                  }
                  if (e.key === "Escape") {
                    e.preventDefault();
                    setMention(null);
                    return;
                  }
                }
                // isComposing：中文输入法选词时的回车不能被当成发送
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
            {mention && (
              <div className="ap-mention">
                {mentionHits(mention.query).map((s, i) => (
                  <button
                    key={s.id}
                    className={`ap-mention-item${i === mention.active ? " on" : ""}`}
                    // mousedown 而不是 click：click 之前 textarea 已经失焦、联想已被收起
                    onMouseDown={(e) => {
                      e.preventDefault();
                      pickMention(s);
                    }}
                  >
                    <span className="ap-mention-name">{s.label}</span>
                    {activeScope.includes(s.id) && <span className="ap-mention-tag">已在范围</span>}
                  </button>
                ))}
              </div>
            )}
            <div className="ap-input-row">
              <span className="ap-input-hint">Enter 发送 · Shift+Enter 换行</span>
              {busy ? (
                <button className="btn danger" onClick={() => void cancel()}>
                  停止
                </button>
              ) : (
                <button
                  className="btn primary"
                  onClick={() => void send()}
                  disabled={!input.trim()}
                >
                  发送
                </button>
              )}
            </div>
          </div>
        </>
      ) : (
        <div className="ap-setup">
          <div className="ap-setup-title">Nexus AI Agent 还没就绪</div>
          <p className="ap-setup-text">{ready.reason ?? "请先完成 Nexus AI Agent 配置。"}</p>
          <p className="ap-setup-text">
            配置好后，本会话的终端输出会旁路复制一份给模型：人继续在终端里干活，
            助手则带着这份上下文回答你的问题。开关与清空都在这个面板里，随时可关。
          </p>
          <button className="btn primary" onClick={onOpenSettings}>
            去设置
          </button>
        </div>
      )}

      {pending && (
        <ConfirmDialog
          opts={
            pending.kind === "clear"
              ? CLEAR_OPTS
              : {
                  title: "确认插入到终端",
                  message: "以下内容将写入当前会话的终端（只插入，不会自动回车执行）：",
                  detail: pending.cmd,
                  okText: "插入",
                  danger: true,
                }
          }
          onResult={onConfirm}
        />
      )}
    </div>
  );
}

function SuggestionCard({
  lang,
  cmd,
  onInsert,
}: {
  lang: string;
  cmd: string;
  onInsert: (cmd: string) => void;
}) {
  return (
    <div className="ap-sug">
      <div className="ap-sug-head">
        <span className="ap-sug-tag">建议命令{lang ? ` · ${lang}` : ""}</span>
        {/* 安全底线：这里只弹确认框，绝不直接写 pty */}
        <button className="ap-sug-btn" onClick={() => onInsert(cmd)}>
          插入到终端
        </button>
      </div>
      <pre className="ap-code">{cmd}</pre>
    </div>
  );
}

/**
 * 一次执行的卡片：命令 + 结果。
 *
 * 「在哪台机器上跑的」必须写得清清楚楚 —— Agent 是全局的、可以同时在好几个会话上下令，
 * 只显示命令的话，用户根本分不清这条输出来自哪台机器，那才是最危险的一种糊涂。
 */
function StepCard({ step }: { step: AgentStep }) {
  if (step.kind === "note") {
    return <div className="ap-note">{step.text ?? ""}</div>;
  }
  if (step.kind === "skip") {
    return (
      <div className="ap-step danger">
        <div className="ap-step-head">
          <span className="ap-step-tag">未执行</span>
          <span className="ap-step-where">{step.targetLabel ?? "未知会话"}</span>
        </div>
        <pre className="ap-code">{step.command ?? ""}</pre>
        {step.reasons?.length ? (
          <div className="ap-step-why">判定理由：{step.reasons.join("；")}</div>
        ) : null}
      </div>
    );
  }

  const done = step.kind === "result";
  const okCode = done && step.exitCode === 0;
  const badCode = done && step.exitCode !== 0 && step.exitCode != null;
  const lines = (step.output ?? "").split("\n").length;
  return (
    <div className={`ap-step${okCode ? "" : badCode ? " bad" : " pending"}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{done ? (okCode ? "已完成" : "已完成 · 非 0") : "执行中"}</span>
        <span className="ap-step-where" title={step.target}>
          {step.targetLabel ?? "未知会话"}
        </span>
        {done && step.exitCode != null && (
          <span className={`ap-step-code${okCode ? "" : " bad"}`}>{step.exitCode}</span>
        )}
      </div>
      <pre className="ap-code">{step.command ?? ""}</pre>
      {done ? (
        (step.output ?? "").trim() ? (
          // 长输出折叠：一屏七八行的核对性输出最常见，全展开会把对话挤没
          lines > 10 ? (
            <details className="ap-out">
              <summary>输出 {lines} 行</summary>
              <pre className="ap-code">{step.output}</pre>
            </details>
          ) : (
            <pre className="ap-code ap-out-plain">{step.output}</pre>
          )
        ) : (
          <div className="ap-step-why">（该命令没有输出）</div>
        )
      ) : (
        <div className="ap-running">
          <span className="ap-typing">
            <i />
            <i />
            <i />
          </span>
          等待输出…
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
  const settled = result !== undefined;
  return (
    <div className={`ap-approve${settled ? (result ? " ok" : " no") : ""}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">需要批准</span>
        <span className="ap-step-where" title={req.target}>
          {req.targetLabel || "未知会话"}
        </span>
      </div>
      <pre className="ap-code">{req.command}</pre>
      {req.reasons.length > 0 && (
        <div className="ap-step-why">风险提示：{req.reasons.join("；")}</div>
      )}
      {settled ? (
        <div className="ap-approve-done">{result ? "已允许执行" : "已拒绝 / 已超时"}</div>
      ) : (
        <div className="ap-approve-row">
          <button className="btn primary" disabled={!busy} onClick={() => onDecide(true)}>
            允许执行
          </button>
          <button className="btn" disabled={!busy} onClick={() => onDecide(false)}>
            拒绝
          </button>
        </div>
      )}
    </div>
  );
}
