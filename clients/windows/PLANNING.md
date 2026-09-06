# NexOS Windows 客户端规划（PLANNING）

> 读者：在 Windows 机器上独立开发本客户端的 AI agent / 开发者。本文档**完全自包含**：
> 你不需要读 NexOS 主仓的任何代码即可开工；所有协议细节、字段名、环境信息都已从
> 服务端源码逐一核实后写在这里（核实基线：分支 `nexhub-monetization`，2026-08-15）。
> 端点速查表见同目录 [API_QUICKREF.md](./API_QUICKREF.md)。

---

## 1. 项目使命

**NexOS — Connecting the Islands（连接 OS）**：AI 时代人人是超级个体，但个体之间形成
信息孤岛（设备/数据/AI/身份/代码五类）。NexOS 是打破这些孤岛的自托管基础设施——
一台 Ubuntu 机器上跑着 Rust 网关（os-api，端口 8080），聚合 IM、系统监控、代码托管
（NexHub）、模型推理、存储等 28 个桌面应用，470+ API 路由。

**Windows 客户端的定位**：超级个体的**桌面侧出入口**。今天 NexOS 只有一个一等公民
界面——浏览器里的 Web 桌面；孤岛在设备侧，出口也必须在设备侧。Windows 客户端是
"打破设备孤岛"的第一个具体载体：让 Windows 用户不开浏览器就能看服务器仪表盘、
在 IM 大厅实时交流、浏览 NexHub 大厅并一键克隆代码仓库。它是一个 **纯 HTTP/WS
客户端**——服务端零改动、零新依赖，所有能力经现有 REST + WebSocket 契约获得。

---

## 2. MVP 功能范围

从功能调研（docs/FEATURE_SURVEY.md）的 ✅ 完全实现功能中，选 Windows 场景价值最高的三块：

| 模块 | 内容 | 来源（服务端已实现，全部真实） |
|---|---|---|
| A. 系统仪表盘 | CPU/内存/磁盘/网络磁贴 + 主机信息 + 服务/ZFS 池状态，周期刷新 | `/api/v1/monitor/*`（真实读 /proc）、`/status` |
| B. IM 大厅+会话 | 大厅公共频道（默认视图）+ 对话/群组收发，WebSocket 实时推送，在线成员列表 | `/api/v1/im/*`（SQLite 持久化）+ WS `/ws` |
| C. NexHub 浏览/克隆 | 大厅列表/搜索/排序/详情（含付费字段）、免费一键克隆、付费条目购买后克隆 | `/api/v1/nexhub/lobby/*`（SQLite + 系统 git） |

### OUT OF SCOPE（明确不做）

- **不改任何服务端代码**。客户端只消费 HTTP/WS；发现服务端问题提 issue（提交到本仓
  `clients/windows/` 内的笔记文件），不要顺手改 `crates/`。
- **不做管理功能**：不发布仓库（publish）、不下架、不做悬赏（bounty）流转、不建用户、
  不动虚拟机/存储/备份等 27 个其他应用的端点。
- 不做联邦/BLE/mTLS（服务端这些能力尚未接线，见 FEATURE_SURVEY §1.1②）。

---

## 3. API 契约

### 3.0 基础约定

- **Base URL**：`http://<服务器>:8080`（当前测试服务器 `192.0.2.106:8080`，主机名
  `ub2604`，两写法等价）。所有路径以 `/api/v1/` 开头（健康检查例外）。
- **认证**：`Authorization: Bearer <token>`。服务端把 bearer 与 `NEXOS_ADMIN_TOKEN`
  环境变量**精确匹配**即注入 admin Principal（当前测试 token：`change-me-admin-token`）；
  匹配失败回退尝试 JWT（客户端无需关心）。路由表里 `requires_auth=true` 的端点
  （本 MVP 中所有 POST/DELETE 写操作）缺 token 或 token 错 → **401**，body
  `{"error":"未认证"}`；有身份但角色不够 → **403** `{"error":"权限不足"}`。
  只读 GET 与 WS 握手不需要 token。
