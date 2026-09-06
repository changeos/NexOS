# API 大厅（推理服务市场）设计与实施

> 状态：已实施（组件 `api-market`，`crates/os-api/src/handlers/api_market.rs`）·
> 文档用途：架构决策存档 + PPT 素材 + AI agent 接续开发依据
> 日期：2026-08-21 · 用户定稿要点：**发布者身份=区块链公钥（唯一通道，无 admin
> 回落）**、价格排序、详细服务器配置、负载监控输出
> 2026-08-31 增量：**联邦大厅**（fed kind `api_market_lobby`，两步联邦 + 幂等
> 合并，§9）与**条目接入信息** `access_info`（消费者凭据 + 视角脱敏，§4a）

## 1. 需求（用户定稿拆解）

- 把本机/本节点对外提供的**推理服务端点**（LLM chat/completions、生图等）挂牌成
  「商品」——一个**API 大厅 / 推理服务市场**
- 挂牌信息必须含：**价格**（免费 / 按 token / 按图）、**详细服务器配置**
  （GPU 型号显存 / CPU 核数 / 内存 / 模型名 / 上下文长度 / 量化 / 区域）、
  **实时负载**（运行/排队/缓存占用/吞吐/时延）
- 发布者身份 = **区块链公钥**（挑战-签名验真，唯一通道，明确不回落 admin）
- 消费者在市场页：**按价格排序**（付费升序、免费垫底）、搜索、看配置、看负载，
  然后拿 `endpoint_url` 直连消费（消费计费走 api_gateway 的 sk-os- 令牌，本批
  不做调用闭环）

对标：OpenRouter 的模型市场页 / One API 的渠道列表——但身份层是 NexOS 的链上
身份（与 IM / NexHub 同一套密钥）。

> PPT 表述：**"推理算力是商品。NexOS API 大厅用链上身份给商品签名（谁在卖），
> 用硬件探测给商品画像（在什么机器上卖），用心跳给商品标温（现在好不好用），
> 用价格给商品排序（值不值得买）。"**

## 2. 数据流拓扑（发布者 → 挂牌 → 消费者）

```
┌─────────────────────── 发布节点（os-api 8080）───────────────────────┐
│                                                                      │
│  发布者（持 secp256k1 私钥）                                          │
│   ① POST /api/v1/nexhub/auth/challenge {pubkey} ──▶ nonce（60s）     │
│   ② 私钥对 nonce 签名（65B r||s||v）                                  │
│   ③ POST /api/v1/nexhub/auth/verify ──▶ token（24h）                 │
│        │（api-market 与 nexhub-lobby 共享同一 ChainAuth 实例，        │
│         │ token 互通——api-market 自身无 auth 端点）                   │
│        ▼                                                             │
│  ④ POST /api/v1/api-market/publish（Bearer token）                   │
│        │  publisher_pubkey = token 反查（body 无自报通道，不可伪造）   │
│        │  server_config = 本地硬件探测 + body 覆盖                    │
│        │    ├─ nvidia-smi --query-gpu=index,name,memory.total         │
│        │    │    → gpus[]（逐卡）+ gpu_count（无卡=空+0 不阻塞）      │
│        │    ├─ /proc/cpuinfo → cpu_model（首个 model name）           │
│        │    │                + cpu_cores（processor 行计数）          │
│        │    └─ /proc/meminfo → ram_gb（MemTotal KiB→GiB 一位小数）    │
│        │    （body 字段 > 探测值；model_name 探测不到必填，缺 400）    │
│        ▼                                                             │
│  SQLite api_market 表（WAL，/tank/os-data/api_market.db              │
│    → /var/lib/os/api_market.db → ./api_market.db）                   │
│        ▲ heartbeat_at/load          ▲ 挂牌行                          │
│        │                             │                               │
│  ⑤ 活节点定期 POST /:id/heartbeat    │                               │
│     {running_req, waiting_req,       │                               │
│      gpu_cache_usage, tokens_per_sec,│                               │
│      latency_ms, load_pct}           │                               │
└──────┼──────────────────────────────┼───────────────────────────────┘
       │                              │
       │ （无新鲜心跳时）              │ 公开只读
       ▼                              ▼
┌─────────────────────── 消费者（市场页 / 任意客户端）──────────────────┐
│  GET /api/v1/api-market?q=&sort=recent|price                         │
│    └─ 价格排序：付费单价升序在前，免费垫底（价格排名基础）             │
│  GET /api/v1/api-market/:id          （详情 + 心跳新鲜度）            │
│  GET /api/v1/api-market/:id/metrics  （负载监控输出）                 │
│    ├─ 有新鲜心跳（≤60s）→ 直接返回心跳数据（零外呼，stale:false）      │
│    ├─ 无新鲜心跳但挂了 metrics_url → 服务端代拉（reqwest 5s 超时，     │
│    │   {metrics:{...}} vllm 约定键规范化）reachable:true / stale:true │
│    └─ 拉不到 → reachable:false 降级（附最后一次心跳数据，诚实不造假）  │
│                                                                      │
│  选定商品后拿 endpoint_url 直连（如                                   │
│  http://host:8080/api/v1/gateway/v1/chat/completions，OpenAI 兼容，   │
│  消费计费走 api_gateway sk-os- 令牌——调用闭环见 §8 TODO）             │
└──────────────────────────────────────────────────────────────────────┘
```

**身份拓扑**：`/api/v1/nexhub/auth/challenge|verify`（公开）签发 token →
api-market 写端点 `Authorization: Bearer <token>` → 服务端 `verify_token` 反查
pubkey → `publisher_pubkey`（归因）+ `publisher_display`（EVM 地址 =
`keccak256(未压缩公钥[1..])[12..]`）。**无 admin 回落**：`NEXOS_ADMIN_TOKEN`
在 api-market 一文不值（发布/下架/心跳全 401/403）——市场里的卖家必须是能被
签名验证的身份，平台侧不能代发。

## 3. 鉴权矩阵（用户定稿：公钥唯一通道）

| 操作 | 链上 token（owner pubkey） | 他人链上 token | admin token | 匿名 |
|------|---------------------------|----------------|-------------|------|
| POST /publish | ✅ publisher=pubkey | ✅（各自独立条目）| ❌ 401 | ❌ 401 |
| DELETE /:id | ✅ 下架 | ❌ 403「仅发布者可下架」| ❌ 401 | ❌ 401 |
| POST /:id/heartbeat | ✅ 更新心跳 | ❌ 403「仅发布者可上报心跳」| ❌ 401 | ❌ 401 |
| GET /（列表）/ /:id / /:id/metrics | ✅ | ✅ | ✅ | ✅（公开）|

