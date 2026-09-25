# WAN 出口共享（network-exit）+ 防火墙基础

> 2026-08-30 · component `network-exit` · `crates/os-api/src/handlers/network_exit.rs`
> （os-p2p meta digest 扩展一个 `exit_offered` 字段 + main.rs 装配行）

「允许其他节点把本节点当作网络出口」（用户原话）——即 v2ray 客户端/服务端模式与
Tailscale exit node 的混合形态：出口节点声明自己可出网，其他节点把目标 TCP 流量经
NexOS 既有加密 P2P overlay（os-p2p：直连/TCP 打洞/中继）送到出口节点转发，出口侧
做逐节点授权（默认 deny）。同时补齐网络管理的防火墙基础功能（规则模型 + 持久化 +
iptables 自定义链落地）。

---

## 一、调研摘要

### 1. v2ray / Xray（多出口代理栈）

- **架构**：本地 SOCKS5 inbound（典型 1080 端口）收应用流量 → 路由规则（按
  域名/IP/协议匹配，规则有序、同规则内多字段 AND）→ 分发到 outbound
  （VMess/VLESS/Shadowsocks/Trojan 远端出站，或 direct/blackhole）。
- **协议**：VMess（时间戳认证的私有协议）、VLESS（无加密层、依赖 TLS/Reality
  传载、按 UUID 用户鉴权）、Shadowsocks（AEAD 流加密）、Trojan（伪装 TLS 流量）。
  传输层可套 ws/grpc/tcp 加 TLS。
- **对我们的借鉴点**：
  - **多出口选择 + 规则路由**：客户端可配多个出站，按目标域名/IP 选路——对应
    我们「默认出口节点 + 未来按目标路由」；
  - **本地 SOCKS5 入口**：用户浏览器/系统代理指向 `127.0.0.1:<port>` 即可获得
    远端出口——零内核侵入、应用级透明度足够，这是 v2ray 十年验证过的客户端
    形态，我们照搬入口形态；
  - **按用户/身份鉴权**：VLESS UUID / Trojan 密码——对应我们按 NodeID（握手
    签名验证的公钥，不可伪造）授权，比静态密码强（身份=密钥是 NexOS 设计特性）。
- **不引入 v2ray 全家桶的理由**：v2ray 解决的是「翻越审查/加密混淆」，加密与
  NAT 穿透我们已有（AES-256-GCM 链路 + 打洞/中继）；引入第二套协议栈只会增加
  攻击面与运维面。

### 2. SD-WAN（Tailscale / WireGuard / ZeroTier）

- **Tailscale exit node 机制**（与用户描述最接近）：节点 A
  `tailscale up --advertise-exit-node` 向协调服务器通告默认路由
  （`0.0.0.0/0` + `::/0`）；节点 B 在客户端选择 A 为出口后，操作系统路由表被
  改写为把全部流量指进 `tailscale0` 隧道；A 开启 `ip_forward` 并对转发流量
  SNAT（等价 `iptables -t nat -A POSTROUTING -o eth0 -j MASQUERADE`）——
  出网包源 IP 换成 A 的本地 IP，回程才可达。
- **ZeroTier**：同思路（managed route + 默认路由 via exit 节点），差别在
  控制面（控制器下发路由表）。
- **对我们的借鉴点**：
  - **出口声明/订阅模型**：`advertise`（声明）+ 控制台 approve（授权）+
    客户端 use（订阅）三段式——对应我们 offer（digest 广播）+ authorize
    （出口侧授权表）+ use（入口侧设默认出口）；
  - **overlay 内转发**：流量全程在加密 overlay 里走到出口节点才落地——我们
    完全一致（os-p2p SEND 帧）；
  - **NAT 网关**：出口节点须做 MASQUERADE——应用级 SOCKS5 模式下由出口节点
    本机代拨目标连接（源 IP 天然是出口节点），等价于用户态 MASQUERADE。
- **不直接抄默认路由模式的原因**：改系统默认路由需要 tun 设备 + `ip rule` +
    内核转发 + root 特权，且失败模式危险（断网、锁死 SSH）。

### 3. 结论：overlay 级出口节点（本期）+ 默认路由级（二期）

NexOS 已有加密 P2P overlay（os-p2p），**不需要引入 v2ray 全家桶**。本期实现
「overlay 级出口节点」：

- **SOCKS5 经 overlay 是零内核侵入的第一步**：入口节点本地起 SOCKS5
  （127.0.0.1:11081），用户浏览器/应用指向它即可经远端出口出网（v2ray 客户端
  模式）；流量在 overlay 内加密传输（复用链路层 AES-256-GCM），出口节点本机
  代拨目标。不动系统路由表、不需要 tun、不需要 root，失败模式只是「代理连不上」
  而非「整机断网」。
