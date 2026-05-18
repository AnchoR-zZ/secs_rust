# TODO / Known Bugs

## Critical

### 1. ~~SendMessageNeedReply 发送失败后调用方被挂起直到 T3 超时~~ [已修复]
`src/hsms/session.rs:272-282`

当 `self.stream.send(msg).await` 失败时，仅 log 了错误，但仍将 `reply_tx` 插入 `t3_replies`。调用方要等完整 T3 超时（默认 5 秒）才能收到 timeout 错误，而非立即拿到真实的 I/O 错误。

**修复：** 发送失败时直接通过 `reply_tx` 返回 `Err(HsmsError::Io(e))`，不插入 `t3_replies`。与 `handle_select_command` / `handle_deselect_command` 模式一致。

### 2. SML formatter/parser 转义字符不对齐，format→parse 往返失败
`src/sml/formatter.rs:158-164` + `src/sml/parser.rs:49-57`

formatter 正确转义了 `"`, `\n`, `\r`, `\t`, `\`，但 parser 的 `parse_string_literal` 使用 `take_while(|c| c != '"')` 不处理任何转义序列。含特殊字符的字符串在 format→parse 往返时会失败。

**建议：** 在 parser 中实现完整的转义序列处理（`\\`, `\"`, `\n`, `\r`, `\t`）。

### 3. ~~Secs2::EMPTY 编码为空 Vec 而非正确的空字节表示~~ [非 Bug / 保持现状]
`src/secs2/encoder.rs:20`

经分析，`Secs2::EMPTY => Ok(vec![])` 是**正确行为**：
1. `EMPTY` 主要用于 HSMS 控制消息（Select/Deselect/Linktest 等），按 SEMI E.37 规范 body 应为 0 字节
2. 若编码为 `[0x01, 0x00]`（空 LIST），HSMS 帧会有 2 字节 body，违反协议
3. `LIST` 内部包含 `EMPTY` 的场景在 SECS-II 标准中本就不存在独立"空项"类型；如需表示空列表应使用 `LIST(vec![])`（已正确编码为 `[0x01, 0x00]`）

**结论：** 无需修改编码逻辑。如需语义保护，可在 `encode_list` 中校验子项不含 `EMPTY`。

### 4. ~~Passive 模式缺少 SO_REUSEADDR，session 结束后重连 bind 可能失败~~ [已修复]
`src/hsms/manager.rs`

采用更优方案：将 `TcpListener` 绑定提到 `run()` 循环之前，跨 session 复用同一个 listener，彻底避免 re-bind 问题。原 `accept_loop_inner` 拆分为 `bind_with_retry` + `accept_connection`。无需引入 `socket2` 依赖。

---

## Important

### 5. Select/Deselect 超时后不触发连接关闭，且调用方收到错误的超时类型
`src/hsms/session.rs:318-324, 347-353`

`handle_select_command` 和 `handle_deselect_command` 将 pending reply 存入 `t3_replies`（超时用 T6 时长）。`check_t6_timeout` 只检查 `t6_replies` 并在超时时触发连接关闭，但 Select/Deselect 的条目在 `t3_replies` 中不会被 `check_t6_timeout` 发现。导致：
1. Select/Deselect 超时后，调用方收到的是 "T3" 超时错误（语义不对，应为 T6 相关）
2. 连接不会被自动关闭，可能停留在不一致状态

**建议：** 将 Select/Deselect 的 pending replies 存入 `t6_replies`，或添加独立的控制事务超时跟踪。

### 6. ~~HsmsMessageCodec::decode 中不必要的 Vec 分配~~ [已修复]
`src/hsms/message.rs:252-258`

移除 `msg_data[..10].to_vec()` 和 `msg_data.to_vec()` 两处不必要的堆分配，直接传 `&[u8]` 切片引用给 `HsmsHeader::decode` 和 `Secs2::decode`。

### 7. ~~Timer 和 T8 检查使用 1 秒轮询间隔，超时精度最多差 1 秒~~ [已修复]
`src/hsms/session.rs:71, 74`

将轮询间隔从 1 秒降低至 200ms，超时精度提升 5 倍，CPU 开销可忽略。

---

## Minor

### 8. system_bytes 全局计数器在测试中导致非确定性行为
`src/util/system_bytes.rs:5-8`

全局 `AtomicU32` 意味着不同测试共享相同的 system bytes 序列，可能导致非确定性行为。

**建议：** 提供一个 per-session 的 system bytes 生成器，或在测试中提供重置能力。

### 9. ~~parse_sml 不验证列表长度标记~~ [已修复]
`src/sml/parser.rs`

`parse_list` 现在支持 `<L[n]>` 方括号语法和 `<L n` 裸数字格式，解析声明长度并与实际子项数对比，不匹配时返回 `SmlError::InvalidFormat` 错误。

### 10. ~~SML formatter 不输出列表长度标记~~ [已修复]
`src/sml/formatter.rs`

formatter 现在输出带元素计数的 `<L[n]>` 格式（如 `<L[2] <A "a"> <A "b">`），与 parser 能力对称，format→parse 往返一致。

### 11. 被注释掉的 bit_size 方法
`src/secs2/types.rs:64-83`

`FormatCode::bit_size` 被注释掉了，可能在解析时有用（如验证数据长度是否与格式代码对齐）。应决定是删除还是启用。

### 12. 缺少 is_connected() 查询方法
`src/hsms/stream_util.rs`

`MonitoredStream` 没有提供查询连接状态的方法，外部无法判断底层 TCP 是否仍然活跃。

### 13. Secs2 缺少 Eq 和 Hash 实现
`src/secs2/types.rs`

`Secs2` 派生了 `Clone + PartialEq + Serialize`，但缺少 `Eq` 和 `Hash`。对于用作字典 key 或集合元素的场景（如根据 stream/function 查找消息类型）不便利。
