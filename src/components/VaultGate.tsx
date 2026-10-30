import { useEffect, useState, type ReactElement, type ReactNode } from "react";
import { api } from "../api";
import MasterPasswordDialog from "./MasterPasswordDialog";
import type { VaultStatus } from "../types";

/**
 * 启动闸：保险库处于「已启用但锁定」时，先要一次主密码再放行主界面。
 *
 * 性能上的硬要求：绝大多数用户根本没开这个功能，他们这条路上**一次 IPC 都不发** ——
 * `initial` 已经由 `main.tsx` 在挂载前取回，这里只是读它，不再追问后端。
 * `initial === null` 表示状态没取到（后端还没起来之类），一律按「未启用」放行：
 * 拦在闸口只会让用户连自救的界面都打不开。
 *
 * 放行后不代表解锁。「稍后再说」进去的是锁定态：保存密码、导入、读密码都会报
 * 「保险库已锁定」，用户可以随时去「设置 → 安全」里解锁。
 */
export default function VaultGate({
  initial,
  children,
}: {
  initial: VaultStatus | null;
  children: ReactNode;
}): ReactElement {
  const needsGate = initial?.enabled === true && !initial.unlocked;
  /** 已放行：直接渲染 children */
  const [passed, setPassed] = useState(!needsGate);
  /** 正在试「在本机记住」的那把钥匙。先给一块空白闸面，别闪一下密码框又自己消失 */
  const [probing, setProbing] = useState(needsGate);

  useEffect(() => {
    // 不需要闸门的那条路在这里就结束，不碰系统凭据服务
    if (!needsGate) return;
    let alive = true;
    void (async () => {
      let ok = false;
      try {
        ok = await api.vaultTryRemembered();
      } catch (e) {
        // 凭据服务不可用 ≠ 密码错，照常落到手输那一屏
        console.warn("用本机记住的钥匙解锁失败，改为手动输入：", e);
      }
      if (!alive) return;
      if (ok) setPassed(true);
      setProbing(false);
    })();
    return () => {
      alive = false;
    };
  }, [needsGate]);

  if (passed) return <>{children}</>;

  // 闸面本身是全屏不透明的，probing 期间什么都不画即可
  if (probing) return <div className="vault-gate" />;

  return (
    <div className="vault-gate">
      <MasterPasswordDialog mode="unlock" onClose={null} onDone={() => setPassed(true)} />
      {/* 解不开也得让人进来：锁定态下界面照用，只是碰不到凭据 */}
      <button type="button" className="btn-link" onClick={() => setPassed(true)}>
        稍后再说，先进入（保险库保持锁定）
      </button>
    </div>
  );
}
