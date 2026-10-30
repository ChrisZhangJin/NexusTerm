import { useEffect, useRef, useState, type ReactElement } from "react";
import { api } from "../api";
import type { SweepReport } from "../types";

export type MasterPasswordMode = "enable" | "unlock" | "change" | "disable";

/**
 * 主密码对话框。启用 / 解锁 / 修改 / 关闭四件事共用一个组件 —— 它们的输入几乎一样，
 * 拆成四个文件只会让四份「两次密码不一致」的校验各自跑偏。
 *
 * **这里没有「忘记主密码 → 重置」**，是有意的：解锁框谁都能点到，而重置会销毁全部凭据
 * —— 不知道密码的人进不来，却能删光你的东西。忘了主密码就是忘了（见启用时的确认项），
 * 不提供任何不需要密码的破坏性入口。后端也没有对应命令，不要补回来。
 */
export default function MasterPasswordDialog({
  mode,
  onClose,
  rememberDefault = false,
  onDone,
}: {
  mode: MasterPasswordMode;
  /** null = 强制态（启动闸用）：不渲染取消按钮、也不响应 Escape */
  onClose: (() => void) | null;
  /** 「在本机记住」复选框的初始值，默认 false */
  rememberDefault?: boolean;
  /** 成功后回调：enable/disable 给 SweepReport；unlock/change 给 null */
  onDone: (result: SweepReport | null) => void;
}): ReactElement {
  // enable/change 里这是「新主密码」，unlock/disable 里这就是主密码本身
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  const [current, setCurrent] = useState("");
  const [remember, setRemember] = useState(rememberDefault);
  // 启用时的强制勾选项。默认关，不勾不给提交
  const [ack, setAck] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");

  // change 的焦点要落在「当前主密码」上，其余模式落在主输入框
  const firstRef = useRef<HTMLInputElement>(null);
  const pwRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    (firstRef.current ?? pwRef.current)?.focus();
  }, []);

  // Escape 取消。用捕获阶段：终端组件的全局键盘钩子也在监听，
  // 不抢在它前面会把 Esc 一起发给远端。
  // onClose 为 null 是启动闸的强制态，这时 Esc 不该有任何效果。
  useEffect(() => {
    if (!onClose) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  /** 请求失败：保留已输入的内容，只把焦点送回密码框。打错一个字符就被清空很烦人。 */
  const fail = (e: unknown) => {
    setErr(String(e));
    setBusy(false);
    const el = pwRef.current;
    el?.focus();
    el?.select();
  };

  const submit = async () => {
    if (busy) return;
    // 本地能判出来的错一律不发请求：省一次往返，也避免后端把空密码当成一次失败尝试
    if (mode === "change" && !current) {
      setErr("请输入当前主密码。");
      firstRef.current?.focus();
      return;
    }
    if (!password) {
      setErr("请输入主密码。");
      pwRef.current?.focus();
      return;
    }
    if ((mode === "enable" || mode === "change") && password !== confirm) {
      setErr("两次输入的主密码不一致。");
      return;
    }
    if (mode === "enable" && !ack) return; // 按钮本来就是灰的，这里只是兜底

    setErr("");
    setBusy(true);
    try {
      if (mode === "enable") onDone(await api.vaultEnable(password, remember));
      else if (mode === "unlock") {
        await api.vaultUnlock(password, remember);
        onDone(null);
      } else if (mode === "change") {
        await api.vaultChangePassword(current, password);
        onDone(null);
      } else {
        onDone(await api.vaultDisable(password));
      }
    } catch (e) {
      fail(e);
    }
  };

  const title =
    mode === "enable"
      ? "启用主密码保护"
      : mode === "unlock"
        ? "输入主密码"
        : mode === "change"
          ? "修改主密码"
          : "关闭主密码保护";
  const okText =
    mode === "enable"
      ? "启用"
      : mode === "unlock"
        ? "解锁"
        : mode === "change"
          ? "修改"
          : "关闭保护";
  const busyText =
    mode === "enable"
      ? "加密中…"
      : mode === "unlock"
        ? "解锁中…"
        : mode === "change"
          ? "处理中…"
          : "还原中…";
  // 启用时不勾确认项就不给提交 —— 这句警告是整个功能里唯一拦得住数据永久丢失的东西
  const canSubmit = !busy && (mode !== "enable" || ack);

  return (
    <div className="modal-backdrop">
      <form
        className="modal"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <h2>{title}</h2>

        {mode === "enable" && (
          <p className="hint" style={{ margin: "0 0 12px" }}>
            启用后，已保存的 SSH 密码、私钥口令、LLM API Key 和 TOTP 种子会用这把主密码加密。
            数据库仍可整个拷到别的机器，输同一个主密码就能全部打开。
          </p>
        )}
        {mode === "change" && (
          <p className="hint" style={{ margin: "0 0 12px" }}>
            只是用新密码重新包一遍密钥，凭据本身不动，瞬间完成。
          </p>
        )}

        {mode === "change" && (
          <div className="row">
            <label htmlFor="vault-cur">当前密码</label>
            <input
              id="vault-cur"
              ref={firstRef}
              type="password"
              value={current}
              onChange={(e) => setCurrent(e.target.value)}
              autoComplete="current-password"
              disabled={busy}
            />
          </div>
        )}

        <div className="row">
          <label htmlFor="vault-pw">
            {mode === "enable" || mode === "change" ? "新主密码" : "主密码"}
          </label>
          <input
            id="vault-pw"
            ref={pwRef}
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete={
              mode === "enable" || mode === "change" ? "new-password" : "current-password"
            }
            disabled={busy}
          />
        </div>

        {(mode === "enable" || mode === "change") && (
          <div className="row">
            <label htmlFor="vault-pw2">再输一遍</label>
            <input
              id="vault-pw2"
              type="password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              autoComplete="new-password"
              disabled={busy}
            />
          </div>
        )}

        {mode === "enable" && (
          <div className="vault-danger">
            <label className="settings-check" style={{ marginBottom: 0 }}>
              <input
                type="checkbox"
                checked={ack}
                onChange={(e) => setAck(e.target.checked)}
                disabled={busy}
              />
              <span>
                我明白<strong>忘记主密码</strong>
                将导致所有已保存的密码、私钥口令、API Key、TOTP
                种子永久无法恢复，且没有找回手段。
              </span>
            </label>
          </div>
        )}

        {mode === "disable" && (
          <div className="vault-danger">
            <strong>关闭后，这些凭据将以明文形式存回 nexus.db。</strong>
            <ul>
              <li>SSH 登录密码</li>
              <li>私钥口令</li>
              <li>LLM API Key</li>
            </ul>
            TOTP 种子会改回本机密钥加密：仍然是密文，但<strong>不可移植</strong>
            ，换到别的机器要重新填。
          </div>
        )}

        {(mode === "enable" || mode === "unlock") && (
          <label className="settings-check">
            <input
              type="checkbox"
              checked={remember}
              onChange={(e) => setRemember(e.target.checked)}
              disabled={busy}
            />
            <span>
              在本机记住这把钥匙
              {/* 这句不能省：「记住了就不用记密码」这个误解会直接导致数据永久丢失 */}
              <em>仅本机有效，换到其它机器仍需输入主密码。请务必自己记住主密码。</em>
            </span>
          </label>
        )}

        {err && <p className="error">{err}</p>}

        <div className="modal-actions">
          {onClose && (
            <button type="button" className="btn" onClick={onClose} disabled={busy}>
              取消
            </button>
          )}
          <button
            type="submit"
            className={mode === "disable" ? "btn primary danger" : "btn primary"}
            disabled={!canSubmit}
          >
            {busy ? busyText : okText}
          </button>
        </div>
      </form>
    </div>
  );
}