- 401 文案引导：`需要 Authorization: Bearer <链上 token>（先 POST
  /api/v1/nexhub/auth/challenge + /auth/verify 签发；api-market 发布者身份=
  区块链公钥，不接受 admin token 回落）`
- 与 nexhub-lobby 的差异（刻意）：nexhub 写端点回落系统 admin（平台托管语义）；
  api-market **不回落**（用户定稿——推理服务的卖家必须是链上身份）。
- 全部 6 条路由 `requires_auth=false`：链上 token 在 handler 内自验（网关系统
  中间件不识别链上 token，挂 true 会把合法调用方全拦在 401）。

## 4. 数据模型（SQLite `api_market`，18 列）

| 字段 | 类型 | 说明 |
|------|------|------|
| id | TEXT PK | UUID v4（刷新保留；联邦幂等去重主键）|
| api_name | TEXT | 商品名（如 `qwen3.5-9b chat`）|
| description | TEXT | 描述 |
| endpoint_url | TEXT | 消费端点（http/https 校验）|
| publisher_pubkey | TEXT | `0x`+66hex 压缩 secp256k1（token 反查）|
| publisher_display | TEXT | EVM 派生展示名 `0x`+40hex |
| server_config | TEXT(JSON) | 见下（探测+覆盖）|
| pricing | TEXT(JSON) | 计价（见下）|
| metrics_url | TEXT? | 发布者负载监控端点（代拉用）|
| tags | TEXT(JSON) | 标签数组（搜索命中）|
| status | TEXT | 恒 `active`（DELETE 直接删行；预留软下线）|
| created_at | TEXT | 首挂时间（RFC3339，刷新保留）|
| heartbeat_at | TEXT? | 最近心跳（≤60s 新鲜）|
| load | TEXT(JSON) | 最近心跳负载（规范化 6 键）|
| download_count | INTEGER | 消费计数（刷新保留；调用闭环未接线，预留）|
| access_info | TEXT(JSON) DEFAULT '' | **接入信息**（2026-08-31，见 §4a；存量行空串=无）|
| source_node | TEXT DEFAULT 'local' | **联邦来源**（2026-08-31）：本地=local；远程=发布节点名 |
| federated | INTEGER DEFAULT 0 | **两步联邦推送标志**（2026-08-31）：/:id/federate 置 1 |

唯一索引 `(api_name, publisher_pubkey)`：**同发布者同名=刷新**（保留
id/created_at/download_count/heartbeat/federated/access_info（body 未带时）），
**不同发布者同名=各自独立条目**。

老库迁移：`create_schema` 内 `PRAGMA table_info` 探测 → `ALTER TABLE ADD COLUMN`
幂等补 `access_info/source_node/federated` 三列（`migrate_add_columns`，与
api_gateway/forwarding 的 migrate_add_* 同款手法）。

### server_config（硬件探测 + body 覆盖，探测不到的必填缺省 400）

| 字段 | 来源 | 必填 |
|------|------|------|
| gpu_count / gpus | nvidia-smi `index,name,memory.total` **逐卡**（同型号多卡=多条目，index 区分；`gpu_count`=卡数。无卡/无命令 → `gpus` 空 + `gpu_count` 0，不阻塞——CPU-only 节点可发布）。**统一内存架构**（GB10/Jetson，2026-09-03）：显存列 `[N/A]`（Spark 实测 `0, NVIDIA GB10, [N/A]`）→ 条目 `vram_mb:null` + `unified_memory:true` + `unified_vram_mb`（/proc/meminfo MemTotal MiB，与 `ram_gb` 同池同口径），大厅展示「GB10 · 统一内存 121.7 GB」| 否 |
| gpu_name / gpu_vram_mb | 首卡（`gpus[0]`）镜像——**向后兼容保留的旧字段**（GB10 首卡 vram null → 镜像 null，真值在 `gpus[0].unified_vram_mb`）| 否 |
| cpu_model | /proc/cpuinfo 首个 `model name`；**aarch64 无该行**（GB10 cpuinfo 只有 MIDR `CPU part` 码）→ 回退 `lscpu` 的 `Model name:`（大小核去重保序拼接，Spark 实测 `Cortex-X925 + Cortex-A725`）| 否 |
| cpu_cores | /proc/cpuinfo `processor` 行计数 | 否 |
| ram_gb | /proc/meminfo `MemTotal`（KiB→GiB，保留一位小数）| 否 |
| model_name | **仅 body**（硬件探测拿不到）| **是**（缺 → 400）|
| max_model_len / context_len / quantization / region | 仅 body | 否 |

优先级：`body 字段 > 本地探测`（`merge_server_config` 纯函数）。GPU 系字段**整组
裁决**：body 带非空 `gpus`（简化形态 `[{name,vram_mb}]`×N，index 可省——发布
表单「型号 ×数量 / 单卡显存」三输入即组装此形态）→ 列表整体覆盖，`gpu_count`
与旧字段未显式给时从胜出列表首卡推导；body 仅带旧字段 `gpu_name`+`gpu_vram_mb`
（老客户端）→ 合成单卡 `gpus`；全缺省 → 探测列表原样。设计取舍：
endpoint→llm 实例配置的「猜测」本批不做（llm 实例态在另一 handler 内存中，
跨组件读取引入装配耦合）——发布者明确知道自己的模型名，body 直给最诚实。

`context_len` 字段契约（2026-09-02）：模型上下文长度的**发布端自报别名**，与
`max_model_len`（vLLM `--max-model-len`）并列的独立可选字段——此前发布 body 带
`context_len` 会被 serde 静默丢弃（大厅「上下文」恒显示 —），现随
publish → 响应/列表/详情 → SQLite → 联邦载荷**全链路透传**（序列化即透传，
无专门接线）。展示端优先 `context_len`、缺省回落 `max_model_len`，两者皆缺 =
真实无值显示 —（不猜）；存量条目（2026-09-02 前落库）无该字段输出中不出现。

### pricing（单价格字段，模式区分语义）

| mode | price_per_1k_tokens | currency | 校验 |
|------|--------------------|----------|------|
| free | 不得带（带 → 400）| 强制 free | — |
| per_token | 必填 >0（每 1k token 单价）| sats（缺省）/ credits | 付费不得 free |
| per_image | 必填 >0（**字段复用=每图单价**，设计定稿单价格字段）| sats（缺省）/ credits | 同上 |

