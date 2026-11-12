/**
 * 计划清单（nx_todo）的展示逻辑：状态 → 勾选符号、进度计数、当前在做的那一项。纯函数，node:test 直接测。
 */

import type { TodoItem } from "../types";

export interface TodoLine {
  mark: string;
  text: string;
  status: TodoItem["status"];
}

export interface TodoView {
  lines: TodoLine[];
  done: number;
  total: number;
  /** 正在做的第一项；没有就是第一项未做的 */
  current: string | null;
}

const MARK: Record<TodoItem["status"], string> = { done: "☑", in_progress: "▶", pending: "☐" };

export function todoView(items: TodoItem[] | null | undefined): TodoView {
  const list = items ?? [];
  const lines = list.map((i) => ({ mark: MARK[i.status] ?? "☐", text: i.text, status: i.status }));
  const doing = list.find((i) => i.status === "in_progress") ?? list.find((i) => i.status === "pending");
  return {
    lines,
    done: list.filter((i) => i.status === "done").length,
    total: list.length,
    current: doing ? doing.text : null,
  };
}
