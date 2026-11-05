import { test } from "node:test";
import assert from "node:assert/strict";

import { isSftpCommand } from "../../src/lib/sftpCmd";

test("第一个词是 sftp 才算", () => {
  for (const l of ["sftp", "  sftp  ", "sftp user@host", "sftp -P 22 host", "sftp\thost"]) {
    assert.equal(isSftpCommand(l), true, l);
  }
});

test("其它情况都不算", () => {
  for (const l of ["sftpx", "echo sftp", "ls sftp", "# sftp", "", "   ", "sudo sftp", "xsftp"]) {
    assert.equal(isSftpCommand(l), false, JSON.stringify(l));
  }
});
