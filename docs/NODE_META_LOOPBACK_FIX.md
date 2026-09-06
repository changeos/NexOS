# 节点元数据组件 —— 回环地址扩散修复交接文档

> 状态：**修复代码为未提交的半成品**（工作区 `git diff` 可见，`cargo check -p os-p2p` 通过），
> 全量测试未跑、未提交、未部署。接手 agent 从「§5 接手步骤」开始。

## 1. 模块是什么

节点元数据组件（os-p2p `crates/os-p2p/src/meta.rs`，约 1400 行含测试）是**全集群唯一的节点存活检测来源**：

```
┌─────────────── 本机 os-p2p 节点 ───────────────┐
│ register_conn ──▶ NodeMetaStore（内存注册表）   │
│   （每次握手）        │  record_conn：建档/更新   │
│                      ▼                         │
│ meta_engine（5s tick，tracked task）            │
│   ├─ 活连接存在 → 直接记心跳成功（+5 分）        │
│   ├─ 无连接 → fingerprint_probe：              │
│   │    TCP connect + 完整握手比对 NodeID        │
│   │    （不 register_conn，不产生连接）          │
│   ├─ 失败 -20 分；连续 5 败 → Inactive（停探）  │
│   └─ 每 6 tick：digest 广播给所有已连节点        │
│         （MetaGossip 帧；合并规则见 meta.rs）    │
│ 持久化：node-meta.json（key 同目录，原子写，     │
│         停机刷盘；NEXOS_P2P_META_FILE 可改）     │
└────────────────────────────────────────────────┘
```

- 分数：成功 +5（封顶 100）、失败 -20（下限 0）、起始 50；探测节奏按分数分级（≥80 每 6 tick、50-79 每 3 tick、<50 每 tick）。
- 复活：仅两种——手动 `POST /api/v1/p2p/node-meta/:id/reactivate`，或他节点 digest 报告 last_seen 新鲜 → Active{score:30}。
- 非活跃 TTL 清除（2026-09-02，见 §10）：Inactive 条目 3 天无心跳（last_seen 距今超 TTL）即整条删除。
- verified 语义：直连/探测匹配 → true；gossip 不洗白；探测指纹不符 → 置回 false + warn。地址带独立验证位（`MetaAddr{addr, verified}`）。
- 相关提交（已在 main）：`e75a0e2`（组件本体）→ `72c5ceb`（指纹验证）→ `66b695e`（os-api 展示层）。

## 2. 要修的问题（根因链，已确诊）

用户报告：节点发现页出现 `127.0.0.1:xxxx` 条目。完整链条：

1. **106 上跑 `cargo test`**（os-api/os-p2p 测试进程 spawn 的临时 P2P 节点）拨了**本机生产端口 7070**（经 127.0.0.1）；
2. 生产 os-api 的 `register_conn` 握手成功（测试节点是真实 NexOS 节点，随机 NodeID），把 `127.0.0.1:33516` 这类**回环观测地址**记入注册表（verified=true）；
3. digest 广播把回环地址扩散到 113 / aliyun / 云锚点——**回环地址只在本机可拨，对其他节点毫无意义**；
4. 测试进程退出 → 节点消失 → 五振出局进 Inactive → 用户在「非活跃节点」列表看到 127.0.0.1。

证据：三台 os-api 节点 + 锚点的注册表都有同两条目（NodeID `0x03b2295b…`/`0x0378ed08…`），first_seen 与 2026-08-24 深夜子代理跑 cargo test 的时间点吻合；106 比其他节点早 17 秒（digest 扩散周期）首次记录。

## 3. 修复规格（原任务书语义）

1. **digest 出口过滤**：`gossip_broadcast` 构造摘要时剔除回环地址（`addr.ip().is_loopback()`）；剔除后无地址的条目整个不发。
2. **merge 入口过滤**：`merge_digest` 不收远端回环地址；远端条目只剩回环地址时——本地无条目则**不新建**，有条目则只更新 last_seen 不动 addrs。
3. **本机 record_conn 保留回环**：同机多实例场景本机可拨，语义有效——只是出不去 digest（第 1 条已挡）。**这个不对称是设计语义，不是遗漏**。
4. **加载清理**：读 node-meta.json 时丢弃「source=gossip 且仅有回环地址」的历史死条目（一次性自愈；Direct 的保留）。
5. （附带排查）测试代码里拨生产端口 7070 的源头——已查 api.rs 集成测试原来用 `127.0.0.1:0` 互拨（不拨 7070，无越界），真正的 7070 拨号源头未定位，见 §6。

## 4. 半成品现状（工作区未提交 diff）

