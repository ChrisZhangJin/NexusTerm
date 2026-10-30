import { useEffect, useState } from "react";
import { api } from "../api";
import { normalizeTimeout } from "../lib/errors";
import { getExtras, updateExtras } from "../lib/sessionExtras";
import type {
  AuthMethod,
  KeyInfo,
  MacroRecord,
  NewSession,
  SessionRecord,
  SessionSecrets,
  TotpPreview,
} from "../types";

interface Props {
  editing: SessionRecord | null;
  /** 连接可绑定的宏（连接后自动执行） */
  macros: MacroRecord[];
  onSave: (payload: NewSession, secrets: SessionSecrets) => Promise<void>;
  onClose: () => void;
}

export default function SessionDialog({ editing, macros, onSave, onClose }: Props) {
  const [name, setName] = useState("");
  const [folder, setFolder] = useState("");
  const [host, setHost] = useState("");
  const [port, setPort] = useState("22");
  const [username, setUsername] = useState("");
  const [authType, setAuthType] = useState<"password" | "key" | "agent">("password");
  const [password, setPassword] = useState("");
  const [keyPath, setKeyPath] = useState("");
  const [passphrase, setPassphrase] = useState("");
  /**
   * 用户是否动过口令输入框。编辑连接时口令是异步读回来的，手快在读回前点保存，
   * 空字符串会把已保存的口令覆盖掉 —— 只有真正编辑过才落库。
   */
  const [passphraseTouched, setPassphraseTouched] = useState(false);
  const [totpSeed, setTotpSeed] = useState("");
  /**
   * 同 `passphraseTouched`：编辑连接时「有没有存过种子」是异步查回来的，
   * 用户没动过这个框就绝不能拿空串去覆盖已存的种子 —— 空串在后端是「清除」。
   */
  const [totpTouched, setTotpTouched] = useState(false);
  /** 库里是否已有种子。种子不回显（后端没有 load），只能靠这个显示「已保存」 */
  const [hasSeed, setHasSeed] = useState(false);
  /**
   * 种子输入框是否明文。本文件里别的密码框都没有这个切换，这里加是因为种子
   * 是从手机 / 网页上手抄过来的一长串 base32：看不见就抄错，而抄错的症状
   * （下次连接时验证码被拒）离病因隔着一次连接，几乎没法归因。
   */
  const [showSeed, setShowSeed] = useState(false);
  const [totpChecking, setTotpChecking] = useState(false);
  const [totpResult, setTotpResult] = useState<{ ok: boolean; msg: string } | null>(null);
  /** 浏览选中的外部密钥是否加密（密钥库里的条目直接看 KeyInfo.encrypted） */
  const [encryptedKey, setEncryptedKey] = useState(false);
  const [connectTimeout, setConnectTimeout] = useState("10");
  const [note, setNote] = useState("");
  const [keys, setKeys] = useState<KeyInfo[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ ok: boolean; msg: string } | null>(null);

  useEffect(() => {
    api.listKeys().then(setKeys).catch(() => {});
    if (editing) {
      setName(editing.name);
      // 根目录（"/"）在界面上展示为空
      setFolder(editing.folder === "/" ? "" : editing.folder);
      setHost(editing.host);
      setPort(String(editing.port));
      setUsername(editing.username);
      setAuthType(editing.authType);
      setKeyPath(editing.keyPath ?? "");
      setConnectTimeout(String(normalizeTimeout(editing.connectTimeout)));
      setNote(editing.note ?? "");
      // TOTP 与 authType 正交（密码登录和密钥登录都可能要二次验证），所以放在
      // authType 的分支外面无条件查一次。种子本身查不回来，只查「有没有」。
      setTotpSeed("");
      setTotpTouched(false);
      setShowSeed(false);
      setTotpResult(null);
      setHasSeed(false);
      api.hasTotpSeed(editing.id).then(setHasSeed).catch(() => {});
      // 载入已保存的密码 / 私钥口令，方便查看和修改
      if (editing.authType === "password") {
        api.loadPassword(editing.id).then((p) => setPassword(p ?? "")).catch(() => {});
        setPassphrase("");
        setPassphraseTouched(false);
      } else if (editing.authType === "key") {
        setPassword("");
        setPassphraseTouched(false);
        api.loadPassphrase(editing.id).then((p) => setPassphrase(p ?? "")).catch(() => {});
      } else {
        setPassword("");
        setPassphrase("");
        setPassphraseTouched(false);
      }
    } else {
      setName("");
      setFolder("");
      setHost("");
      setPort("22");
      setUsername("");
      setAuthType("password");
      setPassword("");
      setKeyPath("");
      setPassphrase("");
      setPassphraseTouched(false);
      setConnectTimeout("10");
      setNote("");
      setTotpSeed("");
      setTotpTouched(false);
      setHasSeed(false);
      setShowSeed(false);
      setTotpResult(null);
    }
  }, [editing]);

  /**
   * 选择密钥库之外的文件（比如 U 盘里的、临时生成的私钥）。
   * 顺手试读一次，判断它是否加密 —— 加密才显示口令框，别让没用口令的人
   * 对着一个多余的输入框发愣。
   */
  const browseKey = async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: false,
      directory: false,
      title: "选择私钥文件（OpenSSH / .ppk）",
      // 不设 filters：私钥通常没有扩展名（id_rsa / id_ed25519），
      // 加了过滤反而会把它们藏起来。
    });
    if (typeof picked !== "string") return;
    setKeyPath(picked);
    try {
      await api.verifyKey(picked);
      setEncryptedKey(false);
    } catch (e) {
      // 已加密 = 需要口令；其它错误（格式不对 / 读不到）留给保存后连接时报
      setEncryptedKey(String(e).includes("需要提供密码") || String(e).includes("已加密"));
    }
  };

  const testConnection = async () => {
    setTestResult(null);
    if (!host.trim()) return setTestResult({ ok: false, msg: "主机不能为空" });
    if (!username.trim()) return setTestResult({ ok: false, msg: "用户名不能为空" });
    if (authType === "key" && !keyPath) return setTestResult({ ok: false, msg: "请选择私钥" });

    const auth: AuthMethod =
      authType === "password"
        ? { kind: "password", password }
        : authType === "key"
          ? { kind: "privateKey", path: keyPath, passphrase: passphrase || undefined }
          : { kind: "agent" };

    setTesting(true);
    try {
      const outcome = await api.sshTestConnect({
        host: host.trim(),
        port: Number(port) || 22,
        username: username.trim(),
        auth,
        // 测试路径不建 PTY，这两个字段用不上，填个占位值
        cols: 80,
        rows: 24,
        connectTimeoutSecs: normalizeTimeout(Number(connectTimeout)),
      });
      setTestResult({
        ok: true,
        msg: outcome.legacyCrypto ? "连接成功（服务器仅支持旧版密钥交换算法）" : "连接成功",
      });
    } catch (e) {
      setTestResult({ ok: false, msg: String(e) });
    } finally {
      setTesting(false);
    }
  };

  /**
   * 拿当前填的种子算一次码，让人跟手机上的 app 对一眼再保存。
   * 校验交给后端（`totp_preview` 解析不了就返回中文错误），前端不重写一遍 base32。
   */
  const previewTotp = async () => {
    setTotpResult(null);
    setTotpChecking(true);
    try {
      const p: TotpPreview = await api.totpPreview(totpSeed.trim());
      setTotpResult({ ok: true, msg: `当前验证码 ${p.code}（${p.remainingSecs} 秒后刷新）` });
    } catch (e) {
      setTotpResult({ ok: false, msg: String(e) });
    } finally {
      setTotpChecking(false);
    }
  };

  const submit = async () => {
    setError(null);
    if (!host.trim()) return setError("主机不能为空");
    if (!username.trim()) return setError("用户名不能为空");
    if (authType === "key" && !keyPath) return setError("请选择私钥");
    if (!/^\d+$/.test(connectTimeout.trim())) return setError("连接超时必须是正整数秒");
    if (Number(connectTimeout) < 1 || Number(connectTimeout) > 600)
      return setError("连接超时需在 1–600 秒之间");

    setSaving(true);
    try {
      await onSave(
        {
          name: name.trim() || `${username}@${host}`,
          // 空 = 根目录；多级用 / 分隔，如「生产环境/华东」
          folder: folder.trim() || "/",
          host: host.trim(),
          port: Number(port) || 22,
          username: username.trim(),
          authType,
          keyPath: authType === "key" ? keyPath : undefined,
          connectTimeout: normalizeTimeout(Number(connectTimeout)),
          // 全空白等于没写，存 null 而不是空串 —— 列表那边只要判一次 falsy
          note: note.trim() || null,
        },
        {
          password: authType === "password" ? password : "",
          // null 表示「没动过」：编辑连接时不能拿空串覆盖已保存的口令
          passphrase: authType === "key" && passphraseTouched ? passphrase : null,
          // 同理。种子和 authType 无关，所以这里不按 authType 过滤
          totpSeed: totpTouched ? totpSeed.trim() : null,
        }
      );
      onClose();
    } catch (e) {
      setError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>{editing ? "编辑连接" : "新建连接"}</h2>

        <div className="row">
          <label>名称</label>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="留空则自动生成" />
        </div>
        <div className="row">
          <label>分组</label>
          <input
            value={folder}
            onChange={(e) => setFolder(e.target.value)}
            placeholder="留空=根目录，多级用 / 分隔，如 生产环境/华东"
          />
        </div>
        <div className="row">
          <label>主机</label>
          <input value={host} onChange={(e) => setHost(e.target.value)} placeholder="192.168.1.10 或 example.com" />
        </div>
        <div className="row">
          <label>端口</label>
          <input value={port} onChange={(e) => setPort(e.target.value)} placeholder="22" />
        </div>
        <div className="row">
          <label>用户名</label>
          <input value={username} onChange={(e) => setUsername(e.target.value)} placeholder="root" />
        </div>
        <div className="row">
          <label>连接超时(秒)</label>
          <input
            value={connectTimeout}
            onChange={(e) => setConnectTimeout(e.target.value)}
            inputMode="numeric"
            placeholder="10"
          />
        </div>
        <p className="hint">
          TCP 握手超过该秒数即判定主机不可达，默认 10 秒。主机响应慢可适当调大。
        </p>

        <div className="row">
          <label htmlFor="session-note">备注</label>
          <textarea
            id="session-note"
            className="note-input"
            value={note}
            onChange={(e) => setNote(e.target.value)}
            rows={3}
            placeholder="这台机器是干嘛的、改动前要通知谁、什么时候不能重启…"
            spellCheck={false}
          />
        </div>
        <p className="hint">
          可空，纯备忘。在连接列表里悬停这条连接就能看到。
          <strong>不要往里写密码</strong> —— 这一列不加密，启用主密码也不会保护它。
        </p>

        <div className="row">
          <label>认证</label>
          <div className="seg">
            {(["password", "key", "agent"] as const).map((t) => (
              <button
                key={t}
                className={authType === t ? "seg-btn active" : "seg-btn"}
                onClick={() => setAuthType(t)}
              >
                {t === "password" ? "密码" : t === "key" ? "私钥" : "Agent"}
              </button>
            ))}
          </div>
        </div>

        {authType === "password" && (
          <div className="row">
            <label>密码</label>
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="随连接配置保存，可随数据库迁移到其他机器"
            />
          </div>
        )}

        {authType === "key" && (
          <>
            <div className="row">
              <label>私钥</label>
              <div className="key-pick">
                <select value={keyPath} onChange={(e) => setKeyPath(e.target.value)}>
                  <option value="">选择密钥…</option>
                  {keys.map((k) => (
                    <option key={k.id} value={k.path}>
                      {k.name} · {k.keyType}
                      {k.bits ? ` ${k.bits}` : ""}
                      {k.encrypted ? " 🔒" : ""}
                      {k.source === "sshDir" ? "（~/.ssh）" : ""}
                    </option>
                  ))}
                  {/* 私钥文件被手工删掉 / 移动过时，别让选择框变成空的不明状态 */}
                  {keyPath && !keys.some((k) => k.path === keyPath) && (
                    <option value={keyPath}>{keyPath}（不在密钥库中）</option>
                  )}
                </select>
                <button type="button" className="btn" onClick={browseKey}>
                  浏览…
                </button>
              </div>
            </div>
            {(keys.find((k) => k.path === keyPath)?.encrypted ?? encryptedKey) && (
              <div className="row">
                <label>私钥密码</label>
                <input
                  type="password"
                  value={passphrase}
                  onChange={(e) => {
                    setPassphrase(e.target.value);
                    setPassphraseTouched(true);
                  }}
                  placeholder="留空则连接时再询问"
                />
              </div>
            )}
            <p className="hint">
              密钥库外的文件也可以直接用「浏览…」选，支持 OpenSSH 与 PuTTY 的 .ppk。
              未填口令时，连接时会弹窗询问，勾选「记住口令」后即保存在本机。
            </p>
          </>
        )}

        {authType === "agent" && (
          <p className="hint">使用系统 ssh-agent 中的凭据。需先在终端执行 ssh-add。</p>
        )}

        {/*
          TOTP 与认证方式正交：密码登录和密钥登录都可能被要求二次验证，
          所以这一段放在 authType 的分支之外，任何认证方式下都渲染。
        */}
        <div className="row">
          <label>二次验证</label>
          <div className="key-pick">
            <input
              // 种子是手抄进来的一长串 base32，看不见极易抄错，所以这里配了显示切换
              type={showSeed ? "text" : "password"}
              value={totpSeed}
              onChange={(e) => {
                setTotpSeed(e.target.value);
                setTotpTouched(true);
                setTotpResult(null);
              }}
              placeholder={
                hasSeed && !totpTouched
                  ? "已保存（留空不修改）"
                  : hasSeed && !totpSeed.trim()
                    ? "保存后将清除已存的种子"
                    : "TOTP 种子，或 otpauth://totp/… 链接"
              }
              autoComplete="off"
              spellCheck={false}
            />
            <button type="button" className="btn" onClick={() => setShowSeed((v) => !v)}>
              {showSeed ? "隐藏" : "显示"}
            </button>
            {hasSeed && (
              <button
                type="button"
                className="btn"
                onClick={() => {
                  setTotpSeed("");
                  setTotpTouched(true);
                  setTotpResult(null);
                }}
                disabled={totpTouched && !totpSeed.trim()}
              >
                清除
              </button>
            )}
          </div>
        </div>

        <div className="row">
          <label />
          <button
            className="btn"
            onClick={previewTotp}
            disabled={totpChecking || saving || !totpSeed.trim()}
          >
            {totpChecking ? "验证中…" : "验证"}
          </button>
        </div>
        {totpResult && <p className={totpResult.ok ? "hint" : "error"}>{totpResult.msg}</p>}

        <p className="hint">
          填了种子之后，服务器索要验证码时就不用再掏手机。点「验证」可以算出此刻的码，
          跟手机上的 app 对一眼再保存 —— 抄错一位的话，不验就得等到下次连接才发现。
          种子加密保存在本机，把数据库拷到别的机器上解不开，换机器需要重填一次。
          连接时第一次会自动提交算出的验证码；一旦被服务器拒绝，本次连接剩下的问答就只预填，
          由你确认后再发 —— 连错几次会锁账号，自动重试只会烧得更快。
        </p>

        <div className="row">
          <label />
          <button className="btn" onClick={testConnection} disabled={testing || saving}>
            {testing ? "测试中…" : "测试连接"}
          </button>
        </div>
        {testResult && (
          <p className={testResult.ok ? "hint" : "error"}>{testResult.msg}</p>
        )}

        {error && <p className="error">{error}</p>}

        {editing && <AutoRunSection sessionId={editing.id} macros={macros} />}

        <div className="modal-actions">
          <button className="btn" onClick={onClose} disabled={saving}>
            取消
          </button>
          <button className="btn primary" onClick={submit} disabled={saving}>
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * 连接成功后的自动化：绑定一条宏 + 兼容模式。宏的录制与编辑在标签栏右侧的「宏」里，
 * 这里只负责「连上之后跑哪条」。
 *
 * 直接读写 `lib/sessionExtras` 的缓存（不走 onSave）：它不属于连接参数，改完立刻生效。
 */
function AutoRunSection({
  sessionId,
  macros,
}: {
  sessionId: string;
  macros: MacroRecord[];
}) {
  const [extras, setExtras] = useState(() => getExtras(sessionId));

  useEffect(() => {
    setExtras(getExtras(sessionId));
  }, [sessionId]);

  const apply = (patch: Partial<typeof extras>) => {
    updateExtras(sessionId, patch);
    setExtras(getExtras(sessionId));
  };

  const bound = macros.find((m) => m.id === extras.macroId && extras.macroOn) ?? null;

  return (
    // 折叠：编辑连接本来就有七八个字段，摘要行给结论，需要改再展开
    <details className="login-path">
      <summary className="login-path-head">
        <span className="login-path-caret" aria-hidden>
          ▸
        </span>
        <span>登录后自动执行</span>
        {bound ? (
          <span className="login-path-steps">
            <span className="login-path-chip">
              {bound.name} · {bound.steps.length} 步
            </span>
          </span>
        ) : (
          <span className="login-path-idle">未设置</span>
        )}
        {extras.compat && <span className="login-path-badge">兼容模式</span>}
      </summary>

      <div className="login-path-body">
        <div className="row">
          <label>执行宏</label>
          <select
            value={extras.macroOn ? extras.macroId : ""}
            onChange={(e) =>
              apply({ macroId: e.target.value, macroOn: !!e.target.value })
            }
          >
            <option value="">不执行</option>
            {macros.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
          </select>
        </div>
        <p className="hint" style={{ margin: "0 0 8px" }}>
          {macros.length === 0
            ? "还没有宏。在标签栏右侧「宏」里新建或录制一条，就会出现在这里。"
            : "连接 / 断线重连成功后自动执行，执行完再恢复上次的目录与 tmux。"}
        </p>

        <label className="settings-check login-path-check">
          <input
            type="checkbox"
            checked={extras.compat}
            onChange={(e) => apply({ compat: e.target.checked })}
          />
          <span>
            兼容模式
            <em>
              不在该连接上开旁路通道。堡垒机只允许一条通道时会自动打开
              （否则连上几秒就会被服务端断开）
            </em>
          </span>
        </label>
      </div>
    </details>
  );
}
