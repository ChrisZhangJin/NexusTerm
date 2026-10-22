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
};
