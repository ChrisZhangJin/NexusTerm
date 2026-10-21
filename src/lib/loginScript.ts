import type { Terminal } from "@xterm/xterm";
import { getTerm, lastOutputAt } from "./termRegistry";

/**
 * 跳板机 / 堡垒机的「登录路径」：登录后先出一屏菜单让你选资产，选完才进目标机器。
 *
 * 这类菜单对 SSH 客户端是黑盒 —— 服务端只是一行行吐文本、等你的按键。所以这里
 * 用两个笨但可靠的手段：
 *
 * - **识别**：读 xterm 的屏幕缓冲（不是原始字节流，已经处理过光标移动与控制码），
 *   找「若干条带序号的行 + 一句『请选择』」的特征。
 * - **学习**：菜单停在屏幕上、输出静止时，把用户在那一刻敲的键按回车切成步骤存下来。
 *   重连时检测到同样的菜单，就按原样敲回去。
 *
 * 之所以要「检测到菜单才录」，是为了不把普通命令历史（`ls`、`cd`）当成登录路径：
 * 屏幕不像菜单时录制器根本不武装。
 */

/** 一个步骤最长多少字符 —— 超过就认为不是「选择」，是命令 */
const STEP_MAX = 64;
/** 最多记几步。正常堡垒机 2~3 步，太多说明误判 */
export const STEPS_MAX = 6;
/** 录制窗口：连接建立后最多观察这么久 */
const RECORD_WINDOW = 90_000;
/** 输出静默多久算「菜单已经画完」 */
const QUIET_MS = 400;

const MENU_WORDS =
  /(请选择|选择要|选择资产|资产列表|请输入序号|输入序号|按序号|选择操作|选择一台|Select |Please select|Choose |Option|Choice|selection)/i;
/** 「1. xxx」「[2] xxx」「3) xxx」——序号行 */
const NUMBERED = /^\s*[\[(（【]?\s*\d{1,3}\s*[\])）】\.、:：]?\s+\S/;

function screenLines(term: Terminal, max: number): string[] {
  const buf = term.buffer.active;
  const cursor = buf.baseY + buf.cursorY;
  const start = Math.max(0, cursor - max + 1);
  const out: string[] = [];
  for (let y = start; y <= cursor; y++) {
    const line = buf.getLine(y);
    if (!line) continue;
    out.push(line.translateToString(true).trimEnd());
  }
  // 去掉尾部空行，避免「菜单 + 空白」把特征冲淡
  while (out.length && !out[out.length - 1].trim()) out.pop();
  return out;
}

/**
 * 屏幕上现在是不是一个「请你选一个」的界面。
 *
 * 判据刻意保守：必须有菜单词，并且至少两条序号行（或最后一行以冒号结尾，
 * 比如 `请选择资产:`）。只看序号行会把 `ls -l`、`top` 的输出当成菜单。
 */
export function looksLikeMenu(term: Terminal): boolean {
  const lines = screenLines(term, 24);
  if (lines.length === 0) return false;
  const text = lines.join("\n");
  if (!MENU_WORDS.test(text)) return false;
  const numbered = lines.filter((l) => NUMBERED.test(l) && l.trim().length <= 100).length;
  if (numbered >= 2) return true;
  const tail = lines[lines.length - 1].trim();
  return numbered >= 1 && /[:：]\s*$/.test(tail);
}

const sleep = (ms: number) => new Promise((r) => window.setTimeout(r, ms));

/**
 * 等「菜单画完并静止」。`after` 之后必须至少来过一次输出，
 * 否则会拿上一次的旧屏幕误判（重连场景尤其明显）。
 */
export async function waitForMenu(
  tabId: string,
  after: number,
  timeoutMs = 25_000
): Promise<boolean> {
  const t0 = Date.now();
  for (;;) {
    await sleep(200);
    const term = getTerm(tabId);
    if (!term) return false;
    const last = lastOutputAt(tabId);
    if (last >= after && Date.now() - last >= QUIET_MS && looksLikeMenu(term)) return true;
    if (Date.now() - t0 > timeoutMs) return false;
  }
}

