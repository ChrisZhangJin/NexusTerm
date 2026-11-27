import { test } from "node:test";
import assert from "node:assert/strict";
import type { ITerminalAddon } from "@xterm/xterm";
import { loadInlineImages } from "../../src/lib/inlineImages";

test("each visual terminal loads its own addon with IIP only and bounded storage", () => {
  const loaded = new Set<ITerminalAddon>();
  for (let i = 0; i < 2; i++) {
    const term = { loadAddon: (addon: ITerminalAddon) => { loaded.add(addon); } };
    assert.equal(loadInlineImages(term, (options) => {
      assert.equal(options.iipSupport, true);
      assert.equal(options.sixelSupport, false);
      assert.equal(options.enableSizeReports, false);
      assert.equal(options.iipSizeLimit, 4_000_000);
      assert.equal(options.pixelLimit, 4_000_000);
      assert.equal(options.storageLimit, 32);
      return { activate() {}, dispose() {} };
    }, () => true), true);
  }
  assert.equal(loaded.size, 2);
});

test("skips addon creation when the WebView cannot compile Wasm", () => {
  assert.equal(loadInlineImages({ loadAddon: () => assert.fail("unexpected load") },
    () => assert.fail("unexpected creation"), () => false), false);
});

test("disposes a failed activation without propagating the error", (t) => {
  t.mock.method(console, "warn", () => {});
  let disposals = 0;
  const term = { loadAddon: () => { throw new Error("activation failed"); } };
  assert.equal(loadInlineImages(term,
    () => ({ activate() {}, dispose: () => { disposals++; } }), () => true), false);
  assert.equal(disposals, 1);
});
