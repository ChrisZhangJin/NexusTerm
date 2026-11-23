import { test } from "node:test";
import assert from "node:assert/strict";

import { opensLink } from "../../src/lib/links";

test("opensLink：Windows / Linux 按 Ctrl，macOS 按 ⌘；单纯点击不打开", () => {
  assert.equal(opensLink({ ctrlKey: true, metaKey: false }, false), true);
  assert.equal(opensLink({ ctrlKey: false, metaKey: false }, false), false);
  assert.equal(opensLink({ ctrlKey: false, metaKey: true }, true), true);
  // macOS 上 Ctrl+点击是右键菜单的习惯，不当打开链接
  assert.equal(opensLink({ ctrlKey: true, metaKey: false }, true), false);
});
