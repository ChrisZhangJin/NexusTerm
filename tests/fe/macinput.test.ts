/**
 * `lib/macInput` 接管层的回归测试。
 *
 * 这一层的历史故障全是「多一个字 / 少一个字」，而且只在真机上闪一下。能在 node 里
 * 复现的关键是：**把 xterm 自己那几个监听也建模进来**。接管层的 bug 从来不是
 * 「它自己发错了」，而是「它以为拦住了 xterm，其实没拦住，于是两边各发一份」。
 * 只断言接管层发了什么是看不出来的 —— 必须看 pty 一共收到几个字节。
 *
 * 纯函数那部分（`KeyLedger` / `isPlainKeystroke` / `xtermWouldEmit` 等）的用例在
 * `complete.test.ts` 里，不在这儿重复。
 */

import { test } from "node:test";
import assert from "node:assert/strict";

import { installMacKeyboard, isPlainChar } from "../../src/lib/macInput";

type Listener = (ev: FakeEvent) => void;

interface FakeEvent {
  type: string;
  stopPropagation(): void;
  preventDefault(): void;
  stopped?: boolean;
  defaultPrevented?: boolean;
  [k: string]: unknown;
}

/**
 * 捕获阶段只有 host 一个节点，之后事件才到 textarea（xterm 的监听在那儿）。
 * `stopPropagation` 因此等价于「xterm 收不到」。
 */
class Harness {
  private hostLs = new Map<string, Set<Listener>>();
  private xtermLs = new Map<string, Listener>();
  /** 真正进 pty 的字节，按先后顺序 */
  sent: string[] = [];
  textarea = { value: "" };
  /** xterm 的 `_keyDownHandled`：`_keyDown` 跑起来时置 false，是 `_keyPress` 的唯一自保 */
  keyDownHandled = false;

  readonly host = {
    addEventListener: (t: string, f: Listener) => {
      let s = this.hostLs.get(t);
      if (!s) this.hostLs.set(t, (s = new Set()));
      s.add(f);
    },
    removeEventListener: (t: string, f: Listener) => this.hostLs.get(t)?.delete(f),
  };

  constructor() {
    // xterm 5.x 的真实行为，逐条对着 lib/xterm.js：
    // `_keyDown` 开头 `this._keyDownHandled=false`
    this.xtermLs.set("keydown", () => {
      this.keyDownHandled = false;
    });
    // `_keyPress(e){ this._keyPressHandled=false; if(this._keyDownHandled) return false; … triggerDataEvent(key) }`
    this.xtermLs.set("keypress", (ev) => {
      if (this.keyDownHandled) return;
      const key = ev.key as string;
      if (isPlainChar(key)) this.xtermSend(key);
    });
  }

  /** 接管层与 xterm 共用的唯一出口（对应 TerminalView 里的 handleInput） */
  readonly send = (d: string): void => {
    this.sent.push(d);
  };

  /** 当前装着的仲裁闸；没装接管层时一律放行 */
  arbitrate: (d: string) => boolean = () => true;

  /** xterm 自己那一路：和 TerminalView 一样，先过仲裁再进 pty */
  private readonly xtermSend = (d: string): void => {
    if (this.arbitrate(d)) this.sent.push(d);
  };

  /** 派发一颗事件：先 host 捕获，没被拦下才轮到 xterm */
  fire(type: string, props: Record<string, unknown> = {}): FakeEvent {
    const ev: FakeEvent = {
      type,
      key: "",
      ctrlKey: false,
      altKey: false,
      metaKey: false,
      shiftKey: false,
      isComposing: false,
      timeStamp: 0,
      stopPropagation() {
        this.stopped = true;
      },
      preventDefault() {
        this.defaultPrevented = true;
      },
      ...props,
    };
    for (const f of this.hostLs.get(type) ?? []) f(ev);
    if (!ev.stopped) this.xtermLs.get(type)?.(ev);
    return ev;
  }

  install(): () => void {
    const term = { textarea: this.textarea } as never;
    const mac = installMacKeyboard(this.host as never, term, this.send, true);
    this.arbitrate = mac.arbitrate;
    return () => {
      mac.uninstall();
      this.arbitrate = () => true;
    };
  }
}

