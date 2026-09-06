# NexOS P2P 组网设计（参考 Ethereum Swarm 网络拓扑）

> 需求（用户 2026-08-20）：参考 Swarm 的网络设计，让所有 NexOS 快速发现彼此；
> 网络组件独立成功能；**少数公网运行的 NexOS 做枢纽，其余 NexOS（局域网/NAT 后）
> 都能跨局域网通讯**。
> 本设计 = Swarm Kademlia 思想的 NexOS 适配（简化版），不依赖 BZZ/Swarm 代码。

## 1. Swarm 可借鉴的三件设计资产

| Swarm 设计 | 它解决了什么 | NexOS 怎么用 |
|---|---|---|
| **Overlay 地址 = 哈希(节点公钥)**，XOR 距离去中心化分片 | 身份即地址，无需中心分配 | 我们的 secp256k1 公钥身份（chain_auth）直接升级为 NodeID；OverlayAddr = keccak(pubkey)（与 EVM 派生同源，身份体系零新增） |
| **Underlay/Overlay 分层**：物理连接（TCP）与逻辑拓扑（Kademlia 邻域）解耦 | 网络演进不绑定传输 | LAN 用 mDNS/mTLS（os-discover 已有），WAN 用公网枢纽中继，逻辑层统一"节点-消息"抽象 |
| **Bootstrap 节点**：新节点先连已知节点，再沿网络学习全拓扑 | 冷启动发现 | 公网 NexOS 天然就是 bootstrap + relay（cloud 上的 SSH 隧道是它的手工原型） |

**拓扑决策（用户 2026-08-20 定稿）：采用 Swarm 同款全分布式 Kademlia**——不设
中心枢纽注册表，节点发现与路由完全经 DHT walk；公网节点仅承担 bootstrap 冷启动
与 NAT 中继服务（是"服务节点"不是"中心"）。为上千节点规模预留，去中心化到底。

## 2. 目标拓扑（全分布式 Kademlia，Swarm 同款）

```
   ●───●       ●            ● = NexOS 节点（NodeID=pubkey, OverlayAddr=keccak(pubkey)）
  /     \    / \           实线 = underlay 连接（mTLS TCP）
 ●       ●──●   ●          逻辑拓扑 = 256-bit XOR 距离空间（Kademlia 邻域）
  \     /    \  /          公网节点（●加粗）= bootstrap + NAT 中继服务者
   ●───●──●───●            LAN 内 = mDNS 直连优先（低延迟路径）
```

- **路由表（k-buckets）**：每个节点按 OverlayAddr 的 256-bit XOR 距离维护 256 个
  邻域桶（proximity order bins，Swarm 同款），每桶 k=16 个已知节点
- **发现（DHT walk）**：冷启动连 bootstrap 节点（env 配置的公网 NexOS）→ 发
  FINDNODE(target=自己) → 沿返回的更近节点迭代查询，直到收敛——这就是
  Swarm Bee 的 ping/findnode/pong 流程
- **连接阶梯（用户 2026-08-20 补丁：公网中继非必须，仅兜底）**，逐级尝试、
  高优先级成功即短路：
  1. **LAN 直连**：mDNS 发现的同网段邻居（延迟最优）
  2. **公网直连**：对端有可达 underlay（公网 NexOS 或映射过端口的节点）
  3. **TCP 打洞**：两个 NAT 后节点互连——经共同可达节点**交换观测端点**
     （见下），双方同时向对方公网 ip:port 发起 TCP 同时打开（simultaneous open），
     成功率受 NAT 类型影响（full-cone/restricted 高，symmetric 低）
  4. **中继兜底**：打洞失败才走任意可达节点的中继（不再要求"公网指定角色"——
     任何愿意的可达节点都能当 relay，ttl/hops 防环）
- **观测端点广播（地址交换所）**：任何节点连接我时，我看到的是它的
  **公网观测地址**（ip:port）——把 `{NodeID → 观测端点}` 存入本地地址簿并随
  FINDNODE/NODES **一起八卦（gossip）给其他节点**。效果：只要全网任一节点
  接触过某 NAT 节点，它的公网端点就全网可知——**一个公网 NexOS 足以充当
  全网地址交换所**（它见过的所有节点端点都可广播给后来者），不同 net 下的
  NexOS 靠这些地址互相发起打洞/直连
