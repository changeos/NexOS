# os-identity：指纹账本与对比组件（2026-08-25 从 os-p2p 抽离）

> 用户定调（原话拆解）：「指纹信息对比现有的库里的指纹信息就可以，是不是单独
> 做一个组件完成指纹对比更好？不要集成在 p2p 里面了」——**os-p2p 回归纯传输层**
> （握手验证是协议、必须留下），**账本 / 对比 / 策略全部外移**。
>
> 架构地位对照：Bitcoin **addrman**（地址管理独立于网络层，节点重启地址结论
> 不丢）/ Tailscale **协调面**（身份与地址知识集中在协调组件，数据面只转发）。

## 1. 架构

```text
                        ┌────────────────────────────────────────────┐
                        │              os-p2p（纯传输层）              │
                        │  握手（挑战-签名 + ECDH）＝协议，必须留下      │
                        │  fingerprint_probe（TCP+握手比对）＝探测引擎  │
                        │  不做「记谁/信谁/地址属于谁」的策略            │
                        └──────┬─────────────────────────────────────┘
                               │ 事实事件（握手证据 / 探测结论 / 转述 / 冲突观测）
                               ▼
        ┌──────────────────────────────────────────────────────────────┐
        │            os-identity（新 crate，纯库——crates/os-identity）  │
        │  IdentityLedger：注册表 + 证据对比 + 冲突 + JSON 持久化          │
        │   · record_evidence(node_id, addr, kind)   ← 证据唯一写入口        │
        │     （冲突观测另有 record_conflict）                            │
        │   · owns_addr(addr, node_id) -> Verified/Unverified/          │
        │     Foreign{owner}/Unknown                ← 对比判定           │
        │   · conflicts() / mismatch_events() / snapshot()  ← 查询 API   │
        │   · 回环拒收（地址集）/ 原子写持久化 / 防抖落盘                  │
        └──────────────▲───────────────────────────────────────────────┘
                       │ 注入共享实例（Arc<Mutex<IdentityLedger>>）
        ┌──────────────┴───────────────────────────────────────────────┐
        │ os-api 装配层（main.rs）                                       │
        │  建 ledger（/tank/os-data/identity-ledger.json，env            │
        │  NEXOS_IDENTITY_FILE）→ 注入 P2pConfig.identity_ledger         │
        │  → 自留一份给 REST handler                                     │
        └──────────────┬───────────────────────────────────────────────┘
                       │ REST（开发期公开读）
       node_view（不改，消费 meta） / agent-coord（独立 online） /
       未来任何消费方：GET /api/v1/identity/records | addr/:addr | conflicts
```

## 2. 事件清单（os-p2p → os-identity 的事实事件）

| # | 产生点（os-p2p） | 事件（`EvidenceKind`） | 账本动作 |
|---|---|---|---|
| 1 | `api::register_conn`（入站/出站握手完成） | `Handshake(node_id, observed)` | 地址升 verified + **从其他身份地址集移除**（地址换人） |
| 2 | `register_conn`（对端自报 NodeID == 本机） | `record_conflict(self_id, observed)` | 同 NodeID 多地址观测（原 identity_conflicts 语义；**仅提示不阻断**，返回累计 warning_count 供日志） |
| 3 | `meta::record_probe_result`（探测握手 NodeID == 目标） | `ProbeVerified(id, addr)` | 地址升 verified（与 meta 侧 verified 洗白同刻） |
| 4 | `meta::record_probe_result`（探测握手 NodeID ≠ 目标） | `ProbeMismatch{ expected: id, addr, actual }` | 期望身份记 `MismatchEvent` + 地址整体改判到 `actual` 名下 verified（探测完成了真实握手——地址换人被实证） |
| 5 | `api::handle_frame`（MetaGossip 合并） | `Gossip{ id, addr, verified: 报告方位 }` | 未验证转述只入 unverified；报告方 verified 位透传，不覆盖本机结论 |
| 6 | `register_conn`（注册前对比查询） | `owns_addr(observed, peer)`（**读**） | 地址已实证属于其他身份 → `warn`（地址归属冲突观测，不阻断；随后按最新握手证据改判归属） |

