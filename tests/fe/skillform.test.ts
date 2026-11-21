/**
 * Skill 编辑器表单逻辑的自测：`lib/skillForm`。跑法：`npm test`
 *
 * 钉住的是「改坏了界面上看不出来、但会让 skill 悄悄变样」的几处：
 * 表单 ⇄ SkillInput 往返不丢字段、secret 不带默认值、参数改名同步所有引用、步骤重排、
 * 后端问题归类到正确的步骤 / 参数。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { Skill, SkillInput } from "../../src/types";
import {
  declareMissing,
  describeHostScope,
  exportFileName,
  formatLastRun,
  groupProblems,
  hasLocalProblems,
  hostsFromText,
  localProblems,
  moveItem,
  newStep,
  parseTimeout,
  placeholderRefs,
  placeholderSegments,
  removeAt,
  removeParam,
  renameParam,
  renamePlaceholder,
  replayChanged,
  respondRef,
  riskRaised,
  secretDefaultsIn,
  setParamSecret,
  setRespondSecret,
  skillToInput,
  splitProblems,
  toForm,
  toInput,
} from "../../src/lib/skillForm";

const full = (): SkillInput => ({
  name: "部署 web",
  description: "拉代码并重启",
  params: [
    { name: "branch", description: "分支", default: "main", required: false, secret: false },
    { name: "sudo_pw", description: "", required: true, secret: true },
  ],
  steps: [
    { kind: "exec", command: "git checkout {{branch}}", timeoutMs: 20000, expectAfter: "Switched to", onUnexpected: "stop" },
    { kind: "keys", keys: "<C-c>" },
    { kind: "expect_respond", expect: "[Y/n]", respond: "y<Enter>", secret: false, onUnexpected: "llm" },
    { kind: "expect_respond", expect: "password:", respond: "{{sudo_pw}}", secret: true },
    { kind: "secret", reason: "sudo 密码", param: "sudo_pw" },
    { kind: "screen_check", expect: "/active \\(running\\)/", timeoutMs: 5000 },
  ],
  verify: { expect: "ok {{branch}}", timeoutMs: 5000 },
  risk: "medium",
  hostScope: { kind: "list", hosts: ["abc-id", "*.prod.example.com"] },
  source: "recorded",
});

test("toForm → toInput 往返不丢字段", () => {
  const a = full();
  assert.deepEqual(toInput(toForm(a)), a);
});

test("往返：最小输入（无 verify / any / 无可选字段）", () => {
  const a: SkillInput = {
    name: "x",
    description: "",
    params: [],
    steps: [{ kind: "exec", command: "ls" }],
    verify: null,
    risk: "low",
    hostScope: { kind: "any" },
    source: "manual",
  };
  assert.deepEqual(toInput(toForm(a)), a);
});

test("toInput：显式 ask 省略、未给 source 的草稿落到 manual、名称 trim", () => {
  const a = full();
  a.steps = [{ kind: "exec", command: "ls", onUnexpected: "ask" }];
  a.name = "  n  ";
  delete a.source;
  const out = toInput(toForm(a));
  assert.deepEqual(out.steps[0], { kind: "exec", command: "ls" });
  assert.equal(out.name, "n");
  assert.equal(out.source, "manual");
});

test("切换步骤种类：只输出当前种类的字段，旧文字留在表单里", () => {
  const f = toForm(full());
  const s = { ...f.steps[0], kind: "keys" as const, keys: "q" };
  const out = toInput({ ...f, steps: [s] }).steps[0];
  assert.deepEqual(out, { kind: "keys", keys: "q", timeoutMs: 20000, expectAfter: "Switched to", onUnexpected: "stop" });
  assert.equal(s.command, "git checkout {{branch}}"); // 切回去还在
});

test("secret 参数：载入时丢弃默认值、输出不带 default；开启 secret 清默认值", () => {
  const a = full();
  a.params[1] = { name: "pw", description: "", default: "hunter2", required: false, secret: true };
  assert.deepEqual(secretDefaultsIn(a), ["pw"]);
  const f = toForm(a);
  assert.equal(f.params[1].defaultValue, "");
  assert.equal("default" in toInput(f).params[1], false);

  const p = setParamSecret({ ...f.params[0] }, true);
  assert.equal(p.defaultValue, "");
  assert.equal("default" in toInput({ ...f, params: [p] }).params[0], false);
  // 即便有人绕过 setParamSecret 直接塞值，toInput 也不输出
  const sneaky = { ...f.params[0], secret: true, defaultValue: "x" };
  assert.equal("default" in toInput({ ...f, params: [sneaky] }).params[0], false);
});

test("expect_respond 开 secret：字面应答被清空，单一引用保留", () => {
  const s = { ...newStep("expect_respond"), respond: "hunter2<Enter>" };
  assert.equal(setRespondSecret(s, true).respond, "");
  const ref = { ...s, respond: "{{ pw }}" };
  assert.equal(setRespondSecret(ref, true).respond, "{{ pw }}");
  assert.equal(setRespondSecret({ ...ref, respondSecret: true }, false).respondSecret, false);
  assert.equal(respondRef("{{pw}}"), "pw");
  assert.equal(respondRef("a{{pw}}"), null);
});

test("parseTimeout / localProblems：空 = 不设，非整数或越界 = 无效", () => {
  assert.deepEqual(parseTimeout(""), { ok: true, value: undefined });
  assert.deepEqual(parseTimeout(" 5000 "), { ok: true, value: 5000 });
  for (const bad of ["abc", "1.5", "-1", "50", "600001"]) assert.equal(parseTimeout(bad).ok, false, bad);

  const f = toForm(full());
  assert.equal(hasLocalProblems(localProblems(f)), false);
  const bad = { ...f, steps: [{ ...f.steps[0], timeoutMs: "x" }, ...f.steps.slice(1)], verifyTimeoutMs: "9" };
  const lp = localProblems(bad);
  assert.deepEqual([...lp.stepTimeout], [bad.steps[0].uid]);
  assert.equal(lp.verifyTimeout, true);
  assert.equal(hasLocalProblems(lp), true);
});

test("renamePlaceholder：保留空白、不碰转义与模板语法、不碰别的名字", () => {
  assert.equal(
    renamePlaceholder("a {{x}} {{ x }} {{xy}} \\{{x}} {{.x}}", "x", "z"),
    "a {{z}} {{ z }} {{xy}} \\{{x}} {{.x}}"
  );
});

test("renameParam：同步命令 / 按键 / 应答 / pattern / verify / secret 步骤 param", () => {
  const f = toForm(full());
  const out = renameParam(f, f.params[0].uid, "ref"); // branch
  const i = toInput(out);
  assert.equal(i.params[0].name, "ref");
  assert.equal((i.steps[0] as { command: string }).command, "git checkout {{ref}}");
  assert.equal(i.verify?.expect, "ok {{ref}}");

  const pw = renameParam(f, f.params[1].uid, "pw2");
  const pi = toInput(pw);
  assert.equal((pi.steps[3] as { respond: string }).respond, "{{pw2}}");
  assert.equal((pi.steps[4] as { param?: string }).param, "pw2");
});

test("renameParam：未受影响的步骤保持同一引用（memo 行不重渲染）", () => {
  const f = toForm(full());
  const out = renameParam(f, f.params[0].uid, "ref");
  assert.equal(out.steps[1], f.steps[1]); // keys 步骤没引用 branch
  assert.notEqual(out.steps[0], f.steps[0]);
});

test("renameParam：新名字无效或重名只改名、不传播", () => {
  const f = toForm(full());
  const uid = f.params[0].uid;
  for (const bad of ["", "1x", "a b", "sudo_pw"]) {
    const out = renameParam(f, uid, bad);
    assert.equal(out.params[0].name, bad);
    assert.deepEqual(out.steps, f.steps, bad);
    assert.equal(out.verifyExpect, f.verifyExpect);
  }
  assert.equal(renameParam(f, 99999, "q"), f);
});

test("removeParam：清掉 secret 步骤对它的引用，占位符留给「未声明」提示", () => {
  const f = toForm(full());
  const out = removeParam(f, f.params[1].uid);
  assert.equal(out.params.length, 1);
  assert.equal(out.steps[4].param, "");
  assert.equal(out.steps[3].respond, "{{sudo_pw}}");
});

test("declareMissing：补齐未声明的参数", () => {
  const f = toForm(full());
  const g = { ...f, params: [], steps: [{ ...newStep("exec"), command: "echo {{a}} {{b}} {{a}}" }] };
  const { form, added } = declareMissing(g);
  assert.deepEqual(added, ["a", "b", "branch"]); // verify 里的引用也算
  assert.deepEqual(form.params.map((p) => p.name), ["a", "b", "branch"]);
  assert.equal(declareMissing(form).added.length, 0);
});

test("moveItem / removeAt：重排与越界", () => {
  assert.deepEqual(moveItem([1, 2, 3], 0, 1), [2, 1, 3]);
  assert.deepEqual(moveItem([1, 2, 3], 2, -1), [1, 3, 2]);
  const a = [1, 2, 3];
  assert.equal(moveItem(a, 0, -1), a);
  assert.equal(moveItem(a, 2, 1), a);
  assert.deepEqual(removeAt(a, 1), [1, 3]);
  assert.equal(removeAt(a, 9), a);
  assert.deepEqual(a, [1, 2, 3]); // 不改原数组
});

test("hostsFromText：换行 / 逗号 / 空白分隔、去重", () => {
  assert.deepEqual(hostsFromText("a\n b, c ，a;d"), ["a", "b", "c", "d"]);
  assert.deepEqual(hostsFromText("  \n "), []);
});

test("placeholderSegments / Refs：已声明、未声明、secret、转义", () => {
  const segs = placeholderSegments("a {{x}} {{y}} \\{{z}} {{pw}}", new Set(["x", "pw"]), new Set(["pw"]));
  assert.deepEqual(
    segs.map((s) => [s.kind, s.text]),
    [
      ["text", "a "],
      ["param", "{{x}}"],
      ["text", " "],
      ["undeclared", "{{y}}"],
      ["text", " \\{{z}} "],
      ["param", "{{pw}}"],
    ]
  );
  assert.equal(segs[5].secret, true);
  assert.equal(segs.map((s) => s.text).join(""), "a {{x}} {{y}} \\{{z}} {{pw}}");
  assert.deepEqual(placeholderRefs("{{x}}{{x}}{{y}}", new Set(["x"])), [
    { name: "x", declared: true, secret: false },
    { name: "y", declared: false, secret: false },
  ]);
  assert.deepEqual(placeholderSegments("", new Set()), []);
});

test("groupProblems：按前缀归到步骤 / 参数 / verify / hosts / 风险 / 整体", () => {
  const g = groupProblems([
    "名称不能为空",
    "第 2 步（exec）：命令为空",
    "第 2 步（exec）的 expectAfter：pattern 无效",
    "第 10 步（keys）：xx",
    "secret 参数「pw」不能有默认值",
    "参数名「1x」无效（…）",
    "参数「a」的默认值无效",
    "verify：pattern 为空",
    "hostScope 为 list 时至少要有一项",
    "风险等级 Low 低于自动评估的 High：第 1 步（exec）：sudo",
    "至少需要一个步骤",
  ]);
  assert.deepEqual(g.general, ["名称不能为空", "至少需要一个步骤"]);
  assert.equal(g.steps.get(1)?.length, 2);
  assert.equal(g.steps.get(9)?.length, 1);
  assert.equal(g.params.get("pw")?.length, 1);
  assert.equal(g.params.get("1x")?.length, 1);
  assert.equal(g.params.get("a")?.length, 1);
  assert.equal(g.verify.length, 1);
  assert.equal(g.hosts.length, 1);
  assert.equal(g.risk.length, 1);
});

test("splitProblems：换行分隔、去空行", () => {
  assert.deepEqual(splitProblems("a\n\n b \n"), ["a", "b"]);
  assert.deepEqual(splitProblems(""), []);
});

test("replayChanged：只看步骤 / 参数 / 验证 / 范围，不看名称描述", () => {
  const a = full();
  assert.equal(replayChanged(a, { ...a, name: "别名", description: "d" }), false);
  assert.equal(replayChanged(a, { ...a, steps: a.steps.slice(1) }), true);
  assert.equal(replayChanged(a, { ...a, verify: null }), true);
  assert.equal(replayChanged(a, { ...a, hostScope: { kind: "any" } }), true);
  assert.equal(replayChanged(a, { ...a, params: [] }), true);
});

test("riskRaised / describeHostScope / exportFileName", () => {
  assert.equal(riskRaised("low", "high"), true);
  assert.equal(riskRaised("high", "high"), false);
  assert.equal(riskRaised("high", "low"), false);
  assert.equal(describeHostScope({ kind: "any" }), "不限");
  assert.equal(describeHostScope({ kind: "list", hosts: ["a", "b", "c"] }), "a、b 等 3 项");
  assert.equal(exportFileName("a/b:c"), "a_b_c.skill.json");
  assert.equal(exportFileName("  "), "skill.skill.json");
});

test("formatLastRun", () => {
  const now = 1_700_000_000_000;
  const s = now / 1000;
  assert.equal(formatLastRun(null, now), "从未运行");
  assert.equal(formatLastRun(s - 10, now), "刚刚");
  assert.equal(formatLastRun(s - 300, now), "5 分钟前");
  assert.equal(formatLastRun(s - 7200, now), "2 小时前");
  assert.equal(formatLastRun(s - 86400 * 3, now), "3 天前");
  assert.match(formatLastRun(s - 86400 * 90, now), /^\d{4}-\d{2}-\d{2}$/);
});

test("skillToInput：去掉 id / 计数 / 时间戳，内容原样", () => {
  const a = full();
  const sk: Skill = {
    ...a,
    source: "imported",
    id: "x",
    successCount: 3,
    failCount: 1,
    lastRunTs: 5,
    createdAt: 1,
    updatedAt: 2,
    schemaVersion: 1,
  };
  assert.deepEqual(skillToInput(sk), { ...a, source: "imported" });
});
