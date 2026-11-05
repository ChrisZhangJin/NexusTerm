import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";
import SkillManager from "./SkillManager";
import Timeline from "./Timeline";
import {
  api,
  onAgentApprove,
  onAgentControl,
  onAgentDone,
  onAgentError,
  onAgentSecret,
  onAgentStep,
  onAgentToken,
} from "../api";
import { isAgentReady, useAgentConfig } from "../lib/agent";
import { toFriendlyError } from "../lib/errors";
import {
  applyControl,
  drivingSessions,
  pausedSessions,
  type ControlState,
} from "../lib/control";
import { enterVerdict, ImeGuard, isImeKey, keyInfoFrom } from "../lib/ime";
import { matchSessions, scanMention } from "../lib/mention";
import { useAgentHistory } from "../lib/useAgentHistory";
import type {
  AgentApproveRequest,
  AgentDoneReason,
  AgentMessage,
  AgentSecretRequest,
  AgentStep,
  AgentTarget,
  SkillInput,
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
   * 当前所有可操作的会话（**作为可指定的靶机候选**）。
   * 由 App 从打开的标签算出来 —— 面板自己不认识标签树。
   */
  sessions?: AgentTarget[];
  /**
   * 抓当前会话的终端快照（scrollback + 视口）。
   *
   * 每次发送都抓一次，作为「当前会话就是上下文」的**首选来源**：
   * 旁路缓冲只记开旁路之后的字节，用户刚连上就问问题时它往往是空的。
   */
  onGetConsole?: (sessionId: string) => { text: string; alt: boolean } | null;
}

/** 单个会话的对话状态。按 sessionId 分桶，切会话时彼此完全隔离。 */
interface Conversation {
  messages: AgentMessage[];
  /** 这个会话正在等一次回答的流结束 */
  busy: boolean;
  /** 上一次是被「切走会话 / 收起面板」打断的 —— 半截答案不可信，要提示 */
  interrupted: boolean;
}

/** 「停止时同时发 Ctrl-C」的本机偏好（"1" = 开，默认关） */
const LS_STOP_CTRL_C = "nx.agent.stopCtrlC";

const EMPTY_CONV: Conversation ={ messages: [], busy: false, interrupted: false };

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
 * 追加一个过程步骤。`result` 会回填到它对应的那条 `exec` 上合成**一张**卡片：
 * 拆成两张会把对话拉长一倍，也看不出哪个结果属于哪条命令。
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
        msgs[i] = {
          ...m,
          step: { ...s, kind: "result", output: step.output, promptKind: step.promptKind ?? s.promptKind },
        };
        return { ...c, messages: msgs };
      }
    }
  }
  // 密码通道：收尾步骤（带 secretStatus）并回同一目标、同一理由的进行中卡片。
  // 注意不能用 output 判断进行中——收尾时可能没有输出尾部（undefined）。
  if (step.kind === "secret" && step.secretStatus !== undefined) {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === "secret" &&
        s.secretStatus === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step };
        return { ...c, messages: msgs };
      }
    }
  }
  // 「手和眼」三类卡（按键 / 等待 / 读屏）：收尾（output 有值）合并回同一目标、同一内容的进行中卡片，
  // 与 exec → result 同理，别把一次动作拆成两张卡。
  if (
    (step.kind === "keys" || step.kind === "wait" || step.kind === "screen") &&
    step.output !== undefined
  ) {
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      const s = m.step;
      if (
        s &&
        s.kind === step.kind &&
        s.output === undefined &&
        s.target === step.target &&
        s.command === step.command
      ) {
        msgs[i] = { ...m, step };
        return { ...c, messages: msgs };
      }
    }
  }
  // 纯展示用的「还在等」心跳：合并进对应的 exec 卡片，不单独起一张卡；
  // 对应的卡片已经收尾（或从没出现过，比如面板是后打开的）就直接丢弃，
  // 不能退化成下面的"推一条新消息"，那样会在对话里冒出一张没头没尾的孤卡。
  if (step.kind === "waiting") {
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
        msgs[i] = { ...m, step: { ...s, elapsedSecs: step.elapsedSecs } };
        return { ...c, messages: msgs };
      }
    }
    return c;
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

