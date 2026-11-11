/** node 测试里没有 Tauri 运行时；steward.ts 只用到这两个导出，且测试不触发 IPC。 */
export const invoke = async (): Promise<never> => {
  throw new Error("Tauri IPC 在单测里不可用");
};
export const listen = async (): Promise<() => void> => () => undefined;
