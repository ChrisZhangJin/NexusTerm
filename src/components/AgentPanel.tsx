import { useCallback, useEffect, useRef, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import ConfirmDialog, { type ConfirmOptions } from "./ConfirmDialog";
import { api, onAgentDone, onAgentError, onAgentToken } from "../api";
import { formatBytes, isAgentReady, useAgentConfig } from "../lib/agent";
import { toFriendlyError } from "../lib/errors";
import type { AgentBufferStats, AgentDoneReason, AgentMessage } from "../types";

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
}: AgentPanelProps) {
  const cfg = useAgentConfig();
  const ready = isAgentReady(cfg);

  /** 按会话分桶的聊天记录。key = sessionId，切会话互不可见。 */
  const [convs, setConvs] = useState<Record<string, Conversation>>({});
  const [input, setInput] = useState("");
  const [attached, setAttached] = useState(false);
  const [stats, setStats] = useState<AgentBufferStats | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [pending, setPending] = useState<
    { kind: "clear" } | { kind: "cmd"; cmd: string } | null
  >(null);

  const conv = convs[sessionId] ?? EMPTY_CONV;
  const messages = conv.messages;
  const busy = conv.busy;

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
        const [offToken, offDone, offError] = await Promise.all([
          onAgentToken(sessionId, (chunk) => {
            if (!disposed) patch(sessionId, (c) => withToken(c, chunk));
          }),
          onAgentDone(sessionId, (p) => {
            if (!disposed) patch(sessionId, (c) => withDone(c, p.reason));
          }),
          onAgentError(sessionId, (msg) => {
            if (!disposed) patch(sessionId, (c) => withError(c, msg));
          }),
        ]);
        // 竞态：listen 是异步的，等它 resolve 时会话可能已经切走了 —— 这时要立刻退订，
        // 否则旧会话的 token 会被 patch 到新会话的 key 上（unlisten 还没进 unlisteners）
        if (disposed) {
          offToken();
          offDone();
          offError();
          return;
        }
        unlisteners.push(offToken, offDone, offError);
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
    if (!q || busy || !sessionId) return;
    if (!ready.ok) {
      // 引导态会盖住输入框，正常到不了这里；真到了就把原因说清楚而不是发一个注定失败的请求
      patch(sessionId, (c) => withError(c, ready.reason ?? "AI 助手尚未配置完成"));
      return;
    }
    setInput("");
    setErr(null);
    stickRef.current = true;
    patch(sessionId, (c) => ({
      busy: true,
      interrupted: false,
      messages: [
        ...c.messages,
        { id: uid(), role: "user", text: q, ts: Date.now() },
        { id: uid(), role: "assistant", text: "", ts: Date.now(), streaming: true },
      ],
    }));
    try {
      // resolve 只代表流已启动；答案走 token/done/error 事件收
      await api.agentAsk(sessionId, q);
    } catch (e) {
      patch(sessionId, (c) => withError(c, toFriendlyError(e).message));
    }
  };

  const cancel = async () => {
    if (!sessionId) return;
    try {
      // 幂等：后端会补发一条 reason=cancelled 的 done，busy 由那条事件解除
      await api.agentCancel(sessionId);
    } catch (e) {
      setErr(`取消失败：${toFriendlyError(e).message}`);
    }
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
        <span className="ap-title">AI 助手</span>
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
                <p>回答里的命令会变成建议卡片，需要你确认后才会写进终端。</p>
              </div>
            )}
            {messages.map((m) => (
              <div key={m.id} className={`ap-msg ${m.role}`}>
                <div
                  className={`ap-bubble ${m.role}${m.error ? " error" : ""}`}
                >
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
            ))}
            {conv.interrupted && (
              <div className="ap-note">
                上次离开该会话时回答还没结束，上面的内容可能不完整。
              </div>
            )}
          </div>

          <div className="ap-input">
            <textarea
              className="ap-ta"
              rows={3}
              value={input}
              disabled={busy}
              placeholder={busy ? "正在回答…" : "问点什么，Enter 发送"}
              onChange={(e) => setInput(e.target.value)}
              onKeyDown={(e) => {
                // isComposing：中文输入法选词时的回车不能被当成发送
                if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  void send();
                }
              }}
            />
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
          <div className="ap-setup-title">AI 助手还没就绪</div>
          <p className="ap-setup-text">{ready.reason ?? "请先完成 AI 助手配置。"}</p>
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