/** 英文输入法敲一颗键：keydown → keypress → input，没有任何 composition 事件 */
function typeAscii(h: Harness, ch: string, keyCode: number): void {
  const down = h.fire("keydown", { key: ch, keyCode, code: "Key" });
  // keypress 只在 keydown 没被 preventDefault 时派发（浏览器行为）
  if (!down.defaultPrevented) {
    h.textarea.value += ch;
    h.fire("keypress", { key: ch, keyCode, charCode: ch.charCodeAt(0) });
    h.fire("input", { inputType: "insertText", data: ch });
  }
}

/**
 * 回归：mac 英文输入法下标点重复。
 *
 * 标点走的是「只 stopPropagation 不 preventDefault」那条分支（要让 WebKit 自己
 * 决定是否替换成全角），于是 keypress 照样到得了 xterm，xterm 发一份、接管层在
 * `input` 里又发一份。
 */
test("英文输入法下每个标点只发一次", () => {
  const h = new Harness();
  const uninstall = h.install();
  try {
    for (const [ch, kc] of [
      [".", 190], [",", 188], [";", 186], [":", 186], ["'", 222],
      ['"', 222], ["!", 49], ["?", 191], ["(", 57], [")", 48],
      ["[", 219], ["]", 221], ["<", 188], [">", 190], ["$", 52],
      ["\\", 220], ["^", 54], ["_", 189], ["`", 192], ["~", 192],
    ] as Array<[string, number]>) {
      h.sent = [];
      typeAscii(h, ch, kc);
      assert.deepEqual(h.sent, [ch], `标点 ${ch} 发出的不是恰好一份`);
    }
  } finally {
    uninstall();
  }
});

/** 字母走的是 preventDefault 那条路，本来就不该有 keypress —— 一并钉住 */
test("英文输入法下普通字母只发一次", () => {
  const h = new Harness();
  const uninstall = h.install();
  try {
    for (const ch of "hello world 123-=/*") {
      h.sent = [];
      typeAscii(h, ch, ch.toUpperCase().charCodeAt(0));
      assert.deepEqual(h.sent, [ch], `字符 ${ch} 发出的不是恰好一份`);
    }
  } finally {
    uninstall();
  }
});

/** 连着敲一句话，总字节必须和敲进去的完全一致 —— 多字 / 丢字都会在这里露馅 */
test("连续输入一整条命令，字节一字不差", () => {
  const h = new Harness();
  const uninstall = h.install();
  try {
    const line = `grep -n "foo.bar" ./src/*.ts | head -5`;
    for (const ch of line) typeAscii(h, ch, ch.toUpperCase().charCodeAt(0));
    assert.equal(h.sent.join(""), line);
  } finally {
    uninstall();
  }
});

/**
 * 中文输入法敲一颗「智能标点」键（`.` → `。`，`?` → `？`，`\` → `、`）。
 *
 * 真机日志（见 macInput 里 PUNCT_RISK 的注释）证明两件事都可能发生：`input`
 * 早于 keydown，且 `ev.key` 到达时可能已经是替换后的全角、keyCode 变成 229。
 * 两种顺序都得只出一个字符，所以两种都跑一遍。
 *
 * @param inputFirst input 先于 keydown 到（WebKit bug 165004 的那一路）
 */
function typeSmartPunct(
  h: Harness,
  half: string,
  full: string,
  keyCode: number,
  inputFirst: boolean
): void {
  if (inputFirst) {
    h.textarea.value += full;
    h.fire("input", { inputType: "insertText", data: full });
    // key 已是全角、keyCode 被输入法上下文染成 229 —— 这正是 xterm 丢字的现场
    h.fire("keydown", { key: full, keyCode: 229, code: "Punct" });
    h.fire("keyup", { key: full, keyCode });
    return;
  }
  const down = h.fire("keydown", { key: half, keyCode, code: "Punct" });
  if (down.defaultPrevented) return;
  h.fire("keypress", { key: half, keyCode, charCode: half.charCodeAt(0) });
  h.textarea.value += full;
  h.fire("input", { inputType: "insertText", data: full });
}

