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

import { historyQuery } from "../../src/lib/historyPanel";
import { feedKeys, newLineState } from "../../src/lib/complete";

// 回归（截图）：敲 `tmu` 按 Tab，bash 补成 `tmux `，再按 Shift+Tab —— 面板没有按 tmux 筛，列出了全部 200 条。
// Tab / ↑ / 方向键之后行状态是「未知」（我们看不到 shell 往行里塞了什么），预填关键字得改从屏幕上读
test("historyQuery：Tab 补全之后（行未知）从屏幕上提示符后面读出已敲的命令", () => {
  const line = newLineState();
  feedKeys(line, "tmu");
  feedKeys(line, "\t");
  assert.equal(line.known, false, "前提：Tab 之后行未知");
  assert.equal(historyQuery(line, "(base) chris@CHS-PowerEdge-R720:~$ tmux "), "tmux");
});

test("historyQuery：↑ 翻出来的行、各种提示符；已知的行照旧用逐键记下的；认不出提示符就不预填", () => {
  const unknown = { known: false, text: "" };
  assert.equal(historyQuery(unknown, "[root@web01 ~]# docker ps -a"), "docker ps -a");
  // `➜` 开头的提示符在每个词边界上都像提示符，分不出命令从哪开始：不预填（全部历史），不拿半截提示符去筛
  assert.equal(historyQuery(unknown, "➜  NexusTerm git:(main) git status"), "");
  assert.equal(historyQuery(unknown, "user@host:~/p$ "), "", "只有提示符：空关键字（全部历史）");
  assert.equal(historyQuery(unknown, "some output line without prompt"), "", "认不出提示符：不预填，不瞎猜");
  assert.equal(historyQuery({ known: true, text: "  kubectl get" }, "anything"), "kubectl get", "已知的行不读屏");
});

import { isUserTyping } from "../../src/lib/control";

test("终端自动应答不能喂进行状态：它会把行标成未知（TerminalView 按 isUserTyping 挡掉）", () => {
  const cpr = "\x1b[12;30R"; // 光标位置报告（DSR 6n 的应答）
  const line = newLineState();
  feedKeys(line, "tmux");
  assert.equal(isUserTyping(cpr), false, "这不是人敲的");
  const fed = newLineState();
  feedKeys(fed, "tmux");
  feedKeys(fed, cpr);
  assert.equal(fed.known, false, "喂进去的话行就成了未知 —— 所以必须挡在行状态外面");
  assert.deepEqual(line, { text: "tmux", known: true, pasting: false });
});
