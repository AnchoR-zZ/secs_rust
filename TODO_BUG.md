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

### 3. Secs2::EMPTY 编码为空 Vec 而非正确的空字节表示
`src/secs2/encoder.rs:20`

`Secs2::EMPTY` 编码时返回 `vec![]`（零字节），不是标准的 SECS-II 消息体。在 LIST 内部编码为空字节时，接收方无法区分"空项"和"缺少项"。

**建议：** 编码为正确的 SECS-II 项，例如 `build_header(FormatCode::List.code(), 0)` 产生 `[0x01, 0x00]`。

### 4. Passive 模式缺少 SO_REUSEADDR，session 结束后重连 bind 可能失败
`src/hsms/manager.rs:168-186`

`accept_loop_inner` 在 session 结束后会被再次调用，重新 `TcpListener::bind`。前一个 session 的 TCP 连接可能仍处于 `TIME_WAIT` 状态，导致 bind 失败（`Address already in use`）。当前用 5 秒重试间隔缓解，但引入不必要的重连延迟，在 Windows 上尤其严重。

**建议：** 使用 `socket2` 或 tokio 的 `TcpSocket::set_reuseaddr(true)` 在 bind 前设置 `SO_REUSEADDR`。

---

## Important

### 5. Select/Deselect 超时后不触发连接关闭，且调用方收到错误的超时类型
`src/hsms/session.rs:318-324, 347-353`

`handle_select_command` 和 `handle_deselect_command` 将 pending reply 存入 `t3_replies`（超时用 T6 时长）。`check_t6_timeout` 只检查 `t6_replies` 并在超时时触发连接关闭，但 Select/Deselect 的条目在 `t3_replies` 中不会被 `check_t6_timeout` 发现。导致：
1. Select/Deselect 超时后，调用方收到的是 "T3" 超时错误（语义不对，应为 T6 相关）
2. 连接不会被自动关闭，可能停留在不一致状态

**建议：** 将 Select/Deselect 的 pending replies 存入 `t6_replies`，或添加独立的控制事务超时跟踪。

### 6. HsmsMessageCodec::decode 中不必要的 Vec 分配
`src/hsms/message.rs:252-258`

每次 decode 都执行 `msg_data[..10].to_vec()` 和 `msg_data.to_vec()` 进行额外堆分配。高吞吐场景下有可测量的性能影响。

**建议：** 使用 `msg_data[..10].try_into().unwrap()` 替代 header 的 Vec 分配，body 部分直接使用切片引用。

### 7. Timer 和 T8 检查使用 1 秒轮询间隔，超时精度最多差 1 秒
`src/hsms/session.rs:71, 74`

T3/T6 超时检查和 T8 inter-character 超时检查都使用 1 秒的轮询间隔，超时最多延迟 1 秒才被检测到。

注：T8 的 `last_read` 更新机制本身是正确的——`MonitoredStream` 在每次 TCP 数据到达时更新时间戳（`stream_util.rs:32-33`），不受 `Framed::decode` 返回 `Ok(None)` 的影响。

**建议：** 如需更高精度，可使用 `tokio::time::Instant` + `tokio::select!` 的 per-reply 精确定时替代轮询。

---

## Minor

### 8. system_bytes 全局计数器在测试中导致非确定性行为
`src/util/system_bytes.rs:5-8`

全局 `AtomicU32` 意味着不同测试共享相同的 system bytes 序列，可能导致非确定性行为。

**建议：** 提供一个 per-session 的 system bytes 生成器，或在测试中提供重置能力。

### 9. parse_sml 不验证列表长度标记
`src/sml/parser.rs:91-92`

`parse_list` 中 `opt(preceded(multispace1, digit1))` 解析了可选的长度标记，但返回值被丢弃，不验证长度是否与实际子项数匹配。

**建议：** 解析长度标记并与实际子项数对比，不匹配时报错。

### 10. SML formatter 不输出列表长度标记
`src/sml/formatter.rs`

formatter 输出 `<L item1 item2>` 但不包含元素计数（如 `<L[2] item1 item2>`），而 parser 可以解析带可选计数的格式。formatter 和 parser 能力不对称。

### 11. 被注释掉的 bit_size 方法
`src/secs2/types.rs:64-83`

`FormatCode::bit_size` 被注释掉了，可能在解析时有用（如验证数据长度是否与格式代码对齐）。应决定是删除还是启用。

### 12. 缺少 is_connected() 查询方法
`src/hsms/stream_util.rs`

`MonitoredStream` 没有提供查询连接状态的方法，外部无法判断底层 TCP 是否仍然活跃。

### 13. Secs2 缺少 Eq 和 Hash 实现
`src/secs2/types.rs`

`Secs2` 派生了 `Clone + PartialEq + Serialize`，但缺少 `Eq` 和 `Hash`。对于用作字典 key 或集合元素的场景（如根据 stream/function 查找消息类型）不便利。
