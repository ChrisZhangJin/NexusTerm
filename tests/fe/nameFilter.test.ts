import { test } from "node:test";
import assert from "node:assert/strict";
import { nameMatcher } from "../../src/lib/nameFilter";

const hit = (pat: string, names: string[]) => names.filter(nameMatcher(pat));

test("无通配符：整名精确匹配，不分大小写", () => {
  assert.deepEqual(hit("07.txt", ["07.txt", "a07.txt", "07.txt.bak", "07Xtxt"]), ["07.txt"]);
  assert.deepEqual(hit("README.md", ["readme.md", "README.md.bak"]), ["readme.md"]);
});

test("*07.txt：以 07.txt 结尾", () => {
  assert.deepEqual(hit("*07.txt", ["07.txt", "a07.txt", "07.txt.bak", "x_07.txt"]), ["07.txt", "a07.txt", "x_07.txt"]);
});

test("前缀 / 包含 / 中间通配", () => {
  assert.deepEqual(hit("07*", ["07.txt", "a07", "0700"]), ["07.txt", "0700"]);
  assert.deepEqual(hit("*07*", ["a07b", "07", "x"]), ["a07b", "07"]);
  assert.deepEqual(hit("a*c", ["abc", "ac", "abcd"]), ["abc", "ac"]);
});

test("其余正则元字符按字面处理", () => {
  assert.deepEqual(hit("a.b*", ["a.b1", "axb1"]), ["a.b1"]);
  assert.deepEqual(hit("(1)*", ["(1) x", "1 x"]), ["(1) x"]);
  assert.deepEqual(hit("a?", ["a?", "ab"]), ["a?"]);
});

test("空输入 / 纯空白不过滤；单个 * 全部命中", () => {
  assert.equal(hit("", ["a", "b"]).length, 2);
  assert.equal(hit("   ", ["a", "b"]).length, 2);
  assert.equal(hit("*", ["a", "b"]).length, 2);
});
