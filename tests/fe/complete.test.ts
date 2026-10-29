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
  historyMatches,
  newLineState,
  predictHistory,
  recordCommand,
} from "../../src/lib/complete";
import { matchSessions, scanMention } from "../../src/lib/mention";
import {
  KeyLedger,
  installMacKeyboard,
  setInputTrace,
  isPlainChar,
  isPlainKeystroke,
} from "../../src/lib/macInput";
import { COMMIT_WINDOW_MS, ImeGuard, enterVerdict, isImeKey, keyInfoFrom, type KeyLike } from "../../src/lib/ime";
import { GhostText, cursorAtLineEnd, cursorFollowsTyped } from "../../src/lib/ghost";

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
  recordCommand(id, "tail -f app.log", false);
  recordCommand(id, "tail -n 100 app.log", false);
  recordCommand(id, "tail -f app.log", false); // 再次使用 → 应排到最前

  assert.equal(predictHistory(id, "tail -f"), "tail -f app.log");

  // 少敲一个字符，仍能预测；且不会因为重复而给出别的
  assert.equal(predictHistory(id, "tail -"), "tail -f app.log");
});

test("历史预测有两个字符的门槛：敲一个字母不该弹一堆噪声", () => {
  const id = "h2";
  recordCommand(id, "docker ps -a", false);
  assert.equal(predictHistory(id, "d"), null);
  assert.equal(predictHistory(id, "do"), "docker ps -a");
});

test("已经打完的整条不再预测（不打扰）", () => {
  const id = "h3";
  recordCommand(id, "ls -la", false);
  assert.equal(predictHistory(id, "ls -la"), null);
});

test("含控制字符的残句不进历史", () => {
  const id = "h4";
  recordCommand(id, "ls\x1b[Axyz", false);
  recordCommand(id, "  ", false);
  assert.equal(predictHistory(id, "ls"), null);
});

test("dropCompletion 放掉历史与缓存", () => {
  const id = "h5";
  recordCommand(id, "uname -a", false);
  assert.equal(predictHistory(id, "una"), "uname -a");
  dropCompletion(id, false);
  assert.equal(predictHistory(id, "una"), null);
});

// ——— 合成候选（新实现只做历史预测） ———
//
// 7fbb27a 之后补全不再去问 `--help` 捞 options，改成整行历史前缀匹配。
// 这里钉住两条不变式：insert 只补缺的那一段；候选按「最近用过的在前」。

test("命令名阶段：只出历史，insert 只补缺的那一段", () => {
  const id = "c1";
  recordCommand(id, "git status --short", false);
  recordCommand(id, "git stash list", false);
  const items = buildCompletions(id, "git st");
  assert.ok(items.length >= 2, "两条历史都该出现");
  const st = items.find((i) => i.label === "git status --short");
  assert.ok(st);
  assert.equal(st.insert, "atus --short"); // 不能把已输入的 "git st" 再写一遍
});

test("insert 绝不含已输入的前缀：token + insert 必须正好补齐整条", () => {
  const id = "c2";
  recordCommand(id, "tail --follow /var/log/syslog", false);
  for (const typed of ["t", "ta", "tail ", "tail --f"]) {
    for (const it of buildCompletions(id, typed)) {
      assert.equal(typed.trimStart() + it.insert, it.label, `${typed} 补出来对不上`);
      assert.ok(it.insert.length > 0, `${typed} 不该出现空 insert（那就是重复提示）`);
    }
  }
});

test("已经打完的整条不再作为候选", () => {
  const id = "c3";
  recordCommand(id, "docker ps -a", false);
  assert.deepEqual(buildCompletions(id, "docker ps -a"), []);
});

test("空行不给任何候选", () => {
  assert.deepEqual(buildCompletions("c4", ""), []);
  assert.deepEqual(buildCompletions("c4", "   "), []);
});

