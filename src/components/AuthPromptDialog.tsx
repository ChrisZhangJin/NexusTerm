import { useEffect, useRef, useState } from "react";
import type { AuthPrompt } from "../types";

/**
 * 认证交互框。两类问题共用：服务端二次验证（keyboard-interactive / RFC 4256，后端挂起
 * 等这里回填）与本地私钥口令（`kind = passphrase`）。
 *
 * 焦点刻意落在**第一个空着的**输入框：PAM 多因素会把密码预填进来，落在第一个框的话
 * 用户还得先按一下 Tab 才能开始输验证码。
 */
export default function AuthPromptDialog({
  prompt,
  reconnecting = false,
  queued = 0,
  onSubmit,
  onCancel,
}: {
  prompt: AuthPrompt;
  /** 这是断线自动重连时的验证：验证码得重新生成，不能沿用断线前那个 */
  reconnecting?: boolean;
  /** 排在这条后面、还没轮到的连接数 */
  queued?: number;
  onSubmit: (answers: string[], remember: boolean) => void;
  onCancel: () => void;
}) {
  const isPassphrase = prompt.kind === "passphrase";
  const [values, setValues] = useState<string[]>(() =>
    prompt.fields.map((f) => f.prefill ?? "")
  );
  // 私钥口令默认记住：绝大多数人不想每次连都输一遍；
  // 二次验证码不能记（一次性，记住只会带来错误）。
  const [remember, setRemember] = useState(isPassphrase);
  const [busy, setBusy] = useState(false);
  const inputs = useRef<(HTMLInputElement | null)[]>([]);

  useEffect(() => {
    // 第一轮没填的那个；全都填好了就回到第一个（方便直接回车提交）
    const idx = values.findIndex((v) => !v);
    inputs.current[idx < 0 ? 0 : idx]?.focus();
    inputs.current[idx < 0 ? 0 : idx]?.select();
    // 只在挂载时定位一次：服务端换轮时组件会因 key 变化重新挂载
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Escape 取消。用捕获阶段：终端组件的全局键盘钩子也在监听，
  // 不抢在它前面会把 Esc 一起发给远端。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onCancel();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onCancel]);

  // 提交后如果服务端迟迟不回应（极少数会在认证阶段静默），别让按钮永远灰着
  useEffect(() => {
    if (!busy) return;
    const t = window.setTimeout(() => setBusy(false), 20_000);
    return () => window.clearTimeout(t);
  }, [busy]);

  const set = (i: number, v: string) =>
    setValues((prev) => prev.map((x, j) => (j === i ? v : x)));

  const submit = () => {
    if (busy) return;
    setBusy(true);
    // OTP 常被从短信 / 验证器里连空格一起复制过来，这里统一去掉首尾空白
    onSubmit(values.map((v) => v.trim()), remember);
  };

  const title = isPassphrase
    ? "解锁私钥"
    : reconnecting
      ? "重连需要二次验证"
      : "需要二次验证";
  const subtitle = isPassphrase
    ? "这把私钥设了口令，解锁后才能用来认证"
    : reconnecting
      ? "连接已中断，重新登录需要再验证一次"
      : "服务器要求补充一次身份验证";

  return (
    <div className="modal-backdrop">
      <form
        className="modal auth-modal"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <div className="auth-head">
          <span className="auth-icon" aria-hidden>
            {isPassphrase ? <KeyGlyph /> : <ShieldGlyph />}
          </span>
          <div className="auth-titles">
            <h2>{title}</h2>
            <p className="auth-sub">{subtitle}</p>
          </div>
          {prompt.round > 1 && !isPassphrase && (
            <span className="auth-round">第 {prompt.round} 步</span>
          )}
        </div>

        <div className="auth-target">
          <span className="auth-target-host" title={prompt.host}>
            {prompt.host}
          </span>
          {prompt.username && (
            <span className="auth-target-user" title={prompt.username}>
              {prompt.username}
            </span>
          )}
        </div>

        {reconnecting && !isPassphrase && (
          <p className="auth-notice warn">
            断线前用过的验证码已经失效，请输入现在这一刻新生成的码。
          </p>
        )}
        {/*
          name 和 instructions 都要显示。曾经写成 `instructions || name`，
          两个都有时 name 被丢掉 —— 堡垒机的 MFA 菜单恰好就放在 name 里。
        */}
        {(prompt.instructions || prompt.name) && (
          <p className="auth-notice">
            {[prompt.name, prompt.instructions]
              .map((t) => t.trim())
              .filter((t, i, a) => t && a.indexOf(t) === i)
              .join("\n")}
          </p>
        )}

        {prompt.fields.map((f, i) => {
          const otp = !isPassphrase && isOneTimeCode(f.prompt);
          return (
            <div className={otp ? "auth-field auth-field-otp" : "auth-field"} key={i}>
              <label htmlFor={`auth-f-${i}`}>
                {cleanPrompt(f.prompt) || `输入 ${i + 1}`}
                {otp && <span className="auth-hint">一次性，约 30 秒过期</span>}
              </label>
              <input
                id={`auth-f-${i}`}
                ref={(el) => {
                  inputs.current[i] = el;
                }}
                // 验证码一律明文：6 位数字遮成圆点后既数不清也核对不了，
                // 而它本来就是一分钟内作废的一次性值，没有遮的必要。
                type={otp || f.echo ? "text" : "password"}
                value={values[i]}
                onChange={(e) => set(i, e.target.value)}
                autoComplete="off"
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                inputMode={otp ? "numeric" : undefined}
                placeholder={otp ? "输入验证码" : undefined}
                // 验证码不需要密码管理器介入，也避免它误填
                name={`auth-field-${i}`}
                disabled={busy}
              />
            </div>
          );
        })}

        {isPassphrase && (
          <label className="settings-check auth-remember">
            <input
              type="checkbox"
              checked={remember}
              onChange={(e) => setRemember(e.target.checked)}
            />
            <span>
              记住口令
              <em>保存在本机连接配置里，下次连接不必再输</em>
            </span>
          </label>
        )}

        <div className="auth-foot">
          <span className="auth-foot-note">
            {queued > 0 ? `还有 ${queued} 个连接在等待验证` : "Enter 提交 · Esc 取消"}
          </span>
          <div className="auth-foot-actions">
            <button type="button" className="btn" onClick={onCancel} disabled={busy}>
              取消
            </button>
            <button type="submit" className="btn primary" disabled={busy}>
              {busy ? "验证中…" : "提交"}
            </button>
          </div>
        </div>
      </form>
    </div>
  );
}

function ShieldGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.7">
      <path d="M12 3l7 3v5.5c0 4.4-3 7.8-7 9.5-4-1.7-7-5.1-7-9.5V6l7-3z" strokeLinejoin="round" />
      <path d="M9 12l2.2 2.2L15.5 10" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}

function KeyGlyph() {
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.7">
      <circle cx="8" cy="8" r="4" />
      <path d="M11 11l8 8M16.5 16.5l2-2M14.5 14.5l2-2" strokeLinecap="round" />
    </svg>
  );
}

/**
 * 服务端的提问原文常带结尾的冒号与空白（`[OTP Code]: `、`Password: `），
 * 放在标签里会多出一截空档。只去掉尾部这些符号，中间内容一个字不动。
 */
function cleanPrompt(prompt: string): string {
  return prompt.replace(/[\s:：]+$/, "");
}

/**
 * 是否是一次性验证码字段。只凭文案判断、不看 `echo`：各服务器对该标志的理解不一致，
 * 有些堡垒机的 OTP 提示（`[OTP Code]: `）就带着 echo=true。
 */
function isOneTimeCode(prompt: string): boolean {
  return /(code|otp|verif|passcode|token)/i.test(prompt) || prompt.includes("验证码");
}
