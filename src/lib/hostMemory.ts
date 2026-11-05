/**
 * 主机记忆（Phase 4.5）的纯逻辑：与界面无关、可自测（`tests/fe/hostmemory.test.ts`）。
 *
 * 后端（`agent::hostmem`）才是规则的权威：文本规整、凭据拒绝、条数上限都在那边做。
 * 这里只做「提交前能先拦掉的」（空 / 超长，省一次往返）和展示用的分组 / 标签。
 */

import type { AgentStep, HostNote, HostNoteKind, HostNoteSource } from "../types";

/** 类别展示顺序 */
export const NOTE_KINDS: HostNoteKind[] = ["prompt", "sudo", "path", "quirk", "other"];

export const KIND_LABEL: Record<HostNoteKind, string> = {
  prompt: "提示符",
  sudo: "sudo",
  path: "路径",
  quirk: "特性",
  other: "其他",
};

export const SOURCE_LABEL: Record<HostNoteSource, string> = {
  agent: "助手记的",
  user: "我写的",
};

/** 与后端 `validate_note_text` 同样的规整：控制字符 / 换行折成空格、空白折叠、去首尾空白。 */
export function normalizeNoteText(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\u0000-\u001f\u007f-\u009f]/g, " ").split(/\s+/).filter(Boolean).join(" ");
}

export type DraftCheck = { ok: true; text: string } | { ok: false; error: string };

/** 提交前的本地检查：空 / 超长。凭据检测留给后端（那里才是权威，前端不复制一份正则）。 */
export function checkDraft(raw: string, maxChars: number): DraftCheck {
  const text = normalizeNoteText(raw);
  if (!text) return { ok: false, error: "内容不能为空" };
  const n = [...text].length;
  if (n > maxChars) return { ok: false, error: `过长（${n} 字，上限 ${maxChars}）` };
  return { ok: true, text };
}

/** 按类别分组（固定顺序，空组不出现）；组内按创建时间从早到晚。 */
export function groupNotes(notes: HostNote[]): { kind: HostNoteKind; notes: HostNote[] }[] {
  const out: { kind: HostNoteKind; notes: HostNote[] }[] = [];
  for (const kind of NOTE_KINDS) {
    const list = notes
      .filter((n) => n.kind === kind)
      .sort((a, b) => a.createdAt - b.createdAt || a.id.localeCompare(b.id));
    if (list.length) out.push({ kind, notes: list });
  }
  return out;
}

/** 条数 / 上限的展示。 */
export function usageLabel(count: number, max: number): string {
  return `${count} / ${max} 条`;
}

/** 这条备忘 Agent 能不能删：只有它自己写的。界面上用来标注「用户编辑过就归用户」。 */
export function agentMayDelete(note: Pick<HostNote, "source">): boolean {
  return note.source === "agent";
}

/** 面板里 kind:"memory" 步骤卡的摘要。 */
export interface MemoryStepInfo {
  op: "add" | "forget" | "read";
  text: string;
  /** 可被用户在卡片上删除的备忘 id（只有 add 带） */
  deletableId: string | null;
}

export function memoryStepInfo(step: Pick<AgentStep, "command" | "text" | "memoryId">): MemoryStepInfo {
  const op = step.command === "forget" ? "forget" : step.command === "read" ? "read" : "add";
  return {
    op,
    text: step.text ?? "",
    deletableId: op === "add" && step.memoryId ? step.memoryId : null,
  };
}

export const MEMORY_OP_TAG: Record<MemoryStepInfo["op"], string> = {
  add: "记忆",
  forget: "忘记",
  read: "回忆",
};

/** 后端对「这条已经不存在」的说法（卡片上点删除时判断是否其实早已被删）。 */
export function isGoneError(message: string): boolean {
  return message.includes("不存在");
}
