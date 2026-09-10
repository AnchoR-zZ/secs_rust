# 使用协议库

`secs2` 和 `sml` 可独立使用。默认开启 `runtime-tokio`，提供真实 TCP 上的单会话 HSMS 端点。实现契约与测试证据见 [ARCHITECTURE.md](ARCHITECTURE.md)。本库管理传输、选择状态、事务和回复关联，应用负责消息的业务含义与 GEM 行为。

Passive 端点只维护一个活动 generation，采用 E37-0298 §9.2.4.1(c) 的停止监听策略：接纳连接后释放监听器，直到旧 generation 清理完成才在原地址重新监听。额外 connect 可能被操作系统拒绝或超时。Disconnect 保留地址和运行意图，不承诺返回时下一次监听已就绪；Active 对端应按 T5 重试。若原地址重新绑定失败，端点报告连接尝试失败并进入 Faulted。

Active 的 T5 从一次 connect 尝试结束后开始计算，包括成功、失败、超时或取消；等待重试期间取消等待不会重置既有期限。标准基线核对与尚未解决的差异见 [STANDARD_TRACE.md](STANDARD_TRACE.md)。

## 运行示例

在 workspace 中运行：

```text
cargo run -p secs_rust --example loopback
cargo run -p secs_rust --no-default-features --example codec
```

[loopback.rs](examples/loopback.rs) 在本机动态分配端口，创建 Active 和 Passive 两个端点，演示等待 Selected、Request、接收 Primary、Reply、Stop 和等待运行时退出。示例消息体仅用于展示数据交换，不代表特定 GEM 过程。

[codec.rs](examples/codec.rs) 不使用异步运行时，演示 SML → 数据项 → SECS-II 二进制 → 数据项 → 规范化 SML。

仅使用纯编解码时，依赖配置为：

```toml
[dependencies]
secs_rust = { path = "../secs_rust", default-features = false }
```

需要 HSMS TCP 时使用默认特性，并由应用提供 Tokio 运行环境：

