# os-identity

NexOS **身份账本**——指纹（NodeID ↔ 地址）证据登记 + 对比判定 + 冲突/失配观测
+ JSON 持久化的独立纯库组件。2026-08-25 从 os-p2p 抽离（用户定调：「指纹信息对比
现有的库里的指纹信息就可以，是不是单独做一个组件完成指纹对比更好？不要集成在
p2p 里面了」）：os-p2p 回归纯传输层，握手/探测只产出**事实事件**；「记谁、信谁、
地址属于谁」的账本与策略全部由本 crate 承担。架构与设计权衡见
`docs/IDENTITY_COMPONENT.md`。

## 能力

- **`IdentityLedger`**（内存 + JSON 持久化原子写）：`IdentityRecord { node_id,
  verified_addrs, unverified_addrs, first_seen, last_seen, conflict_entries,
  mismatch_events }`——一个身份一条。
- **`record_evidence(node_id, addr, kind, now)`**（证据唯一写入口；同 NodeID
  冲突观测另有 `record_conflict`——两口径不混）：
  - `Handshake` / `ProbeVerified` → 地址升 verified（并从其他身份地址集移除
    ——**地址换人**：同一地址同一时刻只属于一个身份）；
  - `ProbeMismatch { actual }` → 期望身份记失配事件 + 地址改判到 `actual` 名下
    verified（探测完成了真实握手，地址换人被实证）；
  - `Gossip { verified }` → 转述地址只入 unverified（报告方验证位透传，不覆盖
    本机已验证结论）。
- **`owns_addr(addr, node_id) -> AddrOwnership`**（对比库核心判定）：
  `Verified` / `Unverified` / `Foreign { owner }`（地址已实证属于其他身份——
  冲突）/ `Unknown`。
- **`record_conflict(node_id, addr, now) -> warning_count`**：同 NodeID 多地址
  观测（原 os-p2p `identity_conflicts` 记账迁入——仅提示不阻断），`conflicts()`
  输出与原 `IdentityConflict` 端点形状一致。
- **`mismatch_events()`**：指纹失配全账本时间线（跨身份取证面）。
- **回环定调**（2026-08-25 用户原话：「127.0.0.1 无论怎么产生的，都应该屏蔽」）：
  地址归属证据一律拒收回环（加载时也剔除存量）；冲突观测例外照记（同机多实例
  恰恰经回环进入，观测面不是可拨凭据）。

## 依赖面

纯逻辑无 IO 依赖：`serde` / `serde_json`（持久化与 REST DTO）/ `tracing`（加载
告警）。NodeID 是**不透明字符串**（`0x`+66 hex——解析/验签属身份发行方
os-p2p / os-common chain_auth）。

## 消费方

- `os-p2p`：`P2pConfig::identity_ledger` 注入共享实例（os-api 装配）或本地内存
  自建（p2p-node CLI / 测试）；register_conn / fingerprint_probe / gossip 合并
  侧的事实事件全部落账本。
- `os-api`：main.rs 建持久化账本（`NEXOS_IDENTITY_FILE`，缺省
  `/tank/os-data/identity-ledger.json`）注入 p2p，`handlers/identity.rs` 暴露
  `GET /api/v1/identity/records | addr/:addr | conflicts`。

## 测试

`cargo test -p os-identity`：证据登记/升降级/地址换人/owns_addr 四态判定/
失配改判/冲突累计/回环拒绝/持久化往返与损坏重建/加载剔除回环/上限截断/
gossip 重复观测不置脏（写放大）/全局条目上限 4096 按 last_seen 淘汰
（12 组）。