test("historyMatches：最近用过的排前面，且封顶 30 条", () => {
  const id = "c5";
  recordCommand(id, "make build", false);
  recordCommand(id, "make test", false);
  assert.deepEqual(historyMatches(id, "make"), ["make test", "make build"]);
  for (let i = 0; i < 40; i++) recordCommand(id, `long-${i}`, false);
  assert.equal(historyMatches(id, "long-").length, 30);
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

/**
 * 假 Terminal：只把 write 的内容攒下来。
 *
 * 带上最小可用的 buffer —— 灰字现在会先问一句「光标是不是在行尾」
 * （见 `cursorAtLineEnd`），没有 buffer 就没法验证这条守卫。
 * 默认光标在行尾、后面没有别的字，也就是「可以画灰字」的那种状态。
 */
function fakeTerm(opts?: { cursorX?: number; lineText?: string }) {
  const written: string[] = [];
  const cells = [...(opts?.lineText ?? "")];
  const cursorX = opts?.cursorX ?? (opts?.lineText?.length ?? 0);
  return {
    written,
    cols: 80,
    cursorX,
    text: () => written.join(""),
    write(s: string) {
      written.push(s);
    },
    buffer: {
      active: {
        type: "normal",
        cursorX,
        cursorY: 0,
        baseY: 0,
        getLine: (y: number) =>
          y === 0
            ? {
                getCell: (x: number) =>
                  x < cells.length ? { getChars: () => cells[x] } : undefined,
              }
            : undefined,
      },
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
  // ECH（擦 n 个字符）而不是 EL（擦到行尾）：只抹掉自己画的那几列
  assert.equal(term.written[n], `${ESC}[3X`);
  assert.equal(g.cols, 2);
});

// ——— 行中间编辑：灰字绝不能动光标后面的真字 ———

test("cursorAtLineEnd：光标后面还有字 → 不画灰字", () => {
  // "abc def" 共 7 字，光标停在 3（"abc| def"）
  const term = fakeTerm({ lineText: "abc def", cursorX: 3 });
  assert.equal(cursorAtLineEnd(term as never), false);
});

test("cursorAtLineEnd：光标贴着行尾 → 可以画", () => {
  const term = fakeTerm({ lineText: "abc def", cursorX: 7 });
  assert.equal(cursorAtLineEnd(term as never), true);
});

test("cursorAtLineEnd：行尾的空格不算内容", () => {
  const term = fakeTerm({ lineText: "abc     ", cursorX: 3 });
  assert.equal(cursorAtLineEnd(term as never), true);
});

test("灰字：人在行中间时一个字节都不写（不覆盖光标后的真字）", () => {
  const term = fakeTerm({ lineText: "abc def", cursorX: 3 });
  const g = new GhostText();
  assert.equal(g.show(term as never, "def", 20), "");
  assert.equal(term.written.length, 0, "一个字节都不该写");
  assert.equal(g.active, false);
});

test("灰字：行中间时连擦除也不该发生（不能拿 CSI K 抹到行尾）", () => {
  const term = fakeTerm({ lineText: "abc def", cursorX: 7 });
  const g = new GhostText();
  g.show(term as never, "gh", 20);
  assert.equal(g.active, true);

  // 光标被挪到行中间（人按了 ← / Backspace / Home）之后再 clear：
  // 这时从光标往后那一段已经不是我们画的字了，写了就是删用户的字
  term.cursorX = 3;
  term.buffer.active.cursorX = 3;
  g.clear(term as never);
  assert.equal(term.written.length, 1, "只有第一次 show 写过的那一次");
  assert.equal(g.active, false, "状态该清干净，屏幕上的残字交给 shell 自己重绘");
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

test("端到端：敲 'tail --f' 到按 Tab，补全的那段正好接上", () => {
  const id = "e2e";
  dropCompletion(id, false);
  const s = newLineState();

  // 先让历史里有一条完整命令
  for (const ch of "tail --follow /var/log/syslog") feedKeys(s, ch);
  recordCommand(id, feedKeys(s, "\r") ?? "", false);

  // 再敲半句
  for (const ch of "tail --f") feedKeys(s, ch);
  assert.equal(s.text, "tail --f");

  const items = buildCompletions(id, s.text);
  assert.equal(items.length, 1);
  assert.equal(items[0].label, "tail --follow /var/log/syslog");
  assert.equal(items[0].insert, "ollow /var/log/syslog");

  // 接受：只把缺的那段写进 pty，行状态同步前进
  s.text += items[0].insert;
  assert.equal(s.text, "tail --follow /var/log/syslog");
  assert.ok(s.text.startsWith("tail --f"), "已输入部分不能被改写");
});

test("端到端：acceptHint 之后行状态与终端内容一致", () => {
  const id = "e2e2";
  dropCompletion(id, false);
  const s = newLineState();
  recordCommand(id, "kubectl get pods -A", false);

  for (const ch of "kubectl g") feedKeys(s, ch);
  const items = buildCompletions(id, s.text);
  assert.equal(items.length, 1);
  s.text += items[0].insert;
  assert.equal(s.text, "kubectl get pods -A");

  // 再回车 → 这条命令被记进历史（且不会因为补全而变成重复项）
  const committed = feedKeys(s, "\r");
  assert.equal(committed, "kubectl get pods -A");
  recordCommand(id, committed, false);
  // 去重生效：历史里仍然只有一条，再敲前缀也只提示这一条
  const again = buildCompletions(id, "kubectl");
  assert.equal(again.length, 1);
  assert.equal(again[0].label, "kubectl get pods -A");
  assert.equal(again[0].insert, " get pods -A");

  // 整条打完 → 不再提示自己
  assert.deepEqual(buildCompletions(id, "kubectl get pods -A"), []);
});

// ——— macOS 丢字修复：整条键路接管 ———
//
// 读 xterm 5.5 源码后确认的事实（`lib/xterm.js`）：
//   · `evaluateKeyboardEvent` 对「keyCode>=48 的单字符」直接返回
//     `o.key = e.key`，`_keyDown` 随后 `triggerDataEvent(i.key)` 并
//     `cancel(e, true)` —— **普通字符是在 keydown 发出的**，
//     之后浏览器不再派发 keypress / input。
//   · `_keyDown` 第一条语句就 `_keyDownSeen = true`，紧接着
//     `if (!this._compositionHelper.keydown(e)) return;` —— 而后者对
//     keyCode 229 一律返回 false。WebKit 在输入法上下文残留时会把普通
//     字母标成 229：标志置了位、字节没发出去，随后那条 input 又被
//     `(!composed || !_keyDownSeen)` 判成重复丢掉。
// 所以不再去调 xterm 的私有标志，而是整条接管。

/** 只记录监听器的假容器：我们关心的是「谁拦下了事件、发了什么」 */
function fakeHost() {
  const map = new Map<string, Array<(ev: unknown) => void>>();
  return {
    addEventListener(type: string, fn: (ev: unknown) => void) {
      const list = map.get(type) ?? [];
      list.push(fn);
      map.set(type, list);
    },
    removeEventListener(type: string, fn: (ev: unknown) => void) {
      const list = map.get(type);
      if (list) map.set(type, list.filter((f) => f !== fn));
    },
    /** 派发一条事件，返回它（含 preventDefault / stopPropagation 的痕迹） */
    fire(type: string, init: Record<string, unknown>) {
      const ev = {
        ...init,
        defaultPrevented: false,
        propagationStopped: false,
        preventDefault() {
          ev.defaultPrevented = true;
        },
        stopPropagation() {
          ev.propagationStopped = true;
        },
      };
      for (const fn of map.get(type) ?? []) fn(ev);
      return ev;
    },
    listeners(type: string) {
      return (map.get(type) ?? []).length;
    },
  };
}

const plainKey = (k: string, over: Record<string, unknown> = {}) => ({
  key: k,
  keyCode: k.length === 1 ? k.toUpperCase().charCodeAt(0) : 0,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  shiftKey: false,
  isComposing: false,
  ...over,
});

/** 装好接管层，返回 { host, term, out, send, un } */
function rig() {
  const host = fakeHost();
  const term = { textarea: { value: "" } };
  const out = { text: "" };
  const send = (d: string) => {
    out.text += d;
  };
  const { uninstall: un, arbitrate } = installMacKeyboard(host as never, term as never, send, true);
  // `ta` 是 xterm 的隐藏 textarea：真机上输入法的预编辑串就写在这儿，
  // 用例里「敲一颗拼音字母」必须连它一起更新（只看 composition 事件是不够的）
  return { host, term, ta: term.textarea, out, send, un, arbitrate };
}

test("接管：keydown 直接把字符发出，并拦下默认行为与冒泡", () => {
  const { host, out, un } = rig();
  const ev = host.fire("keydown", plainKey("c"));
  assert.equal(out.text, "c");
  assert.equal(ev.defaultPrevented, true, "必须 preventDefault，否则 keypress/input 会再来一遍");
  assert.equal(ev.propagationStopped, true, "必须拦住 xterm 的 _keyDown");
  un();
});

test("接管：连打 config 一字不丢、不重复", () => {
  const { host, out, un } = rig();
  for (const ch of "config") host.fire("keydown", plainKey(ch));
  assert.equal(out.text, "config");
  un();
});

test("接管：keyCode 229 的普通字母（丢字现场）照样能发出去", () => {
  // 这就是用户看到 cnfig 的那条路：_keyDownSeen 被置位、_keyDown 提前返回，
  // 字符只能靠 input 来救，而 input 又被判成重复丢掉。
  const { host, out, un } = rig();
  host.fire("keydown", plainKey("o", { keyCode: 229 }));
  assert.equal(out.text, "o", "229 不是「不归我管」的理由，是「必须归我管」的证据");
  un();
});

test("接管：WebKit 无视 preventDefault 又派发了 input，不重复发送", () => {
  const { host, out, un } = rig();
  host.fire("keydown", plainKey("c"));
  host.fire("input", { data: "c", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "c");
  un();
});

test("接管：input 带着 isComposing=true（上下文残留）也不重复发送", () => {
  const { host, out, un } = rig();
  host.fire("keydown", plainKey("c"));
  // 残留状态下 WebKit 会给 input 标上 isComposing=true；信它就等于放弃这颗字符
  host.fire("input", { data: "c", inputType: "insertText", isComposing: true });
  assert.equal(out.text, "c");
  un();
});

test("接管：input 抢在 keydown 之前（WebKit bug 165004）仍只发一次", () => {
  const { host, out, un } = rig();
  host.fire("input", { data: "o", inputType: "insertText", isComposing: false });
  host.fire("keydown", plainKey("o"));
  assert.equal(out.text, "o", "谁先到谁发，后到的在账本里认领");
  un();
});

test("接管：keydown 被跳过、只有 input 到达时补发那颗字符", () => {
  // key="Process"（真组合）时 keydown 不归我们管；但如果浏览器随后只给了一条
  // 普通 insertText，说明它就是一颗普通字符 —— 由 input 这条兜底发出去。
  const { host, out, un } = rig();
  host.fire("input", { data: "g", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "g");
  un();
});

test("接管：组合态里一律不插手", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("keydown", plainKey("n"));
  host.fire("input", { data: "n", inputType: "insertText", isComposing: true });
  assert.equal(out.text, "", "拼音的字母不能被当成命令输入");
  host.fire("compositionend", {});
  host.fire("keydown", plainKey("n"));
  assert.equal(out.text, "n", "组合结束后恢复正常接管");
  un();
});

test("接管：修饰键与不可打印键交给 xterm", () => {
  const { host, out, un } = rig();
  const cases = [
    plainKey("c", { ctrlKey: true }),
    plainKey("c", { altKey: true }),
    plainKey("c", { metaKey: true }),
    plainKey("Enter"),
    plainKey("Tab"),
    plainKey("中"),
  ];
  for (const c of cases) {
    const ev = host.fire("keydown", c);
    assert.equal(ev.defaultPrevented, false, `${JSON.stringify(c)} 不该被接管`);
  }
  assert.equal(out.text, "");
  un();
});

// ——— 第二轮：多出来的空格 ———

test("接管：keydown 后跟着一对 composition 事件，空格不能发两遍", () => {
  // WebKit 在输入法上下文还热着时会给普通按键补 compositionstart/end（空格
  // 本来就是输入法上屏键）。上一版在 compositionstart 里清账本，于是：
  // 空格发过一次 → 账本被清 → 随后那条 input 认领失败 → 再发一次。
  const { host, out, un } = rig();
  host.fire("keydown", plainKey(" "));
  host.fire("compositionstart", {});
  host.fire("compositionend", {});
  host.fire("input", { data: " ", inputType: "insertText", isComposing: true });
  assert.equal(out.text, " ", "只能有一个空格");
  un();
});

test("接管：多字符 input（双空格被系统换成「. 」）不逐字重放", () => {
  // macOS「连按两下空格插入句号」把 "  " 替换成 ". " 再派发一条两字符 input。
  // 那两个空格各自的 keydown 已经发过了，再补一个 "." 就变成三个字符。
  const { host, out, un } = rig();
  host.fire("keydown", plainKey(" "));
  host.fire("keydown", plainKey(" "));
  host.fire("input", { data: ". ", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "  ", "多字符 data 不是「一颗键」，整条忽略");
  un();
});

test("接管：合并成一条的多字符 input 也不会重复发送", () => {
  const { host, out, un } = rig();
  for (const ch of "git") host.fire("keydown", plainKey(ch));
  host.fire("input", { data: "git", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "git", "每个字符的 keydown 都发过了，合并的 input 只是回声");
  un();
});

test("接管：接管的键要清掉隐藏 textarea 里的残留", () => {
  // xterm 只在 `_keyDown` 里清 textarea，而我们把 keydown 整条拦住了 ——
  // 不清就会一直攒，等下次走组合路径时被 `_handleAnyTextareaChanges`
  // 当 diff 整段发出去（多字符的来源之一）。
  const { host, term, out, un } = rig();
  (term.textarea as { value: string }).value = "  ";
  host.fire("keydown", plainKey("x"));
  assert.equal(out.text, "x");
  assert.equal((term.textarea as { value: string }).value, "", "残留必须清干净");
  un();
});

test("接管：input 兜底发完也要清 textarea", () => {
  const { host, term, out, un } = rig();
  (term.textarea as { value: string }).value = "g";
  host.fire("input", { data: "g", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "g");
  assert.equal((term.textarea as { value: string }).value, "");
  un();
});

test("接管：中文上屏不动 textarea（xterm 要读它算上屏范围）", () => {
  // `_finalizeComposition` 会回头 `textarea.value.substring(start, end)`，
  // 提前清掉就是把刚上屏的中文抹了。
  const { host, term, out, un } = rig();
  (term.textarea as { value: string }).value = "你好";
  host.fire("input", { data: "你好", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "");
  assert.equal((term.textarea as { value: string }).value, "你好", "组合路径一个字都不能动");
  un();
});

test("接管：isComposing 谎报（非组合态）时交给 xterm，绝不自己发", () => {
  // 我们没收到 compositionstart 就说明组合没真的开着。残留的特征恰恰是
  // keyCode 229 —— 那种键 xterm 自己会 return false（顺带刷新预编辑），
  // 交给它最安全，随后由 onInput 那条兜底补发出去。
  // 这里同样**不能清 textarea**：万一判断错了，清它就是把刚上屏的字抹掉。
  const { host, term, out, un } = rig();
  (term.textarea as { value: string }).value = "pin";
  const ev = host.fire("keydown", plainKey("n", { keyCode: 229, isComposing: true }));
  assert.equal(ev.defaultPrevented, false);
  assert.equal(ev.propagationStopped, false, "交给 xterm 的组合路径");
  assert.equal(out.text, "");
  assert.equal((term.textarea as { value: string }).value, "pin", "textarea 一个字都不能动");
  un();
});

test("接管：isComposing 谎报但 keyCode 不是 229 时，拦住 xterm 交给 input 兜底", () => {
  // 这种键 xterm 会一路走到 triggerDataEvent —— 再让 onInput 补发一遍就成了
  // 两个字符。既然浏览器自称在组合，这颗键归 IME，我们只能拦住 xterm 那一遍。
  const { host, term, out, un } = rig();
  const ev = host.fire("keydown", plainKey("n", { keyCode: 78, isComposing: true }));
  assert.equal(ev.defaultPrevented, false);
  assert.equal(ev.propagationStopped, true);
  assert.equal(out.text, "");
  assert.equal((term.textarea as { value: string }).value, "", "谎报态不能乱清 textarea 之外没有约束");
  un();
});

// ——— 第五轮：拼音上屏后多一个空格 ———

test("接管：拼音组合中按空格确认，不能让它漏进 pty", () => {
  // WKWebView 给空格的 keyCode 是 32 而不是 229，xterm 的 CompositionHelper
  // 只对 229 提前返回 —— 于是它先把组合文本 finalize 一遍，再把空格当普通
  // 输入发出去。用户看到的就是「pin + 空格 → 拼 」。
  const { host, out, un, ta } = rig();
  host.fire("compositionstart", {});
  // 真机序列（见 macInput 里 `IM_GONE_KEYS` 的注释）：每敲一颗拼音字母，输入法
  // 都会回一条 compositionupdate、并把预编辑串写进隐藏 textarea。漏了这一步的
  // 序列在真机上等于「输入法已经不接电话了」，是另一条自救路径，见下面那条用例。
  for (const ch of "pin") {
    host.fire("keydown", plainKey(ch, { keyCode: 229 }));
    ta.value += ch;
    host.fire("compositionupdate", { data: ta.value });
  }
  const ev = host.fire("keydown", plainKey(" ", { keyCode: 229 }));
  assert.equal(ev.propagationStopped, true, "必须拦住 xterm 的 _keyDown");
  assert.equal(ev.defaultPrevented, false, "不能 preventDefault，否则 IME 上不了屏");
  assert.equal(out.text, "", "确认键一个字节都不发");
  un();
});

test("接管：组合态回车确认同样不漏进 pty", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  const ev = host.fire("keydown", plainKey("Enter", { keyCode: 229 }));
  assert.equal(ev.propagationStopped, true);
  assert.equal(ev.defaultPrevented, false);
  assert.equal(out.text, "");
  un();
});

test("接管：组合态的字母仍然交给 xterm（不能抢拼音）", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  const ev = host.fire("keydown", plainKey("p", { keyCode: 229 }));
  assert.equal(ev.propagationStopped, false);
  assert.equal(ev.defaultPrevented, false);
  assert.equal(out.text, "", "拼音字母绝不能进 shell");
  un();
});

test("接管：isComposing 谎报时的空格也拦掉", () => {
  const { host, out, un } = rig();
  const ev = host.fire("keydown", plainKey(" ", { isComposing: true }));
  assert.equal(ev.propagationStopped, true);
  assert.equal(out.text, "");
  un();
});

test("接管：compositionend 先于 keydown 到达时，空格仍按确认键拦掉", () => {
  // WebKit 的另一种顺序：先把候选提交、派发 compositionend，再派发 keydown。
  // 这时 composing 已经翻回 false，只能靠时间戳认出「同一次按键的两半」。
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", { timeStamp: 5000 });
  const ev = host.fire("keydown", plainKey(" ", { timeStamp: 5006 }));
  assert.equal(ev.propagationStopped, true);
  assert.equal(ev.defaultPrevented, true, "上屏已完成，可以直接拦掉插入，不留残留");
  assert.equal(out.text, "", "确认键哪怕晚一步到也不能漏");
  un();
});

test("接管：上屏之后再敲的空格是真输入，不能被当成确认键吞掉", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", { timeStamp: 5000 });
  // 人手极限约 80ms/键，150ms 之后那颗空格就是想打空格
  host.fire("keydown", plainKey(" ", { timeStamp: 5150 }));
  assert.equal(out.text, " ", "否则「拼 」这种正常输入会变成「拼」");
  un();
});

test("接管：组合结束后空格恢复正常发送", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", {});
  host.fire("keydown", plainKey(" "));
  assert.equal(out.text, " ", "上屏之后用户再敲的空格必须发出去");
  un();
});

test("接管：空格确认后 compositionend 剥掉 textarea 尾部空格", () => {
  // 那颗空格拦不掉「写进 textarea」（按键先被 IME 消费，preventDefault 无效），
  // 而 xterm 的 `_finalizeComposition(true)` 是 `substring(start)` 取到末尾 ——
  // 不剥就会跟着「拼」一起发进 pty。
  const { host, term, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("keydown", plainKey(" ", { keyCode: 229 }));
  (term.textarea as { value: string }).value = "拼 ";
  host.fire("compositionend", {});
  assert.equal((term.textarea as { value: string }).value, "拼");
  assert.equal(out.text, "");
  un();
});

// ——— 第六轮：选候选的数字键（da + 2 → 大2） ———

test("接管：组合态里按数字选候选，不能让它进 pty", () => {
  // 和空格是同一个漏法：xterm 的 CompositionHelper.keydown 只对 229 提前返回，
  // 数字键的 keyCode 是 50，于是 `_keyDown` 一路走到 triggerDataEvent("2") ——
  // 「da」+「2」选第二个候选，屏幕上就成了「大2」。
  const { host, out, un, ta } = rig();
  host.fire("compositionstart", {});
  for (const ch of "da") {
    host.fire("keydown", plainKey(ch, { keyCode: 229 }));
    ta.value += ch;
    host.fire("compositionupdate", { data: ta.value });
  }
  const ev = host.fire("keydown", plainKey("2", { keyCode: "2".charCodeAt(0) }));
  assert.equal(ev.propagationStopped, true, "必须拦住 xterm 的 _keyDown");
  assert.equal(ev.defaultPrevented, false, "不能 preventDefault，否则 IME 选不了候选");
  assert.equal(out.text, "", "选候选的键一个字节都不发");
  un();
});

test("接管：组合态里按其它数字 / 翻页键同样不进 pty", () => {
  const { host, out, un } = rig();
  for (const ch of ["1", "5", "9", ",", ".", "-", "="]) {
    host.fire("compositionstart", {});
    const ev = host.fire("keydown", plainKey(ch, { keyCode: ch.charCodeAt(0) }));
    assert.equal(ev.propagationStopped, true, `${ch} 也该被拦`);
    host.fire("compositionend", {});
  }
  assert.equal(out.text, "");
  un();
});

test("接管：组合态里 keyCode 229 的数字交给 xterm（那还是拼音输入）", () => {
  // 229 的键 xterm 自己会 return false，而且顺手调 _handleAnyTextareaChanges
  // 刷新预编辑 —— 抢过来反而可能让「da」敲出来不显示。
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  const ev = host.fire("keydown", plainKey("2", { keyCode: 229 }));
  assert.equal(ev.propagationStopped, false);
  assert.equal(out.text, "");
  un();
});

test("接管：compositionend 先到时，选候选的数字照样拦掉", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", { timeStamp: 5000 });
  const ev = host.fire("keydown", plainKey("2", { keyCode: 50, timeStamp: 5006 }));
  assert.equal(ev.propagationStopped, true);
  assert.equal(ev.defaultPrevented, true, "上屏已完成，可以直接拦掉插入");
  assert.equal(out.text, "");
  un();
});

test("接管：上屏之后再敲的数字是真输入，不能吞", () => {
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", { timeStamp: 5000 });
  host.fire("keydown", plainKey("2", { keyCode: 50, timeStamp: 5150 }));
  assert.equal(out.text, "2", "否则「大2」这种正常输入会变成「大」");
  un();
});

test("接管：上屏刚过就到的 input 是同一次 IME 的尾巴，不发", () => {
  // 选候选的键被 WebKit 当成普通输入再派发一条 input；那时候 composing 已经
  // 翻回 false，只靠「离上屏多近」才能认出来。
  const { host, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("compositionend", { timeStamp: 5000 });
  host.fire("input", { data: "2", inputType: "insertText", isComposing: false, timeStamp: 5004 });
  assert.equal(out.text, "");
  un();
});

test("接管：数字选候选后 compositionend 剥掉 textarea 尾部的数字", () => {
  const { host, term, out, un } = rig();
  host.fire("compositionstart", {});
  host.fire("keydown", plainKey("2", { keyCode: 50 }));
  (term.textarea as { value: string }).value = "大2";
  host.fire("compositionend", { data: "大", timeStamp: 6000 });
  assert.equal((term.textarea as { value: string }).value, "大");
  assert.equal(out.text, "");
  un();
});

test("接管：上屏文本本身以那个数字结尾时不剥（比如选了「第2」）", () => {
  const { host, term, un } = rig();
  host.fire("compositionstart", {});
  host.fire("keydown", plainKey("2", { keyCode: 50 }));
  (term.textarea as { value: string }).value = "第2";
  host.fire("compositionend", { data: "第2", timeStamp: 6000 });
  assert.equal((term.textarea as { value: string }).value, "第2", "那是词的一部分，不能动");
  un();
});

test("接管：没按确认键时绝不改 textarea（避免误伤正常上屏）", () => {
  const { host, term, un } = rig();
  host.fire("compositionstart", {});
  (term.textarea as { value: string }).value = "a ";
  host.fire("compositionend", {});
  assert.equal((term.textarea as { value: string }).value, "a ");
  un();
});

test("接管：粘贴与组合上屏的 input 不碰", () => {
  const { host, out, un } = rig();
  host.fire("input", { data: "ls -la", inputType: "insertFromPaste", isComposing: false });
  host.fire("input", { data: "中文", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "", "非 ASCII / 非 insertText 一律留给 xterm");
  un();
});

test("接管：卸载后监听器全部撤掉", () => {
  const { host, un } = rig();
  assert.equal(host.listeners("keydown"), 1);
  un();
  assert.equal(host.listeners("keydown"), 0);
  assert.equal(host.listeners("input"), 0);
  assert.equal(host.listeners("compositionstart"), 0);
  assert.equal(host.listeners("compositionend"), 0);
});

test("接管：非 macOS 空转（Windows / Linux 不走自绘发送路径）", () => {
  const host = fakeHost();
  const out = { text: "" };
  const { uninstall: un } = installMacKeyboard(host as never, {} as never, (d) => (out.text += d), false);
  host.fire("keydown", plainKey("c"));
  assert.equal(out.text, "");
  un();
});

// ——— 账本：两条路共用一个出口时怎么去重 ———

test("账本：keydown 发过的字符，input 回声认领掉", () => {
  const l = new KeyLedger();
  assert.equal(l.keyDown("c", 0), true);
  assert.equal(l.input("c", 5), "", "回声不该再发一次");
});

test("账本：input 先发过，随后来的 keydown 不再发", () => {
  const l = new KeyLedger();
  assert.equal(l.input("c", 0), "c");
  assert.equal(l.keyDown("c", 5), false);
});

test("账本：连续两个同样的字母各算各的", () => {
  const l = new KeyLedger();
  assert.equal(l.keyDown("l", 0), true);
  assert.equal(l.keyDown("l", 3), true, "第二颗 l 不能被第一颗的账目吃掉");
  assert.equal(l.input("l", 6), "", "第一颗的回声");
  assert.equal(l.input("l", 9), "", "第二颗的回声");
});

test("账本：过期条目不认领（十秒后的同字符照样要发）", () => {
  const l = new KeyLedger(250);
  assert.equal(l.keyDown("c", 0), true);
  assert.equal(l.input("c", 400), "c", "过期了就不能再当成回声吞掉");
});

test("账本：多字符 data 只补真正缺的那部分", () => {
  const l = new KeyLedger();
  l.keyDown("a", 0);
  l.keyDown("b", 1);
  assert.equal(l.input("abc", 2), "c");
});

test("账本：容量封顶，不会无限增长", () => {
  const l = new KeyLedger();
  for (let i = 0; i < 40; i++) l.keyDown("x", i);
  assert.ok(l.size <= 16, `账本不该无界增长，实际 ${l.size}`);
});

// ——— 判据 ———

test("isPlainChar：只认单个可打印 ASCII", () => {
  assert.equal(isPlainChar("c"), true);
  assert.equal(isPlainChar("~"), true);
  assert.equal(isPlainChar(" "), true);
  assert.equal(isPlainChar(""), false);
  assert.equal(isPlainChar("ab"), false);
  assert.equal(isPlainChar("é"), false);
  assert.equal(isPlainChar("中"), false);
  assert.equal(isPlainChar("\n"), false);
  assert.equal(isPlainChar("\x7f"), false);
});

test("isPlainKeystroke：修饰键与多字符键一律放行给 xterm", () => {
  assert.equal(isPlainKeystroke(plainKey("c")), true);
  assert.equal(isPlainKeystroke(plainKey("C", { shiftKey: true })), true);
  assert.equal(isPlainKeystroke(plainKey("c", { ctrlKey: true })), false);
  assert.equal(isPlainKeystroke(plainKey("c", { altKey: true })), false);
  assert.equal(isPlainKeystroke(plainKey("c", { metaKey: true })), false);
  assert.equal(isPlainKeystroke(plainKey("Enter")), false);
  assert.equal(isPlainKeystroke(plainKey("Process")), false, "输入法的键名不是字符");
  assert.equal(isPlainKeystroke(plainKey("Dead")), false);
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

// ——— compositionend 漏发：composing 卡死自愈 ———
// 真实故障：某些 Windows 输入法 / 窗口内切标签页（不触发 window blur）会让
// compositionend 永远不来。composing 卡在 true 上之后，Tab / Enter / 方向键
// 全部被 `captures()` 判成"输入法在处理"而放行给 shell——补全从此失灵，
// 表现就是"灰字摆在那儿，按 Tab / Enter 却什么都没发生"。

test("compositionend 漏发：组合活着的时候（有 touch 心跳）不会被误判成卡死", () => {
  const g = new ImeGuard();
  g.begin(0);
  // 慢速拼音：每个字母间隔小一秒，但一直有 compositionupdate 心跳
  for (let t = 500; t <= 3500; t += 500) g.touch(t);
  assert.equal(isImeKey(key({ key: "a", keyCode: 65, timeStamp: 3600 }), g), true, "组合还在继续，不该被打断");
});

test("compositionend 漏发：心跳停了足够久之后，下一个真实按键自动解锁", () => {
  const g = new ImeGuard();
  g.begin(0);
  g.touch(100); // 组合里敲过几个字母，随后 compositionend 没来
  // 很久之后用户直接去按 Tab：这颗键本身就是物理键盘产生的证据
  const ev = key({ key: "Tab", keyCode: 9, timeStamp: 100 + COMMIT_WINDOW_MS + 5000 });
  assert.equal(isImeKey(ev, g), false, "卡死太久了，不能再把 Tab 当成输入法在处理");
  assert.equal(g.isComposing, false, "顺手把状态也解开，后面的键不用再等一次超时");
});

test("compositionend 漏发：自愈之后紧接着真的开始新组合，不受影响", () => {
  const g = new ImeGuard();
  g.begin(0);
  // 卡死很久后来了一次新的 compositionstart——不管旧状态，正常开始新的一轮
  g.begin(6000);
  assert.equal(isImeKey(key({ key: "a", keyCode: 65, timeStamp: 6100 }), g), true);
});

// ——— 第七轮：中文输入法「智能全角标点」把逗号/分号等替换成全角，发了两遍 ———

test("接管：热键上下文里按逗号，WebKit 把它替换成全角——只发全角那一条", () => {
  const { host, out, un } = rig();
  // keyCode 229：输入法上下文是热的，但没有真正的 composition（全角标点是单键直接替换）
  host.fire("keydown", plainKey(",", { keyCode: 229 }));
  assert.equal(out.text, "", "先别发，等 WebKit 真正写定的字符");
  host.fire("input", { data: "，", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "，", "只应该有全角那一个字符，半角逗号不能跟着冒出来");
  un();
});

test("接管：热键上下文里的标点其实没被替换——照样只发一次，不丢字", () => {
  const { host, out, un } = rig();
  host.fire("keydown", plainKey(";", { keyCode: 229 }));
  host.fire("input", { data: ";", inputType: "insertText", isComposing: false });
  assert.equal(out.text, ";");
  un();
});

test("接管：真机场景——中文输入法下逗号给的是自己的键码 188，不是 229", () => {
  // 这才是真机上发生的事：没有组合会话，keyCode 就是这颗键自己的键码，
  // 但 IME 照样把它换成全角。9d4d3aa 要求 keyCode === 229，恰好把要修的
  // 这个场景排除在外——半角先发一遍，全角那条 input 再被 xterm 发一遍。
  const { host, out, un } = rig();
  host.fire("keydown", plainKey(",", { keyCode: 188 }));
  assert.equal(out.text, "", "先别发，等 WebKit 真正写定的字符");
  host.fire("input", { data: "，", inputType: "insertText", isComposing: false });
  assert.equal(out.text, "，", "只应该有全角那一个，半角逗号不能跟着冒出来");
  un();
});

test("接管：同一个键码 188 但没被替换（英文输入法）也只发一次", () => {
  // 去掉 229 判据后英文标点也走「等 input」这条路：input 带来的就是最终字符。
  const { host, out, un } = rig();
  host.fire("keydown", plainKey(",", { keyCode: 188 }));
  host.fire("input", { data: ",", inputType: "insertText", isComposing: false });
  assert.equal(out.text, ",", "只发一次，不能重复也不能丢");
  un();
});

// ——— 第八轮：照着真机日志改（前三轮都是猜的，猜错三次）———
//
// 真机（macOS + WKWebView + 系统拼音）敲一个中文逗号，<app_data>/logs/input.log
// 里记到的顺序**和之前所有假设都不一样**：
//
//   beforeinput insertText data="，"   ← 全角结果先到
//   input       insertText data="，"
//   send "，"   via=xterm              ← xterm 的 _inputEvent 只发了一次
//   keydown     key="，" kc=229        ← 键事件后到，且 key 已经是全角
//   keyup       key="，" kc=188

test("接管：真机顺序——input 先到、keydown 的 key 已是全角且键码 229", () => {
  const { host, out, un, arbitrate } = rig();
  host.fire("input", { data: "，", inputType: "insertText", isComposing: false });
  // 这条以前断言的是「接管层一个字节都不发，那一个『，』由 xterm 的 _inputEvent
  // 发」。后来发现靠不住：同样是全角标点，`？` 一个都出不来——keyCode 229 下
  // xterm 的 `_keyDown` 什么都不发，随后的 input 又被 `_keyDownSeen` 门丢掉。
  // 现在倒过来：单字符一律由接管层发，xterm 那一份交给仲裁闸挡。
  assert.equal(out.text, "，", "全角结果必须由接管层发出，不能指望 xterm");
  assert.equal(arbitrate("，"), false, "xterm 若也发一份，必须被仲裁挡掉");
  // 迟到的 keydown（key 已是全角、键码 229）不该再补一个字节
  host.fire("keydown", plainKey("，", { keyCode: 229 }));
  assert.equal(out.text, "，", "keydown 姗姗来迟，不能再补一份");
  un();
});

test("接管：input 比 keydown 先到时，半角标点的兜底定时器必须闭嘴", () => {
  // 这条就是「，,」里第二个字符的来源：input 已经把结果交出去了，keydown 才
  // 姗姗来迟，于是「等 input」的逻辑启动 80ms 兜底，又补发了一个半角。
  const { host, out, un } = rig();
  host.fire("input", { data: ",", inputType: "insertText", isComposing: false });
  host.fire("keydown", plainKey(",", { keyCode: 188 }));
  assert.equal(out.text, ",", "刚刚已经有 input 交出过字符，不能再补第二个");
  un();
});

test("接管：标点替换键的 isComposing 谎报 true 时，照样只发一次（之前这条漏网）", () => {
  const { host, out, un } = rig();
  // 残留态下 keyCode 229 的键 WebKit 经常连 isComposing 都一起谎报成 true，
  // 但真正的 compositionstart 从没发过——composing 判据必须认后者，不认前者。
  host.fire("keydown", plainKey(",", { keyCode: 229, isComposing: true }));
  assert.equal(out.text, "", "先别发，等 input 揭晓");
  host.fire("input", { data: "，", inputType: "insertText", isComposing: true });
  assert.equal(out.text, "，", "只发一次，半角逗号不能跟着冒出来");
  un();
});

test("接管：组合永远不结束时不能让终端失聪（自救后打字照样出字）", () => {
  // 真机复现：拼音敲到一半切走输入法 → WebKit 这边的 composition 从没结束，
  // 于是之后每颗键都是 keyCode 229 + isComposing，任何人都不发 → 终端完全失聪。
  const { host, out, ta, un } = rig();
  host.fire("compositionstart", {});
  ta.value = "ni";
  host.fire("compositionupdate", { data: "ni" });
  // 连敲 3 颗键，输入法毫无回应（既不 update、也不改 textarea），只可能是它走了
  for (const ch of "abc") host.fire("keydown", plainKey(ch, { keyCode: 229, isComposing: true }));
  setInputTrace(false);
  assert.equal(out.text, "abc", "IM 已判定失联，接管层必须自己把字发出去，不能整条放行给一个不发字的 xterm");
  un();
});

test("接管：真·组合还在呼吸时不能误判失聪（中文输入不受影响）", () => {
  // 和上面那条是同一个判据的两面：每颗拼音字母都带来 compositionupdate +
  // 预编辑串变长，这轮组合就是活的，按键必须继续交给 IME，一个字节都不许发出去。
  const { host, out, ta, un } = rig();
  host.fire("compositionstart", {});
  for (const ch of "zhongwen") {
    host.fire("keydown", plainKey(ch, { keyCode: 229, isComposing: true }));
    ta.value += ch;
    host.fire("compositionupdate", { data: ta.value });
  }
  assert.equal(out.text, "", "组合还在呼吸，拼音字母不许漏进 pty");
  // 上屏交回 xterm 的组合路径（这一层不插手，详见 `onInput` 的注释）：
  // 接管层发的是 0 字节，一个多的都没有。
  host.fire("compositionend", { data: "中文" });
  host.fire("input", { data: "中文", inputType: "insertCompositionText" });
  assert.equal(out.text, "", "中文上屏由 xterm 自己的 _finalizeComposition 发，接管层不再插手一遍");
  un();
});

test("接管：自救之后新开一次真正的组合，仍能正常进组合态", () => {
  const { host, out, ta, un } = rig();
  host.fire("compositionstart", {});
  for (const ch of "abc") host.fire("keydown", plainKey(ch, { keyCode: 229, isComposing: true }));
  assert.equal(out.text, "abc", "失联自救：连敲几颗键毫无回音时自己把字发出来");

  // 输入法回来了：新一轮 compositionstart 必须把 `imGone` 复位
  host.fire("compositionend", { data: "" });
  ta.value = "";
  host.fire("compositionstart", {});
  for (const ch of "hao") {
    host.fire("keydown", plainKey(ch, { keyCode: 229, isComposing: true }));
    ta.value += ch;
    host.fire("compositionupdate", { data: ta.value });
  }
  assert.equal(out.text, "abc", "回到活着的组合态后，新敲的拼音字母一个都不该进 pty");
  const space = host.fire("keydown", plainKey(" ", { keyCode: 229, isComposing: true }));
  assert.equal(space.propagationStopped, true, "确认键必须拦住 xterm，否则上屏后还多一个空格");
  assert.equal(space.defaultPrevented, false, "不能 preventDefault，否则 IME 上不了屏");
  assert.equal(out.text, "abc", "确认键自己也不能出字节");
  un();
});

test("接管：自救时 blur 引出的那次 compositionend 不能吞掉触发自救的那颗键", () => {  // 真机日志（豆包输入法 → 敲到一半切走 → 打 hello）复现的是这条：`revive` 里
  // `blur()` 会立刻引出一次 data 为空的 compositionend，若把它当成 IM 的回应，
  // `imGone` 被复位、`composeEndedTs` 记成「刚刚」，紧跟着那颗键就被当成上屏键的
  // 另一半吞掉 —— 5 颗字母只出了 4 颗（`hello` → `helo`）。
  const { host, out, ta, un } = rig();
  (ta as unknown as { blur: () => void }).blur = () => {
    host.fire("compositionend", { data: "" });
  };
  (ta as unknown as { focus: () => void }).focus = () => {};
  host.fire("compositionstart", {});
  for (const ch of "abc") {
    host.fire("keydown", plainKey(ch, { keyCode: 229, isComposing: true }));
  }
  assert.equal(out.text, "abc", "触发自救的第 3 颗键也必须出字");
  un();
});

test("接管：输入法原样交出英文（豆包英文模式）时，结束那个词的敲空格必须进 pty", () => {
  // 真机日志：豆包英文模式一个词一次组合，`compend.data` 就是那几个字母；
  // 结束词的空格既是 IME 的确认键，也是用户实打实敲下的一个空格。
  // 以前一律当「上屏键的另一半」拦死 → `hello world` 打成 `helloworld`。
  const { host, ta, out, un } = rig();
  host.fire("compositionstart", { timeStamp: 900 });
  ta.value = "hello";
  host.fire("compositionupdate", { data: "hello", timeStamp: 950 });
  host.fire("compositionend", { data: "hello", timeStamp: 1000 });
  host.fire("keydown", plainKey(" ", { keyCode: 229, timeStamp: 1010 }));
  assert.equal(out.text, " ", "空格必须由接管层发出去（WebKit 不会替这个 229 键插入字符）");
  // 万一日后 WebKit 又派发了这条尾巴，也不能变成两个空格
  host.fire("input", { data: " ", inputType: "insertText", timeStamp: 1011 });
  assert.equal(out.text, " ", "同一颗键只许一个字符");
  un();
});

test("接管：中文上屏时空格属于输入法，一个字节都不许进 pty", () => {
  const { host, ta, out, un } = rig();
  host.fire("compositionstart", { timeStamp: 900 });
  ta.value = "nihao";
  host.fire("compositionupdate", { data: "nihao", timeStamp: 950 });
  host.fire("compositionend", { data: "你好", timeStamp: 1000 });
  const space = host.fire("keydown", plainKey(" ", { keyCode: 229, timeStamp: 1010 }));
  assert.equal(space.propagationStopped, true, "确认键要拦住 xterm");
  assert.equal(space.defaultPrevented, true, "也不能让它插进 textarea");
  host.fire("input", { data: " ", inputType: "insertText", timeStamp: 1011 });
  assert.equal(out.text, "", "中文句尾不能多一个空格");
  un();
});

test("接管：豆包英文模式下打完一个词直接回车，命令必须执行（回车不能被吞）", () => {
  // 真机：`pwd` + 回车 → 只上屏 `pwd`，回车没了，要再按一次才跑。
  const { host, ta, out, un } = rig();
  host.fire("compositionstart", { timeStamp: 900 });
  ta.value = "pwd";
  host.fire("compositionupdate", { data: "pwd", timeStamp: 950 });
  host.fire("compositionend", { data: "pwd", timeStamp: 1000 });
  host.fire("keydown", plainKey("Enter", { keyCode: 229, timeStamp: 1010 }));
  assert.equal(out.text, "\r", "回车得由接管层补发（xterm 对 229 的回车什么都不发）");
  un();
});

test("接管：中文上屏时的回车属于输入法，不能往 pty 里塞一个回车", () => {
  const { host, ta, out, un } = rig();
  host.fire("compositionstart", { timeStamp: 900 });
  ta.value = "nihao";
  host.fire("compositionupdate", { data: "nihao", timeStamp: 950 });
  host.fire("compositionend", { data: "你好", timeStamp: 1000 });
  host.fire("keydown", plainKey("Enter", { keyCode: 229, timeStamp: 1010 }));
  assert.equal(out.text, "", "中文确认回车不许进 pty");
  un();
});


// ——— 真实 xterm 的 write 是异步的：回归「按 Tab 补全失灵」———

/**
 * 模拟 xterm 5.x 的 write 时序：`write()` 只入队，缓冲在 `flush()` 时才更新。
 * 只有这样能复现「刚 write 完 ECH 就去读 buffer，读到的还是没擦的灰字」。
 * 只解析 GhostText 自己会写的四种序列：文本 / `ESC[nD` / `ESC[nX` / `ESC[2m`·`22m`。
 */
function asyncTerm(opts?: { cursorX?: number; lineText?: string }) {
  const written: string[] = [];
  const queue: string[] = [];
  const cells = [...(opts?.lineText ?? "")];
  let cursorX = opts?.cursorX ?? (opts?.lineText?.length ?? 0);
  const CSI_RE = new RegExp(`${ESC}\\[(\\d*)(?:;(\\d*))?([DXmH])`, "g");
  const apply = (s: string) => {
    let last = 0;
    for (const m of s.matchAll(CSI_RE)) {
      for (const ch of s.slice(last, m.index)) cells[cursorX++] = ch;
      last = (m.index ?? 0) + m[0].length;
      const cmd = m[3];
      const n = m[1] === "" ? 1 : parseInt(m[1], 10);
      if (cmd === "D") cursorX = Math.max(0, cursorX - n);
      else if (cmd === "X") for (let k = 0; k < n; k++) delete cells[cursorX + k];
      else if (cmd === "H") {
        // CUP：helper 只有一行缓冲，只支持第 1 行；列号从 1 起
        const col = m[2] === "" || m[2] === undefined ? 1 : parseInt(m[2], 10);
        if (n === 1) cursorX = col - 1;
      }
      // m：SGR，不影响字符与光标
    }
    for (const ch of s.slice(last)) cells[cursorX++] = ch;
  };
  return {
    written,
    cols: 80,
    rows: 24,
    get cursorX() {
      return cursorX;
    },
    text: () => written.join(""),
    line: () => cells.map((c) => c ?? "").join(""),
    move(x: number) {
      cursorX = x;
    },
    flush() {
      while (queue.length) apply(queue.shift()!);
    },
    write(s: string) {
      written.push(s);
      queue.push(s);
    },
    buffer: {
      active: {
        type: "normal",
        get cursorX() {
          return cursorX;
        },
        cursorY: 0,
        baseY: 0,
        getLine: (y: number) =>
          y === 0
            ? {
                getCell: (x: number) => ({
                  getChars: () => cells[x] ?? "",
                }),
              }
            : undefined,
      },
    },
  };
}

test("灰字：clear 返回 idle/erased/moved，让调用方免读异步缓冲", () => {
  const t = asyncTerm({ lineText: "st" });
  const g = new GhostText();
  assert.equal(g.clear(t as never), "idle", "没画过就是 idle，一个字节都不写");
  assert.equal(t.written.length, 0);

  g.show(t as never, "atus", 40);
  assert.equal(g.clear(t as never), "erased", "光标没动：后面全是我们的灰字");
  assert.ok(t.written.some((s) => s.includes(`${ESC}[4X`)), "写了 ECH 4");
  t.flush();
  assert.equal(t.line(), "st", "flush 后灰字真的没了");

  // 光标被 shell 挪走后再 clear：不能写任何东西
  const t2 = asyncTerm({ lineText: "st" });
  const g2 = new GhostText();
  g2.show(t2 as never, "atus", 40);
  t2.move(0); // shell 把光标挪走了
  assert.equal(g2.clear(t2 as never), "moved");
  assert.equal(t2.written.length, 1, "只有 show 那一次写");
});

test("灰字：ECH 还没 flush 时换建议也能重画（修复前会误判行中间而罢画）", () => {
  const t = asyncTerm({ lineText: "st" });
  const g = new GhostText();
  g.show(t as never, "atus", 40);
  t.flush(); // 屏幕上是 st|atus（灰字），光标回到 2
  // ↑↓ 挑了另一条：show 内部先 clear（ECH 入队，未 flush）再判行尾。
  // 修复前这里会读到旧灰字 → 判「行中间」→ 返回 ""，灰字从此消失。
  const shown = g.show(t as never, "ew -s x", 40);
  assert.equal(shown, "ew -s x");
  t.flush();
  assert.equal(t.line(), "stew -s x");
});

test("Tab 接受路径：灰字画着时 clear 的结论不依赖缓冲 flush", () => {
  // 复现真机时序：下拉+灰字都在 → 按 Tab → acceptHintAt 先 clear 后判行尾
  const t = asyncTerm({ lineText: "tmux " });
  const g = new GhostText();
  g.show(t as never, "attach -t unp", 40);
  t.flush();
  assert.equal(g.clear(t as never), "erased", "修复前：异步 ECH 未生效 + 读缓冲 → 误判行中间 → dismiss");
});

test("Tab 接受决策：ECH 没 flush 时 cursorAtLineEnd 仍读到灰字，决断必须信 clear 的 erased", () => {
  // 这是「erased 短路」存在的理由本身：clear() 写出的 ECH 此刻还在缓冲队列里，
  // 紧接着读 buffer 看到的仍是自己刚要擦掉的灰字。若把决策写成
  // 「clear 之后无条件读 cursorAtLineEnd」，灰字就永远擦不干净 → 永远判行中间。
  const t = asyncTerm({ lineText: "tmux " });
  const g = new GhostText();
  g.show(t as never, "attach -t unp", 40);
  t.flush();
  const cleared = g.clear(t as never);
  assert.equal(cleared, "erased");
  assert.equal(
    cursorAtLineEnd(t as never),
    false,
    "未 flush 时缓冲里还有灰字，cursorAtLineEnd 必然是 false —— 所以不能用它"
  );
  // 正确的决策式（与 TerminalView.acceptHint 一致）：erased ⇒ 当行尾，直接接受
  const accept = cleared === "erased" || cursorAtLineEnd(t as never);
  assert.ok(accept, "灰字画着时按 Tab 必须能决断为接受，而不是误判行中间后收起");
});

test("Tab 接受后 ghost 状态同步清空：再按 Tab 是 idle，不重复擦、不重复插", () => {
  const t = asyncTerm({ lineText: "tmux " });
  const g = new GhostText();
  g.show(t as never, "attach -t unp", 40);
  t.flush();

  // 第一次 Tab：接受。clear 同步清掉内部状态（不依赖 flush），
  // 后续的 Tab 判定（ghostRef.active / 再次 clear）必须立刻看到「没灰字了」，
  // 否则会把刚补进终端的真字再擦一遍 / 再插一遍。
  assert.equal(g.clear(t as never), "erased");
  assert.equal(g.active, false, "状态是同步清的，不能等 flush");
  assert.equal(g.text, "");

  const echWrites = t.written.filter((s) => s.includes(`${ESC}[13X`)).length;
  assert.equal(echWrites, 1, "ECH 13 只写一次：接受后没有第二次擦除");

  // 第二次 Tab：没有灰字 → clear 是 idle、一个字节都不写，
  // 组件层的判定直接放行给 shell，不会把建议再插一遍
  const totalBefore = t.written.length;
  assert.equal(g.clear(t as never), "idle");
  assert.equal(t.written.length, totalBefore, "idle 的 clear 不写任何序列");
});

test("灰字：atMark 按坐标判定，不读缓冲内容", () => {
  const t = asyncTerm({ lineText: "st" });
  const g = new GhostText();
  // 没画过：一律 false（调用方会走 cursorAtLineEnd 分支）
  assert.equal(g.atMark(t as never), false);

  g.show(t as never, "atus", 40);
  t.flush();
  assert.equal(g.atMark(t as never), true, "光标还在作画位置");

  t.move(0); // shell 回显/输出把光标顶走
  assert.equal(g.atMark(t as never), false);
});

test("灰字：光标被挪走后 clear 原地核实并擦掉残影（cleaned）", () => {
  // 这是「Tab 掉回给 shell」那条链的根治用例：状态清了但残影赖在屏幕上，
  // 下一次 Tab 因 active=false 不被接管，bash 自己的补全就接管了终端。
  const t = asyncTerm({ lineText: "tmux " });
  const g = new GhostText();
  g.show(t as never, "attach -t unp", 40);
  t.flush();
  assert.equal(t.line(), "tmux attach -t unp");

  t.move(30); // 回显/输出把光标顶走：sameSpot 不成立
  assert.equal(g.clear(t as never), "cleaned", "旧位置逐格核实仍是我们画的 → 原地擦除");
  t.flush();
  assert.equal(t.line(), "tmux ", "残影被擦干净");
  assert.equal(t.cursorX, 30, "光标被送回擦除时的位置");
  assert.ok(t.written.some((s) => s.includes(`${ESC}[1;6H`)), "擦除前先 CUP 到旧位置");
  assert.equal(g.atMark(t as never), false);
  assert.equal(g.clear(t as never), "idle");
});

test("灰字：旧位置被输出盖掉时 clear 返回 moved、一个字节不写", () => {
  const t = asyncTerm({ lineText: "tmux " });
  const g = new GhostText();
  g.show(t as never, "attach -t unp", 40);
  t.flush();
  t.move(30);
  // 模拟别的输出恰好写在残影开头那几格 → 逐格比对失败
  t.write(`${ESC}[1;6HZZZ`);
  t.flush();
  const before = t.written.length;
  assert.equal(g.clear(t as never), "moved", "认不出是我们画的，不敢写");
  assert.equal(t.written.length, before, "moved 分支零写入");
});