- **公网 NexOS 的真实职责**（重新定义）：①冷启动锚点（完全找不到同类时才需要）
  ②观测端点交换所 ③打洞失败时的中继志愿者——三者都是服务角色，不是必经之路
- **LAN mesh**：同网段 mDNS 发现的邻居直接进路由表（underlay 互通，延迟最优），
  与 DHT 全局发现互补

## 3. 新组件：`os-p2p`（网络层独立 crate）

```
crates/os-p2p/
├── identity.rs    # NodeID=secp256k1 公钥（复用 os-common::chain_auth 的密钥学）
│                  # OverlayAddr = keccak256(pubkey)[12..]（EVM 地址同源）
├── transport.rs   # mTLS 加密传输（复用 os-discover 的 mTLS 实现，抽出共享）
├── discovery.rs   # 双通道发现：LAN(mDNS, 调 os-discover) + WAN(Hub 注册表)
├── kad.rs         # Kademlia 核心：256 邻域 k-buckets/距离计算/FINDNODE 迭代查询收敛
├── endpoints.rs   # 观测端点地址簿 + gossip（随 NODES 消息八卦 {NodeID→公网ip:port}）
├── punch.rs       # TCP 打洞：端点交换→双方同时打开→成功升级直连（失败落 relay）
├── relay.rs       # 中继兜底（阶梯第 4 级）：{src,dst,ttl,hops,payload}，任何可达节点可服务
├── bootstrap.rs   # 冷启动：env 引导节点 + LAN mDNS 种子 + 迭代 walk 入网（仅当找不到同类）
└── api.rs         # 对上层服务：send(node_id,msg) / on_msg / peers() / route(node_id)
```

- **无中心角色**：所有节点地位平等；公网可达性由 `NEXOS_P2P_PUBLIC=1`（或 auto
  探测）声明——声明者承担 bootstrap/relay 服务职责（仍是普通 NexOS，跑全部应用）
- **配置**：`NEXOS_P2P_BOOTSTRAP=host1:port,host2:port`（初始引导，之后从 DHT 学习）；`NEXOS_P2P_LISTEN=:7070`；`NEXOS_P2P_PUBLIC`
- **上层第一批消费者**：联邦大厅（lobby 索引经 os-p2p 同步）、IM 跨节点消息
  （身份漫游天然成立——pubkey 无需中心注册）

## 4. 与现有资产的关系

| 现有资产 | 角色 |
|---|---|
| os-common::chain_auth | NodeID 密钥学直接复用（IM/NexHub 的身份就是节点身份） |
| os-discover（3661 行，零消费→终于有消费者） | LAN 发现层（mDNS beacon 验签 mTLS） |
| forwarding.rs 的 cloud SSH 隧道 | 手工版 Hub 中继，os-p2p 上线后退役为备份通道 |
| avahi `_nexos._tcp` 通告 | LAN 发现的补充（端口/角色通告） |
| 联邦大厅/IM Federation（规划） | os-p2p 之上的第一批应用 |

## 5. 分期

| 期 | 内容 | 验收 |
|---|---|---|
| P1 Kademlia 核心 | os-p2p crate：identity/transport(mTLS)/kad(k-buckets+findnode 收敛)/relay(NAT 中继)；**单机多进程集成测试（5+ 节点随机端口组网，含 2 个"公网"引导、3 个模拟 NAT 经中继）** | 消息经 DHT 路由跨"LAN"互通；节点离线桶失效自愈；断线重连 ✅（2026-08-20） |
| P2 发现融合与实网 | mDNS LAN 种子接入 + **观测端点 gossip + TCP 打洞（连接阶梯落地）** + 真实部署：cloud 仅作冷启动锚点与交换所 + 两台真机跨 LAN 实测（优先打洞、中继兜底）+ 节点列表 API/UI（网络页显示拓扑/桶/每连接的阶梯等级） | 两节点跨 LAN 直连或打洞成功；打洞失败自动落中继 ✅ P2a 引擎（2026-08-20）；真机 cloud 锚点 198.51.100.114:7070 + 本机组网已通；**P2b os-api/UI 接入** ✅（2026-08-21，见 §7） |
| P2b 接入与身份稳定 | **密钥持久化**（`NEXOS_P2P_KEY_FILE`，修 NodeID 漂移）+ **os-api 内嵌组网**（`NEXOS_P2P_ENABLE=1` 才 spawn，6 端点 REST）+ **网络页拓扑 UI**（状态卡/节点表/桶可视化/阶梯统计） | 重启同身份；`GET /api/v1/p2p/status` 返回自身 NodeID；未启用 503 引导 ✅（2026-08-21，见 §7） |
| P3 消费者 | 联邦大厅索引同步 + IM 跨节点（大厅消息互通） | 两节点大厅互见对方发布的项目 ✅（2026-08-22，见 §8） |

