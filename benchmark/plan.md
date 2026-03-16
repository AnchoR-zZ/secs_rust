## Plan: 落地 SECS/HSMS 性能测试基石 (基于 Criterion)

这套计划将具体落实三层测试的物理文件结构和配置。优先引入 Rust 生态中最权威的 `criterion` 框架以及用于分析资源占用的 `pprof`，在新建的 `benchmark` 目录中分工存放不同层级的压测用例，并补齐负载构造器。

**Steps**
1. **配置环境与依赖**: 在 [Cargo.toml](Cargo.toml) 中新增 `[dev-dependencies]` 引入 `criterion`、`tokio`（带 `rt` 甚至 `rt-multi-thread` 特性）和 `pprof`（配合 criterion 生成火焰图）。声明 `[[bench]]` 目标以指向并剥离不同层级的性能测试。
2. **构建混合负载生成器**: 在 [benchmark](benchmark) 目录下创建公共模块（如 `payload_gen.rs`），用于生成“小报文”（心跳类 `S1F1`）、“大报文”（含大体积二进制数据的 `S2F29` 等）以及深度嵌套的 List 数据，供所有层级调用。
3. **L1 微基准落地 (纯编解码)**: 创建 [benchmark/secs2_bench.rs](benchmark/secs2_bench.rs)。针对 `Secs2::encode` 和 `Secs2::decode`，使用 Criterion 的 `black_box` 隔离编译器优化，对比不同 payload 体积和嵌套深度下的 CPU 耗时和吞吐率。
4. **L2 协议栈组件落地 (编解码器与框架)**: 创建 [benchmark/hsms_bench.rs](benchmark/hsms_bench.rs)。启用 Criterion 的 async 运行环境，测试 `HsmsMessageCodec` 配合 `tokio-util` 的 `FramedRead/FramedWrite` 在内存管道 (`tokio::io::duplex`) 上的组帧与解帧性能，重点观测大并发时的内存重分配行为。
5. **L3 端到端会话落地 (模拟真实交互)**: 创建 [benchmark/e2e_bench.rs](benchmark/e2e_bench.rs)。在本地回环地址 (`127.0.0.1`) 建立 `GemManager` 作为服务端，编写一个高频客户端，测量从发送 `DataMessage` 到收到响应的平均延迟（时延 p99 统计）以及满载时的极限 QPS。

**Verification**
- 运行微基准：使用 `cargo bench --bench secs2_bench` 验证纯解码性能，不触发任何网络耗时。
- 生成性能火焰图：执行带有 pprof 特性的运行命令 `cargo bench --bench secs2_bench -- --profile-time=5`，收集 CPU 热点图。
- 自动化指标对比：修改代码后再次运行 `cargo bench`，观察 Criterion 输出的性能是否有统计学上的显著下降 (Regression)。

**Decisions**
- 测试框架选择：采用 `criterion` 而非 `cargo bench` 附带的 `libtest` 甚至直接写 binary，因为前者自带预热机制、离群值过滤和多版本基线对比功能。
- 分析工具引入：加入 `pprof` 支持，因为单纯的 QPS 无法告诉你“为什么慢”，在发现性能由于对象的频繁 `clone()` 或 `to_vec()` 劣化时，火焰图是第一抓手。
- L3 隔离测试：由于 L3 测的是 TCP 和 Tokio 调度，这部分抖动较大，应拆分为独立的 bench 文件运行，允许更长的预热时间以稳定 Socket 状态。