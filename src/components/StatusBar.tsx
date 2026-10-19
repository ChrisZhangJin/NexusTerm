interface Props {
  /** 当前激活标签的状态 */
  status: "idle" | "connecting" | "connected" | "reconnecting" | "error";
  /** 形如 root@192.168.1.10:22，未连接时为空 */
  peer: string | null;
  /** 当前打开的标签数 */
  sessionCount: number;
  /** 其中处于已连接状态的数量 */
  connectedCount: number;
}

const LABEL: Record<Props["status"], string> = {
  idle: "未连接",
  connecting: "连接中",
  connected: "已连接",
  reconnecting: "重连中",
  error: "连接失败",
};

export default function StatusBar({ status, peer, sessionCount, connectedCount }: Props) {
  return (
    <footer className="statusbar">
      <span className={`sb-status sb-${status}`}>
        {status === "reconnecting" ? <span className="spinner sm" /> : <span className="sb-dot" />}
        {LABEL[status]}
      </span>
      <span className="sb-item">{peer ?? "—"}</span>
      <span className="sb-spacer" />
      <span className="sb-item" title={`已打开 ${sessionCount} 个标签，其中 ${connectedCount} 个在线`}>
        {sessionCount} 会话
        {connectedCount !== sessionCount ? `（${connectedCount} 在线）` : ""}
      </span>
    </footer>
  );
}
