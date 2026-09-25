//! os-integration —— 跨 crate 端到端集成测骨架（integration-agent 拥有）
//!
//! 本 crate **不含运行时代码**，仅作为聚合点：在 workspace 中注册为一个独立 crate，
//! 把全部 22 crate 作为 `[dev-dependencies]`（启用 `mock` feature）引入，承载
//! 跨 crate 端到端集成测用例（`tests/<scenario>.rs`）。
//!
//! # 测试场景（integration-agent 规格书 §3 + 本次启动 prompt）
//!
//! | 场景 | 跨越 crate | 验证内容 |
//! |------|-----------|----------|
//! | `vm_creation_chain` | api → compute → storage → core(EventBus) → services(monitor) | VM 创建调用链 + 事件流 + 指标上报 |
//! | `guest_chain_verification` | guest(ChainOrchestrator) → wallet → security(JwtIssuer) → im(ConversationStore) | 访客链上验证全链 + 错误降级 |
//! | `ha_failover_chain` | meta(FailoverOrchestrator) → compute → storage → meta(VipManager) | 故障转移状态机 + 各组件调用顺序 |
//! | `backup_chain` | services(backup) → storage(snapshot/replication) → core(EventBus) → services(monitor) | 备份调度→快照→复制→告警 |
//! | `im_conversation_as_action` | im(AgentOrchestrator) → core(EventBus) | IM 对话→agent 委派→工具调用→黑板共享 |
//! | `api_route_aggregation` | api(Gateway/RouteHandler) | 多组件路由注册聚合 + 匹配分发 + 响应聚合 |
//! | `discover_mtls_federation` | discover(Discovery/PeerAuthenticator/FederationPolicy) → core(EventBus) | mTLS 联邦决策链 + 降级路径 |
//! | `update_rollback` | update(UpdateEngine/SlotManager/RollbackManager) → core(EventBus) | A/B 槽位更新 + watchdog 回滚状态机 |
//!
//! # 测试策略
//!
//! - **全 Mock 注入**：不真起 libvirt/ZFS/网络/链节点；用各 crate feature `mock`
//!   暴露的 `MockXxx` 实现。
//! - **重点验证**：trait 签名跨 crate 兼容、Mock 行为一致、事件/数据流串通、
//!   错误传播正确（某环节失败时上游正确处理）。
//! - **发现问题就修**：暴露的跨 crate 不兼容做最小修复（不改 trait 签名）。
//!   需改 trait 才能集成的记录到 `docs/agents/integration-agent/PROGRESS.md` 阻塞区。

// 占位 lib：本 crate 仅为集成测聚合点，无运行时导出。