- **默认路由级 exit node（ip rule + tun + MASQUERADE）列二期**：Tailscale
  同款全流量接管，需处理 DNS 劫持、回程路由、断网自愈（deadman switch），
  在应用级模式验证出口授权与转发引擎稳定后再上。

---

## 二、架构

### 2.1 拓扑（overlay exit model）

```text
                    ┌──────────────────────── 入口节点 A（use 出口）───────────────────────┐
   浏览器/应用 ──►  │  本地 SOCKS5 127.0.0.1:11081（入口）                                  │
  (系统代理指向)    │    │ CONNECT host:port                                                 │
                    │    ▼                                                                 │
                    │  ingress：net_exit/open {conn_id, host, port} ──┐                    │
                    └─────────────────────────────────────────────────│───────────────────┘
                                                                        │ os-p2p SEND 帧
                                                                        │ （直连/打洞/中继，
                                                                        │  AES-256-GCM 链路）
                    ┌──────────────────────── 出口节点 B（offer + authorize A）─────────┐
                    │  ingress：net_exit/open ──► 授权表检查（A？未过期？B 自身 offer？）│
                    │    │ deny 默认 ──► open_failed                                     │
                    │    ▼ allow                                                         │
                    │  本机拨 SOCKS5 127.0.0.1:11080（出口侧本地代拨）──► TcpStream 目标  │
                    │  data 分块双向回传（64KiB base64 + seq + 窗口背压 ack）             │
                    └────────────────────────────────────────────────────────────────────┘

  vs Tailscale exit node：tun + ip rule 0.0.0.0/0 → tailscale0 + ip_forward + MASQUERADE（内核级）
  vs v2ray：本地 SOCKS5 inbound → 路由规则 → VMess/VLESS outbound（我们 outbound = overlay）
```

### 2.2 与两参考系的对照

| 维度 | Tailscale exit node | v2ray 客户端 | NexOS network-exit（本期） |
|---|---|---|---|
| 出口声明 | `--advertise-exit-node` → 协调服务器 | 配置文件 outbound | digest 自广播 `exit_offered:true`（gossip 全网可学） |
| 授权 | 管理控制台 approve route | UUID/密码 | 出口节点本地授权表 `{NodeID → 过期时刻}`，默认 deny |
| 客户端接入 | 系统路由表 0.0.0.0/0 进 tun | SOCKS5 127.0.0.1:1080 | SOCKS5 127.0.0.1:11081（应用级，不动路由） |
| 传输加密 | WireGuard | VMess/VLESS+TLS | os-p2p 链路 AES-256-GCM（既有） |
| 出口落地 | 内核转发 + MASQUERADE | freedom outbound 直连 | 出口进程本机代拨（用户态 NAT） |
| NAT 后可用 | 是（DERP 中继） | 需公网服务器 | 是（打洞/中继兜底，不依赖公网 IP） |

### 2.3 出口声明发现（os-p2p meta digest 扩展）

- `MetaDigestEntry` / `NodeMetaEntry` 新增 `exit_offered: bool`
  （`#[serde(default)]`——旧节点/旧持久化文件缺字段 → false，向后兼容）。
- 出口节点开启 offer 后，每轮 gossip（每 6 tick）自广播首条带
  `exit_offered:true`；其他节点 `merge_digest` 入库并随自身 digest 转述
  （转述条目透传该位）→ 全网经 1-2 轮 gossip 学到「谁是出口」。
- 入口节点 `GET /api/v1/net-exit/status` 的 `known_exits` 即从
  `node_meta()` 过滤 `exit_offered=true` 得到。
- env：`NEXOS_P2P_EXIT_OFFER=1`（P2pConfig.exit_offered，进程启动默认值；
  运行期以 REST `POST /net-exit/offer` 为准——handler 持久化状态是权威源，
  启动时推送给 p2p）。

### 2.4 overlay 消息协议（`payload.net_exit` 命名空间，与 transfer/fed 并存）

| kind | 方向 | 字段 | 语义 |
|---|---|---|---|
| `open` | 入口→出口 | `conn_id, host, port` | 请求代拨。出口查授权（msg.from 即请求者 NodeID，链路签名验证不可伪造） |
| `opened` | 出口→入口 | `conn_id, ok:true` | 代拨成功（入口回 SOCKS5 应答成功，开始双向泵） |
| `open_failed` | 出口→入口 | `conn_id, reason` | 拒绝（未授权/过期/未 offer/拨不通） |
| `data` | 双向 | `conn_id, seq, bytes(b64)` | 64KiB 分块；收方写本地流后回 ack |
| `ack` | 双向 | `conn_id, seq` | 确认到 seq——发送方据此放行窗口（背压） |
| `close` | 双向 | `conn_id, reason` | 本地流 EOF/错误/超时——双端清理 |

