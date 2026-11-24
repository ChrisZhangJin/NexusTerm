import { useEffect, useState } from "react";
import { api } from "../api";
import { t, useLocale } from "../i18n";
import type { KeyInfo } from "../types";

/**
 * 用户密钥管理。两类条目混在一张表里，靠「来源」区分：
 * - 密钥库（vault）：私钥在应用数据目录，可删除、可导出
 * - `~/.ssh`（sshDir）：只是扫描出来方便直接选用；删除会牵连 git / ssh，故明确禁止，要用先「导入」复制一份。
 */
export default function KeyManagerDialog({ onClose }: { onClose: () => void }) {
  useLocale();
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
      title: t("导入私钥（OpenSSH / PuTTY .ppk）", "Import private key (OpenSSH / PuTTY .ppk)"),
      // 私钥文件通常没有扩展名，不设 filters
    });
    if (typeof picked !== "string") return;
    setBusy(true);
    try {
      const info = await api.importKey(picked);
      await refresh(info.id);
      setNotice(
        info.encrypted
          ? t("已导入「{name}」。该私钥带口令，首次连接时会询问。", "Imported \"{name}\". It has a passphrase, which you'll be asked for on first connect.", { name: info.name })
          : t("已导入「{name}」。", "Imported \"{name}\".", { name: info.name })
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
      title: t("导出私钥", "Export private key"),
      defaultPath: `${current.name}.pem`,
    });
    if (!dest) return;
    setBusy(true);
    try {
      await api.exportKey(current.id, dest);
      setNotice(t("已导出到 {dest}（同名 .pub 一并写出）", "Exported to {dest} (with a matching .pub)", { dest }));
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
      setError(
        t(
          "这是 ~/.ssh 里的密钥，本应用不会删除它。需要独立副本请先用「导入」复制一份。",
          "This key lives in ~/.ssh; NexusTerm won't delete it. Use Import to make a separate copy if you need one."
        )
      );
      return;
    }
    // 密钥删了还能重新生成或导入，但正在用它的会话会静默失效，所以必须问一次。
    if (!confirmDelete) {
      setConfirmDelete(true);
      setNotice(t("再点一次「删除」即永久删除「{name}」", "Click Delete again to permanently delete \"{name}\"", { name: current.name }));
      return;
    }
    setBusy(true);
    setConfirmDelete(false);
    try {
      await api.deleteKey(current.id);
      setSelected(null);
      await refresh();
      setNotice(t("已删除", "Deleted"));
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
      setNotice(t("公钥已复制，可粘贴到服务器的 ~/.ssh/authorized_keys", "Public key copied; paste it into ~/.ssh/authorized_keys on the server"));
    } catch (e) {
      setError(String(e));
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal key-modal" onClick={(e) => e.stopPropagation()}>
        <h2>
          {pane === "generate"
            ? t("生成密钥", "Generate key")
            : pane === "props"
              ? t("密钥属性", "Key properties")
              : t("密钥管理", "Key manager")}
        </h2>

        {/* 提示放在视图切换之外：否则在生成 / 属性子页里操作失败会完全看不到原因 */}
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
                  ? t(
                      "已生成「{name}」。请把公钥安装到服务器（此处可「复制公钥」）。",
                      "Generated \"{name}\". Install the public key on your server (use Copy public key here).",
                      { name: info.name }
                    )
                  : t(
                      "已生成「{name}」。私钥未加密，请妥善保管。",
                      "Generated \"{name}\". The private key isn't encrypted — keep it safe.",
                      { name: info.name }
                    )
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
              setNotice(t("已保存", "Saved"));
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
                      <th>{t("名称", "Name")}</th>
                      <th>{t("类型", "Type")}</th>
                      <th>{t("长度", "Bits")}</th>
                      <th>{t("注释", "Comment")}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {keys.length === 0 && (
                      <tr>
                        <td colSpan={4} className="key-empty">
                          {t(
                            "还没有密钥。可以用「生成」新建一个，或用「导入」把已有私钥加进来。",
                            "No keys yet. Generate a new one, or import an existing private key."
                          )}
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
                            {k.encrypted && <span title={t("已加密", "Encrypted")}>🔒</span>}
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
                  {t("生成…", "Generate…")}
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
                  {t("属性…", "Properties…")}
                </button>
                <button className="btn" onClick={doDelete} disabled={!current || busy}>
                  {t("删除", "Delete")}
                </button>
                <button className="btn" onClick={doImport} disabled={busy}>
                  {t("导入…", "Import…")}
                </button>
                <button className="btn" onClick={doExport} disabled={!current || busy}>
                  {t("导出…", "Export…")}
                </button>
                <button
                  className="btn"
                  onClick={copyPublic}
                  disabled={!current || busy}
                  title={t("复制公钥到剪贴板", "Copy the public key to the clipboard")}
                >
                  {t("复制公钥", "Copy public key")}
                </button>
              </div>
            </div>

            <p className="hint key-hint">
              {t(
                "支持 OpenSSH 与 PuTTY 的 .ppk（含带口令的 v2 / v3）。私钥存在应用数据目录，只有本人可读；~/.ssh 下的密钥只读引用，不会改动。生成或导入后，把公钥追加到服务器的 ~/.ssh/authorized_keys 即可。",
                "Supports OpenSSH and PuTTY .ppk (including passphrase-protected v2 / v3). Private keys are stored in the app data directory, readable only by you; keys in ~/.ssh are referenced read-only and never modified. After generating or importing, append the public key to ~/.ssh/authorized_keys on the server."
              )}
            </p>

            <div className="modal-actions">
              <button className="btn primary" onClick={onClose}>
                {t("关闭", "Close")}
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
  useLocale();
  const [algorithm, setAlgorithm] = useState<"ed25519" | "rsa">("ed25519");
  const [bits, setBits] = useState("4096");
  const [name, setName] = useState("");
  const [comment, setComment] = useState("");
  const [passphrase, setPassphrase] = useState("");
  const [confirm, setConfirm] = useState("");

  const submit = async () => {
    onError(null);
    if (passphrase && passphrase !== confirm) {
      onError(t("两次输入的口令不一致", "The passphrases don't match"));
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
        <label>{t("算法", "Algorithm")}</label>
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
          <label>{t("长度", "Bits")}</label>
          <select value={bits} onChange={(e) => setBits(e.target.value)} disabled={busy}>
            <option value="2048">{t("2048 位（兼容性最好）", "2048-bit (most compatible)")}</option>
            <option value="3072">{t("3072 位", "3072-bit")}</option>
            <option value="4096">{t("4096 位（推荐）", "4096-bit (recommended)")}</option>
          </select>
        </div>
      ) : (
        <p className="hint">
          {t(
            "ED25519 固定 256 位，比 RSA 2048 更安全且握手更快，老系统（OpenSSH < 6.5）可能不支持。",
            "ED25519 is always 256-bit, safer than RSA 2048 with faster handshakes; very old systems (OpenSSH < 6.5) may not support it."
          )}
        </p>
      )}

      <div className="row">
        <label>{t("名称", "Name")}</label>
        <input
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder={t("留空自动生成，如 id_ed25519-20261021", "Leave empty to generate, e.g. id_ed25519-20261021")}
          disabled={busy}
        />
      </div>
      <div className="row">
        <label>{t("注释", "Comment")}</label>
        <input
          value={comment}
          onChange={(e) => setComment(e.target.value)}
          placeholder={t("通常写 用户@主机，方便在 authorized_keys 里认出它", "Usually user@host, so you can spot it in authorized_keys")}
          disabled={busy}
        />
      </div>
      <div className="row">
        <label>{t("口令", "Passphrase")}</label>
        <input
          type="password"
          value={passphrase}
          onChange={(e) => setPassphrase(e.target.value)}
          placeholder={t("可选。留空则私钥明文保存", "Optional. Leave empty to store the key unencrypted")}
          disabled={busy}
        />
      </div>
      {passphrase && (
        <div className="row">
          <label>{t("确认口令", "Confirm passphrase")}</label>
          <input
            type="password"
            value={confirm}
            onChange={(e) => setConfirm(e.target.value)}
            disabled={busy}
          />
        </div>
      )}

      <p className="hint">
        {t(
          "生成只会把私钥写进本机密钥库。还需要把「复制公钥」得到的文本贴到服务器的 ~/.ssh/authorized_keys 里，服务器才会接受这把钥匙。",
          "Generating only stores the private key in the local key library. Paste the text from Copy public key into ~/.ssh/authorized_keys on the server before it will accept this key."
        )}
      </p>

      <div className="modal-actions">
        <button className="btn" onClick={onCancel} disabled={busy}>
          {t("取消", "Cancel")}
        </button>
        <button className="btn primary" onClick={submit} disabled={busy}>
          {busy ? t("生成中…（RSA 4096 可能需要几秒）", "Generating… (RSA 4096 may take a few seconds)") : t("生成", "Generate")}
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
  useLocale();
  const [name, setName] = useState(info.name);
  const [comment, setComment] = useState(info.comment);
  const [busy, setBusy] = useState(false);

  const save = async () => {
    onError(null);
    if (!name.trim()) {
      onError(t("名称不能为空", "Name is required"));
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
        <label>{t("名称", "Name")}</label>
        <input value={name} onChange={(e) => setName(e.target.value)} disabled={busy} />
      </div>
      <div className="row">
        <label>{t("注释", "Comment")}</label>
        <input value={comment} onChange={(e) => setComment(e.target.value)} disabled={busy} />
      </div>
      <div className="key-props">
        <span>{t("类型", "Type")}</span>
        <b>
          {info.keyType} {info.bits ? t("{n} 位", "{n}-bit", { n: info.bits }) : ""}
        </b>
        <span>{t("指纹", "Fingerprint")}</span>
        <b className="key-fp">{info.fingerprint}</b>
        <span>{t("位置", "Location")}</span>
        <b className="key-fp">
          {info.source === "sshDir" ? t("~/.ssh（只读引用）", "~/.ssh (read-only reference)") : t("应用密钥库", "App key library")}
        </b>
        <span>{t("私钥", "Private key")}</span>
        <b className="key-fp">{info.path}</b>
      </div>
      <div className="auth-field">
        <label>{t("公钥", "Public key")}</label>
        <textarea className="key-pub" readOnly value={info.publicKey} rows={3} />
      </div>

      <div className="modal-actions">
        <button className="btn" onClick={onCancel} disabled={busy}>
          {t("返回", "Back")}
        </button>
        <button className="btn primary" onClick={save} disabled={busy}>
          {busy ? t("保存中…", "Saving…") : t("保存", "Save")}
        </button>
      </div>
    </>
  );
}
