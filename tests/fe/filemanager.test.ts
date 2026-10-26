/**
 * SFTP 面板纯逻辑的自测：排序比较、类型/大小/时间/剩余时间格式化。
 * 跑法：`npm test`。
 *
 * `FileManager.tsx` 间接依赖 `lib/prefs.ts`，而那边有一段模块顶层的
 * `localStorage` 迁移代码——纯 Node 环境里没有这个全局，静态 `import`
 * 会在模块求值阶段就炸掉。用动态 `import()` + 顶层 await，在求值之前
 * 先把 `localStorage` 垫上，就能绕开（这段垫片只影响本文件，esbuild
 * 按文件单独打包，不会污染其它测试）。
 */

import assert from "node:assert/strict";
import test from "node:test";
import type { RemoteEntry } from "../../src/types";

const store = new Map<string, string>();
(globalThis as unknown as { localStorage: Storage }).localStorage = {
  getItem: (k: string) => store.get(k) ?? null,
  setItem: (k: string, v: string) => void store.set(k, v),
  removeItem: (k: string) => void store.delete(k),
  clear: () => store.clear(),
  key: () => null,
  get length() {
    return store.size;
  },
} as Storage;

const { compareEntries, formatDuration, formatEta, formatSize, formatTime, typeLabel } = await import(
  "../../src/components/FileManager"
);

function entry(over: Partial<RemoteEntry> = {}): RemoteEntry {
  return {
    name: "a.txt",
    path: "/a.txt",
    isDir: false,
    size: 0,
    mtime: null,
    permissions: null,
    ...over,
  };
}

// ——— typeLabel ———

test("typeLabel：目录固定「文件夹」，与扩展名无关", () => {
  assert.equal(typeLabel(entry({ isDir: true, name: "logs" })), "文件夹");
  assert.equal(typeLabel(entry({ isDir: true, name: "a.tar.gz" })), "文件夹");
});

test("typeLabel：按扩展名大写拼「XXX 文件」", () => {
  assert.equal(typeLabel(entry({ name: "readme.md" })), "MD 文件");
  assert.equal(typeLabel(entry({ name: "archive.tar.gz" })), "GZ 文件");
});

test("typeLabel：没有扩展名 / 隐藏文件（.bashrc 式）都归到「文件」", () => {
  assert.equal(typeLabel(entry({ name: "README" })), "文件");
  assert.equal(typeLabel(entry({ name: ".bashrc" })), "文件");
  assert.equal(typeLabel(entry({ name: "trailing." })), "文件");
});

// ——— compareEntries ———

test("compareEntries：目录永远排文件前面，不受排序列/方向影响", () => {
  const dir = entry({ isDir: true, name: "zzz-dir", size: 999 });
  const file = entry({ isDir: false, name: "aaa-file", size: 1 });
  assert.ok(compareEntries(dir, file, "name", 1) < 0);
  assert.ok(compareEntries(dir, file, "name", -1) < 0, "方向翻转也不该把目录排后面");
  assert.ok(compareEntries(dir, file, "size", -1) < 0);
});

test("compareEntries：按名称升序/降序", () => {
  const a = entry({ name: "a.txt" });
  const b = entry({ name: "b.txt" });
  assert.ok(compareEntries(a, b, "name", 1) < 0);
  assert.ok(compareEntries(a, b, "name", -1) > 0);
});

test("compareEntries：按大小排序", () => {
  const small = entry({ name: "s", size: 10 });
  const big = entry({ name: "b", size: 1000 });
  assert.ok(compareEntries(small, big, "size", 1) < 0);
  assert.ok(compareEntries(small, big, "size", -1) > 0);
});

test("compareEntries：按修改时间排序，mtime 缺失当成最早（0）", () => {
  const unknown = entry({ name: "u", mtime: null });
  const known = entry({ name: "k", mtime: 1000 });
  assert.ok(compareEntries(unknown, known, "mtime", 1) < 0);
});

test("compareEntries：按类型排序", () => {
  const md = entry({ name: "x.md" });
  const zz = entry({ name: "x.zz" });
  const c = compareEntries(md, zz, "type", 1);
  assert.equal(Math.sign(c), Math.sign("MD 文件".localeCompare("ZZ 文件")));
});

test("compareEntries：同一列相等时返回 0（排序稳定，交给 Array.sort）", () => {
  const a = entry({ name: "same", size: 5 });
  const b = entry({ name: "same", size: 5 });
  assert.equal(compareEntries(a, b, "name", 1), 0);
  assert.equal(compareEntries(a, b, "size", 1), 0);
});

// ——— formatSize / formatTime ———

test("formatSize：按 B/K/M/G 分档", () => {
  assert.equal(formatSize(0), "0 B");
  assert.equal(formatSize(1023), "1023 B");
  assert.equal(formatSize(1024), "1.0 K");
  assert.equal(formatSize(1024 * 1024), "1.0 M");
  assert.equal(formatSize(1024 ** 3), "1.0 G");
});

test("formatTime：null 显示占位符，否则格式化到分钟", () => {
  assert.equal(formatTime(null), "—");
  assert.equal(formatTime(0), "1970-01-01 00:00");
});

// ——— formatEta / formatDuration ———

function transfer(over: Record<string, unknown> = {}) {
  return {
    id: "t1",
    kind: "download",
    name: "f",
    size: 1000,
    transferred: 0,
    localPath: "/l",
    remotePath: "/r",
    status: "running",
    startedAt: 0,
    lastSampleAt: 0,
    lastSampleBytes: 0,
    speed: 0,
    ...over,
  };
}

test("formatEta：不在跑、速度为 0、总大小未知都显示占位符", () => {
  assert.equal(formatEta(transfer({ status: "done" })), "—");
  assert.equal(formatEta(transfer({ speed: 0 })), "—");
  assert.equal(formatEta(transfer({ speed: 10, size: 0 })), "—");
});

test("formatEta：按剩余字节 / 速度换算", () => {
  // 剩余 500 字节，速度 100 B/s → 5 秒
  assert.equal(formatEta(transfer({ transferred: 500, size: 1000, speed: 100 })), "00:05");
});

test("formatDuration：不满一小时用 mm:ss，否则 hh:mm:ss", () => {
  assert.equal(formatDuration(5), "00:05");
  assert.equal(formatDuration(65), "01:05");
  assert.equal(formatDuration(3661), "01:01:01");
});
