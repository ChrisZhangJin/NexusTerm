/** 计划清单（nx_todo）：勾选列表 + 进度。RunFeed 与托管卡共用。 */

import { todoView } from "../lib/todo";
import type { TodoItem } from "../types";

export function TodoList({ items, title = "计划" }: { items: TodoItem[]; title?: string }) {
  const v = todoView(items);
  if (v.total === 0) return null;
  return (
    <div className="ap-step todo">
      <div className="ap-step-head">
        <span className="ap-step-tag">{title}</span>
        <span className="ap-step-where">
          {v.done}/{v.total}
        </span>
      </div>
      <ul className="ap-todo">
        {v.lines.map((l, i) => (
          <li key={i} className={`ap-todo-${l.status}`}>
            <span className="ap-todo-mark">{l.mark}</span> {l.text}
          </li>
        ))}
      </ul>
    </div>
  );
}
