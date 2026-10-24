/**
 * Console 自动补全 / `@` 联想的自测。跑法：`npm test`
 *
 * 为什么单独一套：`src` 里这些逻辑和 xterm 的事件回调长在一起，
 * 而补全的错误（insert 多切/少切一个字符、宽字符回退格数算错、
 * 灰字赖在屏幕上、方向键之后行状态被污染）在界面上只是「好像不太对」，
 * 极难复现。纯逻辑部分抽出来，就能一句断言钉死。
 *
 * 这里不引入任何测试框架：node 自带 `node:test`，
 * `scripts/fe-test.mjs` 用 vite 自带的 esbuild 把 TS 转一下就能直接跑。
 */

import assert from "node:assert/strict";
import test from "node:test";

import {
  buildCompletions,
  dropCompletion,
  feedKeys,
  newLineState,
  predictHistory,
  recordCommand,
} from "../../src/lib/complete";
import { matchSessions, scanMention } from "../../src/lib/mention";
import { shouldResyncKeyDownSeen } from "../../src/lib/macInput";
import { COMMIT_WINDOW_MS, ImeGuard, enterVerdict, isImeKey, keyInfoFrom, type KeyLike } from "../../src/lib/ime";
import { GhostText, cursorFollowsTyped } from "../../src/lib/ghost";

// 注：命令参数（flag）补全那一档 —— `optionsFor` / `matchOptions` / `worthAsking`
// 与它的后端 `cmd_options` —— 已经整档下线（src/types.ts 的 `CmdOption`、
// src/api.ts 的 `consoleCommandOptions` 一并删掉了）。当前只剩历史前缀补全，
// 下面的用例也就只覆盖这一层。

const T = "tab-1";

// ——— 行状态：feedKeys ———

test("feedKeys 逐键攒出整行", () => {
  const s = newLineState();
  assert.equal(feedKeys(s, "ls"), null);
  assert.equal(feedKeys(s, " -"), null);
  assert.equal(feedKeys(s, "l"), null);
  assert.equal(s.text, "ls -l");
});

test("feedKeys 回车交出命令并清空行", () => {
  const s = newLineState();
  feedKeys(s, "docker ps");
  assert.equal(feedKeys(s, "\r"), "docker ps");
  assert.equal(s.text, "");
  assert.equal(predictHistory(T, "x"), null); // 还没记进历史
});

test("feedKeys 一次里多个回车只返回第一条", () => {
  const s = newLineState();
  assert.equal(feedKeys(s, "a\rb\rc"), "a");
});

test("feedKeys 退格删一个字符", () => {
  const s = newLineState();
  feedKeys(s, "git st");
  feedKeys(s, "\x7f"); // DEL
  assert.equal(s.text, "git s");
  feedKeys(s, "\x08"); // BS
  assert.equal(s.text, "git ");
});

test("feedKeys 遇到方向键 / Ctrl-U 就清空——状态不可知时不猜", () => {
  const s = newLineState();
  feedKeys(s, "kubectl get po");
  feedKeys(s, "\x1b[A"); // ↑
  assert.equal(s.text, "");

  feedKeys(s, "vim nginx.conf");
  feedKeys(s, "\x15"); // Ctrl-U
  assert.equal(s.text, "");
});

test("feedKeys 不清掉 Tab 之后的状态（Tab 走的是另一条路）", () => {
  const s = newLineState();
  feedKeys(s, "tail -");
  assert.equal(s.text, "tail -");
});

// ——— 历史预测 ———

test("历史：最近用过的优先，重复输入只算一次", () => {
  const id = "h1";
  recordCommand(id, "tail -f app.log");
  recordCommand(id, "tail -n 100 app.log");
  recordCommand(id, "tail -f app.log"); // 再次使用 → 应排到最前

  assert.equal(predictHistory(id, "tail -f"), "tail -f app.log");

  // 少敲一个字符，仍能预测；且不会因为重复而给出别的
  assert.equal(predictHistory(id, "tail -"), "tail -f app.log");
});

test("历史预测有两个字符的门槛：敲一个字母不该弹一堆噪声", () => {
  const id = "h2";
  recordCommand(id, "docker ps -a");
  assert.equal(predictHistory(id, "d"), null);
  assert.equal(predictHistory(id, "do"), "docker ps -a");
});