- **响应包络（照实描述，无 data 包装）**：成功时 HTTP body 就是业务 JSON 本身——
  列表端点直接返回 **JSON 数组**，单对象端点直接返回对象；`Content-Type: application/json`。
  失败时 body 恒为 `{"error": "<中文错误消息>"}`。没有 `{code,data}` 之类的统一信封，
  解析时按"HTTP 状态码 + error 字段"判断即可（前端 client.ts 同款做法：非 2xx 抛错，
  错误消息取 `body.error || body.message`）。
- **错误码约定**（全部为 HTTP 状态码 + `{"error": msg}`，已核实的有）：
  - `400` 参数非法（空内容、非法仓库名、免费条目购买、非法货币组合）
  - `401` 未认证（需 Bearer token 的端点缺失/错误）
  - `402` **付费门禁**：克隆付费条目但无授权，或购买收据金额/货币校验失败
  - `403` 权限不足；或大厅发言但用户尚未加入大厅（先 `GET /im/lobby?user=` 自动加入）
  - `404` 资源不存在（对话/群组/条目/仓库）
  - `500` 服务端内部错误；`502` 服务端 spawn `git clone` 失败；`503` git 通道未配 token
  - 另有领域错误码枚举 `os_common::ApiErrorCode`（not_found/conflict/rate_limited…，
    snake_case），用于 WS error 帧；REST 层目前不输出该结构，客户端不必处理。
- **时间格式**：RFC3339 带本地时区（如 `2026-08-15T10:30:00+08:00`）。
- **分页/排序惯例**：本 MVP 端点**无统一分页参数**。列表全量返回；大厅消息固定最近
  50 条；查询参数风格为 `?q=`（搜索）、`?tag=`、`?sort=recent|downloads`、`?user=`。
  不要发明 `page/limit` 参数——服务端不认。

### 3.1 模块 A：系统仪表盘端点

**GET /api/v1/monitor/metrics**（免认证）— 一发拿全 CPU/内存/磁盘/网络磁贴数据：

```json
{
  "hostname": "ub2604",
  "uptime_secs": 86400,
  "load_avg": [0.52, 0.41, 0.35],
  "cpu_usage": 23.5,
  "cpu_cores": 8,
  "mem_total_bytes": 33550532608,
  "mem_used_bytes": 11270000000,
  "mem_available_bytes": 18000000000,
  "swap_total_bytes": 0,
  "swap_used_bytes": 0,
  "disk_total_bytes": 1000204886016,
  "disk_used_bytes": 402300000000,
  "net_rx_bytes": 12345678901,
  "net_tx_bytes": 9876543210,
  "processes": 312,
  "kernel_version": "6.14.0-13-generic"
}
```

注意：`cpu_usage` 是两次 `/proc/stat` 采样差值，**至少间隔 1s 轮询**才有意义
（建议 2~5s）；`net_rx/tx_bytes` 是累计值，做速率图需客户端自行差分。

**GET /api/v1/monitor/stats**（免认证）— 一次聚合摘要（比率已算好）：
`{cpu_usage, cpu_cores, mem_used_ratio, disk_used_ratio, load_avg_1, uptime_secs, processes, alerts_total, alerts_unacked, zpools_total, zpools_healthy, hostname}`

**GET /api/v1/monitor/services**（免认证）— `[{name, status: "running|stopped|unknown", pid}]`
（探测 os-api/osd/sshd/zfs 进程）

**GET /api/v1/monitor/zpools**（免认证）— `[{name, state: "ONLINE|DEGRADED|OFFLINE|UNKNOWN", size_bytes, allocated_bytes, free_bytes, healthy}]`（真实 `zpool list`，未装 ZFS 时可能返回示例行）

**GET /status**（免认证）— `{hostname, version, capacity: {used_bytes, total_bytes}, health, node_count, cpu_virt, uptime}`（capacity 目前是占位 0，别当真）

**GET /healthz**（免认证）— `{"status":"ok"}`，连通性探针首选。
**GET /api/v1/version** — `{"name":"os-api","version":"0.1.0"}`。

（可选加分）**GET /api/v1/monitor/alerts** 免认证返回最近 100 条告警
`[{id, level: "info|warning|critical", message, source, timestamp, acked}]`；
`POST /api/v1/monitor/alerts/:id/ack`（需 admin）body `{}`。

