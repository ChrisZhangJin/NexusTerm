import { useEffect, useState } from "react";
import { api } from "../api";
import { normalizeTimeout } from "../lib/errors";
import { getExtras, updateExtras } from "../lib/sessionExtras";
import type { KeyInfo, MacroRecord, NewSession, SessionRecord } from "../types";

interface Props {
  editing: SessionRecord | null;
  /** 会话可绑定的宏（连接后自动执行） */
  macros: MacroRecord[];
  onSave: (
    payload: NewSession,
    password: string,
    /** 私钥口令；null = 用户没动它，保持库里原值 */
    passphrase: string | null
  ) => Promise<void>;
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
   * 用户是否动过口令输入框。
   *
   * 这个标记不是洁癖：编辑会话时口令是异步从库里读出来的，若用户手快、
   * 在读回来之前点了保存，空字符串会把已保存的口令**覆盖掉**。
   * 只有真正编辑过才落库，否则原样保留。
   */
  const [passphraseTouched, setPassphraseTouched] = useState(false);
  /** 浏览选中的外部密钥是否加密（密钥库里的条目直接看 KeyInfo.encrypted） */
  const [encryptedKey, setEncryptedKey] = useState(false);
  const [connectTimeout, setConnectTimeout] = useState("10");
  const [keys, setKeys] = useState<KeyInfo[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
        },
        authType === "password" ? password : "",
        // null 表示「没动过」：编辑会话时不能拿空串覆盖已保存的口令
        authType === "key" && passphraseTouched ? passphrase : null
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
        <h2>{editing ? "编辑会话" : "新建会话"}</h2>

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
 * 连接成功后的自动化：绑定一条宏 + 兼容模式。
 *
 * 以前这里是「跳板机登录路径」——只能录「在菜单里选序号」。现在登录路径
 * 只是宏的一种用法，用户还能顺手跑几条初始化命令、`tmux attach`、看日志…
 * 宏本身的录制与编辑在标签栏右侧的「宏」里，这里只负责「连上之后跑哪条」。
 *
 * 直接读写 `lib/sessionExtras` 的缓存（不走 onSave）：它不属于连接参数，
 * 改完立刻生效，不必点「保存」。
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
    // 折叠：编辑连接本来就有七八个字段，这块是次要信息。
    // 摘要行直接给出结论（绑了哪条宏 / 是否兼容模式），需要改再展开。
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