- `crates/os-p2p/src/meta.rs`（+271 行）：过滤逻辑 + 若干新单测（出口剔除/仅回环条目不发/入口不收/加载清理等，**未运行验证**）
- `crates/os-p2p/src/api.rs`（+50/-28 行）：集成测试适配——因出口过滤，跨节点学址测试不能再经 127.0.0.1 互拨（会被过滤，学不到地址），改用 **non_loopback_local_ipv4()**（UDP connect 8.8.8.8 选路技巧拿本机 LAN IP，不真发包）互拨；`spawn_node` 改带 listen 参数
- `cargo check -p os-p2p` **通过**；`cargo test` **未跑**；pre-commit 三道门 **未跑**；**未提交**

## 5. 接手步骤（按序）

1. `cargo test -p os-p2p`——修失败项（时序敏感测试不稳可重跑确认；`Timing::testing()` 已把 meta_tick 压到 150ms）
2. `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings`（pre-commit 会跑，先本地过）
3. 一个 commit：`fix(p2p): 元数据 digest 回环地址出入口过滤——修 127.0.0.1 跨节点扩散`（正文带 §2 根因链），**不要 push**（由主代理推）
4. 部署验证见 §6

## 6. 测试清单（模块整体验收，含本修复）

### 单元/集成（cargo）
```bash
cargo test -p os-p2p          # 基线 79 + 本修复新增（meta 单测 + api 集成）
cargo test -p os-api --lib    # 1105 基线（含 node-meta 端点/reactivate/combined 三段）
```
重点用例名（meta.rs）：出口剔除、仅回环条目不发、入口不收远端回环、加载清理、record_conn 回环照记；指纹验证三态（Verified/Mismatch/Unreachable）既有用例不回归。

### 真机验证（106 主开发机，改完部署重启后）
```bash
# ① 注册表无 127.0.0.1 gossip 条目（历史死条目加载时被清）
curl -s http://127.0.0.1:8558/api/v1/p2p/node-meta | grep -c 127.0.0.1   # 期望 0
# ② combined 三段无 127.0.0.1
curl -s http://127.0.0.1:8558/api/v1/nodes/combined | grep -c 127.0.0.1  # 期望 0
# ③ 心跳/分数运转（113/锚点/aliyun 条目 Active 且 score 上升）
# ④ 五振出局：停一台节点（如 systemctl stop os-api@113 不可行则拔网模拟），
#    等 5×探测周期 → Inactive；reactivate 手动复活：
curl -X POST http://127.0.0.1:8558/api/v1/p2p/node-meta/<node_id>/reactivate
# ⑤ 跨节点复活：113 停机期间从 aliyun 视角仍新鲜（其 digest 报告 last_seen）→ 106 侧应复活
```
验收标准：127.0.0.1 从所有节点的注册表与节点发现页消失且不再复现；分数排名/五振/双路复活行为符合 §1。

### 已知边界（勿误判为 bug）
- 本机直连的回环条目（同机多实例）**会**保留在本地注册表（§3.3 设计语义），但不出现在其他节点。
- fingerprint_probe 到达被探方时，被探方会短暂 accept 后 EOF（探测不 register，入站侧记一次瞬时连接）——既有行为。
- 探测/握手需双向可达；NAT 后节点（无活连接时）可能探不通 → 分数下降 → Inactive，属预期。

## 7. 部署提示（主代理职责，此处备查）

- 106：`cargo build -p os-api && systemctl restart os-api`（web 从磁盘读）
- 113：release 二进制 + web tar 双份 static-dist（`/opt/nexos/static-dist` 与 `/opt/nexos/crates/os-api/static-dist`）
- aliyun（203.0.113.2）：**sshd 当前不可用**（22/221 均拒，待用户阿里云控制台修复）——web 走 Files API（`POST /api/v1/files/upload?path=/opt/nexos/crates/os-api/static-dist`，Bearer `change-me-admin-token`，先 `/files/delete` 整目录防重名加后缀）；**二进制无法更新**，其 digest 旧版本不认 MetaGossip 过滤（无回落问题，仅过滤不生效）
- 云锚点（198.51.100.114:179）：`scp p2p-node → systemctl restart nexos-p2p`

## 8. 环境快查

| 节点 | 地址 | 备注 |
|---|---|---|
| 106 ub2604 | 192.0.2.106 | 主开发机，debug 直跑仓库 |
| 113 node-113 | 192.0.2.113 | oem/<sudo-pass>，/opt/nexos |
| aliyun | 203.0.113.2:8558 | SSH 221 挂了；API token change-me-admin-token |
| 云锚点 | 198.51.100.114:179 | root <redacted>，独立 p2p-node |

## 9. 进入 NexHub 的方式（全部实测可用）