### 3.2 模块 B：IM 大厅 + 会话

服务端模型：对话（conversation）与群组（group）都能收发消息（群组 id 即
conversation_id）；**大厅（lobby）是固定公共频道**，id 恒为 `"lobby"`，全员自动加入、
不可退出。在线判定：`last_seen` 距今 < **60s**。前端心跳间隔 **30s**（HTTP 层，非 WS）。

消息 DTO（所有消息端点共用，字段名与前端 `ImMessage` 对齐）：

```json
{
  "id": "0f1e2d3c-…-uuid v4",
  "conversation_id": "lobby",
  "sender_id": "me",
  "sender_name": "我的显示名",
  "content": "文本内容",
  "msg_type": "text",
  "file_url": null,
  "reply_to": null,
  "created_at": "2026-08-15T10:30:00+08:00",
  "read_by": ["me"]
}
```

`msg_type` ∈ `text | file | image | system`；`sender_id == "system"` 为系统消息
（欢迎广播等），UI 应居中灰显。

#### 3.2.1 大厅端点（4 条）

- **GET /api/v1/im/lobby?user=<我的id>**（免认证）→ 大厅信息 `{id:"lobby", name:"大厅",
  member_count, online_count, last_message}`。**带 `?user=` 即心跳**：刷新 last_seen；
  首次出现的用户自动加入大厅并触发一条欢迎系统消息全员广播。**这是加入大厅的唯一方式**。
- **GET /api/v1/im/lobby/messages?user=<我的id>**（免认证）→ 最近 50 条消息数组
  （时间正序）。`?user=` 同样计心跳。
- **POST /api/v1/im/lobby/messages**（需认证）body：
  `{"user_id": "win-agent", "content": "大家好", "sender_name": "Windows 客户端"}`
  → 201 + 完整消息对象。user_id 缺省 `"me"`。**前置条件**：该 user_id 必须已是大厅
  成员（即先调过一次 GET /lobby?user=），否则 **403**
  `{"error":"用户 xxx 尚未加入大厅（先 GET /api/v1/im/lobby 自动加入）"}`；空内容 → 400。
  发送成功服务端自动向所有 WS 连接广播。
- **GET /api/v1/im/lobby/members**（免认证）→
  `{lobby_id:"lobby", member_count, online_count, members: [{user_id, display_name, last_seen, joined_at, online}]}`。
  在线列表刷新时机：每次大厅心跳后 + 每次收到 `im_lobby_message` 广播后（可能来了新成员）。

#### 3.2.2 会话/群组端点（核心 6 条，另有可选）

- **GET /api/v1/im/groups**（免认证）→ 群组数组
  `[{id, name, owner, kind: "group", members: [id…], last_activity, created_at}]`。
- **POST /api/v1/im/groups**（需认证）body `{"name": "dev-team", "members": ["win-agent"]}`
  → 201 + 群组对象（owner 缺省取 members[0]，否则 `"admin"`）。
- **GET /api/v1/im/conversations**（免认证）→ 对话数组 `[{id, name, is_group, created_by, created_at}]`。
- **POST /api/v1/im/conversations**（需认证）body `{"name": "私聊", "created_by": "win-agent"}` → 201。
- **GET /api/v1/im/conversations/:id/messages**（免认证）→ 该对话全部消息（时间正序）。
  `:id` 可以是 conversation id 也可以是 group id。不存在时**返回空数组而非 404**。
- **POST /api/v1/im/conversations/:id/messages**（需认证）body：
  `{"content": "必填", "sender_id": "win-agent", "sender_name": "Windows 客户端",
  "msg_type": "text", "file_url": null, "reply_to": null}`
  → 201 + 消息对象（sender_id 缺省 `"me"`；对话不存在 → 404）。发送成功服务端向所有
  WS 连接广播 `im_message`。

可选：`POST /api/v1/im/groups/:id/join`（认证，body `{"member": "win-agent"}`）、
`POST /api/v1/im/messages/:id/read`（认证 **且需 admin 角色**，body `{"user_id"}`）、
`GET /api/v1/im/conversations/:id/unread?user=`（→ `{conversation_id, user, unread}`）、
`GET /api/v1/im/search?q=`（→ `{query, count, results: [消息]}`）、
`GET /api/v1/im/status`（→ `{ready, conversations, groups, peers, messages}`）、
`GET|POST /api/v1/im/peers`（Federation 节点记录，目前只记 IP 不真连，可不做）。

