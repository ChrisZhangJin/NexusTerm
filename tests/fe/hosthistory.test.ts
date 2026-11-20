/**
 * `lib/hostHistory` 的回归测试：实际改动分组、文件与备份对号、时间文字。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { changeGroups, changesEmpty, fileRows, fmtDuration, fmtSize, fmtWhen, historyStateLabel } from "../../src/lib/hostHistory";
import type { HostBackupEntry, HostChangeSummary } from "../../src/types";

const empty: HostChangeSummary = {
  filesWritten: [],
  commands: 0,
  packages: [],
  services: [],
  ports: [],
  containers: [],
  downloads: [],
};

const bk = (id: string, path: string): HostBackupEntry => ({ id, path, consoleId: "t1", tsMs: 0, size: 3, sha256: "x" });

test("空组不出现，什么都没有时 changesEmpty", () => {
  assert.equal(changesEmpty(empty), true);
  assert.deepEqual(changeGroups(empty), []);
  const c = { ...empty, commands: 2, packages: ["安装 nginx（apt）"], ports: ["放行 80（ufw）"] };
  assert.equal(changesEmpty(c), false);
  assert.deepEqual(
    changeGroups(c).map((g) => g.label),
    ["软件包", "端口 / 防火墙"]
  );
  assert.equal(changesEmpty({ ...empty, commands: 1 }), false);
});

test("文件与备份对上号：绝对路径相同 / 相对写法是后缀；没对上的备份补在后面", () => {
  const rows = fileRows(
    ["/etc/nginx/nginx.conf", "src/main.rs", "~/app/.env", "new.txt"],
    [bk("0001", "/etc/nginx/nginx.conf"), bk("0002", "/home/me/proj/src/main.rs"), bk("0003", "/home/me/app/.env"), bk("0004", "/opt/x.conf")]
  );
  assert.deepEqual(
    rows.map((r) => [r.path, r.backup?.id ?? null]),
    [
      ["/etc/nginx/nginx.conf", "0001"],
      ["src/main.rs", "0002"],
      ["~/app/.env", "0003"],
      ["new.txt", null],
      ["/opt/x.conf", "0004"],
    ]
  );
  // 绝对路径不按后缀乱配
  assert.equal(fileRows(["/a/main.rs"], [bk("1", "/b/a/main.rs")])[0].backup, undefined);
});

test("时长 / 时间 / 大小 / 状态文字", () => {
  assert.equal(fmtDuration(5_400), "5 秒");
  assert.equal(fmtDuration(72_000), "1 分 12 秒");
  assert.equal(fmtDuration(120_000), "2 分");
  assert.equal(fmtDuration(3_600_000 + 60_000 * 5), "1 小时 5 分");
  const ts = new Date(2026, 10, 20, 9, 5).getTime();
  assert.equal(fmtWhen(ts, ts), "11-20 09:05");
  assert.equal(fmtWhen(ts, new Date(2027, 0, 1).getTime()), "2026-11-20 09:05");
  assert.equal(fmtSize(512), "512 B");
  assert.equal(fmtSize(1536), "1.5 KB");
  assert.deepEqual(historyStateLabel("failed"), { label: "失败", tone: "bad" });
  assert.equal(historyStateLabel("completed").label, "完成");
});