test("已经打完的整条不再预测（不打扰）", () => {
  const id = "h3";
  recordCommand(id, "ls -la");
  assert.equal(predictHistory(id, "ls -la"), null);
});

test("含控制字符的残句不进历史", () => {
  const id = "h4";
  recordCommand(id, "ls\x1b[Axyz");
  recordCommand(id, "  ");
  assert.equal(predictHistory(id, "ls"), null);
});

test("dropCompletion 放掉历史与缓存", () => {
  const id = "h5";
  recordCommand(id, "uname -a");
  assert.equal(predictHistory(id, "una"), "uname -a");
  dropCompletion(id, false);
  assert.equal(predictHistory(id, "una"), null);
});

// ——— 合成候选 ———

test("命令名阶段：只出历史，insert 只补缺的那一段", () => {
  const id = "c1";
  recordCommand(id, "git status --short");
  recordCommand(id, "git stash list");
  const items = buildCompletions(id, "git st");
  assert.ok(items.length >= 2, "两条历史都该出现");
  const st = items.find((i) => i.label === "git status --short");
  assert.ok(st);
  assert.equal(st.insert, "atus --short"); // 不能把已输入的 "git st" 再写一遍
});

test("空行不给任何候选", () => {
  assert.deepEqual(buildCompletions("c4", ""), []);
  assert.deepEqual(buildCompletions("c4", "   "), []);
});

// ——— `@会话名` 联想 ———

const SESSIONS = [
  { id: "t1", label: "web-01" },
  { id: "t2", label: "db-01" },
  { id: "t3", label: "本地终端" },
];

test("scanMention：行首的 @ 能扫到", () => {
  assert.deepEqual(scanMention("@", 1), { start: 0, query: "" });
  assert.deepEqual(scanMention("@db", 3), { start: 0, query: "db" });
});

test("scanMention：空白之后的 @ 才算（邮箱不该触发）", () => {
  assert.deepEqual(scanMention("重启 @db", 6), { start: 3, query: "db" });
  assert.equal(scanMention("mail me@host", 12), null, "`me@host` 里的 @ 前面不是空白");
});

test("scanMention：@ 后面已经打了空白就不算了", () => {
  assert.equal(scanMention("@db 再重启", 9), null);
});

test("scanMention：光标不在 @ 片段里时不弹", () => {
  assert.equal(scanMention("重启 nginx", 9), null);
  assert.equal(scanMention("", 0), null);
});

test("scanMention：连续两个 @ 按离光标近的那个算", () => {
  const r = scanMention("@a@b", 4);
  assert.deepEqual(r, { start: 2, query: "b" });
});

test("matchSessions：没选过的排前面，已选过的排后面", () => {
  const hits = matchSessions(SESSIONS, ["t1"], "");
  assert.deepEqual(hits.map((s) => s.id), ["t2", "t3", "t1"]);
});

test("matchSessions：按名字过滤，大小写不敏感", () => {
  assert.deepEqual(matchSessions(SESSIONS, [], "DB").map((s) => s.id), ["t2"]);
  assert.deepEqual(matchSessions(SESSIONS, [], "01").map((s) => s.id), ["t1", "t2"]);
});

test("matchSessions：结果封顶 8 条，免得标签多的时候糊满屏", () => {
  const many = Array.from({ length: 30 }, (_, i) => ({ id: `s${i}`, label: `host-${i}` }));
  assert.equal(matchSessions(many, [], "").length, 8);
});

// ——— GhostText：灰字的字节序列 ———

/** 假 Terminal：只把 write 的内容攒下来 */
function fakeTerm() {
  const written: string[] = [];
  return {
    written,
    text: () => written.join(""),
    write(s: string) {
      written.push(s);
    },
  };
}
const ESC = String.fromCharCode(27);

test("灰字：暗色 → 文本 → 关暗色 → 光标左移 N 格", () => {
  const term = fakeTerm();
  const g = new GhostText();
  const shown = g.show(term as never, "atus --short", 40);
  assert.equal(shown, "atus --short");
  assert.equal(term.text(), `${ESC}[2matus --short${ESC}[22m${ESC}[12D`);
  assert.equal(g.active, true);
  assert.equal(g.cols, 12);
});