#### 3.2.3 WebSocket 实时通道（复刻此协议，全部已核实）

- **URL**：`ws://<host>:8080/ws?user=<我的用户id>`（query 参数 `user` 标识身份，
  缺省 `"anonymous"`；**握手不需要 Authorization**，不走 Bearer）。
  HTTPS 环境对应 `wss://`（当前测试环境是明文 `ws://`）。
- **方向**：**服务端只推、不收**。客户端发的任何文本帧都会被服务端忽略（仅 Close 帧
  触发断开清理）；发消息走 HTTP POST，服务端落库后广播给所有订阅连接。
  **没有 WS 层心跳/ping 协议**——保活靠 HTTP 大厅心跳（见 3.2.1），WS 断线重连即可
  （Web 前端做法：onclose 后 5s 重连，建议复刻并加指数退避）。
- **消息格式**：每帧一个 JSON 文本帧，`#[serde(tag="type", snake_case)]`，MVP 需处理
  两种 IM 事件，其余类型忽略即可：

```json
{"type": "im_message", "conversation_id": "group-dev-team", "message": { …完整消息 DTO… }}
{"type": "im_lobby_message", "lobby_id": "lobby", "message": { …完整消息DTO，conversation_id 恒为 "lobby"… }}
```

  分发规则（与 Chat.vue 一致）：`im_message` 且 `conversation_id` 是当前打开的会话 →
  追加显示（按 `message.id` 去重，防止与 POST 响应重复）；是其他会话 → 未读数 +1；
  `im_lobby_message` → 追加到大厅视图并刷新在线成员列表（欢迎系统广播意味着新成员）。
  其他可能出现的 type（可安全忽略）：`event` / `progress` / `notification` /
  `error`（结构分别为 `{type,event}` / `{type,task_id,progress,step}` /
  `{type,message,severity}` / `{type,code,message}`）。
- **客户端节奏**（复刻 Web 前端）：进 IM 页 → 并发 `GET /lobby?user=` +
  `GET /lobby/messages?user=` + `GET /lobby/members` → 连 WS → 启动 **30s** HTTP 心跳
  （60s 在线窗口的一半），心跳即 `GET /lobby?user=` + `GET /lobby/members`，失败静默。

### 3.3 模块 C：NexHub 大厅浏览 + 一键克隆

大厅条目 `LobbyEntry`（列表/详情共用；**含付费字段**）：

```json
{
  "repo_name": "nexos",
  "description": "NexOS — 连接 OS",
  "tags": ["rust", "os"],
  "publisher": "NexOS",
  "source_url": "/tank/git-repos/nexos.git",
  "homepage_node": "local",
  "commit_count": 618,
  "size_bytes": 123456789,
  "default_branch": "main",
  "last_commit": "29d10c6 - docs: 全功能调研",
  "last_commit_date": "2026-08-15T09:00:00+08:00",
  "readme_excerpt": "# NexOS …（前 500 字符）",
  "download_count": 3,
  "published_at": "2026-08-15T08:00:00+08:00",
  "price_sats": 0,
  "currency": "free"
}
```

**付费语义**：`price_sats == 0` 且 `currency == "free"` 为免费；`price_sats > 0` 时
`currency` ∈ `btc | nex | usdc | eth`（最小货币单位，BTC 即聪）。付费条目克隆前必须
purchase 取得授权（publisher 本人豁免）。

- **GET /api/v1/nexhub/lobby?q=<搜索>&tag=<标签>&sort=recent|downloads**（免认证）
  → **裸数组** `LobbyEntry[]`。q 匹配名称/描述；sort 缺省 `recent`（按发布时间倒序）。
- **GET /api/v1/nexhub/lobby/stats**（免认证）→
  `{published_count, total_downloads, top_tags: [{tag, count}]}`（最多 10 个标签）。
