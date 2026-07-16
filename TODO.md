# HSMS v2 rewrite status

## Wave 0

- [x] 删除旧 HSMS、SECS-II、SML Runtime 和旧集成测试。
- [x] 建立单 crate 严格分层骨架。
- [x] 冻结 ID、message、config、error、Core Event/Effect、SessionExit、SessionLauncher 和 ApplicationEventPort 合同。
- [x] 提取并标记可供 Wave 1 复核的旧 wire/SECS-II 测试向量。
- [x] 通过全部 Wave 0 工程门禁。

## 后续

- Wave 1：严格 SECS-II、Wire/Validator、纯 HsmsCore。
- Wave 2A：Lifecycle/Admission、Generation I/O、ConnectionSupervisor/Cleanup。
- Wave 2B：Drain 专项封板后组装 SessionDriver。
- Wave 3：SML、conformance、真实 TCP 和故障测试。
- 各实现 Agent 在接管模块时删除对应的 Wave 0 `dead_code` 临时豁免。