/** 追加一张密码卡片（模型请求用户输入密码）。卡片里的输入值只在组件本地 state，不进会话。 */
function withSecret(c: Conversation, req: AgentSecretRequest): Conversation {
  return {
    ...c,
    messages: [...c.messages, { id: uid(), role: "assistant", text: "", ts: Date.now(), secret: req }],
  };
}

/** 记录密码卡片的结局（卡片据此变成终态，输入框消失） */
function withSecretResult(
  c: Conversation,
  requestId: string,
  result: "submitted" | "skipped" | "expired"
): Conversation {
  return {
    ...c,
    messages: c.messages.map((m) =>
      m.secret?.requestId === requestId ? { ...m, secretResult: result } : m
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
 * 一轮结束却**没等到**结论时兜的那一步（多半撞到步数上限）。判据是「busy 仍为
 * true」：有结论的话 kind=answer 走 `withConclusion` 早把它置回 false 了。
 * 不补这一步界面会永远转圈。
 */
function finishWithoutAnswer(c: Conversation): Conversation {
  return withConclusion(c, NO_CONCLUSION);
}

/** 清空对话的确认框文案（真删已保存的对话与运行摘要） */
const CLEAR_CONV_OPTS: ConfirmOptions = {
  title: "清空对话",
  message: "将删除本面板的全部对话记录与已保存的运行摘要（含重启后恢复用的那份），助手不再记得之前做过什么。此操作不可恢复。",
  okText: "清空",
  danger: true,
};

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
  onGetConsole,
}: AgentPanelProps) {
  const cfg = useAgentConfig();
  const ready = isAgentReady(cfg);
  // 对话 / 时间线（2.5）视图切换。时间线只在选中时挂载（才订阅事件）；对话区只是被 CSS 藏起来，状态不丢
  const [view, setView] = useState<"chat" | "timeline">("chat");
  // 技能管理器（3.4）。`draft` 给了就直接弹出预填的「新建技能」编辑器（录制器 / LLM 草稿 / 宏迁移用）
  const [skillUi, setSkillUi] = useState<{ draft?: SkillInput } | null>(null);

  /**
   * Agent 面向全局：整个应用只有**一份**对话。
   * 切标签只换「当前会话」这个上下文，不换聊天记录 —— 助手是同一个助手。
   */
  const [conv, setConv] = useState<Conversation>(EMPTY_CONV);
  const [input, setInput] = useState("");
  /** 输入框。`@` 联想要在选中一项之后把光标放回原处，所以得拿得到它 */
  const taRef = useRef<HTMLTextAreaElement>(null);
  /**
   * 上下键翻历史输入。-1 = 没在翻；0 = 最近一条，1 = 倒数第二条，依此类推。
   * `draftRef` 存的是刚开始翻之前那行还没发出去的草稿，翻到底再按下键要能回得去，
   * 不能让一次手滑把正在写的东西弄丢。
   */
  const historyIndexRef = useRef(-1);
  const draftRef = useRef("");
  /** 翻历史之后光标要落在新文本末尾，而不是停在原来那个（可能已经不存在的）位置 */
  const recallCaretToEndRef = useRef(false);
  /**
   * 输入法状态。macOS(WKWebView) 上 `isComposing` 不可信 —— Safari 把
   * compositionend 派发在确认回车的 keydown 之前，详见 `lib/ime.ts`。
   */
  const imeRef = useRef<ImeGuard>(new ImeGuard());
  useEffect(() => {
    if (!recallCaretToEndRef.current) return;
    recallCaretToEndRef.current = false;
    const ta = taRef.current;
    if (ta) ta.selectionStart = ta.selectionEnd = input.length;
  }, [input]);
  const [err, setErr] = useState<string | null>(null);
  /**
   * 这一轮实际发起的会话。事件订阅挂在它上面，而**不跟随 sessionId** ——
   * 用户切到别的标签时，正在跑的那条流仍要收得到，否则答案会断在半截。
   */
  const [runSession, setRunSession] = useState<string | null>(null);
  const [pending, setPending] = useState<
    { kind: "clear" } | { kind: "clearConv" } | { kind: "cmd"; cmd: string } | null
  >(null);

  /**
   * Agentic 模式的靶机。为空表示「只当前会话」（后端会退回 [sessionId]）。
   * 用 Set 之外的数组是为了让渲染顺序与 tags 一致 —— 勾选拉直这个 useMemo 很绕。
   */
  const [scoped, setScoped] = useState<string[] | null>(null);

  /**
   * 输入框里的 `@` 联想。
   *
   * `@` 是比 chips 更快的一条选机器的路：写「把 @host2 的 nginx 重启一下」时
   * 顺手就定下来了。选完 `@xxx` 会被吃掉 —— 它只是记号，真正的作用范围（也是
   * 发给模型的那份）在 chips 里。
   */
  const [mention, setMention] = useState<{
    /** `@` 在输入里的下标 */
    start: number;
    query: string;
    active: number;
  } | null>(null);

  const messages = conv.messages;
  const busy = conv.busy;
  /** 本轮的作用范围：scoped 为空时就是「仅当前会话」 */
  const activeScope = scoped && scoped.length > 0 ? scoped : [sessionId];

  const patch = useCallback((fn: (c: Conversation) => Conversation) => {
    setConv((prev) => fn(prev));
  }, []);

  // ——— 对话持久化：重载 / 重启后恢复，变化时防抖写盘 ———
  const history = useAgentHistory(
    messages,
    useCallback(
      (fn: (prev: AgentMessage[]) => AgentMessage[]) =>
        setConv((c) => ({ ...c, messages: fn(c.messages) })),
      []
    )
  );

  // ——— 终端控制权（人 / Agent / 暂停） ———

  /** 本轮涉及的各会话的控制权状态（只留活跃的；userDriving 的会被删掉） */
  const [control, setControl] = useState<Record<string, ControlState>>({});
  const pausedIds = pausedSessions(control);
  const drivingIds = drivingSessions(control);
  /** 「停止」时是否同时给被驾驶的终端发 Ctrl-C。默认关，选择记在本机 */
  const [stopCtrlC, setStopCtrlC] = useState<boolean>(
    () => localStorage.getItem(LS_STOP_CTRL_C) === "1"
  );
  const toggleStopCtrlC = (on: boolean) => {
    setStopCtrlC(on);
    localStorage.setItem(LS_STOP_CTRL_C, on ? "1" : "0");
  };

  // ——— 会话切换 / 收起时的收尾 ———

  /**
   * 切走会话或收起面板都会 unlisten，之后流跑完我们也收不到。把消息从 streaming
   * 摘下并标记 interrupted，否则切回来是一个永远转圈的「正在回答」。
   */
  const markInterrupted = useCallback(() => {
    setConv((c) => {
      const streaming = c.messages.some((m) => m.streaming);
      if (!c.busy && !streaming) return c;
      return {
        busy: false,
        interrupted: streaming,
        messages: streaming
          ? c.messages.map((m) => (m.streaming ? { ...m, streaming: false } : m))
          : c.messages,
      };
    });
  }, []);

  // 面板收起时事件订阅会断掉，把还在转圈的那条消息摘下来。
  // **切换会话不触发** —— 对话是全局的，流不该因为切标签而中断。
  useEffect(() => {
    if (!visible) markInterrupted();
  }, [visible, markInterrupted]);

  // ——— 事件订阅：按 sessionId 订阅，切会话即整体换掉 ———

  useEffect(() => {
    if (!visible || !runSession) return;
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];

    void (async () => {
      try {
        const [offToken, offDone, offError, offStep, offApprove, offSecret, offControl] = await Promise.all([
          onAgentToken(runSession, (chunk) => {
            if (!disposed) patch((c) => withToken(c, chunk));
          }),
          onAgentDone(runSession, (p) => {
            if (!disposed) patch((c) => withDone(c, p.reason));
          }),
          onAgentError(runSession, (msg) => {
            if (!disposed) patch((c) => withError(c, msg));
          }),
          onAgentStep(runSession, (step) => {
            if (disposed) return;
            // kind=answer 是这一轮的总结，作为一条完整的助手消息落地并收掉 busy；
            // 其余都当作过程卡片逐条追加。
            patch((c) =>
              step.kind === "answer"
                ? withConclusion(c, step.text ?? "")
                : withStep(c, step)
            );
          }),
          onAgentApprove(runSession, (req) => {
            if (!disposed) patch((c) => withApprove(c, req));
          }),
          onAgentSecret(runSession, (req) => {
            if (!disposed) patch((c) => withSecret(c, req));
          }),
          onAgentControl(runSession, (p) => {
            if (!disposed) setControl((s) => applyControl(s, p));
          }),
        ]);
        // 竞态：listen 是异步的，等它 resolve 时这一轮可能已经结束了 —— 这时要立刻退订
        if (disposed) {
          offToken();
          offDone();
          offError();
          offStep();
          offApprove();
          offSecret();
          offControl();
          return;
        }
        unlisteners.push(offToken, offDone, offError, offStep, offApprove, offSecret, offControl);
        // 订阅之前就已经在跑的一轮（面板刚打开 / 重新挂载）：对一次账，补上发起会话的状态
        api
          .agentControlGet(runSession)
          .then((st) => {
            if (!disposed) setControl((s) => applyControl(s, { session: runSession, state: st }));
          })
          .catch(() => {});
      } catch {
        // 订阅失败：提问时会在 agentRun 上抛出真错误，这里不必再弹一次
      }
    })();

    return () => {
      disposed = true;
      unlisteners.forEach((f) => f());
      // 订阅断了就收不到后续变化，留着旧状态只会让横幅卡在屏幕上
      setControl({});
    };
  }, [runSession, visible, patch]);

  // ——— 清空记忆 ———

  const clearMemory = async () => {
    if (!sessionId) return;
    try {
      await api.agentClear(sessionId);
      setErr(null);
    } catch (e) {
      setErr(`清空失败：${toFriendlyError(e).message}`);
    }
  };

  /** 「清空对话」：先真删盘上的数据，成功后再清界面（失败就保持原样并报错）。 */
  const clearConversation = async () => {
    try {
      await history.clear();
      setConv((c) => ({ ...c, messages: [], interrupted: false }));
      setErr(null);
    } catch (e) {
      setErr(`清空对话失败：${toFriendlyError(e).message}`);
    }
  };

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
      patch((c) => withError(c, ready.reason ?? "Nexus AI Agent 尚未配置完成"));
      return;
    }
    // 只有一个 Agent：不再让用户在「问答 / 执行」之间选，它自己决定回答还是动手。
    await runAgentic(q);
  };

  /**
   * 起手：清空输入框、贴底、把用户消息挂上去。
   *
   * 不再提前立一个空的助手泡：这一轮的过程是一张张 step 卡片，
   * 结论等它自己给出时才落地 —— 先立一个空泡只会让对话里多一条空气泡。
   */
  const startRun = (q: string) => {
    setInput("");
    historyIndexRef.current = -1;
    draftRef.current = "";
    setErr(null);
    stickRef.current = true;
    patch((c) => ({
      busy: true,
      interrupted: false,
      messages: [...c.messages, { id: uid(), role: "user" as const, text: q, ts: Date.now() }],
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
    // `@` 当前会话本身没有意义 —— 它本来就是默认上下文。
    // 只有点到**别的**会话时，才把它加进作用范围（这一步不需要用户再选模式）。
    if (target.id !== sessionId) {
      setScoped([...new Set([...activeScope, target.id])]);
    }
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

  /** Agentic：这一条会真的在终端里跑命令。
   *
   * `await` 到的是**整轮跑完**而不是「已启动」。模型侧的失败后端已发过 error
   * 事件，这里的 catch 只兜 IPC 级意外；finally 无论如何摘掉 busy —— 撞步数
   * 上限时不会有结论 step，少了它界面永远转圈。
   */
  const runAgentic = async (q: string) => {
    const sid = sessionId;
    // 事件订阅挂到这一轮实际发起的会话上：之后切标签也不会断流
    setRunSession(sid);
    startRun(q);
    // 当前屏幕就是上下文。抓不到（标签已关）时留空，后端会退回旁路缓冲
    const snap = onGetConsole?.(sid) ?? null;
    try {
      await api.agentRun({
        sessionId: sid,
        question: q,
        targets: targetsForRun(),
        consoleText: snap?.text,
        consoleAlt: snap?.alt,
      });
    } catch (e) {
      patch((c) => withError(c, toFriendlyError(e).message));
    } finally {
      patch((c) => (c.busy ? finishWithoutAnswer(c) : c));
    }
  };

  const cancel = async () => {
    const sid = runSession ?? sessionId;
    if (!sid) return;
    try {
      // 幂等：没有在跑时后端什么都不做
      await api.agentRunCancel(sid, stopCtrlC);
    } catch (e) {
      setErr(`取消失败：${toFriendlyError(e).message}`);
    }
  };

  /** 「接管」：让 Agent 立刻让路（等同于自己敲了键）。对所有正被驾驶的会话生效。 */
  const takeover = () => {
    for (const id of drivingIds) void api.agentControlTakeover(id).catch(() => {});
  };
  /** 「交还」：Paused → Agent 继续。 */
  const handback = () => {
    for (const id of pausedIds) void api.agentControlHandback(id).catch(() => {});
  };

  /** 用户在审批卡片上的选择。后端收不到（超时）也照样把卡片置成终态。 */
  const decide = async (req: AgentApproveRequest, approved: boolean) => {
    try {
      await api.agentApprove(req.requestId, approved);
    } catch (e) {
      setErr(`提交审批结果失败：${toFriendlyError(e).message}`);
      return;
    }
    patch((c) => withDecision(c, req.requestId, approved));
  };

  /**
   * 用户在密码卡片上的选择：`value` 给了就是提交密码，`undefined` 是跳过。
   * 值经 `agent_secret_ack` 直达后端写进 pty —— 不进对话 state、不进模型上下文。
   * 返回 `null` 表示已处理（卡片已置成终态），否则返回要在卡片内联显示的错误文案
   * （例如密码含控制字符被后端拒绝：这种情况后端没消耗登记，卡片保持打开可重输）。
   */
  const answerSecret = async (
    req: AgentSecretRequest,
    value?: string
  ): Promise<string | null> => {
    try {
      const accepted = await api.agentSecretAck(req.requestId, value);
      // false：这个请求已经超时 / 被取消 / 重复提交，后端不再等了
      patch((c) =>
        withSecretResult(c, req.requestId, accepted ? (value === undefined ? "skipped" : "submitted") : "expired")
      );
      return null;
    } catch (e) {
      return toFriendlyError(e).message;
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
    else if (p.kind === "clearConv") void clearConversation();
    else void insertCommand(p.cmd);
  };

  if (!visible || !sessionId) return null;

  return (
    <div className={`ap-root${view === "timeline" ? " show-tl" : ""}`}>
      <div className="ap-head">
        <span className="ap-title">Nexus AI Agent</span>
        <div className="ap-tabs" role="tablist">
          <button
            role="tab"
            aria-selected={view === "chat"}
            className={`ap-tab${view === "chat" ? " on" : ""}`}
            onClick={() => setView("chat")}
          >
            对话
          </button>
          <button
            role="tab"
            aria-selected={view === "timeline"}
            className={`ap-tab${view === "timeline" ? " on" : ""}`}
            onClick={() => setView("timeline")}
            title="这个终端里用户与 Agent 各做了什么"
          >
            时间线
          </button>
        </div>
        <button className="ap-skill-btn" onClick={() => setSkillUi({})} title="管理技能（可回放的操作流程）">
          技能
        </button>
        {onFloatOut && (
          <button className="ap-x ap-float" onClick={onFloatOut} title="浮出为独立窗口">
            ⧉
          </button>
        )}
        <button className="ap-x" onClick={onClose} title="关闭面板">
          ✕
        </button>
      </div>

      {view === "timeline" && <Timeline sessionId={sessionId} />}

      <div className="ap-status">
        <span className="ap-stat">
          上下文：当前会话{activeScope.length > 1 ? ` + ${activeScope.length - 1} 个` : ""}
        </span>
        <button
          className="ap-mini"
          onClick={() => setPending({ kind: "clear" })}
          title="丢弃该会话已缓存的终端输出，助手会忘掉之前看到的内容"
        >
          清空记忆
        </button>
        <button
          className="ap-mini"
          disabled={busy}
          onClick={() => setPending({ kind: "clearConv" })}
          title="删除本面板的对话记录与已保存的运行摘要（重启后也不会再恢复）"
        >
          清空对话
        </button>
      </div>

      {err && <div className="ap-err">{err}</div>}

      {ready.ok ? (
        <>
          <div className="ap-body" ref={listRef} onScroll={onListScroll}>
            {messages.length === 0 && (
              <div className="ap-empty">
                <p>助手能看到当前会话的终端内容，可以直接问「刚才那条命令为什么报错」。</p>
                <p>
                  需要时它会在终端里执行命令（危险操作先问你）；打 <code>@</code> 可以指定别的会话。
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
              if (m.secret) {
                return (
                  <SecretCard
                    key={m.id}
                    req={m.secret}
                    result={m.secretResult}
                    busy={busy}
                    onAnswer={(value) => answerSecret(m.secret!, value)}
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
            {busy && (
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

          {/* 控制权横幅：谁在操作终端要一眼看得出。用户一敲键盘 Agent 就自动暂停，
              只有点「交还」它才会继续；「接管」则是不用敲键盘也能让它让路。 */}
          {pausedIds.length > 0 ? (
            <div className="ap-control paused" role="status">
              <span className="ap-control-text">
                你正在操作终端，Agent 已暂停（不会往终端里写入）。
              </span>
              <button className="btn primary" onClick={handback}>
                交还
              </button>
            </div>
          ) : busy && drivingIds.length > 0 ? (
            <div className="ap-control driving" role="status">
              <span className="ap-control-text">
                Agent 正在操作终端。你在终端里一敲键盘它就会自动暂停。
              </span>
              <button className="btn" onClick={takeover}>
                接管
              </button>
            </div>
          ) : null}

          {/* 作用范围。放在输入框上方而不是藏在卡片里：
              勾选了哪些机器，是下一次发送前**必须**看得见的东西。 */}
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

          <div className="ap-input">
            <textarea
              ref={taRef}
              className="ap-ta"
              rows={3}
              value={input}
              disabled={busy}
              placeholder={
                busy
                  ? "正在处理…"
                  : "问点什么，或描述你要它做的事（打 @ 可以指定别的会话）"
              }
              onChange={(e) => {
                setInput(e.target.value);
                refreshMention(e.target.value, e.target.selectionStart ?? 0);
              }}
              // 光标被方向键挪动后片段也会变，keyup / select 都要重新扫一次
              onKeyUp={(e) => refreshMention(input, e.currentTarget.selectionStart ?? 0)}
              onClick={(e) => refreshMention(input, e.currentTarget.selectionStart ?? 0)}
              onCompositionStart={() => imeRef.current.begin()}
              onCompositionEnd={(e) => imeRef.current.finish(e.nativeEvent.timeStamp)}
              onBlur={() => {
                // 组合中被切走时 compositionend 不一定来，留着 true 会把之后所有回车都吞掉
                imeRef.current.reset();
                setMention(null);
              }}
              onKeyDown={(e) => {
                const info = keyInfoFrom(e);
                const guard = imeRef.current;

                // 连上下键一起挡：拼音组合中上下键是在选候选词，联想列表若跟着
                // 移动，一次选词就会顺手改掉「要发给谁」。
                if (isImeKey(info, guard)) return;

                const verdict = enterVerdict(info, guard);
                if (verdict === "ime-after-commit") {
                  // 组合刚结束的兜底窗口。这一下若其实是真回车（只是手快），
                  // 拦掉默认行为至少不会往输入框里多塞一个换行。
                  e.preventDefault();
                  return;
                }

                const m = mention;
                if (m) {
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
                } else if (e.key === "ArrowUp" || e.key === "ArrowDown") {
                  // 联想列表没开着才轮到翻历史，且只在光标处在首/末行时触发——
                  // 多行输入框里，中间行按上下键应该是挪光标，不该被历史打断。
                  const ta = e.currentTarget;
                  const atTop = ta.value.slice(0, ta.selectionStart ?? 0).indexOf("\n") === -1;
                  const atBottom = ta.value.slice(ta.selectionEnd ?? ta.value.length).indexOf("\n") === -1;
                  const history = conv.messages.filter((m) => m.role === "user").map((m) => m.text);
                  if (e.key === "ArrowUp" && atTop && history.length > 0) {
                    e.preventDefault();
                    if (historyIndexRef.current === -1) draftRef.current = input;
                    historyIndexRef.current = Math.min(historyIndexRef.current + 1, history.length - 1);
                    recallCaretToEndRef.current = true;
                    setInput(history[history.length - 1 - historyIndexRef.current]);
                  } else if (e.key === "ArrowDown" && atBottom && historyIndexRef.current !== -1) {
                    e.preventDefault();
                    historyIndexRef.current -= 1;
                    recallCaretToEndRef.current = true;
                    setInput(
                      historyIndexRef.current === -1
                        ? draftRef.current
                        : history[history.length - 1 - historyIndexRef.current]
                    );
                  }
                }
                // 输入法的确认回车已经在上面的 verdict 里排除了
                if (verdict === "send") {
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
                <>
                  <label
                    className="ap-stop-ctrlc"
                    title="停止时同时往正在被驾驶的终端发送 Ctrl-C，打断它正在跑的命令"
                  >
                    <input
                      type="checkbox"
                      checked={stopCtrlC}
                      onChange={(e) => toggleStopCtrlC(e.target.checked)}
                    />
                    同时发 Ctrl-C
                  </label>
                  <button className="btn danger" onClick={() => void cancel()}>
                    停止
                  </button>
                </>
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
            配置好后，助手就能看到你当前会话的终端内容：人继续在终端里干活，
            助手带着这份上下文回答你的问题，需要时也能直接在里面执行命令。
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
              : pending.kind === "clearConv"
              ? CLEAR_CONV_OPTS
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

      {skillUi && <SkillManager initialDraft={skillUi.draft} onClose={() => setSkillUi(null)} />}
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
const PROMPT_LABELS: Record<string, string> = {
  password: "等待密码",
  yes_no: "等待确认",
  pager: "分页器",
  menu: "菜单",
  repl: "REPL",
};

function PromptBadge({ kind }: { kind?: string }) {
  const label = kind ? PROMPT_LABELS[kind] : undefined;
  if (!label) return null;
  return (
    <span className="ap-prompt-badge" title={`终端当前停在：${kind}`}>
      {label}
    </span>
  );
}

/** 密码通道结局 → 卡片上的中文短语 */
const SECRET_STATUS_LABELS: Record<string, string> = {
  provided: "已输入",
  declined: "已跳过",
  timeout: "已超时",
  cancelled: "已取消",
  failed: "写入失败",
};

/**
 * 一次执行的卡片：命令 + 结果。「在哪台机器上跑的」必须写清楚 —— Agent 是全局的，
 * 可以同时在好几个会话上下令，只显示命令的话分不清输出来自哪台机器。
 */
function StepCard({ step }: { step: AgentStep }) {
  if (step.kind === "note") {
    return <div className="ap-note">{step.text ?? ""}</div>;
  }
  // 工具调用卡片：把「调了哪个工具、传了什么参数」原样摆出来，让调用过程看得见。
  // 没有它时，模型想开新标签页会写出 `new-tab` 这种不存在的命令，敲进去只换来
  // command not found —— 看起来就像它答应了却没动手。
  if (step.kind === "tool") {
    return (
      <div className="ap-step tool">
        <div className="ap-step-head">
          <span className="ap-step-tag">工具调用</span>
          <span className="ap-step-tool">{step.toolLabel ?? "工具"}</span>
        </div>
        <div className="ap-step-why">
          <code>{step.tool ?? ""}</code>
          {step.toolArgs && step.toolArgs.trim() !== "{}" ? (
            <span className="ap-tool-args">{prettyToolArgs(step.toolArgs)}</span>
          ) : (
            <span className="ap-tool-args dim">（无参数）</span>
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
            {isTab ? "标签页" : isOpen ? "新会话" : "导记录"}
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
          <span className="ap-step-tag">密码</span>
          {step.targetLabel ? (
            <span className="ap-step-where" title={step.target}>
              {step.targetLabel}
            </span>
          ) : null}
          <PromptBadge kind={step.promptKind} />
        </div>
        {step.command ? <div className="ap-step-why">原因：{step.command}</div> : null}
        <div className="ap-step-why">
          {finished ? (SECRET_STATUS_LABELS[step.secretStatus!] ?? step.secretStatus) : "等待用户输入……"}
          {step.text && finished ? `（${step.text}）` : ""}
        </div>
        {finished && out.trim() ? <pre className="ap-code ap-out-plain">{out}</pre> : null}
      </div>
    );
  }
  // 手和眼：发按键 / 等输出 / 读屏幕。output 为空 = 进行中，有值（含空串）= 已收尾。
  if (step.kind === "keys" || step.kind === "wait" || step.kind === "screen") {
    const tag = step.kind === "keys" ? "按键" : step.kind === "wait" ? "等待" : "读屏";
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
              <summary>{step.kind === "screen" ? "屏幕" : "输出"} {lines} 行</summary>
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
            进行中…
          </div>
        ) : null}
      </div>
    );
  }
  if (step.kind === "skip") {
    return (
      <div className="ap-step danger">
        <div className="ap-step-head">
          <span className="ap-step-tag">未执行</span>
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
          <div className="ap-step-why">原因：{step.reasons.join("；")}</div>
        ) : null}
      </div>
    );
  }

  // 没有退出码这回事了：命令是否成功，由模型读下面这段真实输出自己判断，
  // 跟真人盯着终端看一样。这里只中性地标「执行中 / 已完成」，不再染色判对错。
  const done = step.kind === "result";
  const tag = done ? "已完成" : "执行中";
  const lines = (step.output ?? "").split("\n").length;
  return (
    <div className={`ap-step${done ? "" : " pending"}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{tag}</span>
        <span className="ap-step-where" title={step.target}>
          {step.targetLabel ?? "未知会话"}
        </span>
        {done ? <PromptBadge kind={step.promptKind} /> : null}
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
          等待输出…{step.elapsedSecs ? `（已等待 ${step.elapsedSecs} 秒）` : ""}
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
  // 越界批准与危险命令批准含义不同：前者批的是「动这台不在范围里的会话」，
  // 标题与文案必须直说，别让人以为只是又一次命令风险提示。
  const scope = req.kind === "scope";
  return (
    <div className={`ap-approve${settled ? (result ? " ok" : " no") : ""}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">{scope ? "超出作用范围 · 需要批准" : "需要批准"}</span>
        <span className="ap-step-where" title={req.target}>
          {req.targetLabel || "未知会话"}
        </span>
      </div>
      <pre className="ap-code">{req.command}</pre>
      {req.reasons.length > 0 && (
        <div className="ap-step-why">
          {scope ? "越界原因" : "风险提示"}：{req.reasons.join("；")}
        </div>
      )}
      {settled ? (
        <div className="ap-approve-done">
          {result ? (scope ? "已允许，该会话已并入本轮作用范围" : "已允许执行") : "已拒绝 / 已超时"}
        </div>
      ) : (
        <div className="ap-approve-row">
          <button className="btn primary" disabled={!busy} onClick={() => onDecide(true)}>
            {scope ? "允许越界操作" : "允许执行"}
          </button>
          <button className="btn" disabled={!busy} onClick={() => onDecide(false)}>
            拒绝
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
  const [value, setValue] = useState("");
  const [inlineErr, setInlineErr] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [left, setLeft] = useState(req.timeoutSecs);
  const settled = result !== undefined;

  // 倒计时只是提示：真正的超时在后端，到点后端会放弃等待并告诉模型「超时」
  useEffect(() => {
    if (settled) return;
    const t = window.setInterval(() => setLeft((n) => (n > 0 ? n - 1 : 0)), 1000);
    return () => window.clearInterval(t);
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
  const dead = !busy || left === 0;
  return (
    <div className={`ap-approve ap-secret${settled ? (result === "submitted" ? " ok" : " no") : ""}`}>
      <div className="ap-step-head">
        <span className="ap-step-tag">请求输入密码</span>
        <span className="ap-step-where" title={req.target}>
          {req.targetLabel || "未知会话"}
        </span>
      </div>
      <div className="ap-step-why">原因：{req.reason}</div>
      {settled ? (
        <div className="ap-approve-done">
          {result === "submitted"
            ? "已提交（密码只写入了终端，助手看不到它）"
            : result === "skipped"
              ? "已跳过"
              : "已失效（超时或这一轮已结束）"}
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
              终端看起来并没有停在密码提示上，输入可能会被回显到屏幕。
            </div>
          )}
          <input
            className="ap-secret-input"
            type="password"
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            placeholder="在此输入，回车提交"
            value={value}
            disabled={dead || sending}
            onChange={(e) => setValue(e.target.value)}
          />
          {inlineErr && <div className="ap-secret-err">{inlineErr}</div>}
          <div className="ap-approve-row">
            <button className="btn primary" type="submit" disabled={dead || sending || !value}>
              提交
            </button>
            <button className="btn" type="button" disabled={dead || sending} onClick={() => void submit(undefined)}>
              跳过
            </button>
            <span className="ap-secret-left">{dead ? "已失效" : `${left} 秒后超时`}</span>
          </div>
        </form>
      )}
    </div>
  );
}