- **GET /api/v1/nexhub/lobby/:name**（免认证）→ 条目对象 + 两个附加字段
  `clone_url_ssh`（`ssh://oem@<host>:/tank/git-repos/<name>.git`，客户端不用）和
  `clone_url_http`（`http://<host>:8080/git/<name>.git`，**Windows 侧 git clone 用这个**，
  认证方式见 §7）。不存在 → 404。
- **POST /api/v1/nexhub/lobby/:name/clone**（需认证 admin）body `{}`（免费条目）或
  `{"buyer": "<我的id>"}`（付费条目必须带，用于查授权）→ 200：
  ```json
  {"ok": true, "name": "nexos", "cloned": true,
   "source_url": "/tank/git-repos/nexos.git",
   "local_path": "/tank/git-repos/nexos.git",
   "download_count": 4,
   "clone_url_ssh": "ssh://…", "clone_url_http": "http://…:8080/git/nexos.git"}
  ```
  这是**服务端克隆**（落到服务器的 /tank/git-repos，不是用户的 Windows 磁盘）；
  `cloned:false` 表示目标已存在只是计数。付费无授权 → **402**
  `{"error":"该条目为付费内容（1000 btc），请先 POST …/purchase 取得授权"}`。
  服务端 spawn git 失败 → 502。
- **POST /api/v1/nexhub/lobby/:name/purchase**（需认证）body：
  `{"buyer": "win-agent", "txid": "<链上交易id或收据指纹>", "chain": "btc",
  "amount_sats": 1000, "currency": "btc"}`
  → 200 `{ok: true, repo_name, buyer, chain, txid, amount_sats, currency, paid_at, note}`。
  免费条目 → 400；货币不符/金额不足/txid 空 → **402**（一期自证收据：txid 非空即通过，
  二期上链验真）。购买后 clone 带**相同 buyer** 即放行。
- **GET /api/v1/nexhub/lobby/entitlements?repo=&buyer=**（需认证，二者可组合，都不带=
  全量）→ 授权记录数组 `[{repo_name, buyer, chain, txid, amount_sats, currency, paid_at}]`。

客户端的"克隆到 Windows 本机"流程（推荐）：从详情拿 `clone_url_http` → 本机
`git clone http://<任意用户名>:<token>@<host>:8080/git/<name>.git`（HTTP Basic，密码=
admin token；或 `http://<token>@…` Bearer 形式）。付费条目还需先 purchase，clone 到
本机走的是 git 通道，与服务端 402 门禁是两条路——服务端门禁管的是 `/clone` 端点，
git Smart HTTP 对**已存在的裸仓库**直接放行（知道地址即可拉）。UI 上如实呈现这一点。

---

## 4. 技术选型（建议，最终决策权在你）

**首选：Tauri 2 + Rust（+ 任意 Web 前端）**
优点：与主仓同语言（Rust），社区/心智统一，未来代码可互通；UI 用系统 WebView2
（Win10 1803+ 预装），安装包 ~5-10MB、内存占用小——契合 NexOS"轻量网关单体"的
架构品味；tokio-tungstenite/reqwest 的 WS/HTTP 客户端生态成熟，serde 直接复刻本契约。
缺点：Rust GUI 学习曲线；WebView2 版本随系统更新存在渲染差异；调试 DOM 需前端工具链。

**备选 1：C# / WinUI 3（或 WPF）**
优点：最原生的 Windows 集成——托盘、开机自启、Toast 通知、MSIX 打包都是一等公民；
开发效率高。缺点：与主仓技术栈脱节，无法复用任何 Rust 资产；WinUI 3 工具链在
非 Windows 构建机上不可用；团队（如果以后有）需要双语言维护。

**备选 2：Electron**
优点：Web 技术直接搬，生态最全，开发最快。缺点：自带 Chromium，包体 80MB+、
常驻内存 150MB+——为一个"桌面侧出入口"付出这个代价与 NexOS 的轻量哲学相悖；
不推荐，除非你只在乎交付速度。

三种方案对本 MVP 的功能实现没有任何差异（就是 HTTP + WS 客户端）；选型只影响
分发体积、系统集成深度和维护心智。若选 Tauri，托盘/自启/通知分别用
`tray-icon` / 注册表 Run 键 / `tauri-plugin-notification`。

---

## 5. 里程碑（验收标准全部可机器验证）

