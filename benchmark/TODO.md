# Benchmark Roadmap

## Done

- [x] 单连接 RTT 延迟
- [x] 单连接批量吞吐 (batch)
- [x] HSMS单连接滑动窗口吞吐 (pipelined)
- [x] 多连接并发吞吐
- [x] GEM 连接/上线/控制消息延迟
- [x] GEM passthrough 吞吐
- [x] 编解码性能 (SECS2/HSMS/控制帧)

## TODO

- [ ] 测试端口优化 — 目前测试中大量使用固定端口，编写并行测试时需要主动规避端口冲突。
- [ ] GEM单连接滑动窗口吞吐 (pipelined)
- [ ] 单连接双向并发打流 — 评估单连接资源开销，推算单机最大连接数
- [ ] 多连接低负载规模化 — 100/500/1000 连接，每连接真实消息率，验证线性扩展
- [ ] HSMS\GEM 双向并发测试 — host 发请求的同时 equipment 发事件/报警，验证状态机正确性
