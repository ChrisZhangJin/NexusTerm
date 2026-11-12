/**
 * `lib/shiftEnter` 的回归测试：Shift+Enter 在 TUI 里发 `ESC \r`，停在 shell 提示符上时保持当回车。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import {
  SHIFT_ENTER_SEQ,
  atShellPrompt,
  lineHasShellPrompt,
  looksLikeShellPrompt,
  shouldSendMetaEnter,
  textBeforeCursor,
  type ShiftEnterKey,
} from "../../src/lib/shiftEnter";
import type { BufLike } from "../../src/lib/termLine";

const key = (o: Partial<ShiftEnterKey> = {}): ShiftEnterKey => ({
  key: "Enter",
  shiftKey: true,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  imeBusy: false,
  ...o,
});

test("发的是 Meta+Enter", () => {
  assert.equal(SHIFT_ENTER_SEQ, "\x1b\r");
});

test("常见 shell 提示符都认得", () => {
  for (const p of [
    "user@host:~$",
    "root@box:/#",
    "[user@host ~]$",
    "(venv) me@h:~/proj$",
    "bash-5.1$",
    "-bash-4.2$",
    "$",
    "#",
    "%",
    "me@mac ~ %",
    "~/proj %",
    "/ #",
    "~ ❯",
    "➜  proj git:(main)",
    "user@host ~>",
    "PS C:\\Users\\x>",
    "C:\\>",
  ]) {
    assert.equal(looksLikeShellPrompt(p), true, p);
  }
});

test("Ink 输入框、REPL、进度条不算 shell 提示符", () => {
  for (const p of ["", ">", "│ >", "│ > hello", "> hello", ">>>", "100%", "####", "[==>  ] 42%", "echo a #", "mysql>", "...", "$$"]) {
    assert.equal(looksLikeShellPrompt(p), false, JSON.stringify(p));
  }
});

test("提示符后面已经敲了（或 ↑ 翻出了）命令也认得", () => {
  assert.equal(lineHasShellPrompt("user@host:~$ git commit -m 'x'"), true);
  assert.equal(lineHasShellPrompt("user@host:~$ "), true);
  assert.equal(lineHasShellPrompt("➜  proj git:(main) ls"), true);
  assert.equal(lineHasShellPrompt("bash-5.1$ echo $ # %"), true);
});

test("TUI 输入行里的 `$ ` / `# ` 不会被误认", () => {
  assert.equal(lineHasShellPrompt("> echo $ foo"), false);
  assert.equal(lineHasShellPrompt("│ > fix the # comment"), false);
  assert.equal(lineHasShellPrompt("> "), false);
  assert.equal(lineHasShellPrompt(""), false);
  // Ink 渲染完把光标停在输出下方的行首
  assert.equal(lineHasShellPrompt("   "), false);
});

test("光标被关掉（Ink 程序）时即使行像提示符也不算", () => {
  assert.equal(atShellPrompt({ before: "user@host:~$ ", cursorHidden: true }), false);
  assert.equal(atShellPrompt({ before: "user@host:~$ ", cursorHidden: false }), true);
  assert.equal(atShellPrompt({ before: "> hi", cursorHidden: false }), false);
});

test("在 shell 提示符上 Shift+Enter 保持原样，其它情况改发 ESC CR", () => {
  assert.equal(shouldSendMetaEnter(key(), () => true), false);
  assert.equal(shouldSendMetaEnter(key(), () => false), true);
});

test("不是 Shift+Enter / 带别的修饰键 / 输入法占着：一律不管，也不读屏", () => {
  const never = () => {
    throw new Error("不该读屏");
  };
  assert.equal(shouldSendMetaEnter(key({ shiftKey: false }), never), false);
  assert.equal(shouldSendMetaEnter(key({ key: "a" }), never), false);
  assert.equal(shouldSendMetaEnter(key({ ctrlKey: true }), never), false);
  assert.equal(shouldSendMetaEnter(key({ altKey: true }), never), false);
  assert.equal(shouldSendMetaEnter(key({ metaKey: true }), never), false);
  assert.equal(shouldSendMetaEnter(key({ imeBusy: true }), never), false);
});

/** 用字符串数组造一个缓冲：`wrapped[i]` 为真表示第 i 行是上一行自动折行过来的 */
function buf(rows: string[], cursorY: number, cursorX: number, wrapped: boolean[] = []): BufLike {
  return {
    cursorX,
    cursorY,
    baseY: 0,
    getLine: (y) =>
      y < 0 || y >= rows.length
        ? undefined
        : {
            isWrapped: !!wrapped[y],
            translateToString: (_trim?: boolean, start = 0, end?: number) => rows[y].slice(start, end),
          },
  };
}

test("textBeforeCursor 只取光标之前，并顺着自动折行接上前面的行", () => {
  assert.equal(textBeforeCursor(buf(["out", "user@h:~$ ls      "], 1, 13)), "user@h:~$ ls ");
  const b = buf(["junk", "user@h:~$ echo aaaa", "bbbb cc"], 2, 4, [false, false, true]);
  assert.equal(textBeforeCursor(b), "user@h:~$ echo aaaabbbb");
  assert.equal(lineHasShellPrompt(textBeforeCursor(b)), true);
  // 光标停在空行行首（Ink 程序常态）
  assert.equal(textBeforeCursor(buf(["│ > hi │", ""], 1, 0)), "");
});