价格排序跨币种按数值排（sats 与 credits 数值不互换；市场页展示币种列）。

### 4a. access_info（接入信息契约 + 脱敏规则，2026-08-31）

挂牌可携带消费者直连凭据（JSON 列，空对象不序列化）：

```json
"access_info": {
  "api_key": "sk-os-xxxxxxxxxxxx",
  "auth_header": "X-Api-Key: <key>",
  "notes": "额外参数/限流说明"
}
```

| 字段 | 说明 |
|------|------|
| api_key | 消费者调用凭据（如网关 sk-os- 令牌）。**输出按视角脱敏**（下表）|
| auth_header | 鉴权头用法；缺省 `Authorization Bearer`。自定义如 `X-Api-Key: <key>`——curl 示例按字面拼接，`<key>` 占位替换为 api_key |
| notes | 接入备注；非敏感恒明文 |

**脱敏规则**（`mask_api_key`，作用于 GET 列表/详情的输出面；存储面恒明文——
发布者自持）：

| 视角 | 列表/详情的 `access_info.api_key` |
|------|----------------------------------|
| publisher 本人（链上 token 反查 pubkey == publisher_pubkey）| 明文 |
| admin（Bearer 精确等于 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`，构造期定格）| 明文（读面运维视角）|
| 其他（匿名 / 他人链上身份 / 垃圾 token）| `<前4>***<后4>`；长度 ≤8 全掩 `****`（前4+后4 会拼出原文）|

边界语义：admin **只在读面**可见明文（密钥泄露应急/排障）；写面
（publish/delete/heartbeat/federate）仍无 admin 回落（设计定稿不变）。发布
响应（POST /publish）只回发布者本人 → 明文。

发布/刷新：body 带可选 `access_info` → 带则更新（字段 trim、空串→缺省）；
缺省 → 保留既有值（凭据不因改价丢）。联邦载荷原样携带（凭据随条目联邦分发，
接收端输出仍按**各自视角**脱敏——见 §9）。

## 5. API 契约（7 条路由，component=`api-market`）

| 方法 | 路径 | 鉴权 | 说明 |
|------|------|------|------|
| POST | `/api/v1/api-market/publish` | 链上 token | 挂牌；重复（同名+同 pubkey）=刷新保留计数。新 201 / 刷新 200（`refreshed:true`）|
| GET | `/api/v1/api-market` | 公开 | 列表 `?q=`（api_name/description/tags LIKE）`?sort=recent\|price`（价格=付费升序免费垫底）`?scope=all\|local\|fed`（§5.2）|
| GET | `/api/v1/api-market/:id` | 公开 | 详情（+派生 `heartbeat_fresh`）|
| DELETE | `/api/v1/api-market/:id` | owner pubkey | 下架（403「仅发布者可下架」；无 admin 通道）|
| POST | `/api/v1/api-market/:id/heartbeat` | owner pubkey | 自报负载 → 更新 heartbeat_at+load |
| GET | `/api/v1/api-market/:id/metrics` | 公开 | 负载输出：心跳优先 → 代拉 → 降级 |
| POST | `/api/v1/api-market/:id/federate` | owner pubkey | 推送/重新推送到联邦大厅（两步联邦第二步，§9）|

### 5.1 发布（请求/响应）

```json
POST /api/v1/api-market/publish
Authorization: Bearer <nexhub 链上 token>
{
  "api_name": "qwen3.5-9b chat",
  "description": "本地 3090 跑 Qwen3.5-9B，OpenAI 兼容",
  "endpoint_url": "http://192.168.1.10:8080/api/v1/gateway/v1/chat/completions",
  "pricing": { "mode": "per_token", "price_per_1k_tokens": 50, "currency": "sats", "note": "输入+输出合计" },
  "metrics_url": "http://192.168.1.10:8080/api/v1/llm/instances/llm-101/health",
  "tags": ["llm", "chat"],
  "server_config": {
    "model_name": "Qwen3.5-9B", "max_model_len": 32768, "quantization": "awq", "region": "cn-east",
    "gpus": [ { "name": "NVIDIA GeForce RTX 4090", "vram_mb": 24576 },
              { "name": "NVIDIA GeForce RTX 4090", "vram_mb": 24576 } ]
  },
  "access_info": { "api_key": "sk-os-xxxx…", "auth_header": "X-Api-Key: <key>", "notes": "限流 10 qps" }
}
→ 201
{
  "id": "b1c4…", "api_name": "qwen3.5-9b chat", "refreshed": false,
  "publisher_pubkey": "0x02ab…（66hex，token 反查）",
  "publisher_display": "0x7e5f…（40hex EVM 地址）",
  "server_config": {
    "gpu_name": "NVIDIA GeForce RTX 4090", "gpu_vram_mb": 24576,
    "gpu_count": 2,
    "gpus": [ { "index": 0, "name": "NVIDIA GeForce RTX 4090", "vram_mb": 24576 },
              { "index": 1, "name": "NVIDIA GeForce RTX 4090", "vram_mb": 24576 } ],
    "cpu_model": "AMD Ryzen 9 7950X 16-Core Processor", "cpu_cores": 32, "ram_gb": 64.0,
    "model_name": "Qwen3.5-9B", "max_model_len": 32768, "quantization": "awq", "region": "cn-east" },
  "pricing": { "mode": "per_token", "price_per_1k_tokens": 50, "currency": "sats", "note": "…" },
  "metrics_url": "…", "tags": ["llm","chat"], "status": "active",
  "created_at": "2026-08-21T10:00:00+08:00", "heartbeat_at": null, "load": null, "download_count": 0,
  "access_info": { "api_key": "sk-os-xxxx…（本人视角明文）", "auth_header": "X-Api-Key: <key>", "notes": "限流 10 qps" },
  "source_node": "local", "federated": false
}
```

（示例=双 4090 节点：body 只带 `model_name` + 简化形态 `gpus`×2，其余
`gpu_count`/旧字段 `gpu_name`/`gpu_vram_mb` 与 `cpu_model`/`cpu_cores`/`ram_gb`
由探测补齐/从列表推导——body 没带但本机有值；`None` 字段不序列化，空 `gpus`
同样不序列化。CPU-only 节点发布则 `gpu_count: 0`、无 `gpus`/`gpu_name`。）

### 5.2 列表（价格排序 / 搜索 / 来源过滤）

```json
GET /api/v1/api-market?sort=price&q=llm&scope=all
→ 200 [ ApiListing, … ]   // 付费单价升序在前，免费垫底；同价位保持新近在前
```

**`?scope=` 联邦来源过滤（2026-08-31）与向后兼容策略**：

- 响应**恒为平铺数组**——联邦化不改变响应形态（存量前端/测试零改动），元素
  只是新增 `source_node` / `federated` / `access_info` 字段（旧消费者忽略未知
  字段即可）；
- `all`（默认）：本地 + 联邦条目平铺（=旧行为超集）；`local`：仅本机发布
  （`source_node=='local'`）；`fed`：仅联邦远程条目（其余）；
- 未采用 `{local:[…], federated:[…]}` 对象形态的原因：现有前端
  （`client.ts` `apiMarketList` → `ApiGateway.vue` 按 `source_node` 客户端分流）
  与既有 Rust 测试都依赖数组形态——加 scope 参数是零破坏扩展点。

### 5.5 推送联邦（两步联邦第二步）

```json
POST /api/v1/api-market/b1c4…/federate
Authorization: Bearer <发布者 token>
{}
→ 200 { "ok": true, "id": "b1c4…", "action": "federate", "federated": true,
        "first_push": true, "source_node": "local",
        "note": "已推送到联邦大厅（其他 NexOS 节点将自动收到）" }
