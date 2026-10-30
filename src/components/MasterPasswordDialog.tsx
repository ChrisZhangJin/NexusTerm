import { useEffect, useRef, useState, type ReactElement } from "react";
import { api } from "../api";
import type { SweepReport } from "../types";

export type MasterPasswordMode = "enable" | "unlock" | "change" | "disable";

/** 重置那一屏要求手敲的字样。不做大小写宽容：这一步的意义就是让手指慢下来。 */
const RESET_WORD = "RESET";

/**
 * 主密码对话框。启用 / 解锁 / 修改 / 关闭四件事共用一个组件 —— 它们的输入几乎一样，
 * 拆成四个文件只会让四份「两次密码不一致」的校验各自跑偏。
 *
 * 「忘记主密码？」做成本组件内的第二屏，而不是外面的 ConfirmDialog：这一步会永久销毁
 * 全部凭据，必须把销毁什么、保留什么、备份在哪摆在用户眼前，一行 message 装不下。
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
  /** 成功后回调：enable/disable 给 SweepReport；忘记主密码走的重置给被清掉的条数；unlock/change 给 null */
  onDone: (result: SweepReport | number | null) => void;
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
  // 第二屏：忘记主密码 → 销毁重来
  const [resetting, setResetting] = useState(false);
  const [resetWord, setResetWord] = useState("");

  // change 的焦点要落在「当前主密码」上，其余模式落在主输入框
  const firstRef = useRef<HTMLInputElement>(null);
  const pwRef = useRef<HTMLInputElement>(null);
  const resetRef = useRef<HTMLInputElement>(null);

  // 切屏时把焦点带过去，别让用户先点一下才能打字
  useEffect(() => {
    if (resetting) resetRef.current?.focus();
    else (firstRef.current ?? pwRef.current)?.focus();
  }, [resetting]);

  // Escape 取消。用捕获阶段：终端组件的全局键盘钩子也在监听，
  // 不抢在它前面会把 Esc 一起发给远端。
  // onClose 为 null 是启动闸的强制态，这时 Esc 不该有任何效果。
  useEffect(() => {
    if (!onClose) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      // 第二屏先退回第一屏，直接关掉会让用户以为「取消」只是取消了重置
      if (resetting) setResetting(false);
      else onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose, resetting]);

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

  const doReset = async () => {
    if (busy || resetWord.trim() !== RESET_WORD) return;
    setErr("");
    setBusy(true);
    try {
      onDone(await api.vaultReset());
    } catch (e) {
      setErr(String(e));
      setBusy(false);
      resetRef.current?.focus();
    }
  };

  if (resetting) {
    return (
      <div className="modal-backdrop">
        <form
          className="modal"
          onSubmit={(e) => {
            e.preventDefault();
            void doReset();
          }}
        >
          <h2>忘记主密码</h2>

          <div className="vault-danger">
            <strong>没有找回手段，只能全部销毁重来。</strong>
            <br />
            将<strong>永久销毁</strong>：
            <ul>
              <li>已保存的 SSH 登录密码</li>
              <li>私钥口令</li>
              <li>LLM API Key</li>
              <li>TOTP 种子（双因素验证码的来源）</li>
            </ul>
            将保留：
            <ul>
              <li>主机 / 端口 / 用户名 / 分组</li>
              <li>宏</li>
              <li>私钥文件本身</li>
            </ul>
            重置前会自动把旧库备份成应用数据目录里的{" "}
            <strong>nexus.db.pre-reset.bak</strong>
            ，日后想起主密码仍可从它找回，别急着删。
          </div>

          <div className="row">
            <label htmlFor="vault-reset-word">确认</label>
            <input
              id="vault-reset-word"
              ref={resetRef}
              type="text"
              value={resetWord}
              onChange={(e) => setResetWord(e.target.value)}
              placeholder={`输入 ${RESET_WORD} 以确认`}
              autoComplete="off"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              disabled={busy}
            />
          </div>
          <p className="hint">手动输入大写的 {RESET_WORD} 才会点亮下面的按钮。</p>

          {err && <p className="error">{err}</p>}

          <div className="modal-actions">
            <button
              type="button"
              className="btn"
              onClick={() => {
                setResetWord("");
                setErr("");
                setResetting(false);
              }}
              disabled={busy}
            >
              返回
            </button>
            <button
              type="submit"
              className="btn primary danger"
              disabled={busy || resetWord.trim() !== RESET_WORD}
            >
              {busy ? "销毁中…" : "销毁并重来"}
            </button>
          </div>
        </form>
      </div>
    );
  }

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
          {mode === "unlock" && (
            <button
              type="button"
              className="btn-link"
              style={{ marginRight: "auto" }}
              onClick={() => {
                setErr("");
                setResetting(true);
              }}
              disabled={busy}
            >
              忘记主密码？
            </button>
          )}
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