```toml
[dependencies]
secs_rust = { path = "../secs_rust" }
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

## 端点生命周期

`HsmsEndpoint::build(config)` 返回 `(HsmsHandle, HsmsRuntime)`，只验证配置并创建有界通道，不绑定端口或创建任务。应用需要显式运行 `runtime.run()`，通常将其交给 `tokio::spawn`，再调用句柄方法。

| 操作 | 完成时的含义 |
| --- | --- |
| `start()` | 建立运行意图；Passive 已成功绑定监听器，返回实际监听地址；Active 尚不保证 TCP 已连接 |
| `subscribe()` / `snapshot()` | 读取一致的最新状态；订阅不保留每一个历史状态 |
| `control(Select)` | Select 事务成功完成 |
| `control(Deselect)` | 排空后完成 Deselect；成功时保留 TCP，进入 NotSelected |
| `control(Linktest)` | 收到匹配的 Linktest 响应 |
| `control(Separate)` | 执行 Separate 并结束当前连接 |
| `disconnect()` | 关闭调用首次被轮询时捕获的连接；保留运行意图，允许建立下一连接 |
| `stop()` | 停止监督，结束连接并等待清理；成功后同一句柄可再次 `start()` |

Active 默认自动发起一次 Select；可通过 `RuntimePolicy::with_auto_select(false)` 禁用。发送前应等待状态达到 `SessionState::Selected`，而不是将 `start()` 成功当作可发送的证明。

句柄可以克隆。最后一个句柄释放后，运行循环开始最终关闭。保留任务句柄并等待 `run()` 返回，才能确认最终关闭结果。直接丢弃或取消运行时会中止其拥有的传输任务，不能据此声称已经完成优雅关闭。

清理结果不确定时，端点进入 Faulted，阻止连接替换。后续显式 `stop()` 会尝试重新确认旧任务和资源的释放；只有成功清理后才允许重新启动。已接受的协议操作不会自动迁移或重试到新连接。

## 消息、请求与回复

应用通过 `PrimaryMessage` 提供 Stream、Function 和可选消息体。Session ID 来自端点配置，System Bytes 由库分配；W 位由 `send` 或 `request` 决定。

| API | 行为 |
| --- | --- |
| `send(message)` | W=0；成功返回 `SendReceipt`，证明完整帧已写入本地传输，不证明对端业务已执行 |
| `request(message)` | W=1；等待匹配的 `SecondaryMessage` 或事务失败 |
| `take_receiver()` | 仅能成功取出一次；所有句柄克隆共享这个唯一接收端 |
| `recv_primary()` | 接收 Primary、原始头上下文与对应令牌 |
| `reply(token, body)` | 使用原请求的关联信息发送 F+1 Secondary |
| `abort_reply(token)` | 发送无消息体的 SxF0 |
| `abandon_reply(token)` | 仅释放回复能力，不发送帧 |

`InboundToken::Reply` 对应 W=1；W=0 只有 `InboundToken::Data` 标记。回复令牌不可克隆，并绑定原端点和原连接。F255 无法形成 F+1，因此必须 Abort 或 Abandon。丢弃令牌不会代替显式 Abandon；其协议义务会保留到被处理或连接结束。

接收端可调用 `split()`，把 Primary 和结构化协议错误交给不同的应用任务。错误队列与 Primary 队列独立，避免正常消息积压直接占用错误报告的名额。

`None` 消息体表示不存在 Message Text，与 `Some(SecsItem::Binary(vec![]))` 等带类型的空数据项不同。库保留这一区别。

## 错误与超时

`MessageError` 提供精确错误及可选的原 Primary；有原消息时，应用可以自行决定是否重试。`ReplyError::into_parts()` 在已知的 Core 接受前拒绝中返回 `ReplyAdmissionError`，其中保留原令牌和消息体。错误后的重试属于应用策略，不能对送达不确定的操作盲目重发。

`OperationError::DeliveryIndeterminate` 表示不能证明该帧没有被对端看到。`OperationError::RequestTimeout { context }` 表示 T3 已到期，包含原始请求的十字节头和连接编号，可用于应用的 S9/MHEAD 内容。普通请求或 F255 请求的原始函数号、W 位和 System Bytes 都保留。

T3 和 T6 从实际本地完整写入时刻起算，排队时间不是它们的起点。T7 约束连续 NotSelected 时段，T8 约束未完成帧的接收字节进度。`RuntimePolicy` 另行约束写队列驻留、实际写入、排空、清理和总关闭期限。

在操作已进入队列后，取消等待它的应用 future 不会撤销已接受的协议工作。发送、请求、控制和回复在首次轮询时捕获连接；之后发生重连不会让操作自动发往新连接。

## 容量与诊断

使用 `EndpointConfig::active/passive` 设置角色、地址和 Data Session ID，通过 `with_timeouts`、`with_limits`、`with_runtime` 和 `with_secs2_limits` 设置其他参数，再调用 `validate` 或 `HsmsEndpoint::build` 校验。配置应在构建端点前确定；库不写配置文件，应用负责在安装时设置、持久保存并在重启后重新加载这些值。

| 参数 | 库默认值 | 配置范围与单位 |
|---|---|---|
| T3 / T8 | 45 秒 / 5 秒 | 非零 `Duration`；包含 E37-0298 表中要求的 1–120 秒、1 秒步长 |
| T5 / T6 / T7 | 10 秒 / 5 秒 / 10 秒 | 非零 `Duration`；包含基线要求的 1–240 秒、1 秒步长 |
| connect / idle Linktest | 10 秒 / 30 秒 | 非零 `Duration`；Linktest 可设为 `None` 禁用 |
| drain / residence / write / cleanup / shutdown | 5 / 10 / 10 / 5 / 20 秒 | 非零 `Duration`，总关闭期限不会因重试延长 |
| 最大 Message Length | 16 MiB | 含 10 字节头、不含 4 字节前缀；纯值范围为 10–`u32::MAX`，实际端点还受各字节预算约束 |
| 普通命令 / Data 事务 / 回复能力 | 256 / 256 / 256 个 | 正整数；运行时单项及相关队列总和不超过 `2^28` |
| Control Writer / Data Writer | 32 / 256 个 | 独立数量额度，共享 wire FIFO；运行时合计不超过 `2^28` |
| Primary 队列 / tombstone | 256 / 512 个 | 正整数；运行时单项不超过 `2^28` |
| 可靠错误 / 诊断 / 回复命令 | 64 / 64 / 64 个 | 独立正整数额度，受运行时可表示性和总队列上限校验 |
| Primary 命令 / Reader / Primary 队列 / Writer / Reply 字节 | 64 / 32 / 32 / 64 / 32 MiB | 每项至少容纳配置的最大完整帧，且不超过 `2^28` 字节 |

`Duration` 可表达纳秒，但这不是实际唤醒精度承诺。期限在处理输入时比较单调时钟，实际调度受 Tokio、操作系统和负载影响；构建时拒绝单调时钟无法表示的期限。`EndpointLimits` 的最大 Message Length 同时用于收发边界，实际发送仍需通过消息编码、Writer 与命令预算检查。默认允许最多 256 个本端待回复 Data 事务；Control 事务独立限制为一个，入站回复能力默认另有 256 个。各项容量是上限，其他预算可能更早产生背压。

`EndpointLimits` 限制帧、命令、事务、回复能力及队列数量。`RuntimePolicy` 配置编码字节预算、可靠错误队列和诊断队列容量。容量必须至少能容纳一个配置允许的最大完整帧，否则构造阶段拒绝配置。

消息在排队前按不可变内容测量大小；命令数量和编码字节占用受到限制。排队的入站 Primary 保留编码字节额度，取出时释放队列额度。编码字节不等于精确 Rust 堆内存，也不限制应用自行持有的消息数量；解码树还受 `DecodeLimits` 约束。

Reader 的 `inbound_bytes` 与应用队列的 `primary_queue_bytes` 是独立预算，不能将前者当成整个入站管线的总上界。`with_byte_budgets(command, inbound, write)` 会将两个入站预算都设为 `inbound`，之后可用 `with_primary_queue_bytes(bytes)` 单独调整应用队列。保持独立可避免未消费的 Primary 阻塞后续 Secondary 和控制帧。Reader 当前每代只保留一个在途或已报告帧；字节预留覆盖最大完整帧。解码树、缓冲区预留容量、消息元数据和已转移给应用的结果还需另计，配置值不代表进程 RSS 限制。

普通命令的数量额度保留到操作完成。Stop 和 Disconnect 共用一个额外的独立名额，因此未完成请求占满普通命令额度时仍可启动关闭；已有关闭操作占用这个名额时，再次提交关闭仍可能返回 Backpressure。

Reply/Abort/Abandon 使用独立的回复数量额度，Reply/Abort 另占独立的编码字节额度。通过 `RuntimePolicy::with_reply_budget(count, bytes)` 配置，默认是 64 个和 32 MiB；这些额度也保留到操作完成。出站 Primary 不占回复额度，Abandon 不占字节额度。端点命令队列的总上界为 `command_capacity + reply_capacity + 1`。回复仍受 Writer 的有限容量约束，拥塞时会返还未消费的回复输入，应用应处理背压。

可靠接收队列的数量或字节容量耗尽时，连接会关闭，不会覆盖队列中的旧消息。应用应持续消费 Primary，并及时 Reply、Abort 或 Abandon。

`take_diagnostics()` 提供独立的尽力投递诊断流，报告连接结束、建连失败、Reject 分类、响应不匹配和迟到响应。队列满时不会阻塞协议处理；`dropped_count()` 为累计丢弃数，记录序号的间隙也可显示丢失。诊断不能替代可靠消息接收或操作完成结果。

Reject 的 `OperationRejected` 表示拒绝关联到了命令完成对象，包含运行时自动发起的 Select 和应用显式控制；`AutonomousRejected` 表示没有命令完成对象的 Core 自主操作，例如空闲 Linktest。该分类不直接表示操作是否由应用发起。

`handle.snapshot().last_exit()` 独立保存最近一次连接退出与清理报告，包含 generation、关闭原因和 `clean()`；即使诊断丢弃，报告仍保留，并跨 Stop/Start 延续到下一份报告。`clean=false` 表示尚未证明全部资源释放；显式恢复成功后，同一 generation 的报告更新为 `clean=true`。watch 只保留最新值，不提供逐条退出历史。直接丢弃运行时不会凭空生成 Clean 报告，已有报告也不能证明另一代连接已清理。