## 6. 明确不做（本期）

- ~~全分布式 Kademlia~~（已升级为正式方案，见 §2）
- UDP 打洞直连（NAT 后两节点目前经 Hub 中继；打洞是延迟优化，P4+ 可选）
- 跨节点文件传输大带宽优化（走中继先能用，吞吐优化后置）

## 7. P2b 落地状态（2026-08-21：密钥持久化 + os-api 接入 + 网络页拓扑 UI）

### 7.1 三件事

1. **密钥持久化（修 NodeID 漂移）**：`os_p2p::bootstrap::load_or_create_identity`
   ——`NEXOS_P2P_KEY_FILE`（缺省降级链 `/tank/os-data/p2p-node-key` →
   `/var/lib/os/p2p-node-key` → `./p2p-node-key`）存在则加载 secp256k1 私钥（hex，
   可选 `0x` 前缀），不存在则生成并**原子写**（同目录临时文件 + 0600 + rename）；
   损坏文件**告警 + 降级重生成**（漂移一次好过永续崩溃）；目录不可写退回内存身份。
   `config_from_env()` 默认走此逻辑——**CLI（p2p-node）与库（os-api 内嵌）共用
   同一份私钥，锚点/节点重启身份稳定**。
2. **os-api 接入**：新 handler `crates/os-api/src/handlers/p2p.rs`（组件名 `p2p`）。
   main.rs 装配：`NEXOS_P2P_ENABLE=1`（默认关——不影响无 P2P 需求的部署）才在
   网关进程内 `P2pNode::spawn(config_from_env())`（env 全透传，见 §7.3），Handle
   存 handler 并持到进程结束（优雅停机随 os-api 信号一起 shutdown）；另起入站
   消息观测 task（P3 消费者接入前的 `[os-api][p2p] recv` 日志面）。
3. **网络页拓扑 UI**：`crates/os-api/web/src/views/Network.vue`「P2P 节点网络」区
   ——状态卡（NodeID/OverlayAddr 缩略 + 昵称 + 角色 + 监听 + 已知/已连接计数）、
   节点表（NodeID 缩略/underlay/public 徽章/连接态 + 行内「连接」按钮）、桶占用
   迷你条形图（160 桶聚合 16 段，每段 10 个邻域阶）+ 端点簿计数、连接阶梯统计卡
   （Direct/Punched/Relayed/打洞失败）、发消息表单（admin）；5s 轮询
   （`document.hidden` 暂停，回前台立即补一刷）；未启用（503）显示引导文案。
   `client.ts` 新增 `p2p*` 方法组（`p2pStatus/p2pPeers/p2pBuckets/p2pLadder/
   p2pSend/p2pConnect`，connect 自带 30s 期限——打洞可耗时数秒）。

### 7.2 部署拓扑（P2b：os-api 进程内即节点）

