# HSMS v2 rewrite status

> 此文件保留早期重构阶段记录。当前实现、已运行门禁与剩余验收义务以
> [ARCHITECTURE.md](ARCHITECTURE.md) 和 [ACCEPTANCE.md](ACCEPTANCE.md) 为准；
> 下文未勾选的早期集成项不能直接视为当前尚未实现。

## 已完成基础

- [x] 删除旧 HSMS、SECS-II、SML Runtime 和旧集成测试。
- [x] 建立单 crate 严格分层骨架。
- [x] 建立 ID、message、config、error、SessionExit、SessionLauncher 和 ApplicationEventPort 基础合同。
- [x] 完成 SECS-II strict codec、资源限制和 conformance 测试。
- [x] 完成 HSMS Framer、Encoder、Validator、PType=0 Profile 和纯 Codec 组合。
- [x] 完成 SML strict parser、canonical formatter 和 conformance 测试。

## 架构重基线

- [x] 撤销细粒度 Core Event/Effect、写入 Begin/Proceed 栅栏、投递 completion ledger 和复杂 AdmissionTxn 目标设计。
- [x] 冻结“有状态 SessionCore + 单 owner SessionDriver + 薄 I/O adapter”新边界。
- [x] 删除旧 contracts、Core、authority/ledger、Admission 和 runtime 占位实现。
- [x] 生成当前阶段状态 handoff，并将未决 Session 事项保留为局部 TODO。

## 下一阶段

- [x] `TODO(session/passive-select)`：已确认内部先提交 Selected，Select.rsp 取得 wire order 后才对外发布；接纳失败时关闭 generation，不发布且不回滚。
- [x] `TODO(session/control-reserve)`：Control admission `Full` 映射为 Backpressure/ControlBackpressure，`Closed` 映射为 ConnectionLost/TransportLost。
- [x] 实现并验证 2026-09-01 已冻结的 Stage B0 最小内部合同。
- [x] 用 fake transport 完成 connected、Select、Linktest、Reject、Separate 第一条控制纵向切片。
- [x] 按已确认的最小 B2 合同完成 send/request、完整 response matcher、fast Secondary、Data permit、有界 tombstone 与 typed exactly-once completion；通过独立复审和全量门禁。
- [ ] 接入真实 Reader/Writer、T8、partial write 与应用 EventPort。
- [ ] 完成 T3/T6/T7、reply capability、Deselect 与 Data/Control 全闭环。
- [ ] 实现 Active/Passive、T5、generation replacement 和 public Endpoint runtime。
- [ ] 封板 Drain，完成 Clean/Poisoned、真实 TCP、故障与 conformance 测试。
- [ ] 删除所有临时 `dead_code` 豁免。