test("灰字：关暗色用 22m 而不是 0m——0m 会把彩色提示符一起清掉", () => {
  const term = fakeTerm();
  new GhostText().show(term as never, "x", 20);
  assert.ok(!term.text().includes(`${ESC}[0m`));
});

test("灰字：宽字符按列回退，不是按字符数", () => {
  const term = fakeTerm();
  const g = new GhostText();
  g.show(term as never, "中文", 40);
  assert.equal(g.cols, 4, "两个中文字占 4 列");
  assert.ok(term.text().endsWith(`${ESC}[4D`), term.text());
});

test("灰字：放不下就截断，绝不换行", () => {
  const term = fakeTerm();
  const g = new GhostText();
  const shown = g.show(term as never, "abcdefghij", 4); // room=4 → 预算 3
  assert.equal(shown, "abc");
  assert.equal(g.cols, 3);
});

test("灰字：没有空间就一个字也不画", () => {
  const term = fakeTerm();
  const g = new GhostText();
  assert.equal(g.show(term as never, "abc", 1), "");
  assert.equal(term.written.length, 0);
});

test("灰字：clear 只在画过时才擦，重复 show 会先擦旧的", () => {
  const term = fakeTerm();
  const g = new GhostText();
  g.clear(term as never);
  assert.equal(term.written.length, 0, "没画过就不需要擦");

  g.show(term as never, "abc", 20);
  const n = term.written.length;
  g.show(term as never, "de", 20);
  assert.ok(term.written.length > n, "第二次 show 应先擦再画");
  assert.equal(term.written[n], `${ESC}[K`);
  assert.equal(g.cols, 2);
});

// ——— 回显之后要不要把灰字画回来 ———

/** 造一个能回答「光标前那半行是什么」的假 Terminal */
function mockTerm(lineText: string, cursorX: number) {
  const cells = [...lineText];
  return {
    cols: 80,
    written: [] as string[],
    write(s: string) {
      this.written.push(s);
    },
    buffer: {
      active: {
        cursorY: 0,
        baseY: 0,
        cursorX,
        getLine: (y: number) =>
          y === 0
            ? { getCell: (x: number) => (x < cells.length ? { getChars: () => cells[x] } : undefined) }
            : undefined,
      },
    },
  };
}

test("cursorFollowsTyped：光标紧贴刚敲的那半行 → 可以重画", () => {
  // "user@host:~$ ta" 共 15 字符，光标停在末尾
  const t = mockTerm("user@host:~$ ta", 15);
  assert.equal(cursorFollowsTyped(t as never, "ta"), true);
});

test("cursorFollowsTyped：回显只到前缀（滞后一字符）→ 仍算同一输入行", () => {
  // 本地已敲 "ta"，屏幕上只回显到 "t"。这是 SSH 的常态（回显晚一个往返）。
  // 90bdff8 把判据从「整段结尾匹配」放宽成「前缀匹配」，就是为了让这种滞后
  // 不被误判成"命令输出已经顶掉输入行"，否则灰字每按一键闪两次。
  const t = mockTerm("user@host:~$ ta", 14);
  assert.equal(cursorFollowsTyped(t as never, "ta"), true);
});

test("cursorFollowsTyped：真的输出来了（命令行已被顶掉）→ 不能重画", () => {
  const t = mockTerm("total 12", 8);
  assert.equal(cursorFollowsTyped(t as never, "ta"), false);
});

test("cursorFollowsTyped：没敲东西时不重画", () => {
  const t = mockTerm("user@host:~$ ", 13);
  assert.equal(cursorFollowsTyped(t as never, ""), false);
});

test("灰字：clear 之后状态归零", () => {
  const term = fakeTerm();
  const g = new GhostText();
  g.show(term as never, "abc", 20);
  g.clear(term as never);
  assert.equal(g.active, false);
  assert.equal(g.cols, 0);
  assert.equal(g.text, "");
});

// ——— 端到端：敲一行命令的完整链路 ———

