import { useEffect, useState } from "react";
import { api } from "../api";
import type { KeyInfo } from "../types";

/**
 * 用户密钥管理。
 *
 * 对应 Xshell 的「用户密钥」窗口：一张列表（名称 / 类型 / 长度 / 注释）
 * 加右侧一排操作（生成、属性、删除、导入、导出）。
 *
 * 两类条目混在一张表里，靠「来源」区分：
 * - 密钥库（vault）：私钥在应用数据目录，可删除、可导出
 * - `~/.ssh`（sshDir）：只是被扫描出来方便直接选用，删除会牵连 git / ssh，
 *   这里明确禁止，要用只能先「导入」复制一份。
 */
export default function KeyManagerDialog({ onClose }: { onClose: () => void }) {
  const [keys, setKeys] = useState<KeyInfo[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  /** 删除需要二次确认。WebView2 下 window.confirm 可能被吞掉，只能自己做 */
  const [confirmDelete, setConfirmDelete] = useState(false);
  /** null = 列表视图；generate / props = 内嵌表单（不叠第二层模态） */
  const [pane, setPane] = useState<null | "generate" | "props">(null);

  const current = keys.find((k) => k.id === selected) ?? null;

  const refresh = async (keep?: string) => {
    try {
      const all = await api.listKeys();
      setKeys(all);
      // 刷新后尽量保持在原来那一条上，别让用户重新找
      const target = keep ?? selected;
      if (target && all.some((k) => k.id === target)) setSelected(target);
      else setSelected(all[0]?.id ?? null);
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        if (pane) setPane(null);
        else onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [pane, onClose]);

  const doImport = async () => {
    setError(null);
    setNotice(null);
    setConfirmDelete(false);
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      multiple: false,
      directory: false,
      title: "导入私钥（OpenSSH / PuTTY .ppk）",
      // 私钥文件通常没有扩展名，不设 filters
    });
    if (typeof picked !== "string") return;
    setBusy(true);
    try {
      const info = await api.importKey(picked);
      await refresh(info.id);
      setNotice(
        info.encrypted
          ? `已导入「${info.name}」。该私钥带口令，首次连接时会询问。`
          : `已导入「${info.name}」。`
      );
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doExport = async () => {
    if (!current) return;
    setError(null);
    setNotice(null);
    setConfirmDelete(false);
    const { save } = await import("@tauri-apps/plugin-dialog");
    const dest = await save({
      title: "导出私钥",
      defaultPath: `${current.name}.pem`,
    });
    if (!dest) return;
    setBusy(true);
    try {
      await api.exportKey(current.id, dest);
      setNotice(`已导出到 ${dest}（同名 .pub 一并写出）`);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const doDelete = async () => {
    if (!current) return;
    setError(null);
    setNotice(null);
    if (current.source === "sshDir") {
      setError("这是 ~/.ssh 里的密钥，本应用不会删除它。需要独立副本请先用「导入」复制一份。");
      return;
    }
    // 密钥删了还能重新生成或导入，但正在用它的会话会静默失效，所以必须问一次。
    if (!confirmDelete) {
      setConfirmDelete(true);
      setNotice(`再点一次「删除」即永久删除「${current.name}」`);
      return;
    }
    setBusy(true);
    setConfirmDelete(false);
    try {
      await api.deleteKey(current.id);
      setSelected(null);
      await refresh();
      setNotice("已删除");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const copyPublic = async () => {
    if (!current) return;
    setError(null);
    try {
      await api.clipboardWrite(current.publicKey);
      setNotice("公钥已复制，可粘贴到服务器的 ~/.ssh/authorized_keys");
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal key-modal" onClick={(e) => e.stopPropagation()}>
        <h2>{pane === "generate" ? "生成密钥" : pane === "props" ? "密钥属性" : "密钥管理"}</h2>

        {/* 提示放在视图切换之外：生成 / 属性是内嵌子页，
            错误若只渲染在列表页，用户在子页里操作失败会完全看不到原因 */}
        {error && <p className="error">{error}</p>}
        {notice && <p className="key-notice">{notice}</p>}

        {pane === "generate" ? (
          <GeneratePane
            busy={busy}
            onCancel={() => {
              setError(null);
              setPane(null);
            }}
            onDone={async (info) => {
              setPane(null);
              await refresh(info.id);
              setNotice(
                info.encrypted
                  ? `已生成「${info.name}」。请把公钥安装到服务器（此处可「复制公钥」）。`
                  : `已生成「${info.name}」。私钥未加密，请妥善保管。`
              );
            }}
            onError={setError}
            onBusy={setBusy}
          />
        ) : pane === "props" && current ? (
          <PropsPane
            info={current}
            onCancel={() => {
              setError(null);
              setPane(null);
            }}
            onSaved={async () => {
              setPane(null);
              await refresh(current.id);
              setNotice("已保存");
            }}
            onError={setError}
          />
        ) : (
          <>
            <div className="key-body">
              <div className="key-table-wrap">
                <table className="key-table">
                  <thead>
                    <tr>
                      <th>名称</th>
                      <th>类型</th>
                      <th>长度</th>
                      <th>注释</th>
                    </tr>
                  </thead>
                  <tbody>
                    {keys.length === 0 && (
                      <tr>
                        <td colSpan={4} className="key-empty">
                          还没有密钥。可以用「生成」新建一个，或用「导入」把已有私钥加进来。
                        </td>
                      </tr>
                    )}
                    {keys.map((k) => (
                      <tr
                        key={k.id}
                        className={k.id === selected ? "active" : ""}
                        onClick={() => {
                          setSelected(k.id);
                          setConfirmDelete(false);
                        }}
                        onDoubleClick={() => {
                          setSelected(k.id);
                          setPane("props");
                        }}
                      >
                        <td>
                          <span className="key-name">
                            {k.encrypted && <span title="已加密">🔒</span>}
                            {k.name}
                          </span>
                          {k.source === "sshDir" && <span className="key-badge">~/.ssh</span>}
                        </td>
                        <td>{k.keyType}</td>
                        <td>{k.bits || "-"}</td>
                        <td className="key-comment" title={k.comment}>
                          {k.comment || "-"}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>

              <div className="key-actions">
                <button
                  className="btn"
                  onClick={() => {
                    setError(null);
                    setNotice(null);
                    setPane("generate");
                  }}
                  disabled={busy}
                >
                  生成…
                </button>
                <button
                  className="btn"
                  onClick={() => {
                    setError(null);
                    setNotice(null);
                    setPane("props");
                  }}
                  disabled={!current || busy}
                >
                  属性…
                </button>
                <button className="btn" onClick={doDelete} disabled={!current || busy}>
                  删除
                </button>
                <button className="btn" onClick={doImport} disabled={busy}>
                  导入…
                </button>
                <button className="btn" onClick={doExport} disabled={!current || busy}>
                  导出…
                </button>
                <button
                  className="btn"
                  onClick={copyPublic}
                  disabled={!current || busy}
                  title="复制公钥到剪贴板"
                >
                  复制公钥
                </button>
              </div>
            </div>

            <p className="hint key-hint">
              支持 OpenSSH 与 PuTTY 的 .ppk（含带口令的 v2 / v3）。私钥存在应用数据目录，
              只有本人可读；~/.ssh 下的密钥只读引用，不会改动。生成或导入后，
              把公钥追加到服务器的 ~/.ssh/authorized_keys 即可。
            </p>

            <div className="modal-actions">
              <button className="btn primary" onClick={onClose}>
                关闭
              </button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}

/** 生成表单 */
function GeneratePane({
  busy,
  onCancel,
  onDone,
  onError,
  onBusy,
}: {
  busy: boolean;
  onCancel: () => void;
  onDone: (info: KeyInfo) => void;
  onError: (msg: string | null) => void;
  onBusy: (v: boolean) => void;
}) {
  const [algorithm, setAlgorithm] = useState<"ed25519" | "rsa">("ed25519");
  const [bits, setBits] = useState("4096");
  const [name, setName] = useState("");
  const [comment, setComment] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [confirm, setConfirm] = useState("");

  const submit = async () => {
    onError(null);
    if (passphrase && passphrase !== confirm) {
      onError("两次输入的口令不一致");
      return;
    }
    onBusy(true);
    try {
      const info = await api.generateKey({
        algorithm,
        bits: algorithm === "rsa" ? Number(bits) : undefined,
        name: name.trim() || undefined,
        comment: comment.trim() || undefined,
        passphrase: passphrase || undefined,
      });
      onDone(info);
    } catch (e) {
      onError(String(e));
    } finally {
      onBusy(false);
    }
  };

  return (
    <>
      <div className="row">
        <label>算法</label>
        <div className="seg">
          <button
            className={`seg-btn ${algorithm === "ed25519" ? "active" : ""}`}
            onClick={() => setAlgorithm("ed25519")}
            disabled={busy}
          >
            ED25519
          </button>
          <button
            className={`seg-btn ${algorithm === "rsa" ? "active" : ""}`}
            onClick={() => setAlgorithm("rsa")}
            disabled={busy}
          >
            RSA
          </button>
        </div>
      </div>

      {algorithm === "rsa" ? (
        <div className="row">
          <label>长度</label>
          <select value={bits} onChange={(e) => setBits(e.target.value)} disabled={busy}>
            <option value="2048">2048 位（兼容性最好）</option>
            <option value="3072">3072 位</option>
            <option value="4096">4096 位（推荐）</option>
          </select>
        </div>
      ) : (
        <p className="hint">
          ED25519 固定 256 位，比 RSA 2048 更安全且握手更快，老系统（OpenSSH &lt; 6.5）可能不支持。
        </p>
      )}

      <div className="row">
        <label>名称</label>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="留空自动生成，如 id_ed25519-20261021"
          disabled={busy}
        />
      </div>
      <div className="row">
        <label>注释</label>
        <input
          value={comment}
          onChange={(e) => setComment(e.target.value)}
          placeholder="通常写 用户@主机，方便在 authorized_keys 里认出它"
          disabled={busy}
        />
      </div>
      <div className="row">
        <label>口令</label>
        <input
          type="password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          placeholder="可选。留空则私钥明文保存"
          disabled={busy}
        />
      </div>
      {passphrase && (
        <div className="row">
          <label>确认口令</label>
          <input
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            disabled={busy}
          />
        </div>
      )}

      <p className="hint">
        生成只会把私钥写进本机密钥库。还需要把「复制公钥」得到的文本贴到服务器的
        ~/.ssh/authorized_keys 里，服务器才会接受这把钥匙。
      </p>

      <div className="modal-actions">
        <button className="btn" onClick={onCancel} disabled={busy}>
          取消
        </button>
        <button className="btn primary" onClick={submit} disabled={busy}>
          {busy ? "生成中…（RSA 4096 可能需要几秒）" : "生成"}
        </button>
      </div>
    </>
  );
}

/** 属性：名称与注释可改，其余只读 */
function PropsPane({
  info,
  onCancel,
  onSaved,
  onError,
}: {
  info: KeyInfo;
  onCancel: () => void;
  onSaved: () => void;
  onError: (msg: string | null) => void;
}) {
  const [name, setName] = useState(info.name);
  const [comment, setComment] = useState(info.comment);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    onError(null);
    if (!name.trim()) {
      onError("名称不能为空");
      return;
    }
    setBusy(true);
    try {
      await api.updateKeyMeta(info.id, name.trim(), comment);
      onSaved();
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <div className="row">
        <label>名称</label>
        <input value={name} onChange={(e) => setName(e.target.value)} disabled={busy} />
      </div>
      <div className="row">
        <label>注释</label>
        <input value={comment} onChange={(e) => setComment(e.target.value)} disabled={busy} />
      </div>
      <div className="key-props">
        <span>类型</span>
        <b>
          {info.keyType} {info.bits || ""} 位
        </b>
        <span>指纹</span>
        <b className="key-fp">{info.fingerprint}</b>
        <span>位置</span>
        <b className="key-fp">
          {info.source === "sshDir" ? "~/.ssh（只读引用）" : "应用密钥库"}
        </b>
        <span>私钥</span>
        <b className="key-fp">{info.path}</b>
      </div>
      <div className="auth-field">
        <label>公钥</label>
        <textarea className="key-pub" readOnly value={info.publicKey} rows={3} />
      </div>

      <div className="modal-actions">
        <button className="btn" onClick={onCancel} disabled={busy}>
          返回
        </button>
        <button className="btn primary" onClick={save} disabled={busy}>
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
    </>
  );
}
