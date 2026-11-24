import { test } from "node:test";
import assert from "node:assert/strict";

import { mediaErrorDetail } from "../../src/viewer/mediaError";
import { __setLocaleForTest } from "../../src/i18n";

const err = (code: number, message = "") => ({ code, message }) as MediaError;

test("中文：错误码文案，附带原始报错", () => {
  assert.equal(mediaErrorDetail(null), "未知错误");
  assert.equal(mediaErrorDetail(err(4)), "格式或编码不受支持");
  assert.equal(mediaErrorDetail(err(3, " missing plugin ")), "解码失败（文件可能已损坏）（missing plugin）");
  assert.equal(mediaErrorDetail(err(9)), "错误码 9");
});

test("英文：错误码文案，附带原始报错", () => {
  __setLocaleForTest("en");
  try {
    assert.equal(mediaErrorDetail(null), "Unknown error");
    assert.equal(mediaErrorDetail(err(4, "no decoder")), "Format or codec not supported (no decoder)");
    assert.equal(mediaErrorDetail(err(9)), "Error code 9");
  } finally {
    __setLocaleForTest("zh");
  }
});