```

仅 owner pubkey（他人 403「仅发布者可推送联邦」；admin token 401——写面无
回落，与 publish/delete/heartbeat 同款语义）。联邦远程副本（`source_node` 非
local）在本节点推送 → 403（引导回源节点）。P2P 未启用时广播静默跳过，但
`federated` 标志仍置位（发布侧决策）。

### 5.3 心跳（活节点自报）

```json
POST /api/v1/api-market/b1c4…/heartbeat
Authorization: Bearer <发布者 token>
{ "running_req": 3, "waiting_req": 1, "gpu_cache_usage": 72.5, "tokens_per_sec": 128, "latency_ms": 340, "load_pct": 66 }
→ 200 { "ok": true, "id": "b1c4…", "heartbeat_at": "…", "stale": false, "load": {…规范化 6 键…} }
```

**服务端常驻心跳兜底**（2026-09-03，修「页面一关心跳就过期、联邦消费者看到
不可达」）：handler 构造时常驻任务每 **60s**（`HEARTBEAT_SWEEP_INTERVAL`）
对本节点 active 本地条目跑一轮 `refresh_local_heartbeats`（`api_market.rs`）——

- **存活证明，不是负载探测**：只刷 `heartbeat_at=now`，`load` 保留最后一次
  上报值（不造数据）；
- **页面驱动优先**：心跳已新鲜（≤60s——前端 60s 自动上报刚写过，更真）的
  条目跳过，兜底永不覆盖页面上报；页面驱动端点本身保留不动；
- **只碰本地条目**：联邦远程副本（`source_node` 非 local）不刷——活性归源
  节点管；
- **联邦可见性 ≤30min**：心跳刷新随 §9.6 的 30 分钟定期重播（+上线补推）
  自然扩散——消费者侧（联邦条目）看到的心跳时间差上限即重播周期；前端据此
  只展示「源节点心跳：N 分钟前」，不把它映射成可达性判定。

### 5.4 负载监控输出（消费者）

```json
GET /api/v1/api-market/b1c4…/metrics
→ 200（三态）
// ① 新鲜心跳（≤60s，零外呼）
{ "id": "b1c4…", "reachable": true, "stale": false, "source": "heartbeat",
  "metrics": { "load_pct": 66.0, "running": 3.0, "waiting": 1.0, "gpu_cache": 72.5, "tokens_per_sec": 128.0, "latency_ms": 340.0 },
  "ts": "2026-08-21T10:05:00+08:00" }
// ② 无新鲜心跳但有 metrics_url → 服务端代拉（reqwest 5s 超时，{metrics:{…}} 规范化）
{ "id": "…", "reachable": true, "stale": true, "source": "metrics_url", "metrics": {…}, "ts": "现在" }
// ③ 代拉失败/超时 或 既无心跳也无 metrics_url → 降级（附最后一次心跳数据若有）
{ "id": "…", "reachable": false, "stale": true, "source": "metrics_url"|"none",
  "metrics": null, "ts": "最后一次心跳时间|null", "error": "代拉失败（…）" /* 仅②③的失败分支 */ }