/** macOS 系统拼音的智能标点映射里最常用的几个 */
const SMART_PUNCT: Array<[string, string, number]> = [
  [".", "。", 190],
  [",", "，", 188],
  ["?", "？", 191],
  ["\\", "、", 220],
  [";", "；", 186],
  [":", "：", 186],
  ["!", "！", 49],
  ["<", "《", 188],
  [">", "》", 190],
];

/**
 * 回归：全角标点必须**恰好出一个**。
 *
 * 两个方向的故障都出过：`、` `/` 出两个（xterm 和接管层各发一份），`？` 一个
 * 都出不来（接管层见非 ASCII 就整条让给 xterm，而 xterm 在 keyCode 229 下
 * `_keyDown` 什么都不发、`input` 又被 `_keyDownSeen` 门丢掉）。
 */
for (const first of [false, true]) {
  test(`中文输入法全角标点恰好出一个（input ${first ? "先于" : "后于"} keydown）`, () => {
    const h = new Harness();
    const un = h.install();
    try {
      for (const [half, full, kc] of SMART_PUNCT) {
        h.sent = [];
        h.textarea.value = "";
        typeSmartPunct(h, half, full, kc, first);
        assert.deepEqual(h.sent, [full], `${half} → ${full} 发出的不是恰好一份`);
      }
    } finally {
      un();
    }
  });
}

/** 没被替换的标点（中文输入法下 `/` 原样输出）走的是另一条分支，一并钉住 */
for (const first of [false, true]) {
  test(`中文输入法下不替换的 / 也只出一个（input ${first ? "先于" : "后于"} keydown）`, () => {
    const h = new Harness();
    const un = h.install();
    try {
      h.sent = [];
      typeSmartPunct(h, "/", "/", 191, first);
      assert.deepEqual(h.sent, ["/"]);
    } finally {
      un();
    }
  });
}

/** 连打同一个全角标点不能被当成重复吞掉 —— 每颗键有自己的 keydown */
test("连打 。。。 三个都在", () => {
  const h = new Harness();
  const un = h.install();
  try {
    h.sent = [];
    for (let i = 0; i < 3; i++) typeSmartPunct(h, ".", "。", 190, false);
    assert.equal(h.sent.join(""), "。。。");
  } finally {
    un();
  }
});

/**
 * 仲裁闸的契约（`OnceGuard`）。这是整层最后的兜底：不管 WKWebView 把事件派成
 * 什么顺序，同一颗键里同一个字符只放第一份过去。
 */
test("仲裁：同一颗键里重复字符只放第一份", () => {
  const h = new Harness();
  const un = h.install();
  try {
    h.fire("keydown", { key: "。", keyCode: 229, code: "Punct" });
    assert.equal(h.arbitrate("。"), true, "第一份要放行");
    assert.equal(h.arbitrate("。"), false, "第二份是同一颗键的重复");
    assert.equal(h.arbitrate("、"), true, "不同字符照常放行");

    h.fire("keydown", { key: "。", keyCode: 229, code: "Punct" });
    assert.equal(h.arbitrate("。"), true, "下一颗 keydown 重新开账");
  } finally {
    un();
  }
});

/** 控制字节、转义序列、整段粘贴永远原样放行 —— 仲裁只管单个可见字符 */
test("仲裁：回车 / 方向键 / 粘贴不去重", () => {
  const h = new Harness();
  const un = h.install();
  try {
    for (const d of ["\r", "\x03", "\x1b[A", "你好", "ls -la\r"]) {
      assert.equal(h.arbitrate(d), true, `${JSON.stringify(d)} 第一次该放行`);
      assert.equal(h.arbitrate(d), true, `${JSON.stringify(d)} 第二次也该放行`);
    }
  } finally {
    un();
  }
});

/** 卸载之后不能再有任何监听残留，否则换终端会叠成两层接管、每个字符翻倍 */
test("卸载后不再处理事件", () => {
  const h = new Harness();
  h.install()();
  h.sent = [];
  typeAscii(h, ".", 190);
  // 接管层已走，只剩被建模的 xterm 自己那一份
  assert.deepEqual(h.sent, ["."]);
});
