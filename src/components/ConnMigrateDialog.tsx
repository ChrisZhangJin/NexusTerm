import { useEffect, useRef, useState, type ReactElement } from "react";
import { api } from "../api";
import {
  describeExport,
  describeImport,
  exportFileName,
  CONN_FILE_EXT,
  validateExportPassword,
  type MigrateReport,
} from "../lib/connMigrate";

export type ConnMigrateMode = { kind: "export" } | { kind: "import"; path: string };

/**
 * 连接迁移的密码弹窗。导出要「设一个导出密码」（输两遍），导入只要「输入导出密码」。
 *
 * 这个密码和主密码毫无关系：文件要能脱离本机、在另一台电脑上打开。
 * 凭据明文从头到尾只在 Rust 侧，这里只经手导出密码和文件路径。
 */
export default function ConnMigrateDialog({
  mode,
  onClose,
  onDone,
}: {
  mode: ConnMigrateMode;
  onClose: () => void;
  /** 成功后回调；导入成功时调用方负责刷新连接列表 */
  onDone: (report: MigrateReport) => void;
}): ReactElement {
  const isExport = mode.kind === "export";
  const [password, setPassword] = useState("");
  const [confirm, setConfirm] = useState("");
  // 默认带私钥：不带的话新电脑上密钥认证的连接登不上。但它是整个文件里最敏感的东西，所以警告常驻
  const [includeKeys, setIncludeKeys] = useState(true);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const pwRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    pwRef.current?.focus();
  }, []);

  // Escape 取消。用捕获阶段，理由同 MasterPasswordDialog；忙碌时不响应，
  // 免得写文件写到一半界面先关了。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.preventDefault();
      e.stopPropagation();
      if (!busy) onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose, busy]);

  const submit = async () => {
    if (busy) return;
    if (isExport) {
      const bad = validateExportPassword(password, confirm);
      if (bad) {
        setErr(bad);
        return;
      }
    } else if (!password) {
      setErr("请输入导出密码。");
      return;
    }
    setErr("");
    setBusy(true);
    try {
      if (isExport) {
        const { save } = await import("@tauri-apps/plugin-dialog");
        const path = await save({
          title: "导出全部连接",
          defaultPath: exportFileName(),
          filters: [{ name: "NexusTerm 连接迁移文件", extensions: [CONN_FILE_EXT] }],
        });
        if (!path) {
          setBusy(false);
          return;
        }
        const r = await api.connectionsExport(path, password, includeKeys);
        onDone(describeExport(r, path));
      } else {
        const r = await api.connectionsImport(mode.path, password);
        onDone(describeImport(r));
      }
    } catch (e) {
      // 保留已输入的内容：打错一个字符就被清空很烦人
      setErr(String(e));
      setBusy(false);
      pwRef.current?.focus();
      pwRef.current?.select();
    }
  };

  return (
    <div className="modal-backdrop">
      <form
        className="modal"
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <h2>{isExport ? "导出全部连接" : "导入连接"}</h2>

        {isExport ? (
          <p className="hint" style={{ margin: "0 0 12px" }}>
            设一个<strong>导出密码</strong>
            来加密这个文件。它和主密码无关，也不会被保存 —— 到新电脑导入时要用它，忘了就打不开。
          </p>
        ) : (
          <p className="hint" style={{ margin: "0 0 12px", wordBreak: "break-all" }}>
            文件：{mode.path}
          </p>
        )}

        <div className="row">
          <label htmlFor="mig-pw">导出密码</label>
          <input
            id="mig-pw"
            ref={pwRef}
            type="password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
            autoComplete={isExport ? "new-password" : "current-password"}
            disabled={busy}
          />
        </div>
        {isExport && (
          <div className="row">
            <label htmlFor="mig-pw2">再输一遍</label>
            <input
              id="mig-pw2"
              type="password"
              value={confirm}
              onChange={(e) => setConfirm(e.target.value)}
              autoComplete="new-password"
              disabled={busy}
            />
          </div>
        )}

        {isExport && (
          <>
            <label className="settings-check">
              <input
                type="checkbox"
                checked={includeKeys}
                onChange={(e) => setIncludeKeys(e.target.checked)}
                disabled={busy}
              />
              <span>
                包含私钥文件
                <em>勾选后密钥认证的连接在新电脑上可直接登录。</em>
              </span>
            </label>
            {includeKeys && (
              <div className="vault-danger">
                <strong>文件里会带上你的私钥。</strong>
                拿到这个文件<strong>并且</strong>知道导出密码的人，可以登录这些连接对应的服务器。
                导出密码请单独告知自己，不要和文件放在一起。
              </div>
            )}
          </>
        )}

        {err && <p className="error">{err}</p>}

        <div className="modal-actions">
          <button type="button" className="btn" onClick={onClose} disabled={busy}>
            取消
          </button>
          <button type="submit" className="btn primary" disabled={busy}>
            {busy ? (isExport ? "导出中…" : "导入中…") : isExport ? "选择位置并导出…" : "导入"}
          </button>
        </div>
      </form>
    </div>
  );
}
