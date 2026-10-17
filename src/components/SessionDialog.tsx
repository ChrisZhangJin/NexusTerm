import { useEffect, useState } from "react";
import { api } from "../api";
import { normalizeTimeout } from "../lib/errors";
import { credentialServiceName, sshDirLabel } from "../lib/platform";
import type { DiscoveredKey, NewSession, SessionRecord } from "../types";

interface Props {
  editing: SessionRecord | null;
  onSave: (payload: NewSession, password: string) => Promise<void>;
  onClose: () => void;
}

export default function SessionDialog({ editing, onSave, onClose }: Props) {
  const [name, setName] = useState("");
  const [folder, setFolder] = useState("/");
  const [host, setHost] = useState("");
  const [port, setPort] = useState("22");
  const [username, setUsername] = useState("");
  const [authType, setAuthType] = useState<"password" | "key" | "agent">("password");
  const [password, setPassword] = useState("");
  const [keyPath, setKeyPath] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [connectTimeout, setConnectTimeout] = useState("10");
  const [keys, setKeys] = useState<DiscoveredKey[]>([]);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api.listSshKeys().then(setKeys).catch(() => {});
    if (editing) {
      setName(editing.name);
      setFolder(editing.folder);
      setHost(editing.host);
      setPort(String(editing.port));
      setUsername(editing.username);
      setAuthType(editing.authType);
      setKeyPath(editing.keyPath ?? "");
      setConnectTimeout(String(normalizeTimeout(editing.connectTimeout)));
    } else {
      setName("");
      setFolder("/");
      setHost("");
      setPort("22");
      setUsername("");
      setAuthType("password");
      setPassword("");
      setKeyPath("");
      setConnectTimeout("10");
    }
  }, [editing]);

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
          folder: folder.trim() || "/",
          host: host.trim(),
          port: Number(port) || 22,
          username: username.trim(),
          authType,
          keyPath: authType === "key" ? keyPath : undefined,
          connectTimeout: normalizeTimeout(Number(connectTimeout)),
        },
        password
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
          <input value={folder} onChange={(e) => setFolder(e.target.value)} placeholder="/" />
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
              placeholder={`保存在${credentialServiceName()}，不写入数据库`}
            />
          </div>
        )}

        {authType === "key" && (
          <>
            <div className="row">
              <label>私钥</label>
              <select value={keyPath} onChange={(e) => setKeyPath(e.target.value)}>
                <option value="">选择 {sshDirLabel()} 下的密钥…</option>
                {keys.map((k) => (
                  <option key={k.path} value={k.path}>
                    {k.name}
                    {k.encrypted ? " 🔒" : ""} · {k.keyType}
                  </option>
                ))}
              </select>
            </div>
            {keys.find((k) => k.path === keyPath)?.encrypted && (
              <div className="row">
                <label>私钥密码</label>
                <input
                  type="password"
                  value={passphrase}
                  onChange={(e) => setPassphrase(e.target.value)}
                  placeholder="该私钥已加密"
                />
              </div>
            )}
          </>
        )}

        {authType === "agent" && (
          <p className="hint">使用系统 ssh-agent 中的凭据。需先在终端执行 ssh-add。</p>
        )}

        {error && <p className="error">{error}</p>}

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
