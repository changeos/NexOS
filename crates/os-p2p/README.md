# os-p2p —— NexOS 点对点组网层（P2a：加密链路 + 观测端点八卦 + TCP 打洞 + mDNS 种子；P2b：密钥持久化）

> 设计定稿：`docs/NEXOS_P2P_NETWORK_DESIGN.md`（用户 2026-08-20，含同日连接阶梯补丁；
> P2b 落地状态注记见其 §7）。
> **全分布式 Kademlia（Swarm 同款）**：不设中心注册表，节点发现与路由完全经
> DHT walk；公网节点（`NEXOS_P2P_PUBLIC=1`）仅承担 bootstrap 冷启动、观测端点
> 交换所与 NAT 中继服务——是"服务节点"不是"中心"。P2a 在 P1（Kademlia 核心 +
> 中继 + 35 测试）之上落地四件事：**每连接 ECDH+AES-256-GCM 加密**、**观测端点
> 八卦（地址交换所）**、**TCP 打洞 + 连接阶梯**、**mDNS LAN 种子**，外加部署
> 载体 `p2p-node` CLI。P2b 增补：**密钥持久化**（`NEXOS_P2P_KEY_FILE`——
> `config_from_env` 默认加载/生成 secp256k1 私钥，原子写 0600，**CLI 与 os-api
> 内嵌节点共用，重启 NodeID 稳定**；os-api 侧接入见
> `crates/os-api/src/handlers/p2p.rs`）。

---

## 1. 目标拓扑与连接阶梯（P2a 核心图景）

```text
                        互 联 网 （公网 underlay 可直拨）
     ┌──────────────────────────────────────────────────────────────┐
     │   ● P1（公网 NexOS：bootstrap 锚点 + 端点交换所 + relay）      │
     │   │      ▲ observe(A)=Pa      ▲ observe(B)=Pb                 │
     │   │      └────── NODES 应答捎带 {NodeID→观测ip:port} 八卦 ────┘│
     └───┼──────────────────────────────────────────────────────────┘
         │ 出站（NAT 映射口 Pa）                │ 出站（NAT 映射口 Pb）
    ┌────┼────────────────────────┐   ┌───────┼─────────────────────┐
    │ A 的 NAT 域                 │   │ B 的 NAT 域                │
    │  ● A ── PUNCH1{token,Pa} ─→ P1 转交 ─→ B ●                    │
    │  ● A ←─ PUNCH2{token,Pb} ── P1 转交 ←── B ●                   │
    │  ● A ═══ 同时打开 connect(Pb) ═══╦═══ connect(Pa) ═══ B ●     │
    └────────────────────────────────┴───┴─────────────────────────┘

  连接阶梯（Handle::connect，高优先级成功即短路）：
    ① LAN 直连     mDNS `_nexos-p2p._tcp` 发现的同网段邻居（首选种子）
    ② 公网直连     桶内 underlay 可拨 → 直接 TCP 拨号          → Direct
    ③ TCP 打洞     端点簿有观测端点 → PUNCH 交换 + 同时打开    → Punched
    ④ 中继兜底     {dst→relay} 路由（打洞失败才落，定向 walk 后再查）→ Relayed
```

- **每个节点地位平等**：公网可达性由 `NEXOS_P2P_PUBLIC=1` 声明——声明者多担
  服务职责（冷启动锚点 / 地址交换所 / 中继志愿者），仍是普通 NexOS。
- **观测端点八卦（地址交换所）**：任何节点连接我时，我看到的是它的公网观测
  地址（socket 对端 ip:port——NAT 映射口）。`{NodeID→观测端点}` 入地址簿并随
  FINDNODE/NODES 一起八卦（每消息 ≤32 条）：**只要全网任一节点接触过某 NAT
  节点，它的公网端点就全网可知**——一个公网 NexOS 足以充当全网地址交换所。
  节点也从八卦里学到"自己的"观测端点（打洞时通告）。