```text
  ┌───────────────── NexOS 实例 A（os-api 进程）─────────────────┐
  │  axum REST :8080                 P2pNode（NEXOS_P2P_ENABLE=1）│
  │  ┌───────────────────────┐ Handle ┌────────────────────────┐ │
  │  │ GET  /api/v1/p2p/status│◀──────▶│ kad 160 桶 / 端点簿    │ │
  │  │ GET  /api/v1/p2p/peers │ 观察面 │ 连接阶梯 / relay 信箱  │ │
  │  │ GET  /api/v1/p2p/buckets│       │ listen :7070           │ │
  │  │ GET  /api/v1/p2p/ladder │       │ （ECDH+AES-256-GCM）   │ │
  │  │ POST /api/v1/p2p/send   │─admin─▶ Handle::send           │ │
  │  │ POST /api/v1/p2p/connect│─admin─▶ 连接阶梯（直连→打洞→中继）│
  │  └───────────────────────┘        └───────────┬────────────┘ │
  │  私钥 NEXOS_P2P_KEY_FILE（0600 原子写，重启同 NodeID）│        │
  └─────────────────────────────────────────────────┼────────────┘
                                                    │ 加密 underlay
                          ┌───────── 互联网 ─────────┴───────┐
                          │ ● cloud 锚点 198.51.100.114:7070    │
                          │   （p2p-node CLI 载体，同款       │
                          │    KEY_FILE 持久身份；           │
                          │    NEXOS_P2P_PUBLIC=1：锚点 +     │
                          │    交换所 + relay）               │
                          └─────────┬────────────────────────┘
                                    │
  ┌───────────────── NexOS 实例 B（os-api，NAT 后）─────────────┐
  │  同 A：REST :8080 + P2pNode :7070 + KEY_FILE 持久身份        │
  │  与 A 的连通阶梯：① LAN mDNS 直连 ② 公网直连                │
  │                  ③ 观测端点 TCP 打洞 ④ 中继兜底             │
  └──────────────────────────────────────────────────────────────┘
```

### 7.3 环境变量全表（全部 `NEXOS_P2P_*`）

| 变量 | 默认 | 作用 | 消费方 |
|---|---|---|---|
| `NEXOS_P2P_ENABLE` | 未设（关） | `1/true/yes` 才在 os-api 进程内 spawn 组网节点（P2b；未启用时 `/api/v1/p2p/*` 全部 503 引导文案） | os-api main.rs |
| `NEXOS_P2P_BOOTSTRAP` | 空（孤网等入站） | 引导节点 `host:port,...`（逗号分隔，非法项跳过）——如 `198.51.100.114:7070` | os-p2p bootstrap |
| `NEXOS_P2P_LISTEN` | `:7070` | 组网监听地址（支持省 IP 形式） | os-p2p bootstrap |
| `NEXOS_P2P_PUBLIC` | 未设（普通节点） | `1/true/yes` = 公网服务节点（bootstrap 锚点 + 观测端点交换所 + relay 志愿者） | os-p2p bootstrap |
| `NEXOS_P2P_MDNS` | 开 | `0/false` 关闭 mDNS LAN 种子（`_nexos-p2p._tcp`，与 avahi 的 `_nexos._tcp` 不串扰；无组播环境自动静默降级 env 引导） | os-p2p bootstrap |
| `NEXOS_P2P_NAME` | 空 | 节点昵称（CLI 横幅 / `GET /api/v1/p2p/status` 的 `self.name` / 网络页状态卡） | CLI + os-api |
| `NEXOS_P2P_KEY_FILE` | 降级链（见 §7.1） | secp256k1 私钥文件（hex）：存在加载/不存在生成并原子写 0600/损坏告警重生成——**重启 NodeID 稳定** | os-p2p bootstrap（CLI 与 os-api 共用） |
| `NEXOS_P2P_INACTIVE_TTL_SECS` | 259200（3 天） | 节点元数据非活跃条目 TTL（秒，2026-09-02）：`Inactive` 且 `last_seen` 距今超 TTL 即整条删除（含 `node-meta.json` 重写；启动即扫一次 + 每 300 tick ≈ 25 分钟一扫）；`0` = 禁用清除（向后兼容开关）；非法值回落默认。详见 docs/NODE_META_LOOPBACK_FIX.md §10 | os-p2p meta（心跳引擎启动读取） |

### 7.4 端点契约（组件 `p2p`）

