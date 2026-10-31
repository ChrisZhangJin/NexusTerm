/**
 * 替掉真实的 `src/api.ts`。
 *
 * 自测跑在 node 里，没有 Tauri 运行时，也不该真的去连后端。
 * 这里只做两件事：记录调用（好断言「同一程序只问一次」），
 * 以及返回一份固定的参数表。
 */

export const api = {
  /** 宏回放会把字节写到这里；测试据此断言「发了什么、顺序对不对」 */
  sshWrite(tabId: string, data: number[]): Promise<void> {
    writes.push({ tabId, data });
    return Promise.resolve();
  },

  /** 输入日志落盘用（`lib/macInput` 的现场记录）。自测里没有后端，直接吞掉 */
  appendInputLog(): Promise<void> {
    return Promise.resolve();
  },
};

/** 宏回放记录：每次 sshWrite 一条 */
export const writes: { tabId: string; data: number[] }[] = [];

/** 与 `src/api.ts` 同语义：按 UTF-8 编码成字节数组 */
export function stringToBytes(s: string): number[] {
  return Array.from(new TextEncoder().encode(s));
}

/** FileManager 订阅用；自测里没有真实的后端事件流，给个 no-op unlisten */
export function onSftpProgress(): Promise<() => void> {
  return Promise.resolve(() => {});
}
