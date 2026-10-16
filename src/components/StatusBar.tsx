interface Props {
  /** 未连接 / 连接中 / 已连接 */
  status: "idle" | "connecting" | "connected" | "error";
  /** 形如 root@192.168.1.10:22，未连接时为空 */
  peer: string | null;
  sessionCount: number;
}

const LABEL: Record<Props["status"], string> = {
  idle: "未连接",
  connecting: "连接中",
  connected: "已连接",
  error: "连接失败",
};

export default function StatusBar({ status, peer, sessionCount }: Props) {
  return (
    <footer className="statusbar">
      <span className={`sb-status sb-${status}`}>
        <span className="sb-dot" />
        {LABEL[status]}
      </span>
      <span className="sb-item">{peer ?? "—"}</span>
      <span className="sb-spacer" />
      <span className="sb-item">{sessionCount} 个会话</span>
    </footer>
  );
}
