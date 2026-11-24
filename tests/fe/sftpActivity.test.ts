import { test } from "node:test";
import assert from "node:assert/strict";

import { describeSftpActivity } from "../../src/lib/sftpActivity";
import { __setLocaleForTest } from "../../src/i18n";

test("没有活动返回 null", () => {
  assert.equal(describeSftpActivity({ running: 0, queued: 0 }), null);
});

test("中文：进行中与排队都列出", () => {
  assert.equal(
    describeSftpActivity({ running: 2, queued: 3 }),
    "SFTP 面板里还有2 个正在进行的传输、队列中等待的 3 个任务，关闭会中断传输（队列不会保留）。"
  );
});

test("英文：单复数与连接词", () => {
  __setLocaleForTest("en");
  try {
    assert.equal(
      describeSftpActivity({ running: 1, queued: 0 }),
      "The SFTP panel still has 1 transfer in progress. Closing will interrupt them (the queue won't be kept)."
    );
    assert.equal(
      describeSftpActivity({ running: 2, queued: 1 }),
      "The SFTP panel still has 2 transfers in progress and 1 queued task. Closing will interrupt them (the queue won't be kept)."
    );
  } finally {
    __setLocaleForTest("zh");
  }
});