**M1 连通与仪表盘**
功能：设置页（服务器地址 + token，持久化本地）；健康检查；仪表盘四磁贴
（CPU%/内存比/磁盘比/网速）+ 主机信息条 + 服务/ZFS 池列表。
验收：① 启动 App 输入 `192.0.2.106:8080` + `change-me-admin-token` 后，`GET /healthz`
返回 200 且仪表盘磁贴 30s 内至少刷新 2 次（CPU 值有变化）；② 故意填错 token 后
点"测试连接"，得到 HTTP 401 且 App 弹提示；③ 断网重连后磁贴自动恢复刷新。

**M2 IM 实时**
功能：大厅默认视图（消息流 + 在线成员头像条 + 在线计数）、发言框；会话/群组列表
+ 打开会话收发；WS 实时推送 + 未读徽章。
验收：① App 首次进入大厅触发自动加入（Web 端 Chat 页能看到"欢迎 win-agent 加入"
系统消息）；② 在 Web 端大厅发一条消息，App **≤2s** 内收到并显示（WS 广播）；
③ App 发言后 Web 端 ≤2s 收到；④ 在线列表 30s 心跳后包含 App 用户且 `online:true`；
⑤ 杀掉网络 10s 再恢复，WS 自动重连且补拉 `GET /lobby/messages` 无重复消息（按 id 去重）。

**M3 NexHub 浏览/克隆**
功能：大厅列表页（卡片：名称/描述/标签/下载数/价格徽章）、搜索框（q）+ 排序
（recent/downloads）、详情页（readme_excerpt、commit_count、双 clone 地址）、
"一键克隆"按钮（服务端 clone）+ "复制本机 clone 命令"、付费条目 purchase 流程。
验收：① 列表加载且 `GET /stats` 数字与列表一致；② 搜索 `nex` 过滤生效；
③ 免费条目点克隆 → 200 且 `download_count` +1（对比克隆前后列表值）；④ 对一个
`price_sats>0` 的条目（可用 publish 造一个，或让服务端同学造）直接 clone → 收到
402 且 UI 引导 purchase；purchase 后带相同 buyer clone → 200；⑤ 详情页
`clone_url_http` 可被复制，本机 `git clone` 成功。

**M4 打磨**
功能：系统托盘（最小化到托盘 + 未读角标）、开机自启、新消息 Toast 通知、
断线状态栏指示、多服务器配置。
验收：① 关闭窗口后进程存活于托盘，收到大厅消息时弹 Toast 且托盘角标 +1；
② 重启 Windows 后 App 自启（检查注册表 `HKCU\...\Run` 或启动文件夹）；
③ 拔网线 5s 内状态栏显示"离线"，恢复后自动回到"在线"。

---

## 6. 开发工作流

- **获取代码**：
  `git clone http://<任意用户名>:change-me-admin-token@192.0.2.106:8080/git/nexos.git`
  （主机名 `ub2604` 与 IP 等价；这就是 NexHub 自举——仓库由它自己的 git 服务托管）。
- **分支约定**：`feature/windows-<主题>`（如 `feature/windows-m1-dashboard`）。
- **改动范围铁律**：**只改 `clients/windows/` 目录**。不要动 `crates/`、`docs/`、
  `web/`——那是服务端与其他协作者（含在途分支 nexhub-monetization）的领地。
- **提交规范**（对齐主仓惯例 Conventional Commits）：
  `feat(windows): 大厅消息流` / `fix(windows): WS 重连丢消息` / `docs(windows): …`。
  小步提交，每个里程碑完成后打 tag `windows-m1`、`windows-m2`…。
- **push 回 NexHub**：`git push origin feature/windows-xxx`（origin 即你的 clone 来源，
  push 用与 pull 相同的 token 认证）。服务端 git Smart HTTP 收 push 需 token，已具备。
- **CI/测试**：本目录不参与 Rust workspace 构建（主仓 `Cargo.toml` members 未含
  `clients/`，**不要加进去**）。只要你没动 `clients/windows/` 之外的目录，就无需跑
  主仓的 `cargo test`；在你的目录里自带最小构建脚本/说明即可。
