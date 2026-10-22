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

const out = path.join(os.tmpdir(), "nx-fe-test-bundle.mjs");

const redirect = {
  name: "fe-test-redirect",
  setup(build) {
    build.onResolve({ filter: /(^|\/)\.\.\/api$/ }, () => ({ path: stub("api-stub.ts") }));
    build.onResolve({ filter: /^@xterm\// }, () => ({ path: xtermStub }));
  },
};

await esbuild.build({
  entryPoints: [path.join(testsDir, "complete.test.ts")],
  bundle: true,
  outfile: out,
  platform: "node",
  format: "esm",
  target: `node${process.versions.node.split(".")[0]}`,
  plugins: [redirect],
  external: ["node:*"],
  logLevel: "warning",
});

const r = spawnSync(process.execPath, ["--test", out], { stdio: "inherit" });
process.exit(r.status ?? 1);
