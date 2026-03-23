# SECS/GEM 库 Benchmark

请为我的 Rust SECS/GEM 库编写 benchmark 测试的相关代码。这些测试性能的相关代码放在 `secs_rust/benchmark` 文件夹中。

为保证 `cargo bench` 可以直接识别这些 benchmark，请在 `Cargo.toml` 中为每个 benchmark 显式配置 `path`。
另外，`benchmark` 目录下的共享代码不使用 `mod.rs` 作为入口，而是通过各个 bench 文件显式 `mod data_generator;` 或 `#[path = "data_generator.rs"] mod data_generator;` 的方式复用。

## Benchmark 测试要求

### 1. SECS-II 消息编解码性能测试

请创建 `secs_rust/benchmark/secs2_benchmark.rs`，包含以下测试场景：

#### 1.1 单一数据类型测试
为每种数据类型创建独立的 benchmark，使用以下数据规模：

- **ASCII**: 10, 50, 100, 500, 1000, 5000, 10000 字符
- **BINARY**: 10, 50, 100, 500, 1000, 5000, 10000 字节
- **BOOLEAN**: 10, 50, 100, 500, 1000, 5000, 10000 个元素
- **整数类型 (I1/I2/I4/I8/U1/U2/U4/U8)**: 10, 50, 100, 500, 1000, 5000, 10000 个元素
- **浮点类型 (D4/D8)**: 10, 50, 100, 500, 1000, 5000, 10000 个元素
- **LIST 嵌套深度**: 1, 2, 3, 5, 7, 10 层（每层 10 个子元素）

> **数据规模选择说明**：
> - **细粒度 (10 → 50 → 100 → 500 → 1000)**：更准确地绘制性能曲线，发现缓存边界效应（L1/L2/L3 Cache），识别性能突变点
> - **极限测试 (5000, 10000)**：测试大内存分配性能，验证系统稳定性，模拟批量数据传输场景
> - **LIST 深度扩展 (1~10层)**：测试递归深度限制，验证栈溢出风险，模拟复杂的状态报告结构

#### 1.2 综合混合数据结构测试
创建模拟真实场景的复杂数据结构：
- **S1F1 (Are You There)**: 简单的空消息
- **S1F2 (On Line Data)**: 包含多个字段的响应
- **S2F13 (Equipment Constant Request)**: 包含多个常量ID的列表
- **S2F14 (Equipment Constant Data)**: 包含多种数据类型的混合列表
- **S6F11 (Event Report Send)**: 模拟事件报告，包含嵌套的 LIST 结构
- **复杂嵌套结构**: 5层嵌套的 LIST，每层混合不同数据类型

#### 1.3 吞吐量指标
对于每个测试，输出：
- 编码吞吐量：MB/秒、消息数/秒
- 解码吞吐量：MB/秒、消息数/秒
- 平均延迟（微秒）

### 2. HSMS 通讯模式吞吐量测试

请创建 `secs_rust/benchmark/hsms_benchmark.rs`，包含以下测试场景：

#### 2.1 HSMS 消息编解码性能
- 测试 `HsmsMessageCodec` 的 `encode` 和 `decode` 性能
- 测试不同大小的消息体（小：10字节、中：100字节、大：1000字节）
- 测试不同消息类型（Data、SelectReq/Rsp、LinktestReq/Rsp）

#### 2.2 模拟网络吞吐量
- 该部分作为第二阶段 benchmark，与 `HsmsMessageCodec` 的纯编解码 benchmark 分开实现
- 使用 `tokio` 创建本地 loopback 连接
- 测试消息往返延迟（RTT）
- 测试消息批量发送/接收性能
- 如运行时间和稳定性可接受，再增加并发连接下的吞吐量测试

> **说明**：网络 loopback benchmark 容易受到 Tokio 调度、操作系统 socket 缓冲区以及 Windows 本地回环栈抖动影响，
> 因此必须与纯 codec benchmark 分开看待，不能直接横向比较。

#### 2.3 吞吐量指标
- 消息编解码吞吐量：消息数/秒
- 网络吞吐量（如实现）：MB/秒、消息数/秒
- 连接建立延迟
- 消息往返延迟（RTT）

