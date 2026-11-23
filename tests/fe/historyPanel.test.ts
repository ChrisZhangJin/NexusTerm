import { test } from "node:test";
import assert from "node:assert/strict";

import { replaceLineInput, searchHistory } from "../../src/lib/historyPanel";
import { historyItems, recordCommand } from "../../src/lib/complete";

const H = ["tmux attach -t nexus", "ls -la", "tmux ls", "kubectl get pods", "docker exec -it web bash", "echo tmux"];

test("searchHistory：空关键字给全部（最近的在前）", () => {
  assert.deepEqual(searchHistory(H, ""), H);
  assert.deepEqual(searchHistory(H, "   "), H);
});

test("searchHistory：以关键字开头的在前，包含的在后，组内保持最近的在前", () => {
  assert.deepEqual(searchHistory(H, "tmux"), ["tmux attach -t nexus", "tmux ls", "echo tmux"]);
  assert.deepEqual(searchHistory(H, "TMUX"), ["tmux attach -t nexus", "tmux ls", "echo tmux"], "不区分大小写");
});

test("searchHistory：多个词都要出现", () => {
  assert.deepEqual(searchHistory(H, "exec web"), ["docker exec -it web bash"]);
  assert.deepEqual(searchHistory(H, "tmux att"), ["tmux attach -t nexus"]);
  assert.deepEqual(searchHistory(H, "nothing here"), []);
});

test("searchHistory：上限", () => {
  const many = Array.from({ length: 500 }, (_, i) => `cmd ${i}`);
  assert.equal(searchHistory(many, "").length, 200);
  assert.equal(searchHistory(many, "cmd", 10).length, 10);
});

test("replaceLineInput：知道敲了什么就退格删掉；不知道就 Ctrl-E + Ctrl-U；都不带回车", () => {
  assert.equal(replaceLineInput("tmux", "tmux ls"), "\x7f\x7f\x7f\x7ftmux ls");
  assert.equal(replaceLineInput("", "ls"), "ls");
  assert.equal(replaceLineInput("中文", "ls"), "\x7f\x7fls", "按字符数退格");
  assert.equal(replaceLineInput(null, "ls"), "\x05\x15ls");
  assert.ok(!replaceLineInput("a", "b").includes("\r"));
});

test("historyItems：最近用过的在前，返回副本", () => {
  recordCommand("hp-test", "a");
  recordCommand("hp-test", "b");
  recordCommand("hp-test", "a");
  const got = historyItems("hp-test");
  assert.deepEqual(got, ["a", "b"]);
  got.push("x");
  assert.deepEqual(historyItems("hp-test"), ["a", "b"]);
});
