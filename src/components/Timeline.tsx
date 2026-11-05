import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { api, onAgentTimeline } from "../api";
import { RecorderControls, RecorderDraftCard } from "./RecorderBar";
import {
  ACTOR_LABEL,
  KIND_META,
  buildRows,
  describeEvent,
  formatClock,
  isNearBottom,
  mergeEvents,
  outputSummary,
  rowEqual,
  type TimelineEvent,
  type TimelineRow,
} from "../lib/timeline";

/** 首屏拉取多少条（后端环形上限 512，这里取最近一段就够看） */
const INITIAL_LIMIT = 300;

/**
 * 一行。`memo` + 自定义比较：事件流只追加、已有事件对象引用不变，
 * 所以新来一条时只有它自己（和被追加了输出的那一行）会重渲染，几百条也不卡。
 * `onToggle` 必须是稳定引用（见 `useCallback`），`open` 是布尔，都不破坏 memo。
 */
const Row = memo(
  function Row({
    row,
    open,
    onToggle,
  }: {
    row: TimelineRow;
    open: boolean;
    onToggle: (seq: number) => void;
  }) {
    const { ev, outputs } = row;
    const meta = KIND_META[ev.kind];
    return (
      <div className={`tl-row ${ev.actor} k-${ev.kind}`}>
        <span className="tl-time">{formatClock(ev.tsMs)}</span>
        <span className={`tl-badge ${ev.actor}`}>{ACTOR_LABEL[ev.actor]}</span>
        <span className="tl-icon" title={meta.label}>
          {meta.icon}
        </span>
        <div className="tl-main">
          <div className={`tl-text${ev.kind === "command" || ev.kind === "keys" ? " mono" : ""}`}>
            {ev.kind === "output" ? <span className="tl-dim">输出</span> : describeEvent(ev)}
            {ev.truncated && ev.kind !== "output" && <span className="tl-dim"> …</span>}
            {ev.note && <span className="tl-note">{ev.note}</span>}
          </div>
          {ev.kind === "output" && <pre className="tl-out">{ev.text}</pre>}
          {outputs.length > 0 && (
            <>
              <button className="tl-out-toggle" onClick={() => onToggle(ev.seq)}>
                {open ? "▾" : "▸"} {outputSummary(outputs)}
              </button>
              {open &&
                outputs.map((o) => (
                  <pre className="tl-out" key={o.seq}>
                    {o.text}
                  </pre>
                ))}
            </>
          )}
        </div>
      </div>
    );
  },
  (a, b) => a.open === b.open && a.onToggle === b.onToggle && rowEqual(a.row, b.row)
);

/**
 * 统一时间线：该终端会话里用户与 Agent 各做了什么（命令 / 按键 / 提示 / 输出 / 密码占位），
 * 按时间排，标明是谁做的。数据由后端 `agent/timeline.rs` 合并，这里只负责展示。
 *
 * - 首屏 `agentTimeline` 拉最近一段，之后靠 `nx://agent/timeline/{session}` 增量追加（按 seq 去重）；
 * - 自动滚动只在贴底时生效，用户往上翻了就不再抢滚动条，并提示有几条新事件；
 * - 密码永远只有占位，这里拿不到也不会显示任何密文。
 */
export default function Timeline({ sessionId, hostLabel }: { sessionId: string; hostLabel?: string }) {
  const [events, setEvents] = useState<TimelineEvent[]>([]);
  const [opened, setOpened] = useState<ReadonlySet<number>>(() => new Set());
  const [unseen, setUnseen] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  const eventsRef = useRef<TimelineEvent[]>([]);
  eventsRef.current = events;

  // 先订阅再拉首屏：中间到的事件不会丢，重复的靠 seq 去重
  useEffect(() => {
    let disposed = false;
    let off: (() => void) | null = null;
    setEvents([]);
    setOpened(new Set());
    setUnseen(0);
    pinned.current = true;
    void onAgentTimeline(sessionId, (batch) => {
      if (disposed) return;
      const prev = eventsRef.current;
      const next = mergeEvents(prev, batch);
      if (next === prev) return;
      if (!pinned.current) {
        const last = prev.length > 0 ? prev[prev.length - 1].seq : 0;
        const added = batch.filter((e) => e.seq > last).length;
        if (added > 0) setUnseen((n) => n + added);
      }
      eventsRef.current = next;
      setEvents(next);
    }).then((un) => {
      if (disposed) un();
      else off = un;
    });
    api
      .agentTimeline(sessionId, 0, INITIAL_LIMIT)
      .then((initial) => {
        if (!disposed) setEvents((prev) => mergeEvents(prev, initial));
      })
      .catch(() => {});
    return () => {
      disposed = true;
      off?.();
    };
  }, [sessionId]);

  const rows = useMemo(() => buildRows(events), [events]);

  // 贴底时跟随新事件；没贴底就别动滚动条
  useLayoutEffect(() => {
    const el = listRef.current;
    if (el && pinned.current) el.scrollTop = el.scrollHeight;
  }, [rows]);

  const onScroll = useCallback(() => {
    const el = listRef.current;
    if (!el) return;
    const near = isNearBottom(el.scrollHeight, el.scrollTop, el.clientHeight);
    pinned.current = near;
    if (near) setUnseen(0);
  }, []);

  const toBottom = useCallback(() => {
    const el = listRef.current;
    if (!el) return;
    pinned.current = true;
    el.scrollTop = el.scrollHeight;
    setUnseen(0);
  }, []);

  const onToggle = useCallback((seq: number) => {
    setOpened((s) => {
      const n = new Set(s);
      if (n.has(seq)) n.delete(seq);
      else n.add(seq);
      return n;
    });
  }, []);

  // 后端清空记忆后，前端手里的是过期数据：用户可点「刷新」用后端现状整体替换
  const refresh = useCallback(() => {
    api
      .agentTimeline(sessionId, 0, INITIAL_LIMIT)
      .then((all) => {
        eventsRef.current = all;
        setEvents(all);
        setOpened(new Set());
      })
      .catch(() => {});
  }, [sessionId]);

  return (
    <div className="ap-tl">
      <div className="tl-bar">
        <span className="tl-count">{rows.length} 条</span>
        <RecorderControls sessionId={sessionId} hostLabel={hostLabel} />
        <span className="tl-legend">
          <span className="tl-badge user">用户</span>
          <span className="tl-badge agent">Agent</span>
        </span>
        <button className="ap-mini" onClick={refresh} title="按后端现状重新载入（清空记忆后用）">
          刷新
        </button>
      </div>
      <RecorderDraftCard sessionId={sessionId} />
      <div className="tl-list" ref={listRef} onScroll={onScroll}>
        {rows.length === 0 ? (
          <div className="ap-empty">
            <p>这个终端还没有记录。你在终端里提交的命令、助手执行的动作都会按时间出现在这里。</p>
          </div>
        ) : (
          rows.map((r) => (
            <Row key={r.ev.seq} row={r} open={opened.has(r.ev.seq)} onToggle={onToggle} />
          ))
        )}
      </div>
      {unseen > 0 && (
        <button className="tl-new" onClick={toBottom}>
          ↓ {unseen} 条新事件
        </button>
      )}
    </div>
  );
}
