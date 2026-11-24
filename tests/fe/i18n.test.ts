import { test } from "node:test";
import assert from "node:assert/strict";

import { __setLocaleForTest, locale, systemLocale, t } from "../../src/i18n";

test("测试环境默认中文（现有用例按中文断言）", () => {
  assert.equal(locale(), "zh");
});

test("t：按当前语言取一条，{name} 占位替换", () => {
  __setLocaleForTest("zh");
  assert.equal(t("已导入 {n} 个连接", "Imported {n} connections", { n: 3 }), "已导入 3 个连接");
  __setLocaleForTest("en");
  assert.equal(t("已导入 {n} 个连接", "Imported {n} connections", { n: 3 }), "Imported 3 connections");
  assert.equal(t("甲", "keep {missing} as is"), "keep {missing} as is", "没给的变量原样保留");
  __setLocaleForTest("zh");
});

test("systemLocale：zh 开头的用中文，其余英文，拿不到时中文", () => {
  assert.equal(systemLocale(["zh-CN", "en-US"]), "zh");
  assert.equal(systemLocale(["zh-Hant-TW"]), "zh");
  assert.equal(systemLocale(["en-US", "zh-CN"]), "en");
  assert.equal(systemLocale(["ja-JP"]), "en");
  assert.equal(systemLocale([]), "zh");
});