**回环定调延续**（2026-08-25 用户原话：「127.0.0.1 无论怎么产生的，都应该
屏蔽」）：任何证据的回环地址不入 verified/unverified 集（账本侧拒绝——与
os-p2p meta 注册表的层层拦截同构）；**例外**是冲突观测（事件 #2）——同机多实例
恰恰经回环进入，`remote_addr` 是 socket 观测地址（知情面）而非可拨凭据，照记。
持久化加载时无条件剔除地址集回环存量（纵深防御）。

## 3. 接线方式与权衡（内嵌 vs 注入 vs 事件回调）

`P2pConfig::identity_ledger: Option<Arc<Mutex<IdentityLedger>>>`（os-identity
`SharedLedger`）：

- **os-api 装配（注入共享实例）**：main.rs 建持久化账本 → 注入 p2p → 自留一份
  给 REST handler。写读同一实例——**账本即唯一权威源**，第一条握手证据开始
  就不丢（重启经 JSON 文件恢复）。
- **p2p 单独跑（本地自建）**：`None` 时 `P2pNode::spawn` 自建本地内存账本
  （`config_from_env` 路径给 standalone CLI 落 key 同目录
  `identity-ledger.json`）。p2p-node CLI 独立跑时 `fingerprint_probe` 的
  verified 判定、`owns_addr` 对比、冲突记账照常工作。

**为什么不是纯事件回调（`identity_sink: Arc<dyn Fn(IdentityEvent)>`）**：
传输层的指纹判定需要**查询反馈**而非单向通知——`register_conn` 注册前要
`owns_addr` 查地址归属、CLI 独立跑要本地判定。sink 形态下 p2p 仍需自建一份
影子账本才能判定，等于维护两份账本、写读还会漂移；共享实例形态让「记账」与
「查账」天然一致，代价是 p2p 对 os-identity 有一层纯库依赖（无 IO、无运行时
耦合）。曾评估的第三条路——ledger 完全外置、p2p 每次跨进程查询——对单进程
装配的 NexOS 纯属过度设计。

**锁纪律**：账本以 std Mutex 短临界区共享（与 `api::State` 同款约定，持锁绝不
await），且 **identity 锁绝不与 state 锁嵌套**（两锁无顺序关系——所有事件
写入都在 state 临界区之外执行）。

## 4. 与 meta.rs / addrman 的关系

| | os-p2p `meta`（NodeMetaStore） | os-identity（IdentityLedger） |
|---|---|---|
| 职责 | **存活判定**：健康分 / 五振出局 / 心跳节奏 / gossip 新鲜度复活 | **身份事实**：地址归属 / verified 证据链 / 冲突与失配取证 |
| verified 位 | 保留（展示位；`NodeMetaEntry.verified` / `MetaAddr.verified`——**对外输出不变**，node_view 等现有消费零改动） | 权威源（同证据双记账：register_conn / 探测 / gossip 两边同刻落笔） |
| 冲突 | 不收（回环不入册） | 收（`conflict_entries`——原 identity_conflicts 语义迁入，含回环观测） |
| 持久化 | `node-meta.json`（key 同目录） | `identity-ledger.json`（os-api：/tank/os-data/；CLI：key 同目录） |
| 对照 | —（存活面） | Bitcoin addrman：地址结论独立于网络层、重启不丢、谎报地址靠实证降级 |

**meta.rs 的 `MetaAddr.verified` 为什么暂不改为「ledger 查询支撑」**：改查询
需要把账本句柄穿透进 `NodeMetaStore` 的纯函数层（心跳/合并/序列化全部重写），
而「事件同步」（两边同刻落笔、meta 输出形状不变）以最小改动达成同样的事实
一致性，且 node_view 的现有消费不能破（本批红线）。账本是身份问题的唯一
权威源；meta 的 verified 位退化为展示镜像，后续批次若要收口可从
`owns_addr` 派生。

## 5. REST 端点（os-api 装配桥接，开发期公开读）