test("端到端：acceptHint 之后行状态与终端内容一致", () => {
  const id = "e2e2";
  dropCompletion(id, false);
  const s = newLineState();
  recordCommand(id, "kubectl get pods -A");

  for (const ch of "kubectl g") feedKeys(s, ch);
  const items = buildCompletions(id, s.text);
  assert.equal(items.length, 1);
  s.text += items[0].insert;
  assert.equal(s.text, "kubectl get pods -A");

  // 再回车 → 这条命令被记进历史（且不会因为补全而变成重复项）
  const committed = feedKeys(s, "\r");
  assert.equal(committed, "kubectl get pods -A");
  recordCommand(id, committed);
  // 去重生效：历史里仍然只有一条，再敲前缀也只提示这一条
  const again = buildCompletions(id, "kubectl");
  assert.equal(again.length, 1);
  assert.equal(again[0].label, "kubectl get pods -A");
  assert.equal(again[0].insert, " get pods -A");

  // 整条打完 → 不再提示自己
  assert.deepEqual(buildCompletions(id, "kubectl get pods -A"), []);
});

// ——— macOS 丢字修复：beforeinput 复位判定 ———
//
// 这段判定如果放得太宽（例如不排除输入法组合），会把中文上屏搅乱；
// 放得太窄又修不好丢字。所以四个维度各钉一条。

test("mac 丢字：普通字符的 composed insertText 要复位", () => {
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: true,
      data: "b",
      inputType: "insertText",
      isComposing: false,
    }),
    true
  );
});

test("mac 丢字：输入法组合中的字符绝不能碰", () => {
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: true,
      data: "中",
      inputType: "insertText",
      isComposing: true,
    }),
    false
  );
});

test("mac 丢字：粘贴（insertFromPaste）不归它管", () => {
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: true,
      data: "ls -la",
      inputType: "insertFromPaste",
      isComposing: false,
    }),
    false
  );
});

test("mac 丢字：非 composed 事件不碰（那是正常 keydown 路径）", () => {
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: false,
      data: "a",
      inputType: "insertText",
      isComposing: false,
    }),
    false
  );
});

test("mac 丢字：空 data（删除等）不碰", () => {
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: true,
      data: null,
      inputType: "insertText",
      isComposing: false,
    }),
    false
  );
  assert.equal(
    shouldResyncKeyDownSeen({
      composed: true,
      data: "",
      inputType: "insertText",
      isComposing: false,
    }),
    false
  );
});

// ——— 输入法确认回车 vs 发送 ———
// 这一组对应 macOS 上的真实故障：打拼音按回车选词，消息被直接发出去了。

/** 造一个键盘事件；默认是「普通的裸回车」 */
const key = (over: Partial<KeyLike> = {}): KeyLike => ({
  key: "Enter",
  shiftKey: false,
  isComposing: false,
  keyCode: 13,
  timeStamp: 1000,
  ...over,
});

test("Safari 顺序：compositionend 先到，确认回车仍不能被当成发送", () => {
  const g = new ImeGuard();
  g.begin();
  g.finish(1000); // Safari 先派发这个
  // 随后才派发 keydown：此时 isComposing 已被清成 false（这就是原 bug 的成因）
  const v = enterVerdict(key({ isComposing: false, keyCode: 229, timeStamp: 1000.4 }), g);
  assert.equal(v, "ime");
  assert.notEqual(v, "send");
});

test("Chrome 顺序：keydown 带 isComposing=true", () => {
  const g = new ImeGuard();
  g.begin();
  assert.equal(enterVerdict(key({ isComposing: true, keyCode: 229 }), g), "ime");
});

test("keydown 先于 compositionend：靠组合状态兜住", () => {
  const g = new ImeGuard();
  g.begin(); // 组合已开始，end 还没来
  assert.equal(enterVerdict(key({ isComposing: false, keyCode: 13 }), g), "ime");
});

test("个别输入法不给 229：组合刚结束的窗口内也要拦", () => {
  const g = new ImeGuard();
  g.begin();
  g.finish(1000);
  assert.equal(enterVerdict(key({ keyCode: 13, timeStamp: 1005 }), g), "ime-after-commit");
});

