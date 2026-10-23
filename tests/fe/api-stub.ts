/**
 * 替掉真实的 `src/api.ts`。
 *
 * 自测跑在 node 里，没有 Tauri 运行时，也不该真的去连后端。
 * 这里只做两件事：记录调用（好断言「同一程序只问一次」），
 * 以及返回一份固定的参数表。
 */

import type { CmdOption } from "../../src/types";

export const calls: { tabId: string; program: string }[] = [];

/** 只有 `tail` 有参数表；其它程序一律空 —— 顺便覆盖「程序不存在」那条路 */
const TABLE: Record<string, CmdOption[]> = {
  tail: [
    { flags: ["-f", "--follow"], summary: "跟踪新增内容", takesValue: false },
    { flags: ["-n", "--lines"], summary: "输出末尾 N 行", takesValue: true },
    { flags: ["--verbose"], summary: "显示详细过程", takesValue: false },
  ],
};

export const api = {
  consoleCommandOptions(tabId: string, program: string): Promise<CmdOption[]> {
    calls.push({ tabId, program });
    return Promise.resolve(TABLE[program] ?? []);
  },

  /** 宏回放会把字节写到这里；测试据此断言「发了什么、顺序对不对」 */
  sshWrite(tabId: string, data: number[]): Promise<void> {
    writes.push({ tabId, data });
    return Promise.resolve();
  },
};

/** 宏回放记录：每次 sshWrite 一条 */
export const writes: { tabId: string; data: number[] }[] = [];

/** 与 `src/api.ts` 同语义：按 UTF-8 编码成字节数组 */
export function stringToBytes(s: string): number[] {
  return Array.from(new TextEncoder().encode(s));
}
