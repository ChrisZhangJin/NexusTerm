/**
 * 前端自测的 runner：`npm test`
 *
 * 项目里没有（也不想为此引入）测试框架，但有几处纯逻辑必须能验：
 * console 补全的 insert 切片、灰字的字节序列、`@` 联想的下标计算。
 * 这些错了在界面上只是「好像不太对」，靠手点根本抓不住。
 *
 * 做法很轻：node 自带 `node:test`，只需要把 TS 转成 JS。
 * esbuild 是 vite 的既有依赖，不新增任何包。
 *
 * 两处重定向是必须的：
 * - `../api` → tests/fe/api-stub（node 里没有 Tauri 运行时）
 * - `@xterm/*` → 空桩（这些模块只被 `import type` 用到，运行时不需要）
 */

import { fileURLToPath } from "node:url";
import path from "node:path";
import fs from "node:fs";
import os from "node:os";
import { spawnSync } from "node:child_process";
import * as esbuild from "esbuild";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const testsDir = path.join(root, "tests", "fe");

/** macOS 上 /tmp 是 /private/tmp 的软链；不取 realpath 会把同一模块当成两份 */
const stub = (name) => fs.realpathSync(path.join(testsDir, name));

const xtermStub = path.join(os.tmpdir(), "nx-fe-test-xterm-stub.ts");
fs.writeFileSync(xtermStub, "export {};\n");

const redirect = {
  name: "fe-test-redirect",
  setup(build) {
    build.onResolve({ filter: /(^|\/)\.\.\/api$/ }, () => ({ path: stub("api-stub.ts") }));
    build.onResolve({ filter: /^@xterm\// }, () => ({ path: xtermStub }));
  },
};

/**
 * 每个 `*.test.ts` 单独打一个包、各自一个进程跑：
 * 用例之间共享的模块级状态（比如 api-stub 里的调用记录）不会互相污染。
 */
const entries = fs
  .readdirSync(testsDir)
  .filter((f) => f.endsWith(".test.ts"))
  .sort();

if (entries.length === 0) {
  console.error("没有找到任何 tests/fe/*.test.ts");
  process.exit(1);
}

const outs = [];
for (const entry of entries) {
  const out = path.join(os.tmpdir(), `nx-fe-test-${entry.replace(/\.ts$/, "")}.mjs`);
  await esbuild.build({
    entryPoints: [path.join(testsDir, entry)],
    bundle: true,
    outfile: out,
    platform: "node",
    format: "esm",
    target: `node${process.versions.node.split(".")[0]}`,
    plugins: [redirect],
    external: ["node:*"],
    logLevel: "warning",
  });
  outs.push(out);
}

/**
 * 把子进程固定跑在 UTC 上。
 *
 * `formatTime` 按**本地时区**格式化（界面上就该这么显示），而用例断言的是
 * 绝对时间串（`formatTime(0) === "1970-01-01 00:00"`）。写死的那串只在 UTC
 * 成立 —— 在东八区跑就是 `08:00`，于是同一份代码在不同机器上红绿不一致。
 *
 * 与其把断言改成「拿被测函数自己的算法再算一遍」（那就成了恒真断言），
 * 不如把时区钉死：断言仍然在验真实输出，且谁跑都一样。
 *
 * Node 上 `process.env.TZ` 赋值会即时生效（会通知 V8 重置时区缓存），
 * 子进程继承 env，所以在这里设就够了。
 */
process.env.TZ = "UTC";

const r = spawnSync(process.execPath, ["--test", ...outs], { stdio: "inherit" });
process.exit(r.status ?? 1);
