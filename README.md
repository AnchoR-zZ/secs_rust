# secs_rust

`secs_rust` 正在 `codex/hsms-v2-rewrite` 分支上进行绿地重写，目标是提供严格分层的 HSMS-SS 与 SECS-II 协议库。

项目已于 2026-08-04 完成架构重基线：保留已经完成的 SECS-II strict codec、HSMS Wire/Validator 与 PType=0 Profile，撤销过细的 Core Event/Effect、Writer 两阶段栅栏和 Runtime 回执账本设计。新的目标是 **有状态、I/O-free 的 SessionCore + 单所有者 SessionDriver + 薄 TCP 适配层**。HSMS 状态机、传输与可运行 TCP Endpoint 尚待按新纵向切片计划实现。

目标架构与实施顺序见 [HSMS v2 参考计划](docs/hsms-v2-rewrite-reference-plan.md)，继续冻结与已经撤销的边界见 [Wave 0 contracts 重基线](docs/wave0-contracts.md)。

## 当前边界与实现状态

- `secs2`：E5 数据模型、DecodeLimits，以及已经完成的 Wave 1 strict binary codec。
- `hsms::api`：应用消息、事件、类型化 Control 意图和 completion 值。
- `hsms::core`：将重写为粗粒度、有状态且 I/O-free 的 `SessionCore`；现有细粒度 Event/Effect 与 authority/ledger 不再是兼容边界。
- `hsms::wire`：Raw/Validated Frame 合同；framing/validation 由 Wave 1 实现。
- `hsms::generation`：目标为单 owner `SessionDriver` 与 Reader/Writer/EventPort 薄适配层；当前仍未形成纵向闭环。
- `hsms::supervisor`：保留 Generation 退出语义；Active/Passive、T5 和 replacement runtime 尚待实现。
- `hsms::generation::event_port`：非阻塞 ApplicationEventPort；队列满或关闭必须显式回报，不能静默丢事件。

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