- **TCP 打洞（simultaneous open）**：双方经共同中介交换 `{观测端点+会话token}`
  → 约定时刻互相向对方观测端点出站 connect（本地端口复用中介连接的映射口）
  → 出站+入站竞速，**先完成标准握手注册者胜**（重复连接由 register_conn 去重）
  → 失败重试 3 次 ×800ms → 仍败落中继。

## 2. 模块架构（9 模块 / 55 测试）

```text
┌────────────── 上层服务（P3 消费者：联邦大厅 / IM Federation；载体：p2p-node CLI）─┐
│ Handle::send / connect(阶梯) / on_msg / peers / known_endpoints / ladder_stats    │
└──────────────┬───────────────────────────────────────────────────────────────────┘
               ▼
┌─ api.rs 引擎装配：任务编组 + SEND 路由 + 观察面 ─────────────────────────────────┐
│  cmd_loop ─▶ route_frame（直连/信箱/relay 转发/lookup 重试）                      │
│  accept/reader/writer（每连接：ECDH 会话密钥上的加密帧收发）                      │
│  maintenance（ping 剔除/桶刷新/重拨/中继+端点 TTL）                               │
└──┬────────┬──────────┬───────────┬─────────────┬──────────────┬─────────────────┘
   ▼        ▼          ▼           ▼             ▼              ▼
┌ kad ┐ ┌ transport ┐ ┌ crypto ┐ ┌ endpoints ┐ ┌ punch ┐   ┌ bootstrap ┐
│160桶 │ │版本化帧   │ │ECDH临时 │ │观测地址簿  │ │PUNCH1/2│   │mDNS 种子+  │
│k=16  │ │明文握手帧 │ │密钥+KDF │ │gossip≤32  │ │连接阶梯│   │env 引导+   │
│α=3   │ │+密文帧    │ │AES-256  │ │TTL 清理    │ │同时打开│   │walk+保活   │
│收敛  │ │IO 读写    │ │-GCM     │ │            │ │3×800ms│   │            │
└──┬───┘ └────┬─────┘ └────┬────┘ └─────┬──────┘ └───┬────┘   └─────┬──────┘
   └──────────┴───── identity.rs（NodeID=secp256k1 公钥 / OverlayAddr=EVM 同源）──┘
                            └── relay.rs（{dst→relay} 路由 + 信箱 100 条/节点）
```

| 模块 | 职责（设计 §3 对应） |
|---|---|
| `identity.rs` | NodeID=压缩公钥（chain_auth 同源）、OverlayAddr=keccak20、XOR 距离/邻域阶/桶选择纯函数 |
| `crypto.rs` | ECDH 临时密钥（k256 ecdh）+ SHA-256 KDF + AES-256-GCM 会话密码（P2a） |
| `transport.rs` | 版本化帧（明文握手帧 + 加密数据帧）+ 认证密钥交换握手 + punch 帧类型 |
| `kad.rs` | 160 邻域 k-buckets（k=16）+ FINDNODE 迭代查询收敛原语 + 失效剔除 |
| `endpoints.rs` | 观测端点地址簿（地址交换所）+ NODES 捎带八卦（≤32 条/消息）+ TTL（P2a） |
| `punch.rs` | PUNCH1/PUNCH2 端点交换 + 同时打开拨号 + `Handle::connect` 连接阶梯（P2a） |
| `relay.rs` | 可达性记录 `{dst→经 relay_id}` + NextHop 路由决策 + 信箱（100 条/节点） |
| `bootstrap.rs` | `_nexos-p2p._tcp` mDNS LAN 种子（不可用静默降级 env）+ 冷启动 walk + 保活（P2a） |
| `api.rs` | `P2pNode::spawn → Handle`、任务编组、SEND 路由、维护循环、观察面 |

**依赖面**：`os-common`（chain_auth）+ tokio/serde/k256(ecdh)/aes-gcm/socket2/
mdns-sd；不依赖 os-discover（mDNS 直用 mdns-sd，独立于其联邦状态机）、不依赖
os-api。socket2 为 tokio 传递依赖共版本直引（打洞映射复用的 listener 侧
SO_REUSEPORT 绑定；workspace 注册 + ADR 归档待仓库 owner 补记）。

## 3. 身份与地址（身份体系零新增）

