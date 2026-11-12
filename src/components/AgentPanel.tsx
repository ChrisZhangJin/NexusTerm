import { useCallback, useEffect, useRef, useState } from "react";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";
import SkillManager from "./SkillManager";
import { HostMemoryDialog } from "./HostMemory";
import Timeline from "./Timeline";
import StewardPanel, { type StewardRunBridge } from "./StewardPanel";
import HostCard from "./HostCard";
import { RecordingBadge } from "./RecorderBar";
import { RunCard, isRunCard } from "./RunFeed";
import { api } from "../api";
import { isAgentReady, useAgentConfig } from "../lib/agent";
import { pauseBannerText, type StewardBudgetPref } from "../lib/agentBudget";
import {
  finishWithoutAnswer,
  sliceFromMessage,
  uid,
  withError,
} from "../lib/agentRun";
import { toFriendlyError } from "../lib/errors";
import { enterVerdict, ImeGuard, isImeKey, keyInfoFrom } from "../lib/ime";
import { matchSessions, scanMention } from "../lib/mention";
import { useAgentHistory } from "../lib/useAgentHistory";
import { useAgentRun } from "../lib/useAgentRun";
import { useHostRuns } from "../lib/useHostRuns";
import { basePhase, hostDetail, hostPauseKind, isTerminal, needsReattach } from "../lib/host";
import { stewardApi, stewardConsoleIds } from "../lib/steward";
import { setStewardConsoles } from "../lib/termRegistry";
import type { AgentMessage, AgentTarget, Skill, SkillInput } from "../types";

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
  const [view, setView] = useState<"chat" | "timeline" | "steward">("chat");
  // 技能管理器（3.4）。`draft` 给了就直接弹出预填的「新建技能」编辑器（录制器 / LLM 草稿 / 宏迁移用）
  const [skillUi, setSkillUi] = useState<{ draft?: SkillInput } | null>(null);
  const [memOpen, setMemOpen] = useState(false);

  /**
   * Agent 面向全局：整个应用只有**一份**对话（`useAgentRun`，对话与托管两个视图共用，
   * 事件只订阅这一份 —— 审批 / 密码卡片不会被两处各答一次）。
   * 切标签只换「当前会话」这个上下文，不换聊天记录 —— 助手是同一个助手。
   */
  const [err, setErr] = useState<string | null>(null);
  const run = useAgentRun({ sessionId, onError: setErr });
  // 托管任务总表（每个 console 的胶囊 / 卡片）。任务结束时后端已停契约，这里顺手撤掉屏幕帧采集
  const hostRuns = useHostRuns(() => {
    void stewardApi
      .status()
      .then((l) => setStewardConsoles(stewardConsoleIds(l)))
      .catch(() => {});
  });
  /** 托管动作（停止 / 继续 / 接管 / 交还）：失败落一行错误 */
  const hostAct = async (f: () => Promise<unknown>) => {
    try {
      await f();
    } catch (e) {
      setErr(toFriendlyError(e).message);
    }
  };
  /** 当前终端上的托管（对话页也显示它的胶囊与卡片） */
  const hostedHere = hostRuns.forConsole(sessionId);
  // 托管不占对话的 busy；但它的密码卡 / 审批卡在跑着的托管里要能答
  const hostedHereLive = !!hostedHere && !isTerminal(hostedHere.phase);
  const { conv, setConv, patch, runSession, setRunSession, pausedIds, drivingIds } = run;
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
  /** 当前 / 最近一轮是「对话」发起的还是「托管」发起的（只决定托管页要不要认领这一轮） */
  const [runMode, setRunMode] = useState<"chat" | "steward">("chat");
  /** 托管那一轮起手时挂上的目标消息 id：托管页只展示它之后的消息 */
  const [stewardFromId, setStewardFromId] = useState<string | null>(null);
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

  // ——— 对话持久化：重载 / 重启后恢复，变化时防抖写盘 ———
  const history = useAgentHistory(
    messages,
    useCallback(
      (fn: (prev: AgentMessage[]) => AgentMessage[]) =>
        setConv((c) => ({ ...c, messages: fn(c.messages) })),
      []
    )
  );

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
  const startRun = (q: string, mode: "chat" | "steward" = "chat"): string => {
    // 托管是从「托管」页发起的，不该顺手清掉对话输入框里还没发的草稿
    if (mode === "chat") {
      setInput("");
      historyIndexRef.current = -1;
      draftRef.current = "";
    }
    setErr(null);
    stickRef.current = true;
    setRunMode(mode);
    const id = uid();
    patch((c) => ({
      busy: true,
      interrupted: false,
      messages: [...c.messages, { id, role: "user" as const, text: q, ts: Date.now() }],
    }));
    return id;
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
      // cwd：文件工具的相对路径按它补全（前端探测到的终端当前目录）
      .map((id) => ({ id, label: alive.get(id) ?? "当前会话", cwd: sessions.find((s) => s.id === id)?.hint?.cwd ?? null }));
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

  /**
   * 托管页点「开始托管」：后端 `host_start` 先拿终端租约、再签契约，起一个常驻的托管任务后**立即返回**。
   * 之后的步骤仍走这个终端的 `nx://agent/step` 频道（同一份订阅、同一份对话状态），
   * 相位与卡片走 `nx://host/{id}`（`useHostRuns`）。任务只有完成 / 失败 / 停止才结束：
   * 模型问人、你接管、预算用尽都只是挂起，所以这里不再在 `finally` 里停契约。
   */
  const runSteward = async (
    sid: string,
    goal: string,
    budget?: StewardBudgetPref,
    tier?: string,
    yolo?: boolean
  ): Promise<void> => {
    if (run.conv.busy) return;
    setRunSession(sid);
    setStewardFromId(startRun(`托管目标：${goal}`, "steward"));
    const snap = onGetConsole?.(sid) ?? null;
    const label = sessions.find((x) => x.id === sid)?.label ?? "当前会话";
    try {
      await api.hostStart({
        consoleId: sid,
        goal,
        authorityTier: tier,
        yolo,
        label,
        consoleText: snap?.text,
        consoleAlt: snap?.alt,
        budget: budget ? { maxSteps: budget.maxSteps, maxMinutes: budget.maxMinutes } : undefined,
        // 重启后找回这个终端用（SSH 记录 / 标签 / cwd / tmux）
        consoleRef: sessions.find((x) => x.id === sid)?.hint,
      });
      // 契约已签：马上让前端开始为这个终端上报屏幕帧（不等托管页的轮询）
      void reconcileStewardConsoles();
    } catch (e) {
      patch((c) => withError(c, toFriendlyError(e).message));
    } finally {
      // 转圈只代表「正在发起」；托管中的状态看胶囊（HostCard），不占着对话输入框
      patch((c) => (c.busy ? { ...c, busy: false } : c));
    }
  };

  /** 按后端契约表对一次屏幕帧采集门控（托管结束后后端已停契约，这里撤掉采集） */
  const reconcileStewardConsoles = async () => {
    try {
      setStewardConsoles(stewardConsoleIds(await stewardApi.status()));
    } catch {
      /* 对账由 StewardPanel 的轮询负责 */
    }
  };

  /**
   * 技能管理器里点「运行」：关掉管理器，在当前会话上回放。
   * 与 runAgentic 同构（订阅挂到本次会话、busy 转圈、停止按钮同一条取消路径）；
   * 进度 / 审批 / 密码卡片都走这条会话的事件频道，所以必须由面板而不是管理器自己发起。
   */
  const runSkill = async (skill: Skill, params: Record<string, string>) => {
    const sid = sessionId;
    if (!sid || busy) return;
    setSkillUi(null);
    setRunSession(sid);
    startRun(`运行技能「${skill.name}」`);
    const label = sessions.find((x) => x.id === sid)?.label ?? "当前会话";
    try {
      await api.skillRun(skill.id, params, sid, label);
    } catch (e) {
      patch((c) => withError(c, toFriendlyError(e).message));
    } finally {
      patch((c) => (c.busy ? finishWithoutAnswer(c) : c));
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

  /** 托管页看到的这一轮：只含托管那一轮的消息，动作全部转给同一份 `useAgentRun` */
  const stewardOwned = runMode === "steward";
  /** 托管页那一轮对应的托管任务（未结束的优先） */
  const hosted = stewardOwned ? hostRuns.forConsole(runSession) : null;
  const hostedLive = !!hosted && !isTerminal(hosted.phase);
  const hostedBase = hosted ? basePhase(hosted.phase) : null;
  const stewardBridge: StewardRunBridge = {
    session: stewardOwned ? runSession : null,
    active: hostedLive,
    otherBusy: busy && !stewardOwned,
    messages: sliceFromMessage(messages, stewardFromId),
    paused: hostedLive && hostedBase?.kind === "paused",
    pauseText: hosted ? (hostDetail(hosted.phase) ?? undefined) : undefined,
    pauseKind: hosted ? hostPauseKind(hosted.phase) : undefined,
    userPaused: hostedLive && hosted?.phase.kind === "userDriving",
    driving: hostedLive && (hosted?.phase.kind === "running" || hosted?.phase.kind === "waitingHuman"),
    host: hosted,
    hostFor: hostRuns.forConsole,
    awaitingReattach: hostRuns.awaitingReattach,
    stopCtrlC: run.stopCtrlC,
    toggleStopCtrlC: run.toggleStopCtrlC,
    start: runSteward,
    cancel: async () => {
      if (hosted) await hostAct(() => api.hostStop(hosted.contractId, run.stopCtrlC));
    },
    pause: async () => {
      if (hosted) await hostAct(() => api.hostPause(hosted.contractId));
    },
    resume: async (b?: StewardBudgetPref) => {
      if (hosted)
        await hostAct(() =>
          api.hostResume(hosted.contractId, b ? { maxSteps: b.maxSteps, maxMinutes: b.maxMinutes } : undefined)
        );
    },
    takeover: () => {
      if (hosted) void hostAct(() => api.hostTakeover(hosted.contractId));
    },
    handback: () => {
      if (hosted) void hostAct(() => api.hostHandback(hosted.contractId));
    },
    decide: run.decide,
    answerSecret: run.answerSecret,
    ready: ready.ok,
    notReadyReason: ready.reason,
    error: err,
  };

  if (!visible || !sessionId) return null;

  return (
    <div className={`ap-root${view === "timeline" ? " show-tl" : view === "steward" ? " show-steward" : ""}`}>
      <div className="ap-head">
        <span className="ap-title">Nexus AI Agent</span>
        <RecordingBadge sessionId={sessionId} />
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
          <button
            role="tab"
            aria-selected={view === "steward"}
            className={`ap-tab${view === "steward" ? " on" : ""}`}
            onClick={() => setView("steward")}
            title="会话织物：终端屏幕状态与托管契约（P0 只观察）"
          >
            托管
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

      {view === "timeline" && (
        <Timeline
          sessionId={sessionId}
          hostLabel={sessions.find((s) => s.id === sessionId)?.label}
          onOpenSkillDraft={(draft) => setSkillUi({ draft })}
        />
      )}

      {view === "steward" && (
        <StewardPanel sessionId={sessionId} sessions={sessions} run={stewardBridge} />
      )}

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
          onClick={() => setMemOpen(true)}
          title="查看 / 编辑助手对这台主机记下的备忘（提示符、sudo 习惯、常用路径……）"
        >
          主机备忘
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

      {view === "chat" && hostedHere && !isTerminal(hostedHere.phase) && !needsReattach(hostedHere.phase) && (
        <HostCard
          status={hostedHere}
          label={sessions.find((s) => s.id === hostedHere.consoleId)?.label}
          stopCtrlC={run.stopCtrlC}
        />
      )}
      {/* 等重新接驳的托管（重启后 / 终端关了）：不管在哪个 tab 都摆出来，接驳要你确认 */}
      {view === "chat" &&
        hostRuns.awaitingReattach.map((h) => (
          <HostCard key={h.contractId} status={h} label={h.label} tabs={sessions} stopCtrlC={run.stopCtrlC} />
        ))}

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
              if (isRunCard(m)) {
                return (
                  <RunCard
                    key={m.id}
                    message={m}
                    busy={busy || hostedHereLive}
                    onDecide={(req, ok) => void run.decide(req, ok)}
                    onAnswerSecret={run.answerSecret}
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
            {busy && !conv.paused && (
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
          {busy && conv.paused ? (
            <div className="ap-control paused" role="status">
              <span className="ap-control-text">{pauseBannerText(conv.paused)}</span>
              <button className="btn primary" onClick={() => void run.resumeRun()}>
                继续
              </button>
            </div>
          ) : pausedIds.length > 0 ? (
            <div className="ap-control paused" role="status">
              <span className="ap-control-text">
                你正在操作终端，Agent 已暂停（不会往终端里写入）。
              </span>
              <button className="btn primary" onClick={run.handback}>
                交还
              </button>
            </div>
          ) : busy && drivingIds.length > 0 ? (
            <div className="ap-control driving" role="status">
              <span className="ap-control-text">
                Agent 正在操作终端。你在终端里一敲键盘它就会自动暂停。
              </span>
              <button className="btn" onClick={run.takeover}>
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
                      checked={run.stopCtrlC}
                      onChange={(e) => run.toggleStopCtrlC(e.target.checked)}
                    />
                    同时发 Ctrl-C
                  </label>
                  {!conv.paused && (
                    <button
                      className="btn"
                      title="当前这一步做完后暂停，终端还给你；之后可以「继续」"
                      onClick={() => void run.pauseRun()}
                    >
                      暂停
                    </button>
                  )}
                  <button className="btn danger" onClick={() => void run.cancel()}>
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

      {skillUi && <SkillManager
          initialDraft={skillUi.draft}
          onClose={() => setSkillUi(null)}
          onRunSkill={sessionId && !busy ? (sk, p) => void runSkill(sk, p) : undefined}
          scopeTarget={sessionId ? { sessionId, label: sessions.find((s) => s.id === sessionId)?.label ?? "当前会话" } : undefined}
        />}
      {memOpen && (
        <HostMemoryDialog
          sessionId={sessionId}
          label={sessions.find((s) => s.id === sessionId)?.label ?? ""}
          onClose={() => setMemOpen(false)}
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