背压：每连接每方向一个 `tokio::sync::Semaphore`（初始 8 permits）；读端发送
一块占一个 permit，收到 ack 归还；permit 耗尽时读端 `acquire_timeout(30s)`
阻塞（暂停读本地 socket），超时视为对端死亡关连接。overlay 是 fire-and-forget
消息，ack 机制防止读端跑在生产端前面把对端消息队列灌爆。

### 2.5 SOCKS5 两端（均只绑 127.0.0.1——不对外暴露）

| 端口（默认） | 位置 | 用途 | 目标代拨方式 |
|---|---|---|---|
| **11081**（`NEXOS_EXIT_ENTRY_SOCKS_PORT`） | 入口节点 | 用户浏览器/应用指向这里 | 经 overlay `open` → 远端出口 |
| **11080**（`NEXOS_EXIT_SOCKS_PORT`） | 出口节点 | 本机 os-p2p ingress 收到远端 open 后本机拨它；本机进程也可直接用 | 直接 `TcpStream::connect` 目标 |

- 两个端口都是**最小 SOCKS5 服务端**：仅 CONNECT、无认证（`05 00` 应答）、
  支持 IPv4/域名/IPv6 三种地址类型；入口端把 CONNECT 翻译成 overlay open，
  出口端直接代拨。
- 出口侧 ingress 用**最小 SOCKS5 客户端**拨 11080（greeting + CONNECT +
  解析应答）——授权检查在 ingress 做（overlay 层，凭 NodeID），SOCKS5 层无
  认证（来源必须是本机——loopback 绑定即保证；远端流量只能走 overlay 消息
  进 ingress 过授权，这是对远端节点的唯一入口）。
- 浏览器/系统代理配置一句话：HTTP/SOCKS 代理填 `127.0.0.1:11081`（SOCKS5）。

### 2.6 授权模型（默认 deny）

- 出口节点持久化 `/tank/os-data/net-exit.json`（env `NEXOS_EXIT_STATE`）：
  `{offered:bool, authorizations:[{node_id, granted_at, expires_at}], default_exit:Option<node_id>}`。
- `authorize {node_id, ttl_min}`：授权/续期（expires_at = now + ttl）；
  `DELETE /net-exit/authorize/:node_id`：撤销。
- ingress 收到 `open` 时三查：本节点 `offered`、`authorizations` 命中
  `node_id`、未过期——任一不满足即 `open_failed{reason:"unauthorized"}`。
- NodeID 不可伪造（握手挑战-签名），授权表即身份凭证——等价 v2ray 的 UUID
  用户鉴权但更强（无需分发秘密，公钥即身份）。

---

## 三、REST 端点（component `network-exit`）

| method | path | 鉴权 | 动作 |
|--------|------|------|------|
| GET | `/api/v1/net-exit/status` | 公开 | `{offered, exit_for:[已授权未过期 node_id], authorizations:[…], known_exits:[{node_id,last_seen,alive}], default_exit, local_socks:"127.0.0.1:11081", p2p_node_id}` |
| POST | `/api/v1/net-exit/offer` | admin | `{enabled}` 切换本节点出口声明（持久化 + 推送 p2p digest） |
| POST | `/api/v1/net-exit/authorize` | admin | `{node_id, ttl_min}` 授权某节点经本节点出网（默认 deny，逐节点 TTL） |
| DELETE | `/api/v1/net-exit/authorize/:node_id` | admin | 撤销授权 |
| POST | `/api/v1/net-exit/use` | admin | `{exit_node_id}` 设默认出口（None 清除） |
| POST | `/api/v1/net-exit/proxy` | admin | `{host, port, exit_node_id?}` 经默认/指定出口探活一次（open→close），返回 `{ok, exit_node, error?}` |
| GET | `/api/v1/firewall/rules` | 公开 | 规则列表（真实数据，空表起步——**无 seed 演示数据**） |
| POST | `/api/v1/firewall/rules` | admin | `{direction, proto, port, source, action, note?, force?}` 添加（deny + in + tcp/any + 22 且 source=any → 400 除非 `force:true`） |
| POST | `/api/v1/firewall/rules/:id/toggle` | admin | `{enabled}` 启/停规则 |
| DELETE | `/api/v1/firewall/rules/:id` | admin | 删除 |
| POST | `/api/v1/firewall/apply` | admin | 规则 → iptables 自定义链（见下） |
| GET | `/api/v1/firewall/status` | 公开 | `iptables -L NEXOS-FW[-OUT]` 实况回读（含 applied/降级提示） |