| 概念 | 定义 | 来源 |
|---|---|---|
| `NodeID` | secp256k1 压缩公钥（`0x`+66 hex） | = chain_auth 链上身份（IM/NexHub 用户名即节点身份） |
| `OverlayAddr` | `keccak256(未压缩公钥[1..])[12..]` 20 字节 | = EVM 地址派生同源（`0x7e5f…bdf` 公开向量验证） |
| 邻域阶 PO | 两地址共同前导比特数（0..=159） | Swarm proximity order 的 160-bit 投影 |
| 观测端点 | socket 对端 ip:port（NAT 映射口） | 地址交换所的记账单元（P2a） |

## 4. 协议帧格式（P2a：加密链路 + 版本门）

```text
握手帧（明文——交换身份与密钥材料）：
┌───────────────┬────────────────────────────────────────────┐
│ u32 BE 长度   │ JSON 信封（hello / auth_challenge / response）│
└───────────────┴────────────────────────────────────────────┘

数据帧（握手后全部加密）：
┌───────────────┬──────────────┬───────────────────────────────┐
│ u32 BE 长度   │ nonce (12B)  │ AES-256-GCM 密文 ‖ 标签 (16B)  │
└───────────────┴──────────────┴───────────────────────────────┘
密文 = JSON 信封；长度 ≤ 4 MiB 沿用；GCM 标签不符 = 篡改/密钥不配 → 断连

信封 = {"type":<kind>, "version":<u32>, "src":"0x<66hex>", "dst":"0x<66hex>"|null,
        "ttl":<u8>, "hops":<u8>, "payload":{…}}

kind / payload 契约：
  hello          {underlay, public, eph: "0x<66hex>"}  ── eph = ECDH 临时公钥
  auth_challenge {req_id, nonce}                       ── 双向挑战
  auth_response  {req_id, signature}                   ── 签名覆盖 nonce+双方 eph
  ping / pong    {req_id}                              ── 存活探测
  findnode       {req_id, target}                      ── 最近 ≤K 个节点
  nodes          {req_id, nodes: […], endpoints: [{id,addr}]}  ── +观测端点八卦 ≤32
  send           {payload}                             ── dst=最终接收者（多跳）
  relay_announce {}                                    ── NAT→公网："经你中继可达我"
  punch1         {token, endpoints: ["ip:port"]}       ── 打洞发起（经中介转交）
  punch2         {token, endpoints: ["ip:port"]}       ── 打洞应答（回显 token）

version 字段 = 协议版本（P2a=2）。**无明文回落**：Hello 版本不一致立即拒连
（P2pError::VersionMismatch）——同版本才可互联，避免半加密连接。
```

**握手时序（双向认证 + ECDH，5s 超时）**：

```text
  A                              B（TCP 建立后对称执行，无死锁）
  │ ── hello{version, id, underlay, public, eph_A} ──▶ │
  │ ◀──────────────────────────────────────── eph_B ── │
  │ ── auth_challenge{nonce_A} ─────────────────────▶ │
  │ ◀── auth_challenge{nonce_B} ────────────────────── │
  │ ── auth_response{sig(nonce_B‖eph_A‖eph_B)} ─────▶ │ ← chain_auth 验签
  │ ◀── auth_response{sig(nonce_A‖eph_B‖eph_A)} ────── │   （临时公钥入签名
  │        ECDH(eph) → SHA-256(秘密‖nonce_lo‖nonce_hi) │    转录本——中途替换
  │        → 双方各自派生同一 256-bit 会话密钥          │    任一 eph 即验签失败）
  │ ═══════ 之后所有帧 AES-256-GCM（12B 随机 nonce）═══════▶
```

- nonce 按字典序 canonical 排序入 KDF——两侧无需角色约定即得同一密钥。
- MITM 防护根：**密钥协商被认证到 NodeID 私钥上**（SIGMA 思想）——冒充身份
  或替换临时公钥都会使转录本不一致 → 验签失败（有正反测试覆盖）。

## 5. 连接阶梯与打洞时序

