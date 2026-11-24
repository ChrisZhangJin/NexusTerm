/**
 * 主窗口 / 终端 / 连接 / 保险库这一块的纯函数文案：切到英文后输出英文，切回中文不变。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { __setLocaleForTest } from "../../src/i18n";
import { describeExport, describeImport, validateExportPassword } from "../../src/lib/connMigrate";
import { toFriendlyError } from "../../src/lib/errors";
import { restoreNotice } from "../../src/lib/context";
import { isSidecarDenied } from "../../src/lib/sessionExtras";
import { MARK_COLORS } from "../../src/lib/highlight";

// lib/prefs 模块顶层会读 localStorage：先垫上再动态 import（同 filemanager.test.ts）
const store = new Map<string, string>();
(globalThis as unknown as { localStorage: Storage }).localStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
  clear: () => store.clear(),
  key: () => null,
  get length() {
    return store.size;
  },
} as Storage;
const { SHORTCUT_LABELS, validateCombo } = await import("../../src/lib/prefs");

const withEn = (fn: () => void) => {
  __setLocaleForTest("en");
  try {
    fn();
  } finally {
    __setLocaleForTest("zh");
  }
};

test("connMigrate：英文文案", () => {
  withEn(() => {
    assert.equal(validateExportPassword("short", "short"), "Export password must be at least 8 characters.");
    assert.equal(validateExportPassword("12345678", "x2345678"), "The export passwords don't match.");
    const exp = describeExport(
      { sessions: 3, passwords: 2, passphrases: 0, totpSeeds: 1, keys: 0, warnings: [] },
      "/tmp/a.nexusconn"
    );
    assert.equal(exp.text, "Exported 3 connection(s) (including 2 password(s), 1 TOTP seed(s)) to /tmp/a.nexusconn.");
    const imp = describeImport({
      imported: 2,
      skipped: 1,
      passwords: 0,
      passphrases: 0,
      totpSeeds: 0,
      keys: 0,
      warnings: [],
    } as unknown as Parameters<typeof describeImport>[0]);
    assert.match(imp.text, /^Imported 2 connection\(s\); 1 duplicate/);
  });
  assert.match(validateExportPassword("short", "short")!, /至少 8/);
});

test("errors：RAW_SUFFIX 分隔符不随语言变，兜底文案跟语言走", () => {
  withEn(() => {
    const r = toFriendlyError("Connection refused\n\n原始错误：os error 61");
    assert.deepEqual(r, { message: "Connection refused", detail: "os error 61" });
    assert.match(toFriendlyError("os error 60").message, /^Connection timed out/);
    assert.equal(toFriendlyError("").message, "Unknown error");
  });
  assert.equal(toFriendlyError("").message, "未知错误");
});

test("context.restoreNotice：英文", () => {
  withEn(() => {
    const rows = restoreNotice({ cwd: "/srv", tmux: "main", at: 0 } as Parameters<typeof restoreNotice>[0], "reconnect");
    assert.deepEqual(rows, [
      ["Directory", "/srv"],
      ["tmux", "Reattach session main"],
    ]);
  });
});

test("isSidecarDenied：中英文后端文案都认", () => {
  assert.equal(isSidecarDenied(new Error("该服务器不支持并行通道")), true);
  assert.equal(isSidecarDenied("The server does not allow parallel channels (bastion host)"), true);
  assert.equal(isSidecarDenied("connection timed out"), false);
});

test("prefs / highlight：标签按当前语言取", () => {
  assert.equal(SHORTCUT_LABELS.copy, "复制");
  assert.equal(MARK_COLORS[0].label, "高亮黄");
  withEn(() => {
    assert.equal(SHORTCUT_LABELS.copy, "Copy");
    assert.equal(MARK_COLORS[0].label, "Yellow");
    assert.match(validateCombo("A")!, /^Must include a Ctrl \/ Alt/);
  });
});
