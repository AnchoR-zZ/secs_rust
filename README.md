# secs_rust

`secs_rust` 是可独立复用的 SECS-II、SML 与单会话 HSMS TCP 协议库，采用严格分层设计。

已实现纯 SECS-II/SML 编解码与真实 TCP 上的公开 HSMS 端点，包括 Active/Passive 监督、协议定时器、发送/请求、入站接收、回复、关闭清理和独立诊断流。架构采用不执行 I/O 的 SessionCore、单所有者 SessionDriver 和受监督的连接任务。实现、故障注入与交付验收记录见 [ACCEPTANCE.md](ACCEPTANCE.md)。

使用方式与错误语义见 [USAGE.md](USAGE.md)，当前实现契约和验证记录见 [ARCHITECTURE.md](ARCHITECTURE.md)。可运行 `cargo run -p secs_rust --example loopback` 验证双端点通信，或运行 `cargo run -p secs_rust --no-default-features --example codec` 使用纯编解码。

标准基线、条款映射和支持范围见 [STANDARD_TRACE.md](STANDARD_TRACE.md)。本库不宣称完整标准认证，不自动实现 GEM 或消息业务流程。早期设计与交接记录保留在本地 docs 目录，不作为当前 API 文档或发布包内容。

## 当前边界与实现状态

- `secs2`：已完成 E5 数据模型、DecodeLimits 和 strict binary codec。
- `sml`：已完成严格 Scanner/Parser、消息模型和 canonical Formatter。
- `hsms::api` / `hsms::endpoint`：公开消息、控制、发送/请求、接收、回复、状态订阅与诊断接口。
- `hsms::core`：确定性协议状态、响应匹配、tombstone、Deselect、T3/T6/T7 和入站分类。
- `hsms::wire`：已完成 Raw/Validated Frame、增量 Framer、Encoder 和 Validator。
- `hsms::profile`：已完成 PType=0 与 SECS-II 的严格纯转换。
- `hsms::generation`：真实 Reader/Writer、T8、有界资源、顺序执行、写入可见性结算和有期限清理。
- `hsms::supervisor`：Active/Passive、T5、连接编号与替换门禁、显式故障恢复。
- 入站 Primary 与结构化错误采用独立可靠队列；诊断采用独立尽力投递队列并计数丢弃。

## 工程门禁

```text
cargo fmt -p secs_rust -- --check
cargo check -p secs_rust --all-targets --all-features
cargo clippy -p secs_rust --all-targets --all-features -- -D warnings
cargo test -p secs_rust --all-features
cargo test -p secs_rust --doc
git diff --check
```

本分支允许 Breaking API；父 workspace 的 `simulator_gem` 迁移不属于当前重写范围。
