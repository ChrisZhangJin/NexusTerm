/**
 * 灰字在**真实 xterm 解析器**（@xterm/headless）上的回归测试。
 *
 * 其它灰字用例用的是假终端，不解析转义、写入也是同步的，抓不到这一类 bug：
 * xterm 的 `write()` 是异步的，缓冲里的光标要等之前排队的数据解析完才更新。
 */

import assert from "node:assert/strict";
import test from "node:test";
import { Terminal } from "@xterm/headless";

import { GhostText } from "../../src/lib/ghost";

const flush = (t: Terminal) => new Promise<void>((r) => t.write("", r));
const row = (t: Terminal, y: number) => t.buffer.active.getLine(y)!.translateToString(true);

async function typedThenEnter(echoPending: boolean) {
  const term = new Terminal({ cols: 80, rows: 10, allowProposedApi: true });
  term.write("$ ");
  await flush(term);
  const g = new GhostText();
  // shell 回显用户敲的字；应用里灰字在下一帧补画，那时回显可能还没解析完
  term.write("rm alternatives.log.*");
  if (!echoPending) await flush(term);
  g.show(term as never, "a-collector/ -rf", 40);
  await flush(term);
  // 回车：runTypedLine → dismissHints → clear()，随后 pty 回显换行与输出
  const res = g.clear(term as never);
  term.write("\r\nrm: cannot remove ...\r\n$ ");
  await flush(term);
  return { res, line: row(term, 0) };
}

test("回显已解析完再画灰字：回车后行上不留灰字", async () => {
  const { line } = await typedThenEnter(false);
  assert.equal(line, "$ rm alternatives.log.*");
});

// 回归：回显还在 xterm 写队列里时画灰字，show() 记下的是**旧**光标，灰字却画在新光标后面；
// 回车时 clear() 判「光标被挪走」、一个字节都不写 —— 灰字永久留在已执行的命令行上
// （截图：`rm alternatives.log.*a-collector/ -rf`）。
test("回显还没解析完就画灰字：回车后行上同样不留灰字", async () => {
  const { res, line } = await typedThenEnter(true);
  assert.equal(line, "$ rm alternatives.log.*");
  assert.notEqual(res, "moved");
});

test("灰字画着时继续回显：先擦后写，重画仍贴在光标后", async () => {
  const term = new Terminal({ cols: 80, rows: 10, allowProposedApi: true });
  term.write("$ git ");
  const g = new GhostText();
  g.show(term as never, "status", 40); // 回显 "git " 还没解析完
  // 下一个字的回显到了：writeOut 先 clear 再写
  g.clear(term as never);
  term.write("s");
  await flush(term);
  g.show(term as never, "tatus", 40);
  await flush(term);
  assert.equal(row(term, 0), "$ git status");
  g.clear(term as never);
  term.write("\r\n");
  await flush(term);
  assert.equal(row(term, 0), "$ git s");
});
