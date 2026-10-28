/**
 * 连接分组路径计算的自测：`lib/folders`。跑法：`npm test`
 *
 * 这一层是「改名 / 拖动分组」唯一的真逻辑，界面那边只是把结果喂给
 * `reorder_sessions`。算错了的后果不是看起来不对，是一批连接的归属被写坏，
 * 所以前缀匹配、根目录的两种写法（`""` / `"/"`）、自己套自己这几处都得钉住。
 */

import assert from "node:assert/strict";
import test from "node:test";

import type { SessionRecord } from "../../src/types";
import {
  folderDropTarget,
  folderMoveItems,
  isSelfOrDescendant,
  leafFolder,
  normFolder,
  parentFolder,
  resolveFolderInput,
} from "../../src/lib/folders";

const rec = (id: string, folder: string, sortOrder = 0): SessionRecord => ({
  id,
  name: id,
  folder,
  host: "10.0.0.1",
  port: 22,
  username: "root",
  authType: "password",
  keyPath: null,
  startupCmd: null,
  theme: null,
  connectTimeout: null,
  sortOrder,
  createdAt: 0,
  updatedAt: 0,
});

// ——— 规整 ———

test("normFolder：根的几种写法都归一成空串", () => {
  for (const raw of ["", "/", "//", "   ", " / "]) {
    assert.equal(normFolder(raw), "", `「${raw}」该是根`);
  }
});

test("normFolder：剥掉首尾斜杠、压掉空段和段内空白", () => {
  assert.equal(normFolder("office"), "office");
  assert.equal(normFolder("/office/train/"), "office/train");
  assert.equal(normFolder("office//train"), "office/train");
  assert.equal(normFolder(" office / train "), "office/train");
});

test("parentFolder / leafFolder", () => {
  assert.equal(parentFolder("a/b/c"), "a/b");
  assert.equal(parentFolder("a"), "");
  assert.equal(parentFolder("/a/b/"), "a");
  assert.equal(leafFolder("a/b/c"), "c");
  assert.equal(leafFolder("a"), "a");
  assert.equal(leafFolder(""), "");
});

// ——— 改名输入框 ———

/**
 * 回归：旧版 `onRenameDone` 里有一句 `.replace(/\//g, "")`，把用户填的斜杠
 * 直接吃掉 —— 填 `/office/train` 变成 `officetrain`，看起来就是「这个字符不起作用」。
 */
test("输入框：斜杠不再被吃掉，填绝对路径就是换层级", () => {
  assert.equal(resolveFolderInput("train", "/office/train"), "office/train");
  assert.equal(resolveFolderInput("train", "/office/train/"), "office/train");
});

test("输入框：不带前导斜杠 = 相对自己的父级", () => {
  // 顶层分组：父级是根，相对和绝对同义
  assert.equal(resolveFolderInput("train", "office/train"), "office/train");
  // 嵌套分组：接在父级 office 后面
  assert.equal(resolveFolderInput("office/train", "prod/train"), "office/prod/train");
});

test("输入框：不带斜杠就是原来的单纯改名", () => {
  assert.equal(resolveFolderInput("train", "dev"), "dev");
  assert.equal(resolveFolderInput("office/train", "dev"), "office/dev");
});

test("输入框：绝对路径能把嵌套的分组提回根", () => {
  assert.equal(resolveFolderInput("office/train", "/train"), "train");
});

test("输入框：空、纯空白、只填一个斜杠、没改动，都当作没发生", () => {
  assert.equal(resolveFolderInput("office/train", ""), null);
  assert.equal(resolveFolderInput("office/train", "   "), null);
  assert.equal(resolveFolderInput("office/train", null), null, "Esc 取消");
  assert.equal(resolveFolderInput("office/train", "/"), null, "只给了根，没给名字");
  assert.equal(resolveFolderInput("office/train", "train"), null, "同名");
  assert.equal(resolveFolderInput("office/train", "/office/train"), null, "同路径");
});

// ——— 自己套自己 ———

test("isSelfOrDescendant：自己和子孙都算", () => {
  assert.equal(isSelfOrDescendant("office", "office"), true);
  assert.equal(isSelfOrDescendant("office", "office/train"), true);
  assert.equal(isSelfOrDescendant("office", "/office/train/deep"), true);
});

test("isSelfOrDescendant：同前缀的兄弟分组不算（按段比，不是按字符串前缀）", () => {
  assert.equal(isSelfOrDescendant("office", "officex"), false);
  assert.equal(isSelfOrDescendant("office", "office2/train"), false);
  assert.equal(isSelfOrDescendant("office/train", "office"), false);
  assert.equal(isSelfOrDescendant("office", ""), false);
});

// ——— 搬家 ———

test("搬家：整棵子树换前缀，sortOrder 原样保留", () => {
  const sessions = [
    rec("a", "/train", 3),
    rec("b", "/train/east", 1),
    rec("c", "/train/east/db", 0),
  ];
  const items = folderMoveItems(sessions, "train", "office/train");
  assert.deepEqual(items, [
    ["a", 3, "office/train"],
    ["b", 1, "office/train/east"],
    ["c", 0, "office/train/east/db"],
  ]);
});

test("搬家：不碰子树以外的连接，名字像也不行", () => {
  const sessions = [
    rec("in", "/train"),
    rec("sibling", "/trainee"),
    rec("other", "/office"),
    rec("root", "/"),
  ];
  const items = folderMoveItems(sessions, "train", "office/train");
  assert.deepEqual(
    items.map((i) => i[0]),
    ["in"]
  );
});

test("搬家：挪到根，folder 要写回存储认的「/」而不是空串", () => {
  const sessions = [rec("a", "/office/train"), rec("b", "/office/train/east")];
  assert.deepEqual(folderMoveItems(sessions, "office/train", "train"), [
    ["a", 0, "train"],
    ["b", 0, "train/east"],
  ]);
  // 连末段都不要了：直接摊到根
  assert.deepEqual(folderMoveItems(sessions, "office/train", ""), [
    ["a", 0, "/"],
    ["b", 0, "east"],
  ]);
});

test("搬家：根不是可搬运的分组", () => {
  assert.deepEqual(folderMoveItems([rec("a", "/")], "", "office"), []);
  assert.deepEqual(folderMoveItems([rec("a", "/")], "/", "office"), []);
});

test("搬家：子树下一个连接都没有就不用写库", () => {
  assert.deepEqual(folderMoveItems([rec("a", "/office")], "train", "office/train"), []);
});

// ——— 拖拽落点 ———

test("落点：末段名不变，只换父级", () => {
  assert.equal(folderDropTarget("train", "office"), "office/train");
  assert.equal(folderDropTarget("a/b/c", "x/y"), "x/y/c");
  assert.equal(folderDropTarget("office/train", ""), "train", "拖到空白处 = 提到根");
  assert.equal(folderDropTarget("office/train", "/office/"), "office/train", "拖回原处 = 原地");
});
