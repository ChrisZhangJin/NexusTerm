# russh-sftp 本地补丁

基线：crates.io `russh-sftp 2.4.0`（checksum `9de67aace74530a29086db0671fa200c470a58eb380081f28ad512ffb0c5356b`）。

选 2.4.0 而不是更高的版本，是因为它依赖的 `russh` 正是我们 vendoring 打过补丁的
0.49.x（`russh-sftp 3.x` 要求 russh 0.63，与这批堡垒机的算法兜底补丁不兼容）。

## 补丁 1：读循环拿到错误必须退出

位置：`src/client/mod.rs`，`run()` 里那个负责读的 `runtime::spawn`。

原代码：

```rust
result = process_handler(&mut rd, &mut handler) => {
    match result {
        Err(Error::UnexpectedEof) => break,
        Err(err) => warn!("{}", err),   // ← 不 break
        Ok(_) => (),
    }
},
```

只有 `UnexpectedEof` 会退出；其余错误打一行日志后**立刻再进下一轮**。

一旦底层 SSH 连接断掉，`read_packet` 返回的通常不是 EOF 而是别的错误，于是这个
任务直接空转成忙等 —— 单个 SFTP 会话就能吃满一个核，而且永远不会自己停下来。
NexusTerm 空闲时长期占 10% CPU（16 核机器上实测 = 1.3~1.7 个核）根因在此。

改法与上游 3.0.1 完全一致：出错即 `break`。后续本文件会自动走到 `rc.cancel()`，
把写那一侧的循环一并收掉。

## 升级注意事项

升级 `russh-sftp` 前先确认上游是否保留了这处 `break`（3.0.1 已经保留）；丢弃补丁
等于把这个忙等 bug 放回 app 里。
