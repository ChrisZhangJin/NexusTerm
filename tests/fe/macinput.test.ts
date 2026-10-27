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
      if (isPlainChar(key)) this.send(key);
    });
  }

  /** 接管层与 xterm 共用的唯一出口（对应 TerminalView 里的 handleInput） */
  readonly send = (d: string): void => {
    this.sent.push(d);
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
    return installMacKeyboard(this.host as never, term, this.send, true);
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

/** 卸载之后不能再有任何监听残留，否则换终端会叠成两层接管、每个字符翻倍 */
test("卸载后不再处理事件", () => {
  const h = new Harness();
  h.install()();
  h.sent = [];
  typeAscii(h, ".", 190);
  // 接管层已走，只剩被建模的 xterm 自己那一份
  assert.deepEqual(h.sent, ["."]);
});
