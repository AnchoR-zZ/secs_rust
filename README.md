# secs_rust

`secs_rust` 正在 `codex/hsms-v2-rewrite` 分支上进行绿地重写，目标是提供严格分层的 HSMS-SS 与 SECS-II 协议库。

项目整体仍按 Wave 计划推进：公共值类型、配置、错误、Core Event/Effect 和 Runtime 边界已冻结，**SECS-II Wave 1 strict codec 已完成**；HSMS 状态机、传输与可运行 TCP Endpoint 仍处于各自后续 Wave。旧 `HsmsCommunicator`、旧协议循环和固化旧语义的测试已经移除。

目标架构与 Agent 分工见 [HSMS v2 参考计划](docs/hsms-v2-rewrite-reference-plan.md)，已冻结边界的使用规则见 [Wave 0 contracts](docs/wave0-contracts.md)。

## 当前边界与实现状态

- `secs2`：E5 数据模型、DecodeLimits，以及已经完成的 Wave 1 strict binary codec。
- `hsms::api`：应用消息、事件、类型化 Control 意图和 completion 值。
- `hsms::core`：Sans-I/O Event/Effect 合同；状态机由 Wave 1 实现。
- `hsms::wire`：Raw/Validated Frame 合同；framing/validation 由 Wave 1 实现。
- `hsms::supervisor`：Generation 退出与 SessionLauncher 边界；运行时由 Wave 2 实现。
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