未启用（`NEXOS_P2P_ENABLE` 未设）时 **net-exit 端点 503**（防火墙端点不依赖
p2p，照常工作——规则管理与 iptables 落地是本机能力）。

---

## 四、防火墙链设计（iptables）

- **规则模型**：`FirewallRule {id, direction(in/out), proto(tcp/udp/icmp/any),
  port(Option<u16>), source(CIDR 或 any), action(allow/deny), enabled, note}`；
  持久化 `/tank/os-data/firewall.json`（env `NEXOS_FIREWALL_FILE`，原子写）。
  **空表起步**——真实数据原则，无任何预置/演示规则。
- **自定义链，不污染用户规则**：
  - `NEXOS-FW`：挂到 `INPUT`（`iptables -C INPUT -j NEXOS-FW || -I INPUT 1 -j NEXOS-FW`）；
  - `NEXOS-FW-OUT`：挂到 `OUTPUT`（同款守卫）——in/out 规则分链，避免同链
    混挂导致 INPUT 语境的 `--dport` 规则误伤 OUTPUT 流量。
- **apply 顺序（幂等）**：
  1. `iptables -N NEXOS-FW`（已存在则忽略失败）→ `-F NEXOS-FW`（**flush 先行**
     ——旧规则全清，规则集与 JSON 状态一致）；
  2. 同款建/清 `NEXOS-FW-OUT`；
  3. 逐条 `iptables -A NEXOS-FW -p tcp --dport 443 -s 10.0.0.0/8 -j ACCEPT`
     （deny → DROP；out 方向入 NEXOS-FW-OUT；proto=any 省略 `-p`；
     port=None 省略 `--dport`；source=any 省略 `-s`）；
  4. 挂接守卫（`-C` 已在则跳过）。
- **执行**：真实 iptables 经 sudo（`sudo iptables …` 子进程，复用 storage 的
  sudo 模式；sudo/iptables 不可用时**降级** `applied:false + warning`，不 500）。
  测试经 `ExitConfig.ipt_sudo_bin` 注入假 sudo 脚本断言 argv。
- **危险规则防呆**：`deny` + `in` + 端口 22 + `source=any`（把管理口对全网
  关死）后端 400 拒绝，body 带 `force` 提示；`force:true` 才放行。前端 apply
  前对含 deny-22 的规则集弹 confirm。

---

## 五、端口与 env 清单

| env | 默认 | 作用 |
|---|---|---|
| `NEXOS_P2P_ENABLE` | 未设 | os-p2p 组网节点开关（net-exit 依赖；未设 → net-exit 端点 503，防火墙不受影响） |
| `NEXOS_P2P_EXIT_OFFER` | 未设 | P2pConfig.exit_offered 启动默认值（运行期以 /net-exit/offer 持久化状态为准，启动时推送） |
| `NEXOS_EXIT_STATE` | `/tank/os-data/net-exit.json` | 出口状态持久化（offered / 授权表 / 默认出口） |
| `NEXOS_EXIT_SOCKS_PORT` | `11080` | 出口侧本地 SOCKS5（127.0.0.1，本机 ingress 代拨入口） |
| `NEXOS_EXIT_ENTRY_SOCKS_PORT` | `11081` | 入口侧本地 SOCKS5（127.0.0.1，用户应用指向这里） |
| `NEXOS_FIREWALL_FILE` | `/tank/os-data/firewall.json` | 防火墙规则持久化（原子写） |
| `NEXOS_IPT_SUDO` | `sudo` | iptables 前缀程序（测试注入假脚本用） |

## 六、测试

`cargo test -p os-api network_exit`（13 条）+ `cargo test -p os-p2p exit`（2 条）：
SOCKS5 握手/CONNECT 解析（固定字节样本）、SOCKS5 服务端↔客户端回环数据面、
授权表过期/默认 deny、防火墙 CRUD/持久化/危险端口拒绝（force 放行）、iptables
命令组装（链序/flush 先行/jump 守卫）、假 sudo argv 断言、digest exit_offered
广播与合并（含旧格式缺字段兼容）、双节点端到端（offer+authorize+use+proxy 探活
+ 入口 SOCKS5 全流量经 overlay 到 mock 目标）。

## 七、二期路线

1. **默认路由级 exit node**（Tailscale 同款）：tun 设备 + `ip rule` 把
   `0.0.0.0/0` 指进 overlay + 出口节点内核转发 + MASQUERADE；需 DNS 劫持、
   断网自愈（deadman switch：心跳丢失自动回退本机直连）。
2. **UDP 转发**（SOCKS5 UDP ASSOCIATE / tun 级天然支持）与按目标路由规则
   （v2ray 式域名/IP 分流：国内直连、跨域走指定出口）。
3. **计量与配额**：出口节点按 NodeID 统计出网字节数（授权表带 quota）。