/** 等输出静止（这次输出结束，屏幕稳定下来） */
export async function waitQuiet(
  tabId: string,
  quiet = 700,
  timeoutMs = 20_000
): Promise<boolean> {
  const t0 = Date.now();
  for (;;) {
    await sleep(120);
    if (Date.now() - lastOutputAt(tabId) >= quiet) return true;
    if (Date.now() - t0 > timeoutMs) return false;
  }
}

/** 脚本按行切分，去掉空行与首尾空白 */
export function parseSteps(script: string): string[] {
  return script
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter(Boolean)
    .slice(0, STEPS_MAX);
}

/** 把步骤数组还原成存库格式 */
export function joinSteps(steps: string[]): string {
  return steps.slice(0, STEPS_MAX).join("\n");
}

/**
 * 登录路径录制器。生命周期跟着一个标签的一次连接：
 * 连接建立时 `start()`，菜单出现时武装，用户按回车时收下一步。
 *
 * 只在「屏幕是菜单」时记录，所以普通操作不会被误记。录到新路径时通过
 * `onDone` 回调交给上层落库 —— 这里不碰存储，方便单测。
 */
export class LoginRecorder {
  private armed = false;
  private steps: string[] = [];
  private current = "";
  private startedAt = 0;
  private deadline = 0;
  private lastStepAt = 0;
  private done = false;
  private timer?: number;

  constructor(
    private tabId: string,
    private onDone: (script: string) => void
  ) {}

  start(): void {
    this.startedAt = Date.now();
    this.deadline = this.startedAt + RECORD_WINDOW;
    this.timer = window.setInterval(() => this.tick(), 400);
  }

  /** 用户按键（来自 termRegistry 的旁路监听） */
  handleInput(data: string): void {
    if (!this.armed) return;
    // 方向键 / 功能键 / 粘贴控制码不是「选择」，直接忽略整段
    if (data.startsWith("\x1b")) return;
    for (const ch of data) {
      if (ch === "\r" || ch === "\n") {
        const step = this.current.trim();
        this.current = "";
        this.armed = false;
        if (step && step.length <= STEP_MAX) {
          this.steps.push(step);
          this.lastStepAt = Date.now();
          if (this.steps.length >= STEPS_MAX) this.finish();
        }
        return;
      }
      if (ch === "\x7f" || ch === "\b") {
        this.current = this.current.slice(0, -1);
        continue;
      }
      if (ch === "\x03" || ch === "\x15") {
        // Ctrl-C / Ctrl-U：用户放弃了这次选择
        this.current = "";
        this.armed = false;
        return;
      }
      this.current += ch;
      if (this.current.length > STEP_MAX) {
        this.current = "";
        this.armed = false;
        return;
      }
    }
  }

  private tick(): void {
    if (this.done) return;
    const now = Date.now();
    if (now > this.deadline) {
      this.finish();
      return;
    }
    // 放过一步之后，菜单迟迟不回来 → 说明已经进到目标机器，录制结束
    if (this.steps.length > 0 && !this.armed && now - this.lastStepAt > 6000) {
      this.finish();
      return;
    }
    if (this.armed) return;
    const term = getTerm(this.tabId);
    if (!term) return;
    // 3 秒的反向宽限：start() 是在 dial 返回之后调用的，而堡垒机的菜单可能在
    // 认证完成的那一刻就已经画好了 —— 卡死在 startedAt 上会永远等不到「新输出」。
    // 又不能用更早的时间：重连时旧屏还留着上一次的菜单，得排除掉。
    const last = lastOutputAt(this.tabId);
    if (last >= this.startedAt - 3000 && now - last >= QUIET_MS && looksLikeMenu(term)) {
      this.armed = true;
      this.current = "";
    }
  }

  private finish(): void {
    if (this.done) return;
    this.done = true;
    this.dispose();
    if (this.steps.length) this.onDone(joinSteps(this.steps));
  }

  dispose(): void {
    if (this.timer) window.clearInterval(this.timer);
    this.timer = undefined;
  }
}