| 方法 | 路径 | 鉴权 | 响应要点 |
|---|---|---|---|
| GET | `/api/v1/p2p/status` | 公开 | `{enabled, self:{node_id,overlay_addr,name,public}, listen, peers_known, peers_connected}` |
| GET | `/api/v1/p2p/peers` | 公开 | `PeerInfo[]`：`{id, underlay|null, public, relay, connected, relayed_by_me, route_via}` |
| GET | `/api/v1/p2p/buckets` | 公开 | `{buckets:[{po,count,entries[]}], known_endpoints:[{id,addr}]}`（地址交换所） |
| GET | `/api/v1/p2p/ladder` | 公开 | `{direct, punched, relayed, punch_failed}` |
| POST | `/api/v1/p2p/send` | admin | body `{node_id, text}`；fire-and-forget（无路由暂存 + 触发查找） |
| POST | `/api/v1/p2p/connect` | admin | body `{node_id}`；返回 `{ok, node_id, path:"direct"|"punched"|"relayed"}`；全阶梯失败 502 |

未启用统一语义：任何方法/路径 → **503** `{"error":"P2P 未启用（NEXOS_P2P_ENABLE=1）"}`。

### 7.5 验收状态（P2b）

- [x] os-p2p 密钥持久化：生成→重启同身份（单测 + CLI 双轮 NodeID 一致）；
      损坏文件降级重生成并告警；不可写目录退内存身份（59 测试全绿）
- [x] os-api：p2p.rs 6 端点 + 鉴权矩阵（读公开/写 admin）+ 未启用 503 语义 +
      双节点组网观察面测试（os-api 新增 10 测试）
- [x] 前端：npm build 0 TS 错误 + static-dist 重嵌入；网络页 P2P 区四块 UI + 5s 轮询
- [x] 本机冒烟：`NEXOS_P2P_ENABLE=1 NEXOS_P2P_BOOTSTRAP=198.51.100.114:7070` 启动
      os-api → `GET /api/v1/p2p/status` 返回自身 NodeID，peers 含 cloud 锚点（见
      MEMORY.md 冒烟记录）
- [x] P3 消费者（联邦大厅 / IM Federation 经 os-p2p 同步）✅（2026-08-22，见 §8）

## 8. P3 落地状态（2026-08-22：IM 跨节点大厅消息 + NexHub 跨节点项目发现）

### 8.1 联邦架构（os-p2p 之上的第一批消费者）

```text
  ┌──────────────────── NexOS 节点 A（os-api，NEXOS_P2P_ENABLE=1）────────────────────┐
  │                                                                                   │
  │  POST /api/v1/im/lobby/messages ──▶ im_messages 落地 + WS 广播                    │
  │        │（agent/系统消息不联邦）                                                   │
  │        ▼                                                                          │
  │  ImFederation.federate_lobby_message ──▶ fed_broadcast(Handle) ──▶ 逐个 send     │
  │                                                                                   │
  │  POST /api/v1/nexhub/lobby/publish（owner_kind=pubkey 才广播）                    │
  │        └─▶ LobbyFedEndpoint.broadcast_entry ──▶ P2pLobbyTransport ──┐             │
  │                                                                     │ Handle       │
  │  ┌────────────────────────────────────────────────────────────────── ◀┘             │
  │  │ FederationBridge（main.rs 装配的入站观测 task：handle.on_msg()）               │
  │  │   fed=="im_lobby"      ──▶ ImFederation.ingest                                 │
  │  │       去重（id 内存缓存 1000 + DB）→ 写 im_messages（sender_id=fed:<node>:<原>）│
  │  │       → WS 广播本地在线用户（远程消息走现有渲染管线，气泡 🌐 来自 node-x）      │
  │  │   fed=="nexhub_lobby"  ──▶ LobbyFedEndpoint.ingest                             │
  │  │       去重（repo+node 内存缓存 + DB）→ 写 hub_lobby（source_node=来源节点）     │
  │  │       → 大厅行 🌐 远程徽章；克隆按原始 source_url 从远程拉取                    │
  │  │   其他载荷（{text} 调试消息等）──▶ 仅记日志                                     │
  │  └────────────────────────────────────────────────────────────────────             │
  └───────────────────────────────── 加密 underlay（ECDH+AES-256-GCM）────────────────┘
                                                  │
                                  ┌── 互联网 / LAN ─┴──────────────┐
                                  │        节点 B（同款装配）       │
                                  │  入站 payload → FederationBridge│
                                  │  出站同 A（对称）               │
                                  └───────────────────────────────┘
```

### 8.2 联邦消息协议（经 os-p2p SEND 帧的应用 payload）

