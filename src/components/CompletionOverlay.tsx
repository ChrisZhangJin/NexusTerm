/**
 * 终端补全候选的浮动下拉。
 *
 * 尺寸刻意压得很小（一行一条：写法 + 一句话说明），
 * 它盖住的是**正在敲的那行命令**，挡住输入比提示本身更烦人。
 * 因此高度封顶、靠光标定位、越界自动翻到上方。
 */

import { useEffect, useRef } from "react";
import type { CompletionItem } from "../lib/complete";

export interface CompletionOverlayProps {
  items: CompletionItem[];
  /** 当前高亮项 */
  active: number;
  /** 定位：光标在视口里的像素坐标 */
  x: number;
  y: number;
  /** 终端可视高度（px），判断是否该翻到光标上方 */
  viewportHeight: number;
  onPick: (index: number) => void;
  onHover: (index: number) => void;
}

/** 单条高度与下拉的最大高度，与 styles.css 里的 .cp-item / .cp-pop 保持一致 */
const ITEM_H = 26;
const POP_MAX_H = 26 * 7;

export default function CompletionOverlay({
  items,
  active,
  x,
  y,
  viewportHeight,
  onPick,
  onHover,
}: CompletionOverlayProps) {
  const listRef = useRef<HTMLDivElement>(null);

  // 让高亮项始终留在可视范围内：往下翻时把列表滚到它出现为止
  useEffect(() => {
    const el = listRef.current;
    if (!el) return;
    const top = active * ITEM_H;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + ITEM_H > el.scrollTop + el.clientHeight) {
      el.scrollTop = top + ITEM_H - el.clientHeight;
    }
  }, [active]);

  if (items.length === 0) return null;

  // 下方放不下就翻到光标上方；上方也放不下就取一个能塞下的高度贴边显示
  const wanted = Math.min(items.length * ITEM_H, POP_MAX_H);
  const below = viewportHeight - y - ITEM_H;
  let top: number;
  let height: number;
  if (below >= wanted) {
    top = y + ITEM_H;
    height = wanted;
  } else {
    height = Math.min(y - ITEM_H, POP_MAX_H);
    top = y - ITEM_H - height;
  }

  return (
    <div className="cp-pop" style={{ left: x, top, height }} onMouseDown={(e) => e.preventDefault()}>
      <div className="cp-list" ref={listRef}>
        {items.map((it, i) => (
          <button
            key={it.label + i}
            className={`cp-item${i === active ? " on" : ""}`}
            onMouseEnter={() => onHover(i)}
            onClick={() => onPick(i)}
            title={it.detail}
          >
            <span className="cp-kind">{it.kind === "flag" ? "参数" : "历史"}</span>
            <span className="cp-label">{it.label}</span>
            <span className="cp-detail">{it.detail}</span>
          </button>
        ))}
      </div>
    </div>
  );
}