test("Safari 的时间戳可能倒挂（keydown 早于 compositionend），用绝对值照样能拦", () => {
  const g = new ImeGuard();
  g.begin();
  g.finish(1000);
  assert.equal(enterVerdict(key({ keyCode: 13, timeStamp: 996 }), g), "ime-after-commit");
});

test("窗口之外的真回车必须放行，不能永久吞掉回车", () => {
  const g = new ImeGuard();
  g.begin();
  g.finish(1000);
  assert.equal(enterVerdict(key({ timeStamp: 1000 + COMMIT_WINDOW_MS }), g), "send");
  assert.equal(enterVerdict(key({ timeStamp: 1000 + COMMIT_WINDOW_MS + 1 }), g), "send");
});

test("229 只挡那一下：紧接着的真回车仍然能发送", () => {
  const g = new ImeGuard();
  g.begin();
  g.finish(1000);
  // 确认键
  assert.equal(enterVerdict(key({ keyCode: 229, timeStamp: 1000.2 }), g), "ime");
  // 人再按一次回车（隔了一会儿）
  assert.equal(enterVerdict(key({ timeStamp: 1600 }), g), "send");
});

test("从来没组合过：回车就是回车", () => {
  const g = new ImeGuard();
  assert.equal(enterVerdict(key(), g), "send");
});

test("Shift+Enter 与其它键一律交给浏览器", () => {
  const g = new ImeGuard();
  assert.equal(enterVerdict(key({ shiftKey: true }), g), "pass");
  assert.equal(enterVerdict(key({ key: "a", keyCode: 65 }), g), "pass");
  // 组合中按 Shift+Enter 也不该被我们拦成「确认」
  g.begin();
  assert.equal(enterVerdict(key({ shiftKey: true, isComposing: true }), g), "pass");
});

test("失焦 reset 之后不会把之后所有回车都吞掉", () => {
  const g = new ImeGuard();
  g.begin(); // 组合中被切走，compositionend 没来
  g.reset();
  assert.equal(g.isComposing, false);
  assert.equal(enterVerdict(key({ timeStamp: 5000 }), g), "send");
});

test("中文输入全过程模拟：打字 → 空格选词 → 回车发送", () => {
  const g = new ImeGuard();
  // 敲 nihao 拼音：组合开始
  g.begin();
  // 空格选字：组合结束
  g.finish(2000);
  // 人回过神来按回车发送（远超窗口）
  assert.equal(enterVerdict(key({ timeStamp: 2600 }), g), "send");
});

test("React 的 keyCode 归一化成 0 时，要从 nativeEvent 里取到 229", () => {
  const info = keyInfoFrom({
    key: "Enter",
    shiftKey: false,
    keyCode: 0, // React 合成事件在某些版本里就是这样
    nativeEvent: { isComposing: false, keyCode: 229, timeStamp: 1200 },
  });
  assert.equal(info.keyCode, 229);
  assert.equal(info.timeStamp, 1200, "时间戳必须取 nativeEvent 的，否则窗口算错");

  const g = new ImeGuard();
  g.begin();
  g.finish(1200);
  assert.equal(enterVerdict(info, g), "ime");
});

test("合成事件没给 keyCode 也能正常发送", () => {
  const info = keyInfoFrom({
    key: "Enter",
    shiftKey: false,
    nativeEvent: { isComposing: false, keyCode: 13, timeStamp: 9000 },
  });
  assert.equal(enterVerdict(info, new ImeGuard()), "send");
});

test("拼音组合中的上下键是在选候选词，不能连带挪动 @ 联想列表", () => {
  const g = new ImeGuard();
  g.begin();
  // 组合中：任何键都被输入法占用
  assert.equal(isImeKey(key({ key: "ArrowDown", keyCode: 40 }), g), true);
  assert.equal(isImeKey(key({ key: "ArrowUp", keyCode: 38 }), g), true);
  // 组合结束后：方向键交回界面
  g.finish(1000);
  assert.equal(isImeKey(key({ key: "ArrowDown", keyCode: 40, timeStamp: 1400 }), g), false);
});

test("229 也代表输入法占用（不只回车）", () => {
  const g = new ImeGuard();
  assert.equal(isImeKey(key({ key: "ArrowDown", keyCode: 229 }), g), true);
});