```text
 A.connect(B)                     C（共同中介 = 已连节点，公网优先）        B
   │ 已直连 B？──是──▶ Direct（短路）                                     │
   │ 桶内 B.underlay 可拨？──是──▶ 直接拨号 ──▶ Direct                     │
   │ 端点簿有 B 的观测端点？──否──┐                                        │
   │   │                        ▼                                        │
   │ ── punch1{dst=B, token, [Pa]} ─▶ │ dst≠我 → 直连转交（不知 B 即丢）──▶ │ 学习 Pa
   │                                 │                        B 回 PUNCH2 │
   │ ◀── 转交 ── punch2{dst=A, token, [Pb]} ────────────────────────────── │ 发后稍候
   │ 收到即拨 Pb（绑 Pa）              │                        B 拨 Pa（绑 Pb）
   │   ════════ TCP 同时打开（3 轮 × 800ms，轮转端点）════════           │
   │ 出站拨通 → 标准握手（ECDH+签名）→ register_conn        入站由 listener │
   │   先建立者胜（重复连接去重拒绝）        accept 路径承接 → 竞速判定    │
   │ ──成功──▶ Punched（A↔B 真实直连，消息 hops=0）                        │
   │ ──全败──▶ {B→relay} 路由在？（无则定向 walk 再查）──▶ Relayed        │
   │ ──仍无──▶ Err(PunchFailed)                                          │
```

- **映射复用**：打洞出站 socket 绑定中介连接的本地端口（`Conn::local`）；
  `dial_from_listen_port` 配置让全部出站连接复用监听口——loopback 上即模拟
  full-cone NAT 稳定映射（listener 以 SO_REUSEPORT 绑定，入站仍由 listener
  承接，不与已连接 socket 抢流量）。
- **竞速语义**：打洞是出站+入站同时进行——对端拨来的连接落在本节点 listener
  上走标准 accept/握手路径；拨号循环每步检查"是否已连"（对端先建立即成功）。
- **阶梯统计**：`Handle::ladder_stats()` → `{direct, punched, relayed,
  punch_failed}`（CLI status 展示）。

## 6. 观测端点八卦（地址交换所）

```text
 N0(NAT) ──出站──▶ P1(公网/交换所)          P1 观测 N0 = 203.0.113.9:51234
                       │ （NAT 映射口，随连接建立入簿）
                       ▼ NODES 应答捎带 endpoints 八卦（≤32 条/消息，最新优先）
            ┌──────────┴──────────┐
            ▼                     ▼
       N1 学到 {N0→映射口}    N0 学到自己的观测端点
       （可对 N0 打洞）        （打洞 PUNCH1 通告的依据）
```

- API：`Handle::known_endpoints()`（快照）/ `lookup_endpoint(node_id)`。
- 条目 TTL（默认 600s，`Timing::endpoint_ttl`）——死 NAT 映射不滞留地址簿。

## 7. mDNS LAN 种子与配置

| 环境变量 | 语义 | 默认 |
|---|---|---|
| `NEXOS_P2P_BOOTSTRAP` | 引导节点 `host:port,...`（逗号分隔，非法项跳过） | 空（孤网等入站） |
| `NEXOS_P2P_LISTEN` | 监听地址（支持 `:7070` 省 IP） | `:7070` |
| `NEXOS_P2P_PUBLIC` | `1/true/yes` = 公网服务节点（bootstrap+交换所+relay） | 未设置 |
| `NEXOS_P2P_MDNS` | `0/false` 关闭 mDNS LAN 种子 | 开（standalone 节点行为） |
| `NEXOS_P2P_NAME` | 节点昵称（p2p-node CLI 展示） | 空 |
| `NEXOS_P2P_KEY_FILE` | secp256k1 私钥文件（hex）——重启身份稳定（P2b） | 降级链 `/tank/os-data/p2p-node-key` → `/var/lib/os/p2p-node-key` → `./p2p-node-key` |

- mDNS 服务类型 **`_nexos-p2p._tcp`**（mdns-sd 纯 Rust，无 avahi 依赖）——与
  avahi 的 `_nexos._tcp` 通告**服务类型与端口都不同**，两套发现互不串扰。