NexHub = 本生态的 git 服务 + 项目大厅，仓库根 `/tank/git-repos/`（主仓 nexos.git）。

| # | 通道 | 用法 | 适用 |
|---|---|---|---|
| ① | **本地裸仓库直连** | `git /tank/git-repos/nexos.git`（仓库内 remote 名 `nexos-local`；`git push nexos-local main`） | 106 本机最常用 |
| ② | **HTTP Smart Git（token 认证）** | `git clone http://oem:change-me-admin-token@ub2604:8558/git/nexos.git`（或 IP 代主机名；Basic 密码=token，也支持 Bearer 头） | 任何机器，跨节点 clone/PR 的主通道 |
| ③ | **SSH 通道** | `git clone ssh://oem@192.0.2.106/tank/git-repos/nexos.git`（首次需接受 host key：`-c strictHostKeyChecking=accept-new`） | 有 SSH key 的机器 |
| ④ | **管理 API** | `GET /api/v1/coderepo/repos`（仓库列表+clone_url_http/ssh）；详见 handlers 里的 coderepo 端点族 | 程序化查询 |
| ⑤ | **大厅 API** | `GET /api/v1/nexhub/lobby`（本地大厅条目：nexos/finalshell-rs 等，含 commit_count/federated 字段）；联邦大厅端点见 os-nexhub | 项目发现/联邦 |
| ⑥ | **Web UI** | 浏览器开 `http://<节点>:8558/` → 桌面「NexHub」图标（路由 /codehub）：浏览/克隆/发布/联邦大厅/PR | 人工操作 |

注意：
- 更新应用（`POST /api/v1/update/check`）的更新源就是 ① 的 nexos.git tag 列表——发版即 tag，stable 通道只认正式 semver tag。
- ② 的 token 即各节点 `NEXOS_ADMIN_TOKEN`（开发期 change-me-admin-token）；aliyun 的 sshd 修复前，其 HTTP git 通道 ②⑥ 正常可用。

## 10. 非活跃条目 TTL 自动清除（2026-09-02）

> 需求（用户原话）：「集群节点里加个规则，非活跃节点，三天不心跳就移除」。
> 背景：Inactive 条目此前**永不过期**——`GET /api/v1/nodes/combined` 的
> inactive 组永久累积（实例：aliyun 重装换代的两个旧 NodeID 躺了半个月）。

规则（实现：`crates/os-p2p/src/meta.rs`，`purge_expired_inactive`）：

- **清除条件**：状态为 `Inactive` 且 `last_seen` 距今 **> TTL**（严格大于）→
  条目从注册表**整条删除**：内存删除 + 置脏走防抖落盘重写 `node-meta.json`
  ——重启后不再出现。
- **TTL 默认 3 天**（259,200 秒）；env `NEXOS_P2P_INACTIVE_TTL_SECS` 覆盖，
  **`0` = 禁用清除**（向后兼容开关）；非法值告警并回落默认。
- **只清 Inactive**：Active 条目永不过期——远古僵尸交评分/五振机制自会处理
  （探败出局后就进了本规则管辖），漏判好过误杀。复活的既有语义天然续命：
  直连重连 / 手动 reactivate / 他节点新鲜 digest 报告三条路都刷新
  `last_seen`，无需特判。
- **扫描节奏**（节流，不逐 tick 扫全表）：心跳引擎每 **300 tick**（默认
  `meta_tick`=5s → 约 25 分钟）扫一次；引擎启动（注册表加载完成）后另有
  **一次即时机扫描**。
- **日志**：清除逐条 `eprintln`（os-api 网关不装 tracing subscriber，
  journald 里只有 eprintln 可见——[os-p2p] 入站审计同款考量）：
  `[os-p2p][meta] 清除非活跃超期条目（259200s 无心跳 TTL）node=0x… inactive=…s unseen=…s`
  （inactive = 距出局时刻 since；unseen = 距最后一次确认存活 last_seen，即清判据）。
- **观察面零改动**：`GET /api/v1/p2p/node-meta` 与 `/api/v1/nodes/combined` 的
  inactive 组自动变短——条目删了自然不在，端点/前端均无改动。

### 首次部署效果（预期行为，勿误判）

存量**超期**条目（如 aliyun 重装换代的旧 NodeID——last_seen 已超 3 天的
Inactive 档案）会在**启动扫描时立即清除**（上述 eprintln 逐条可见），
`node-meta.json` 随之重写；未满 3 天的 Inactive 条目保留，到期后在下一个
扫描窗口（≤25 分钟）清除。回滚方式：`NEXOS_P2P_INACTIVE_TTL_SECS=0` 重启
即恢复旧行为（永不清除）。
