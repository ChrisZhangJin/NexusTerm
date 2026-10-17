import { api } from "../api";
import type { AuthMethod, SessionRecord } from "../types";

/**
 * 按会话配置解析认证方式。
 *
 * 密码一律从系统凭据服务取（Keychain / 凭据管理器 / Secret Service），
 * 不读数据库 —— 这是密码永不落库的落地点。
 * 终端与 SFTP 两条通道共用此函数，避免认证规则两处漂移。
 */
export async function resolveAuth(
  s: SessionRecord,
  passphrase?: string
): Promise<AuthMethod> {
  if (s.authType === "password") {
    const pwd = await api.loadPassword(s.id);
    if (!pwd) throw new Error("未保存密码，请编辑会话并填写");
    return { kind: "password", password: pwd };
  }

  if (s.authType === "key") {
    if (!s.keyPath) throw new Error("该会话未配置私钥路径");
    return { kind: "privateKey", path: s.keyPath, passphrase };
  }

  return { kind: "agent" };
}