```json
// IM 大厅联邦消息（fed=="im_lobby"）
{"fed": "im_lobby", "node": "node-106", "message": { ...完整 IM Message JSON... }}
// NexHub 大厅联邦条目（fed=="nexhub_lobby"）
{"fed": "nexhub_lobby", "node": "node-106", "entry": { ...完整 LobbyEntry JSON... }}
```

- `node` = 发布节点名（`NEXOS_P2P_NAME`；空/超长净化为 "peer"）；IM 接收端把它编进
  `sender_id` 前缀（`fed:<node>:<原 pubkey>`），NexHub 接收端写进 `hub_lobby.source_node`。
- **广播范围** = 当前已连接 peer（一跳 fan-out，`fed_broadcast` 逐个 send）；
  接收端 ingest 落地后**不转播**——天然无环。

### 8.3 不联邦的内容（裁决）

| 内容 | 原因 |
|---|---|
| IM 助手回复（`sender_kind=="agent"`，含 agent:nexos-assistant） | 每节点 AI 只回本地，联邦网内不重复 AI 回答 |
| IM 系统消息（`sender_id=="system"` / `msg_type=="system"`，含入廊欢迎） | 入廊是本地事件，无需全网播报 |
| NexHub admin 字符串条目（NexOS 常驻/local/平台托管） | 避免平台托管条目在联邦网内重复扩散——只联邦 publisher=pubkey 条目 |

### 8.4 去重与防护

- **IM**：消息 id（UUID）双重判定——内存缓存最近 1000 条 + DB `find_message` 兜底
  （重启后缓存为空仍不重复写）；`conversation_id` 强制归位 lobby（防伪造会话注入）；
  `sender_id` 改写 `fed:<node>:<原>`（与本地 `0x` pubkey 空间天然隔离）。
- **NexHub**：`repo_name+source_node` 联合键内存缓存 + DB 权威判定；同名条目
  **本地/首到者受保护**（来源不同 → Skipped）；同源重发 → 刷新快照但保留本地
  `download_count`；`repo_name` 走与本地发布同款校验（防路径穿越）。

### 8.5 改动面

| 文件 | 内容 |
|---|---|
| `crates/os-api/src/handlers/im.rs` | `ImFederation`（发送/接收/去重）+ `ImShared.fed_p2p/fed_seen` + POST /lobby/messages 挂联邦广播 |
| `crates/os-nexhub/src/nexhub_lobby.rs` | `LobbyEntry.source_node`（幂等 ALTER）+ `LobbyFedTransport` trait（依赖反转，os-nexhub 不依赖 os-p2p）+ `LobbyFedEndpoint`（广播/ingest）+ publish 挂广播 + clone 响应带 source_node |
| `crates/os-api/src/handlers/p2p.rs` | `fed_broadcast`（连 peer fan-out）+ `P2pLobbyTransport`（trait 的 os-p2p 实现）+ `FederationBridge`（入站分发） |
| `crates/os-api/src/main.rs` | 装配：im/nexhub 端点在 Box 前取出 → p2p spawn 成功后注入 Handle + 起 bridge 分发 task |
| 前端 `Chat.vue` / `CodeHub.vue` / `client.ts` | 🌐 远程徽章（fed: 前缀 / source_node 字段）+ 克隆提示"将从远程节点拉取" |

### 8.6 验收状态（P3）

- [x] os-nexhub：联邦发送/接收/去重/source_node 迁移/纯函数（新增 10 测试，85 全绿）
- [x] os-api im：载荷构造/federable 裁决/无 P2P 静默/写入字段/去重/非法忽略/WS 广播（新增 6 测试）
- [x] os-api p2p：fed_broadcast 送达/孤网 0/P2pLobbyTransport/bridge 三分派 + 忽略
      （新增 7 测试，其中 **端到端 fed_end_to_end_two_nodes_im_and_nexhub**：双节点
      真实组网——A 经 REST 发大厅消息 → B 落地（fed: 前缀）→ B 广播大厅条目 →
      A 落地（source_node 标记））
- [x] 前端：npm build 0 TS 错误 + static-dist 重嵌入
- [x] 未启用 P2P（默认）时联邦发送/接收静默停用，单机语义零变化