- **与主仓的目录关系**：`clients/windows/` 是 workspace 之外的独立子项目，独立
  工具链（Tauri/C#/Electron 自带），服务端契约以本文档为准；若服务端 API 将来变更，
  以主仓 `crates/os-api/web/src/api/client.ts` 与 handler 源码为最终权威。

---

## 7. 服务端环境

| 项 | 值 |
|---|---|
| 地址 | `http://192.0.2.106:8080`（= `http://ub2604:8080`，局域网） |
| 测试 token | `change-me-admin-token`（= `NEXOS_ADMIN_TOKEN`，git push/pull 密码同此；**生产必须换强 token**） |
| 可用性 | 服务 24h 在线（宿主机 systemd 拉起的 os-api 常驻进程），无需自己起服务端 |
| 协议 | 明文 HTTP / WS（局域网信任环境）。**勿在公网裸奔**：NexHub 设计文档 §6.1 结论——大厅一旦"公开+付费"，明文 `http://:8080` 会让 token 与支付凭据被嗅探/重放，公网部署必须前置 Caddy 类反代做 TLS 终止。Windows 客户端按"可配置 baseUrl"设计，将来切 `https://` 只改配置 |
| 已有数据 | IM 库有 demo 会话/群组与大厅欢迎消息；NexHub 大厅已有 `nexos` 条目（publisher: NexOS）；另有 `token-test2` 空仓库 |
| 服务端版本 | os-api 0.1.0（`GET /api/v1/version` 可查） |

---

## 8. 验收自测清单（curl + 手动步骤）

以下命令在任意能连通服务器的机器执行（PowerShell 用 `curl.exe`）：

```bash
BASE=http://192.0.2.106:8080
TOK=change-me-admin-token

# 1. 连通性（应返回 {"status":"ok"}）
curl $BASE/healthz

# 2. 认证语义：无 token 的写操作 → 401 {"error":"未认证"}
curl -X POST $BASE/api/v1/im/lobby/messages -H 'Content-Type: application/json' \
     -d '{"user_id":"probe","content":"hi"}'

# 3. 带 token：加入大厅（首次会触发欢迎广播）→ online_count ≥1
curl "$BASE/api/v1/im/lobby?user=win-probe"

# 4. 大厅最近 50 条消息（数组）
curl "$BASE/api/v1/im/lobby/messages?user=win-probe"

# 5. 发大厅消息 → 201 + 消息对象（此时 Web 端 Chat 大厅应实时出现该消息）
curl -X POST $BASE/api/v1/im/lobby/messages \
     -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
     -d '{"user_id":"win-probe","content":"来自 Windows 自测","sender_name":"WinProbe"}'

# 6. 未加入大厅的用户直接发言 → 403
curl -X POST $BASE/api/v1/im/lobby/messages \
     -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' \
     -d '{"user_id":"never-joined-xyz","content":"x"}'

# 7. 仪表盘：指标 + 摘要（两次调用间隔≥1s，cpu_usage 才有意义）
curl $BASE/api/v1/monitor/metrics; sleep 2; curl $BASE/api/v1/monitor/metrics
curl $BASE/api/v1/monitor/stats

# 8. NexHub：列表 / 搜索 / 详情 / 统计
curl "$BASE/api/v1/nexhub/lobby"
curl "$BASE/api/v1/nexhub/lobby?q=nex&sort=downloads"
curl "$BASE/api/v1/nexhub/lobby/nexos"        # 含 clone_url_http 与 price_sats/currency
curl "$BASE/api/v1/nexhub/lobby/stats"

# 9. 一键克隆（服务端克隆，download_count+1）
curl -X POST $BASE/api/v1/nexhub/lobby/nexos/clone -H "Authorization: Bearer $TOK" -d '{}'

# 10. WS 大厅广播：开两个终端，A 持续监听，B 发消息，A 应在 1~2s 内看到 im_lobby_message 帧
#    A: curl -N --no-buffer "$BASE/ws?user=win-probe"
#    B: 重跑第 5 步

# 11. git 通道（Windows 本机克隆验证）
git clone http://oem:change-me-admin-token@192.0.2.106:8080/git/token-test2.git
```

全部通过即可认为 M1~M3 的服务端契约侧无障碍；剩下的是客户端 UI 与工程质量。
