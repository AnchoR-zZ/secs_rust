# HSMS v2 rewrite status

## 已完成基础

- [x] 删除旧 HSMS、SECS-II、SML Runtime 和旧集成测试。
- [x] 建立单 crate 严格分层骨架。
- [x] 建立 ID、message、config、error、SessionExit、SessionLauncher 和 ApplicationEventPort 基础合同。
- [x] 提取并标记可供 Wave 1 复核的旧 wire/SECS-II 测试向量。
- [x] 通过全部 Wave 0 工程门禁。

## 2026-08-04 架构重基线

- [x] 撤销细粒度 Core Event/Effect、写入 Begin/Proceed 栅栏、投递 completion ledger 和复杂 AdmissionTxn 目标设计。
- [x] 冻结“有状态 SessionCore + 单 owner SessionDriver + 薄 I/O adapter”新边界。
- [ ] 建立旧内部合同的行为保留清单并删除/隔离未组装的 authority/ledger。
- [ ] 用 fake transport 完成 Select/Linktest/send/request 第一条纵向切片。
- [ ] 接入真实 Reader/Writer、T8、partial write 与应用 EventPort。
- [ ] 完成 T3/T6/T7、reply capability、Deselect 与 Data/Control 全闭环。
- [ ] 实现 Active/Passive、T5、generation replacement 和 public Endpoint runtime。
- [ ] 封板 Drain，完成 Clean/Poisoned、真实 TCP、故障与 conformance 测试。
- [ ] 实现 SML，并删除旧架构残留及所有临时 `dead_code` 豁免。