```

metrics 键名别名（心跳键与 vllm 约定键都收，先命中先用）：

| 规范化键 | 接受的输入键 |
|----------|--------------|
| load_pct | load_pct / load / gpu_util |
| running | running / running_req / num_requests_running |
| waiting | waiting / waiting_req / num_requests_waiting |
| gpu_cache | gpu_cache / gpu_cache_usage / kv_cache_usage |
| tokens_per_sec | tokens_per_sec / token_throughput / tps |
| latency_ms | latency_ms / latency / e2e_latency_ms |

未知字段为 `null`（不出现在 JSON 中）——前端按「未知」渲染，不猜。

## 6. 给前端的对接要点（JSON 速查）

- **市场卡片**（2026-09-03 徽章语义分流 + 排版重排，四分区卡片）：
  - ① **主区**：**标题行同行不折行**——`api_name`（超长 ellipsis）+ 价格徽章
    （`pricing` 的 `mode`+`price_per_1k_tokens`+`currency`，free 显示「免费」）
    + 身份徽章（本机/联邦 `source_node`）+ 状态徽章；描述默认两行 clamp
    （`-webkit-line-clamp`，点卡片展开全文）；`tags` 行内小 pill；
  - **状态徽章分流**（2026-09-03，修「明明可以调用却显示不可达」）：
    - **本地条目** = 负载直连探测徽章：绿=新鲜心跳（≤60s）/ 灰=metrics_url
      代拉降级 / 红=不可达（对发布者自己有意义的探测，语义保留）；
    - **联邦条目** = 「🌐 经源节点中继」**常驻徽章**——中继路径可达性由消费
      行为证明（调用即通），不做主动探测（联邦条目相应跳过 metrics 轮询）；
      另有独立行「源节点心跳：N 分钟前」（`heartbeat_at` 时间差，无心跳
      `--` 不猜；快照可见性 ≤30min 重播周期，见 §5.3/§9.6）；
  - ② **硬件/规格区**：真两列自适应网格 `repeat(auto-fill, minmax(180px,1fr))`
    （窄卡自然回单列，不依赖媒体查询）——每格标签小字灰色上标 / 值主体
    （换行不截断）；GPU 型号+显存/统一内存同格合并（多卡同型
    「`RTX 4090 ×2 · 24 GB/卡`」）、CPU 型号+核数、内存、模型、**上下文**
    （`context_len` 优先回落 `max_model_len`，皆缺 = —）、量化/区域（有值才
    显示）；
  - ③ **接入信息折叠面板**（默认收起，`details/summary`）：密钥/鉴权头/备注
    + curl 示例代码块（MD 命令行块样式：等宽/深色底/横滚/右上复制按钮）；
  - ④ **操作区**（卡片底部单行不折行）：`publisher_display`（`0x` 短地址，
    超长 ellipsis）/ 打赏 / `download_count` / `created_at`（右贴齐）；
  - 本地条目 `heartbeat_at` 非空且 `GET /:id` 的 `heartbeat_fresh:true` →
    在线徽标。卡片列宽下限 `min(340px,100%)` 不溢出。
- **排序切换**：`sort=recent`（默认，最新上架）/ `sort=price`（价低→价高→免费）。
- **搜索框**：`?q=<词>`（命中名称/描述/标签，URL 编码）。
- **详情页**：`GET /:id` 全字段 + `heartbeat_fresh`；「调用」按钮复制
  `endpoint_url`。
- **负载面板**（仅本地条目）：轮询 `GET /:id/metrics`（建议 10-30s）——
  `reachable:true`+`stale:false` 绿标 / `stale:true` 灰标（降级）/
  `reachable:false` 红标（不可达），`metrics.*` 画曲线；`source` 区分数据来源。
  联邦条目不轮询（中继可达性由消费行为证明，见上方状态徽章分流）。
- **发布表单**：先经 `/api/v1/nexhub/auth/*` 登录拿 token；`server_config` 只需
  填 `model_name`（必填）+ 可选 `context_len`/`max_model_len`/`quantization`/
  `region`——硬件字段后端自动探测；GPU 覆盖表单为「型号 ×数量 / 单卡显存」
  三输入，提交组装简化形态 `gpus: [{name, vram_mb}]`×count（`gpu_count` 与
  旧字段服务端从列表推导）。
- **接入信息卡块**（2026-08-31；折叠+curl 规则 2026-09-02 修订）：条目带
  `access_info` 时折叠面板显示密钥（按视角脱敏）/鉴权头/备注 + 完整 curl
  示例与「复制 curl」按钮——curl 由 `endpoint_url` + 鉴权头 + 模型名
  （`server_config.model_name` → tags → notes 兜底）拼装；鉴权头规则（与后端
  纯函数 `curl_auth_header_line` 同一契约）：
  - **明文分支**（密钥无 `***` 脱敏标记 = publisher 本人/admin 视角）→
    `-H 'Authorization: Bearer sk-os-xxxx'`（自定义 `auth_header` 含 `<key>`
    占位则字面替换，纯头名如 `X-Api-Key` 补冒号）；
  - **占位分支**（脱敏视角/发布端未配 key）→ `-H 'Authorization: Bearer
    <你的令牌>'`（i18n 占位符）+ 一行说明「完整令牌需发布者本人/admin 视角
    或向发布者索取」——脱敏残值（`前4***后4`）**永不**拼进 curl；
  - 缺省 `Authorization Bearer`（无冒号形态）规范化为带冒号的
    `Authorization: Bearer <令牌>`（旧缺陷：按字面拼出只有头名没有值的头）。
- **本地/联邦二级 Tab**：客户端按 `source_node` 分流（local/缺省=本地大厅，
  其余=联邦大厅）；本机条目卡片带「推送联邦/重新推送联邦」按钮（owner 调
  `POST /:id/federate`），已推送条目显示 `federated` 态。

## 7. 实现（代码地图）

| 关注点 | 位置（`crates/os-api/src/handlers/api_market.rs`）|
|---------|------|
| DTO / 计价校验 / 负载规范化 | `ApiListing` / `ServerConfig` / `Pricing`（`validate_pricing`）/ `LoadMetrics::from_json`（别名表）|
| 接入信息 | `AccessInfo` / `normalize_access_info` / `mask_api_key`（脱敏）/ `apply_access_info_mask`（输出面改写）/ `curl_auth_header_line`（curl 鉴权头两分支：明文/占位，前端同契约）/ `ApiMarketRouteHandler::access_info_revealed`（视角判定）|
| 硬件探测 | `probe_server_config_blocking`（spawn_blocking 内）+ 纯解析 `parse_cpuinfo_model` / `parse_cpuinfo_core_count` / `parse_meminfo_ram_gb` / `parse_nvidia_gpu_csv`（单行）/ `parse_nvidia_gpus_output`（整段多卡）/ `parse_lscpu_model`（aarch64 CPU 型号回退，GB10 大小核拼接）/ `apply_unified_vram`（统一内存条目填 meminfo 池总量）；合并 `merge_server_config`（body > 探测；GPU 系整组裁决）|
| 心跳新鲜度 | `heartbeat_age_secs` / `heartbeat_fresh`（60s 窗口，时钟超前宽容）|
| 代拉 | `fetch_metrics`（共享 Lazy reqwest Client + 每请求 5s 超时；测试可注入亚秒）|
| SQLite | `create_schema`（19 列 + 唯一索引 + `migrate_add_columns` 老库迁移）/ `insert_listing` / `load_listings`（q/scope 过滤 + recent 序；price 序在 Rust 侧 stable sort）|
| 联邦大厅 | `ApiMarketFedEndpoint`（`set_p2p`/`set_transport`/`broadcast_entry`/`ingest`/`ingest_from`/`dispatch`）+ `FED_KIND_API_MARKET_LOBBY` / `build_api_market_fed_payload` / `sanitize_fed_node`（§9）|
| 跨网中继 | `FED_KIND_API_RELAY_REQ/RESP` + `ApiRelayRequest`/`ApiRelayEvent`/`relay_roundtrip`/`relay_open_stream`（消费者侧）+ `handle_relay_req`/`relay_execute_and_reply`/`normalize_relay_url`/`relay_url_allowed`（源端白名单）+ `sweep_relay_state`（巡检清理）+ `RelayLimits`（超时/限额，§10）|
| 装配 | `main.rs`：`ApiMarketRouteHandler::with_chain_auth(nexhub_chain_auth.clone())`——与 nexhub-lobby **同一 ChainAuth 实例**（token 互通；401 文案引导 nexhub auth 端点）；`federation()` 在 Box 进网关前取出，`llm_shared.external_state().set_relay(…)` 消费者侧注入（2026-09-02），`spawn_p2p_if_enabled` 内 `api_market_fed.set_p2p(handle, name)` 注入 + `FederationBridge { api_market: Some(…) }` 入站分发 |
| 测试 | 模块内集成测：真密钥对挑战-签名登录覆盖鉴权矩阵/探测优先级/刷新保留/价格排序/心跳 stale/代拉三态/路由归属 + access_info（迁移/发布轮换/三视角脱敏）+ 联邦（两步语义/幂等合并/桥分发/scope 兼容/删除不撤远端/联邦条目代拉/source_node_id 验签记录）+ 中继（白名单封闭集/非流式端到端/403 与方法拒/流式块序/>1MiB 分块重组/超时清理/伪造应答防御）|

## 8. 范围与后续（诚实清单）

- **本期不做**：消费者调用闭环（`download_count` 递增 / 按调用计费结算——
  接 api_gateway 的 sk-os- 令牌与 per_token 计费模式是自然下一步）；
  metrics_url 的 Prometheus 文本格式（当前约定 JSON `{metrics:{…}}`）。
  （跨节点联邦市场已於 2026-08-31 落地，见 §9。）
- **已知取舍**：price 排序跨币种按数值（市场页展示币种列消除歧义）；
  `per_image` 复用 `price_per_1k_tokens` 单价格字段（设计定稿单价格字段，
  语义按 mode 区分）；DELETE 为物理删行（`status` 列预留软下线态）。

## 9. 联邦大厅（fed kind `api_market_lobby`，2026-08-31）

照 NexHub 联邦大厅（os-nexhub `LobbyFedEndpoint`）与 IM 联邦的同一套模式：
os-p2p overlay 上的 JSON 载荷广播 + 接收端幂等落库。启用条件：两端
`NEXOS_P2P_ENABLE=1` 且互相组网（fed 广播范围=当前已连接 peer 一跳，接收端
不转播——天然无环）。

### 9.1 协议（载荷契约）

```json
{
  "fed": "api_market_lobby",
  "node": "node-106",
  "node_id": "0x<发布节点 NodeID 66hex>",
  "entry": { …完整 ApiListing JSON（含 access_info/federated/source_node）… }
}
```

- **kind**：`payload.fed == "api_market_lobby"`（`FED_KIND_API_MARKET_LOBBY`）；
- **node**：发布节点名。**node 字段整体缺失 → Invalid 丢弃**；`node="peer"`
  （匿名——发送端 `NEXOS_P2P_NAME` 未设时 sanitize 的回退值）**收下**
  （2026-09-03 真机根因修复：此前匿名按"缺 node"静默拒收，"IM 能通、市场
  收不到"；物理归因不依赖节点名——`source_node_id` 是验签 NodeID，匿名
  多节点靠 NodeID 兜底防碰撞）。新发送端（同日修复）空名时合成稳定短名
  `node-<NodeID 前 8 hex>`（`set_p2p` 兜底），联邦归因跨重启可读；
- **node_id**（2026-09-02）：发布节点 NodeID——接收端落列 `source_node_id` 的
  兜底来源（优先取 os-p2p 验签 `msg.from`，载荷自报仅老版本兼容）；
- **entry**：完整 `ApiListing` 快照——`access_info` 明文随载荷分发（对端
  **存储面明文、输出面按各自视角脱敏**：接收端的明文只对其 publisher 本人
  （=同一发布者在该节点登录）与该节点 admin 可见）；`source_node` 发送端恒
  `local`（接收端改写为 `node`）。

### 9.2 两步联邦（照 NexHub 语义）

1. `POST /publish`：**只写本地**，不广播（`federated=false`；重发布保留既有
   推送标志）；
2. `POST /:id/federate`（owner pubkey；无 admin 回落）：置 `federated=true`
   落库 + 广播最新快照；重复调用=重新推送（`first_push:false`，对端同源刷新）。

不存在「直接发布到联邦」的路径——联邦条目只能从本地已发布条目推送；联邦
远程副本（`source_node` 非 local）不可在本节点转发（403 引导回源节点）。

### 9.3 接收端幂等合并（`ApiMarketFedEndpoint::ingest`）

处置结果（观测面枚举 `ApiMarketFedIngest`）：

| 结果 | 条件 | 动作 |
|------|------|------|
| `Written` | 本地无该条目 | 写入：`source_node=node`、`download_count` 清零起步（对端计数是它的活跃度）、`federated` 随载荷 |
| `Refreshed` | 已有条目且**同 source_node**（同源重发=对端刷新快照）| 覆盖快照、沿用本地 id、保留本地 `download_count`；心跳/负载取源端快照（heartbeat 在发布节点上跑） |
| `Duplicate` | 逐字节相同载荷重放（内存缓存命中，容量 1000）| 不触碰 DB |
| `Skipped` | 已有条目但**来源不同**（本机先发布或他节点先到）| 保护本地条目，跳过 |
| `Invalid` | 错 kind / **node 字段缺失**（匿名 `node="peer"` 收下）/ 缺 entry / 必填缺失（id/api_name/endpoint_url/publisher_pubkey）/ 非 http(s) endpoint / DB 读写故障 | 丢弃（**每个拒绝分支都落日志**——2026-09-03 观测性修复，真机排查无静默丢弃；DB 读失败不毒化 seen 缓存，可重试） |

**幂等去重键**：先 `id`（主键），无则 `api_name+publisher_pubkey`（唯一索引）
——对端重建条目（换了 id）仍按名+发布者归并（Refreshed 沿用本地 id）。内存
缓存键含**完整快照串**：只拦逐字节重放，同源新快照（发布侧重新 publish+
federate）必穿透到 DB 权威路径（nexhub 2026-08-23 同款修复语义——对端刷新
永远到得了本节点）。

### 9.4 删除/下架与心跳的联邦语义

- **删除不撤远端**：本地下架只删本地行，**不广播撤销载荷**——远端副本由
  源节点重新 publish+federate 刷新，或在对端保持旧快照（与 NexHub 大厅同款
  语义：联邦是尽力而为的快照传播，不是强一致删除）；
- **心跳在发布节点上跑**：heartbeat 是 owner-only（链上 token + publisher
  pubkey），自然只在发布者自己的节点发生；联邦副本上的心跳数据来自源端快照；
- **metrics 代拉对联邦条目同样可用**：消费节点 `GET /:id/metrics` 无本地
  新鲜心跳时走 `metrics_url` 服务端代拉（指向源节点端点，跨节点可达）。

### 9.5 装配（main.rs，照 im/nexhub/live 注入方式）

```text
build_gateway:
  api_market_handler = ApiMarketRouteHandler::with_chain_auth(nexhub_chain_auth.clone())
  api_market_fed = api_market_handler.federation()      // Box 进网关前取出
  register_component("api-market", …)
  spawn_p2p_if_enabled(…, api_market_fed)               // p2p spawn 成功后：
    api_market_fed.set_p2p(handle, name)                // ① 发送端注入（同步锁写，先行）
    FederationBridge { im, nexhub, live, api_market: Some(api_market_fed) }
      └─ tokio::spawn(loop rx.recv() → bridge.dispatch) // ② 再起入站消费 task
```

`set_p2p` 的广播闭包走 `handlers/p2p.rs::fed_broadcast`（fire-and-forget
`tokio::spawn`；本地指纹目标跳过防自回路重复入库）。P2P 未启用时发送静默
停用（federated 标志仍置位）、接收端不装配——单机部署零开销。

### 9.6 三通道补覆盖（补推 + 重播 + 定向补播，2026-09-03）

**缺陷背景（真机实证）**：fed_broadcast 只把载荷发给「**当时已连接**」的
peer（一跳、接收方不转播——设计如此防环）。严格 NAT 对端（如 Spark）常年
无活连接，发布/推送时刻永远不在已连接集合里 → **永远收不到市场条目**（实测
106 对 106/Spark 三次 federate、甚至 `/p2p/connect` 拉起 relayed 连接后重推
均未送达）。

**真机跟进（同日第二轮验收）**：`/p2p/connect` 对 Spark 返回 relayed 成功
但 Spark 在 peers 表 `connected` 恒 false——**中继路由是按需逐消息的，不产生
常驻 Conn**。因此仅靠"连接 watcher + connected 广播"两通道对 Spark 类节点
无效（watcher diff 永远看不到它上线；重播遍历 connected 也到不了它）。
Spark 重启后联邦大厅仍空（实测）。

修法是在"发布时广播"之外加**三条补覆盖通道**，均复用同一 `api_market_lobby`
载荷与幂等 ingest（接收端零改动）：

| 通道 | 触发时机 | 范围 | 实现入口 |
|------|---------|------|---------|
| **上线补推（backfill）** | p2p 连接建立（`spawn_conn_watcher` 观测到**新出现**的已连接 peer，含进程启动首拍种子的既有连接）| 对该新连 peer 定向 `send_to` 本节点全部 federated 条目快照 | `ApiMarketFedEndpoint::backfill_to(peer)`（main.rs 在 conn watcher 回调里 spawn） |
| **定期重播·广播相位（replay）** | 端点装配时常驻任务，每 **30 分钟**一轮 | 对**当前已连接** peer 走广播面重播本节点全部 federated 条目（`fed_broadcast` fan-out） | `ApiMarketFedEndpoint::replay_round()`（`install_transport` 内 `tokio::spawn` 循环） |
| **定期重播·定向补播相位（directed）** | 同上（重播轮的第二相位） | 对 **node-meta 注册表 Active ∖ 当前 connected** 的已知活跃节点逐条 `send_to` 定向补播（按需路由，中继可达——覆盖 Spark 类**无常驻连接**的节点） | 同 `replay_round()`（目标集 = `FedKnownActiveFn`，生产闭包在 `set_p2p` 内装配：`node_meta()` Active ∖ `peers()` connected ∖ 本机指纹，纯过滤内核 `fed_direct_replay_targets`） |

**补推/重播语义（红线与保证）**：

- **只发本节点条目**：`status='active' AND federated=1 AND source_node='local'`
  ——联邦远程条目（federated 随载荷=1 但来源是别的节点）**不转播**（防环：
  转播会以本节点 NodeID 覆盖来源归因，违反"接收方不转播"的一跳语义）；
- **幂等零负担**：对端 ingest 对同快照重放命中 seen 缓存 → `Duplicate`
  （不触碰 DB）；快照变（心跳/负载/重新 federate）→ 键不同 → 穿透到 DB
  权威路径 → `Refreshed`（**心跳经重播联邦传播**——顺带补上此前"心跳只在
  源端快照、不主动传播"的观感缺口）；
- **限幅**：补推/重播（含定向相位）逐条间隔 **100ms**
  （`FED_BACKFILL_SPACING`）防 burst；条目多时日志只汇总一行（不逐条刷屏）；
- **自回路防护**：目标指纹==本机 NodeID 跳过——watcher/生产目标集闭包侧
  过滤（第一道，与 `fed_broadcast` 同语义）+ `backfill_to`/`replay_round`
  内 node_hex 比对（兜底）；
- **目标集语义**（定向补播相位）：Active（心跳引擎判活）且**不在**当前
  connected 集（避免与广播相位重叠重复投递）；Inactive（五振出局）不补播；
  每轮现拉注册表——节点出局/复活自然进出目标集；
- **零帧保证**：无 federated 条目 / P2P 未装配 / 目标集空 → 不发任何帧；
- **挂点说明**：os-p2p 无连接事件面（`register_conn` 只写内部表，上层只有
  `on_msg()` 应用消息广播），连接感知是 os-api 侧 `spawn_conn_watcher`
  （`handlers/p2p.rs`）的**1s 轮询 diff**——语义等价"连接建立事件"的最小
  回调注入；更短命的闪断与无常驻连接的中继节点由重播两相位兜底。

**第三轮真机跟进（同日）**：定向补播帧已送达 Spark（观测日志见帧、hops=1）
但表仍空——排查结论：① 中继帧与直连帧走**同一条** `deliver_local →
on_msg → bridge` 消费路径（os-p2p `FrameKind::Send` dst==self 唯一投递口，
源码实证 + 三节点中继拓扑端到端测试）；② bridge 的 api_market 消费臂未装配
时此前**静默丢弃**——现在落告警日志；③ **真凶**：接收端把匿名节点名
`node="peer"`（发送端 `NEXOS_P2P_NAME` 未设的 sanitize 回退）当"缺 node"
拒收且**无日志**——IM 同场景接受 "peer"，故"IM 能通、市场收不到"。修复：
匿名收下（归因靠验签 `source_node_id`）+ 发送端空名合成 `node-<hex8>` +
全部 Invalid 拒绝分支落日志 + DB 读失败撤 seen 键（不毒化重试）。

测试：`api_market.rs` #43-49（fake overlay：断连错过广播 → 补推 Written /
同快照 Duplicate 零 DB 触碰 / 心跳后重播 Refreshed / 无 federated 条目零帧 /
常驻重播任务接线 / **目标集过滤矩阵**（Active∩无连接 收、Inactive 不收、
connected 不收、自指纹不收）/ **定向补播端到端**（B 在目标集但无 conn →
replay 经 send_to 送达 Written、同快照 Duplicate、目标集清空零定向帧、
注入自指纹被兜底过滤）/ **匿名 peer 收下 + 缺 node 字段仍 Invalid + 同
peer 名异 NodeID Skipped**）+ `p2p.rs` 连接观测 task 单测 + 真机组网端到端
×2（直连拨入补推；**三节点中继拓扑**：A/B 互不直连只连锚点 P，重播定向
补播经 P 中继转发 hops=1 送达 B 落库——Spark 场景复现）。

## 10. 跨网中继（fed kind `api_relay_req` / `api_relay_resp`，2026-09-02）

**缺陷背景（Spark 实测）**：联邦条目的 `endpoint_url` 常是发布者内网地址
（如 `qwen3.5-9b@ub2604` 的 `http://192.0.2.106:8558/v1`）——条目数据经
overlay 同步没问题，但消费者 llm_external 的 chat/test **直连 HTTP** 跨网
够不着（`上游请求失败: error sending request for url (http://192.0.2.106:8558/...)`）。
修法：消费者对联邦导入条目经 overlay 把 HTTP 请求**定向发给源节点**，源节点
白名单裁决后代发（单跳，源节点即出口——不做多跳转发链）。

### 10.1 协议（kind × 载荷）

| kind | 方向 | 载荷要素 | 语义 |
|------|------|----------|------|
| `api_relay_req` | 消费者→源节点（定向） | `{req_id, method, url, headers{}, body_b64?, stream, ci, cn}` | 代发请求。body >1 MiB 按 [`RELAY_CHUNK_BYTES`]（1 MiB）分块多帧：帧 0 带 method/url/headers/stream + 首块，余帧只带 body_b64；ci/cn = 块序/总块数（cn ≤ 32 → 请求上限 32 MiB）。帧序即字节序 |
| `api_relay_resp` | 源→消费者（定向，可多帧） | `{req_id, seq, status?, headers?, chunk_b64?, done, error?}` | 应答。seq 从 0 单调递增；**seq=0 带 status + 响应头**（非流式整包 + 流式首帧同规则）；每帧可带一个 chunk（≤1 MiB，>1 MiB 响应体自动分块多帧）；done=true 收尾；error 非空 = 断流/失败原因（帧丢弃、上游中断等）。流式（stream=true）时源端把上游 `bytes_stream()` 逐块透传（SSE 逐块，帧序即字节序） |

超时（缺省 `RelayLimits`）：req 级 30s（无响应 pending 清理缺省；消费者按
语义覆盖——test≈10s / chat 整包≈120s，**deadline 制整体预算**）、流式首帧
15s（`relay_open_stream` 的 Head 窗口——注意非流式首帧=整包完成，窗口即整包
预算）、流式空闲 60s 断流；req_id 关联 map（pending ≤256）与源端分块重组
缓冲（≤16 个）由巡检任务按 TTL 定期清理。os-p2p 帧上限 4 MiB——1 MiB 块
base64（4/3 膨胀 ≈1.34 MiB）安全余量充足（transfer.rs / live 中继分块同款）。

**防伪造**：resp 帧按 req_id 回填事件流时校验**发送方 == pending 定向目标**
（overlay 对端可伪造请求/应答——定向校验 + req_id 随机 UUID 双保险）。

### 10.2 白名单安全模型（源节点红线：绝不做开放代理）

收到 `api_relay_req` 时**先裁决后外呼**，两道闸：

1. **方法闸**：仅 `GET` / `POST`（其余 403「仅支持 GET/POST 中继」）；
2. **URL 闸**：请求 url 必须命中**本节点已发布条目**（`source_node=='local'`
   且 active）`endpoint_url` `E` 的封闭集合——归一化（`reqwest::Url` 解析，
   点段穿越 `/v1/../x` 被归并后比对；尾斜杠等价）后精确匹配
   `{E, E/models, E/chat/completions}` 之一：

   - 覆盖 llm_external 的两条真实请求形态：test=`<base>/models`、
     chat=`<base>/chat/completions`（base=E，OpenAI 根地址形态），以及
     endpoint 直填 chat 完整地址的发布形态（E 本身）；
   - 任意其他路径 / 主机 / 端口一律 403「该 URL 不属于本节点发布的条目」——
     不开放任意子树（防 `E/../internal` 之类探测）；
   - **联邦远程条目不参与白名单**（不二次转发——单跳语义，跨网只能由条目
     的源节点自己代发）。

请求头透传（鉴权头原样），剥 hop-by-hop（host/connection/content-length/
transfer-encoding/keep-alive/upgrade）；源端总超时上限 600s（与 llm_external
CHAT_STREAM_TIMEOUT 同量级），空闲断流由消费者侧执行。

### 10.3 via_node 语义（消费者侧，详见 docs/LLM_EXTERNAL_APIS.md）

- 联邦接收端 ingest 现记录**验签来源 NodeID**：条目新增列 `source_node_id`
  （`0x`+66hex；桥传入 `msg.from`——不可伪造；本地发布恒空串。老对端发的
  无 node_id 载荷 → 空=直连语义）。同名不同 NodeID 的"刷新"按异源保护跳过
  （节点名可撞，物理节点以验签 NodeID 为准）；
- 前端联邦大厅一键导入把 `source_node_id` 写进 `llm_external_apis.via_node`
  ——该登记的 chat/test 走本节 §10.1 中继，不直连。

### 10.4 装配（main.rs 单点插入）

```text
build_gateway:
  …
  api_market_fed = api_market_handler.federation()
  llm_shared.external_state().set_relay(Some(api_market_fed.clone()))  // 消费者侧注入
  …
  spawn_p2p_if_enabled(…, api_market_fed):
    api_market_fed.set_p2p(handle, name)   // 广播 + 定向发送面 + 巡检任务
    FederationBridge { …, api_market } 入站分发（lobby/req/resp 三 kind 统一
    经 ApiMarketFedEndpoint::dispatch，带验签 from）
```

### 10.5 限制与未尽事项

- **单跳**：中继只到条目的源节点（出口=源节点）；源节点不可达时无兜底路由；
- **流式背压**：resp 帧经无界通道投递（SSE 消费慢时内存堆积——与 live 中继
  的丢帧保实时策略不同，本期选完整性；背压/有界通道留待压测后定）；
- 白名单粒度按**整个 endpoint**：发布者换 endpoint 需重新 publish+federate，
  消费端旧 via_node 条目指向的 URL 若不再命中白名单会收到 403（错误文案即
  引导）。
