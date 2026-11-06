/**
 * `lib/connMigrate`（连接迁移）的回归测试：导出密码校验、默认文件名、结果文案。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  describeExport,
  describeImport,
  exportFileName,
  validateExportPassword,
} from "../../src/lib/connMigrate";

test("导出密码：太短 / 不一致被拒，合格通过", () => {
  assert.match(validateExportPassword("short", "short")!, /至少 8/);
  assert.match(validateExportPassword("12345678", "12345679")!, /不一致/);
  assert.equal(validateExportPassword("12345678", "12345678"), null);
});

test("导出密码按字符数计，不按 UTF-16 单元（与后端 chars().count() 一致）", () => {
  // 4 个 emoji = 8 个 UTF-16 单元，但只有 4 个字符 —— 后端会拒，前端也必须拒
  assert.notEqual(validateExportPassword("😀😀😀😀", "😀😀😀😀"), null);
  assert.equal(validateExportPassword("密码密码密码密码", "密码密码密码密码"), null);
});

test("默认文件名带日期与 .nexusconn 扩展名", () => {
  assert.equal(exportFileName(new Date(2026, 0, 5)), "nexus-connections-20260105.nexusconn");
});

test("导出结果文案列出各类凭据数量，有警告时 warn=true", () => {
  const r = describeExport(
    { sessions: 3, passwords: 2, passphrases: 0, totpSeeds: 1, keys: 1, warnings: ["私钥 x：未嵌入"] },
    "/tmp/a.nexusconn"
  );
  assert.match(r.text, /3 条连接/);
  assert.match(r.text, /2 个密码/);
  assert.doesNotMatch(r.text, /私钥口令/);
  assert.match(r.text, /1 个 TOTP 种子/);
  assert.equal(r.warn, true);
  assert.deepEqual(r.details, ["私钥 x：未嵌入"]);
});

test("导入结果文案说明跳过了重复连接", () => {
  const r = describeImport({
    imported: 2, skipped: 3, passwords: 2, passphrases: 0, totpSeeds: 0, keys: 0, warnings: [],
  });
  assert.match(r.text, /导入 2 条/);
  assert.match(r.text, /3 条与已有连接重复/);
  assert.equal(r.warn, false);
});