- 种子优先级：**mDNS 发现的 LAN 邻居 > env 引导**（同网段直连延迟最优）；
  保活循环持续吃 mDNS 事件，新邻居上线即拨。
- **mDNS 不可用（无组播环境/容器）静默降级**：daemon 起不来或无结果都不报错，
  直接走 env 引导（有测试覆盖该路径）。

## 8. p2p-node CLI（部署载体）

**部署到非 NexOS 公网机（cloud）跑锚点/交换所/中继角色的可执行体**：

```bash
cargo build --release -p os-p2p --bin p2p-node

# cloud 锚点（公网机）：一行即成 bootstrap 锚点 + 地址交换所 + relay 志愿者
NEXOS_P2P_PUBLIC=1 NEXOS_P2P_NAME=anchor-1 ./target/release/p2p-node

# NAT 后普通节点：指向锚点即可入网（打洞优先、中继兜底）
NEXOS_P2P_BOOTSTRAP=<锚点ip>:7070 NEXOS_P2P_NAME=laptop-1 ./p2p-node
```

```text
[p2p-node] name        = anchor-1
[p2p-node] NodeID      = 0x03f2…89ea
[p2p-node] OverlayAddr = 0xd5c3…f1df
[p2p-node] listen      = 0.0.0.0:7070
[p2p-node] 命令: status | peers | send <node_id> <text> | quit

status   → 路由表（k-buckets 摘要）/ 端点簿（地址交换所）/ 连接阶梯统计
peers    → 已知节点清单（连接状态 / underlay / 公网角色 / 中继路由）
send     → 向某节点发应用消息；[recv] from=… 行实时打印收到的事件
connect  → 手动触发连接阶梯（打印 Direct/Punched/Relayed）
quit     → 优雅停机
```

Rust 侧接入（os-api 已接入：`crates/os-api/src/handlers/p2p.rs`，`NEXOS_P2P_ENABLE=1`
时 main.rs 内嵌 spawn；网络页 Network.vue「P2P 节点网络」区消费 6 端点）：

```rust
use os_p2p::{config_from_env, ConnectPath, P2pNode};

let handle = P2pNode::spawn(config_from_env())?;   // 须在 tokio runtime 内
match handle.connect(&peer_id).await? {            // 连接阶梯
    ConnectPath::Punched => { /* NAT 穿透直连 */ }
    ConnectPath::Relayed => { /* 走中继 */ }
    ConnectPath::Direct  => { /* LAN/公网直连 */ }
}
handle.send(&peer_id, serde_json::json!({"im": "你好 NexOS"}));
let mut rx = handle.on_msg();                      // 订阅入站消息
```

## 9. 测试（55 个：43 单测 + 11 集成 + 1 文档测，全部默认跑）

**P1 单测**（29）：距离/邻域阶手工向量（PO 0/1/9/159/160）、EVM 同源公开向量、
桶位置自洽、closest XOR 排序、桶满保留旧节点、连续失败剔除、帧编解码往返、
ttl/hops 变换、超长帧防护、握手双向认证 + 冒充身份拒绝、NextHop 决策矩阵、
信箱 100 上限丢最旧、注册 TTL、env 解析。

**P2a 单测**（14）：ECDH 对称密钥协商 + 转录本方向性、非法临时公钥拒绝、
GCM 往返/篡改检出/错密钥互解、版本门拒连（v1↔v2）、**ECDH 防 MITM 正反**
（中途替换临时公钥 → 两侧验签失败）、握手后双向加密帧往返、punch 载荷 serde、
端点簿观测/学习/查询、**八卦采样 ≤32 条 + 排除应答者 + 保留请求者**、
端点 TTL 清理、打洞 token 唯一性、**PunchPlan 约定时刻**（发起方即刻/响应方
延迟 + delay 字节线性映射）、端点轮转、种子合并（mDNS 优先 + 去重）、
服务类型与 avahi 区分、env 全变量（含 MDNS 开关）。

**集成测试**（`tests/topology.rs` 5 个 P1 + `tests/ladder.rs` 5 个 P2a +
`tests/cli.rs` 1 个）：