### 3. GEM 层性能测试

请创建 `secs_rust/benchmark/gem_benchmark.rs`，包含以下测试场景：

#### 3.1 GEM 消息构建性能
测试 `secs_rust/src/gem/message.rs` 中的消息构建函数：
- **S1F1/S1F2**: `build_s1f1()`, `build_s1f2_reply()` 构建性能
- **S1F13/S1F14**: `build_s1f13()`, `build_s1f14_reply()` 构建性能
- **S1F15/S1F16**: `build_s1f15()`, `build_s1f16_reply()` 构建性能
- **S1F17/S1F18**: `build_s1f17()`, `build_s1f18_reply()` 构建性能
- 测试不同参数组合（空mdln/softrev vs 有值）

#### 3.2 GEM 状态机性能
测试 `secs_rust/src/gem/gem_state.rs` 中的状态机：
- **状态转换性能**: 测试 `DeviceState::on_event()` 单次转换延迟
- **完整流程测试**: 测试从 NotConnected → Selected → Online 的完整状态转换链
- **高频状态转换**: 测试连续执行 1000 次状态转换的吞吐量
- **边界条件**: 测试无效事件被忽略时的性能

具体测试场景：
```
场景1: NotConnected → NotSelected → Selected(EquipmentOffLine)
场景2: EquipmentOffLine → AttemptOnLine → OnlineLocal → OnlineRemote
场景3: OnlineRemote → OnlineLocal → EquipmentOffLine
场景4: EquipmentOffLine → HostOffline → OnlineRemote
场景5: 完整12种状态转换事件的混合测试
```

#### 3.3 GEM 控制器性能
测试 `GemControl` 包装类：
- `GemControl::new()` 创建性能
- `handle_event()` 处理性能
- 不同 `StateMachineConfig` 配置下的性能差异

#### 3.4 GEM 吞吐量指标
- 消息构建吞吐量：消息数/秒
- 状态转换吞吐量：转换次数/秒
- 平均转换延迟（纳秒级）

## 技术要求

1. 使用 `criterion` crate 进行 benchmark（已在 Cargo.toml 中配置）
2. 使用 `criterion::black_box` 防止编译器过度优化
3. 生成 HTML 报告（criterion 已配置 `html_reports` feature）
4. 代码应具有良好的模块化结构
5. 提供可复用的测试数据生成函数
6. 添加详细的注释说明每个测试的目的
7. 优先完成纯计算型 microbenchmark（SECS-II、HSMS codec、GEM builder、GEM state machine），再实现网络 loopback benchmark

## 文件结构

```
secs_rust/
├── benchmark/
│   ├── secs2_benchmark.rs  # SECS-II 编解码 benchmark
│   ├── hsms_benchmark.rs   # HSMS 消息 benchmark
│   ├── gem_benchmark.rs    # GEM 层 benchmark
│   └── data_generator.rs   # 测试数据生成器
├── Cargo.toml              # 需要添加 [[bench]] 配置
└── ...
```

## Cargo.toml 配置

需要在 `Cargo.toml` 中添加：

```toml
[[bench]]
name = "secs2_benchmark"
harness = false
path = "benchmark/secs2_benchmark.rs"

[[bench]]
name = "hsms_benchmark"
harness = false
path = "benchmark/hsms_benchmark.rs"

[[bench]]
name = "gem_benchmark"
harness = false
path = "benchmark/gem_benchmark.rs"
```

## 运行 Benchmark

完成后，应支持以下命令：
```bash
# 运行所有 benchmark
cargo bench

# 运行特定 benchmark
cargo bench --bench secs2_benchmark
cargo bench --bench hsms_benchmark
cargo bench --bench gem_benchmark

# 生成 HTML 报告到 target/criterion/
```

## 指标说明

本次 benchmark 统一使用以下指标：

- 吞吐量：MB/秒、消息数/秒或转换次数/秒
- 平均延迟：微秒级或纳秒级

本次不要求输出 `P50/P95/P99` 分位延迟，以保持实现简单、统计口径一致，并避免为 Criterion 额外引入自定义延迟采样逻辑。