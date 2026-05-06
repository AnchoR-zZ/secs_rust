# secs_rust

`secs_rust` 是一个面向半导体设备通信场景的 Rust 库，提供 SECS-II 数据建模与编解码、HSMS 传输层，以及 SML 文本格式解析与格式化能力。

> ⚠️ 本项目未经生产环境验证，可能存在未发现的 bug。目前仅作为学习 Rust 的个人项目使用，不建议直接用于生产环境。

## 能力概览

- `secs2`: SECS-II 数据类型、二进制编码与解码。
- `hsms`: 基于 Tokio 的 HSMS 连接管理、控制消息和数据消息收发。
- `sml`: SML 文本消息解析与格式化，便于调试和测试。
- `util`: 辅助工具，例如系统字节生成。

模块入口在 `src/lib.rs` 中直接导出：

```rust
pub mod hsms;
pub mod secs2;
pub mod util;
pub mod sml;
```

## 何时选哪一层 API

如果你只需要构造或解析 SECS-II 报文体，从 `secs_rust::secs2` 开始即可。

如果你已经有明确的主动/被动建连逻辑，只需要 HSMS 连接状态、控制报文和请求-回复模型，使用 `secs_rust::hsms::communicator::HsmsCommunicator`。

如果你需要把消息转成可读文本，或者把 SML 文本还原成结构化消息，使用 `secs_rust::sml`。

## 架构分层

```text
Application
  |
  +-- hsms::HsmsCommunicator      HSMS协议实现
  |
  +-- secs2::Secs2                SECS-II 数据模型与编解码
  |
  +-- sml                         文本表示与调试辅助
```

## 添加依赖