```text
  P1（既有，全保绿）                      P2a ladder.rs
  ┌────────────────────────────┐         ┌──────────────────────────────────┐
  │ ① 五节点收敛               │         │ ⑥ 观测端点记录+八卦传播（三实例）│
  │ ② 直连+中继互通 ttl/hops    │         │    （交换所观测/转述一致/自回灌）│
  │ ③ kill 剔除+绕行           │         │ ⑦ loopback 打洞：reuse-port 模拟 │
  │ ④ 迟到者 walk 入网          │         │    NAT 映射 → ConnectPath=Punched│
  │ ⑤ 离线信箱重投             │         │    + 直连 0 跳 + 阶梯计数        │
  └────────────────────────────┘         │ ⑧ 打洞失败（死映射）→ PunchFailed │
                                         │    内部短路 → Relayed + 中继可达 │
  tests/cli.rs                           │ ⑨ 阶梯短路：有 underlay 不打洞   │
  ┌────────────────────────────┐         │    （punched/punch_failed==0）   │
  │ ⑩ CLI 冒烟：spawn 二进制    │         │ ⑪ mDNS 不可用降级 env 引导       │
  │   stdin 驱动 status/peers   │         └──────────────────────────────────┘
  │   /send/quit exit 0        │
  └────────────────────────────┘
```

- 打洞测试拓扑：P1（公网交换所）+ A/B（NAT，`dial_from_listen_port=true` 各自
  绑定监听口出站——两个 socket 模拟 NAT 稳定映射）；成功路径验证同时打开 +
  `ConnectPath::Punched`；失败路径（reuse 关 → 观测端点为死临时口）验证
  3×重试全败后落中继。
- 集成节奏用 `Timing::testing()`（亚秒级 tick）；全套 5 连跑零 flake。

## 10. 已知限制（P2a 边界 → 后续去向）

1. **对称 NAT 打洞成功率低**：映射复用式打洞对 full-cone/restricted NAT 有效；
   symmetric NAT（每目标独立映射）需要端口预测或 relay-only——当前直接落
   中继（行为正确，非最优）。
2. **mDNS 通告 IP 枚举简陋**：监听 unspecified 时用 loopback 保底（同机发现
   可用）；多网卡真实出口 IP 枚举（if-addrs）待补。
3. **八卦按新鲜度采样**：≤32 条/消息，非按请求者邻域定制——上千节点规模下
   可升级为 XOR 距离加权采样。
4. **send 仍 fire-and-forget**：无端到端 ACK/重传 beyond 信箱（P1 已知限制，
   上层叠加）。
5. **帧仍为 JSON**：调试友好优先；带宽敏感可换 bincode（信封已隔离在
   transport.rs，替换面小）。
6. **socket2 直引未入 workspace 注册**：与 tokio 传递依赖共版本（lockfile 既有
   0.6）；`[workspace.dependencies]` 注册 + ADR 归档待仓库 owner 补记。

## 11. 验收状态（P2a + P2b）

- [x] `cargo test -p os-p2p` 全绿（P2b 后 59 个：47 单测 + 11 集成 + 1 文档测，
      < 2s，含密钥持久化四态：生成→重启同身份 / 损坏重生成 / 不可写降级 /
      降级链形状；CLI 冒烟含双轮 NodeID 一致）
- [x] workspace 四道门：`check` / `clippy -D warnings` / `fmt --check` /
      `test --features mock`（4307 全绿）
- [x] `cargo build --release -p os-p2p --bin p2p-node` 成功（3.7 MB，冒烟通过）
- [x] 本 README（P2a 拓扑图：连接阶梯/模块架构/握手时序/打洞时序/交换所/
      测试拓扑——PPT 素材齐备）
- [x] 两台真机跨 LAN 实测（cloud 锚点 198.51.100.114:7070 + 本机组网验证）
- [x] os-api 接入（P2b，2026-08-21）：handler `p2p.rs` 6 端点 + main.rs
      `NEXOS_P2P_ENABLE=1` 装配 + 网络页拓扑 UI（详见设计文档 §7）
- [ ] osd 编排接入（os-api 已就绪，systemd env 透传属部署面）