> 如实口径：三条端点透出的是**全网拓扑情报**（身份-地址归属、活跃时间线、
> 冲突观测），对内网侦察有价值——开发期公开读仅为联调便利，**生产前必须
> 收紧鉴权**（「不涉敏感数据」的说法不成立，已纠正）。

| 方法 | 路径 | 语义 |
|---|---|---|
| GET | `/api/v1/identity/records` | 全量身份记录（`IdentityRecord` 数组，按 last_seen 降序：verified/unverified 地址集 + first/last_seen + conflict_entries + mismatch_events） |
| GET | `/api/v1/identity/addr/:addr` | 地址归属查询：`{addr, owner, verified, ownership, record}`——owner node_id + verified 状态 + 归属记录全量；无主 `owner=null`。`:addr` 支持 `ip:port` 全格式（IPv4 / `[v6]:port`）与裸地址自动补 7070：裸 IPv4 `1.2.3.4` → `1.2.3.4:7070`、裸 IPv6 多冒号 `2001:db8::1` → `[2001:db8::1]:7070`、`[v6]` 括号裸地址 → 补端口（IPv6 不括号直接拼端口是非法串，分支处理）；不支持主机名 |
| GET | `/api/v1/identity/conflicts` | 同 NodeID 多地址观测（与 `GET /api/v1/p2p/identity-conflicts` 同源同形——p2p 端点保留，本端点是组件化后的规范路径） |

未启用（`NEXOS_P2P_ENABLE` 未开 → 账本无数据源）：503 +
`{"error":"身份账本未启用（NEXOS_P2P_ENABLE=1 开启组网后自动装配）"}`。

## 6. 迁移说明

- **`identity_conflicts` 旧数据**：原实现是 `api::State` 里的内存 HashMap
  （重启清零）。迁移选**重启清零**（不导出）：冲突是观测面不是账务——旧数据
  只在本机内存里、无既有持久化格式可迁；新实现迁入账本 `conflict_entries`
  后随 `identity-ledger.json` **持久化，重启不再丢**（增强而非回归）。代码
  注释与本节即迁移记录。
- **`IdentityConflict` 结构**：定义移到 os-identity（字段
  `node_id/remote_addr/first_seen/last_seen/warning_count` 原样），os-p2p lib.rs
  转发导出——os-api `GET /api/v1/p2p/identity-conflicts` 输出形状不变，前端
  无感。
- **`Handle::identity_conflicts()`**：签名不变；内部改读账本
  （`Cmd::IdentityConflicts` → `ledger.conflicts()`）。
- **workspace**：根 `Cargo.toml` members + `[workspace.dependencies]` 注册
  `os-identity`；os-p2p / os-api 按需 `workspace = true` 引用。

## 7. 测试

| crate | 覆盖 |
|---|---|
| os-identity（12 组单测） | Handshake 建档/去重置前、Gossip 分级与不降级、owns_addr 四态、ProbeMismatch 改判+时间线、Handshake 地址换人、冲突按地址累计+降序、回环拒绝（含失配留痕不收址）、持久化往返+损坏重建+防抖、加载剔除回环存量、地址集/事件上限、**gossip 重复观测不置脏（写放大）**、**全局条目上限 4096 按 last_seen 淘汰** |
| os-p2p（78 lib 测试全绿） | 既有 meta/冲突/指纹验证测试随接线适配全绿；「注入共享账本 → 握手证据落账（owns_addr=Verified）→ 停机强刷 → 重载判定不变」端到端；**账本脏+meta 干净 → 防抖到期账本照常落盘（早退短路回归）** |
| os-api --lib（identity handler 7 测） | 路由矩阵/未启用 503/records 形状/addr 归属四态+裸 IP 补 7070+非法 400/**IPv6 三形态（裸多冒号、[v6] 括号、[v6]:port 全格式）+单锁按键取记录**/conflicts 同源同形/真实双节点 mesh 经 p2p 事件填账 |

红线复核：node_view 输出形状未动（`/api/v1/nodes/combined` 消费无感）；
transport / handshake 协议零改动；`cargo test -p os-identity`、`-p os-p2p`、
`-p os-api --lib` 全绿；workspace check / clippy `-D warnings` 过。