> **注意**：该库目前尚未发布到 [crates.io](https://crates.io)。在发布之前，暂时只能用Git Dependency引入

## 快速开始

### 1. SECS-II 编解码

下面的示例展示如何构造一个嵌套列表、编码成二进制，再解码回 `Secs2`。

```rust
use secs_rust::secs2::{encode, Secs2};

fn main() -> Result<(), secs_rust::secs2::Error> {
    let message = Secs2::LIST(vec![
        Secs2::ASCII("MDLN".to_string()),
        Secs2::LIST(vec![
            Secs2::U1(vec![1]),
            Secs2::U2(vec![200]),
        ]),
    ]);

    let bytes = encode(&message)?;
    let decoded = Secs2::decode(&bytes)?.expect("encoded bytes should decode");

    assert_eq!(decoded, message);
    Ok(())
}
```

`Secs2` 当前支持的主要类型包括：

- `LIST`
- `BINARY`
- `BOOLEAN`
- `ASCII`
- `I1` / `I2` / `I4` / `I8`
- `U1` / `U2` / `U4` / `U8`
- `D4` / `D8`
- `EMPTY`

### 2. 最小 HSMS 请求-回复流程

`HsmsCommunicator::new` 会返回一个通信器和一个上行消息接收器。通信器负责发送命令，接收器负责把下层收到的数据消息交给上层业务。

```rust
use secs_rust::hsms::{
    communicator::HsmsCommunicator,
    config::{ConnectionMode, HsmsConfig},
    message::HsmsMessage,
    ConnectionState,
};
use secs_rust::secs2::Secs2;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let passive_config = HsmsConfig {
        mode: ConnectionMode::Passive,
        ip: "127.0.0.1".to_string(),
        port: 15100,
        t3: Duration::from_secs(3),
        t5: Duration::from_secs(1),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let active_config = HsmsConfig {
        mode: ConnectionMode::Active,
        ip: "127.0.0.1".to_string(),
        port: 15100,
        t3: Duration::from_secs(3),
        t5: Duration::from_secs(1),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let (passive, mut passive_rx) = HsmsCommunicator::new(passive_config);
    let (active, _active_rx) = HsmsCommunicator::new(active_config);

    tokio::spawn({
        let passive = passive.clone();
        async move {
            while let Some(msg) = passive_rx.recv().await {
                if msg.header.w_bit {
                    let reply = msg.build_reply_message(
                        msg.header.stream,
                        msg.header.function + 1,
                        Secs2::ASCII("REPLY".to_string()),
                    );
                    let _ = passive.send_reply(reply).await;
                }
            }
        }
    });

    let mut state_rx = active.state_rx();
    while *state_rx.borrow() != ConnectionState::Selected {
        state_rx.changed().await?;
    }

    let request = HsmsMessage::build_request_data_message(
        0,
        1,
        1,
        secs_rust::util::next_system_bytes(),
        Secs2::ASCII("HELLO".to_string()),
    );

    let reply = active.send_message_with_reply(request).await?;
    assert_eq!(reply.header.stream, 1);
    assert_eq!(reply.header.function, 2);

    active.shutdown().await?;
    passive.shutdown().await?;
    Ok(())
}
```

更完整的连接生命周期、Deselect、Separate 和自动重连流程，可直接参考 `tests/hsms_integration_test.rs`。

## 核心 API 导览

### `secs2`

- `Secs2`: SECS-II 数据模型。
- `encode(&Secs2) -> Result<Vec<u8>, secs2::Error>`: 编码为二进制。
- `Secs2::decode(&[u8]) -> Result<Option<Secs2>, secs2::Error>`: 从二进制解析。

这一层不关心网络连接，适合纯编解码、单元测试和上层消息构造。

### `hsms`

- `HsmsCommunicator::new(config) -> (HsmsCommunicator, mpsc::Receiver<HsmsMessage>)`
- `send_message(...)`
- `send_message_with_reply(...)`
- `send_reply(...)`
- `state() / state_rx()`
- `send_select() / send_not_select() / send_not_connect() / shutdown()`

`ConnectionState` 目前分为三种：

- `NotConnected`
- `NotSelected`
- `Selected`

这一层适合自己管理业务状态，但不想自己重写 HSMS 控制命令的场景。

### `sml`

- `parse_sml(...)`: 从 SML 文本解析结构化消息。
- `to_sml_compact(...)`: 输出单行紧凑格式。
- `to_sml_secs(...)`: 输出多行格式化文本。
- `to_sml_hsms(...)`: 直接格式化 HSMS 消息。

示例：

```rust
use secs_rust::secs2::Secs2;
use secs_rust::sml::{to_sml_compact, to_sml_secs};

fn main() {
    let data = Secs2::LIST(vec![
        Secs2::ASCII("MDLN".to_string()),
        Secs2::LIST(vec![Secs2::U1(vec![1]), Secs2::U2(vec![200])]),
    ]);

    let compact = to_sml_compact(&data);
    let pretty = to_sml_secs(&data);

    println!("{}", compact);
    println!("{}", pretty);
}
```

## 配置要点

### `HsmsConfig`

默认配置包括：

- `session_id = 0`
- `ip = "0.0.0.0"`
- `port = 5000`
- `mode = Passive`
- `connect_timeout = 10s`
- `t3 = 5s`
- `t5 = 10s`
- `t6 = 5s`
- `t7 = 10s`
- `t8 = 5s`
- `linktest = 30s`

其中：

- `Active` 表示主动连接远端。
- `Passive` 表示本地监听，等待远端连接。
- `t3` 到 `t8` 对应 HSMS 常用超时参数。

## 当前约束与注意事项

- 该库未经生产验证，可能存在未发现的 bug，个人用于学习 Rust 的项目。
- 测试和 benchmark 中大量使用固定端口，编写并行测试时需要主动规避端口冲突。

## 验证方式

在 `secs_rust` 目录下可以直接运行：

```bash
cargo build
cargo test
cargo bench
```

如果只想看关键集成测试：

```bash
cargo test --test hsms_integration_test
```

如果只想跑基准测试：

```bash
cargo bench hsms_benchmark
cargo bench secs2_benchmark
```