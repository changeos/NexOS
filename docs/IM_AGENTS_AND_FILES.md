# IM 多 AI Agent 接入与文档传输设计与实施

> 状态：已实施（2026-08-21；2026-08-22 增补 §7 消息推送通知 webhook；
> 2026-08-22 迭代 `/im/search` 升级会话范围+member 门，§3.2）·
> 文档用途：架构存档 + PPT 素材 + **外部 agent 接入协议指南**
> （Windows 演示机 / PPT agent 直接照 §6/§7 接入）+ AI agent 接续开发依据
> 代码：`crates/os-api/src/handlers/im.rs`（单文件组件，27 条路由，77 个单测）
> 测试基线：os-api 全绿（2026-08-21 批次 983；2026-08-22 增补 N 面 11 个）；
> clippy -D warnings 零告警；`os-api --check` 路由表零冲突
> 前置设计：[IM_BLOCKCHAIN_AUTH_DESIGN.md](IM_BLOCKCHAIN_AUTH_DESIGN.md)（链上身份 = secp256k1 公钥，
> 2026-08-17）——本批次在其上叠加，认证模型零改动

---

## 0. 一页速览（PPT 提取层）

| 维度 | 数值 / 结论 |
|---|---|
| 组件 | `im`（os-api 网关内 RouteHandler，路由 22 → **24 条**） |
| Agent 可见性 | 消息新字段 `sender_kind: "human"|"agent"`（serde default human，**存量零迁移**） |
| @ 提及 | 服务端解析 `@<名字>`（`[一-龥A-Za-z0-9_-]{1,42}`）→ `mentions` 列（去重保序） |
| 内置 agent | **「NexOS助手」**：`@NexOS助手` 触发 → 本地推理 → 回同会话一条 `sender_kind=agent` 消息 |
| 推理通道 | OpenAI 兼容 `POST $NEXOS_IM_AGENT_LLM_URL`（默认 `http://127.0.0.1:8000/v1/chat/completions`），模型 `NEXOS_IM_AGENT_MODEL`（默认 `qwen3.5-9b`） |
| 降级策略 | LLM 不可达 → 固定话术回复（**绝不静默丢弃**，绝不阻塞发消息请求） |
| 防风暴 | 同会话 3s 去抖窗口 + 代次超越放弃：**窗口内多条 @ 只响应最后一条** |
| 文档传输 | `POST /api/v1/im/files`（base64-JSON，≤64MiB）→ `/tank/im-files/<YYYYMM>/<uuid>-<净化名>` |
| 附件核对 | 发消息带 `attachment:{file_id,...}` → 服务端按 file_id 核对，**伪造 size/filename 一律被落盘真值覆盖** |
| 下载鉴权 | Bearer IM token / `?token=<IM token>` / `?token=<admin token>` 三选一（URL 直链场景） |
| WS 帧 | `im_message` / `im_lobby_message` 帧 `message` 体**原样透传全部新字段**（Value 透传，零改 http.rs） |
| 推送通知 | `POST /im/notify/register` 注册 webhook → **IM 一有消息即异步 POST 到 agent**（消除轮询，§7） |
| 推送投递 | body=完整 Message JSON + Header `X-NexOS-Event: lobby_message\|conversation_message`；超时 5s；**不含任何 token** |
| 推送韧性 | 连败 ≥5 次自动注销（`status=disabled` + `last_error` 记录原因，重新注册即恢复）；派发完全不阻塞消息路径 |
| 新增测试 | G 面 11（mention/助手）+ F 面 7（附件）= 18 个；N 面 11（推送通知，2026-08-22）——共 **29 个**（含本地 TcpListener 假接收端端到端） |

一句话：**人类与 AI agent 在同一个链上身份、同一套 REST/WS 通道里平等对话**——
agent 自声明 `sender_kind` 区分渲染，`@NexOS助手` 把本地大模型变成会话里的一个
"人"，附件通道让 PPT/文档即传即收，全部新字段向后兼容（serde default，存量
客户端不升级也不坏）。

---

## 1. 需求拆解（三面）

### A. Agent 可见性（sender_kind）

外部 agent（Windows 演示机上的对话代理、PPT 生成 agent）与人类用户共用链上身份
（各自持有自己的 secp256k1 私钥）。前端需要区分"这条消息是人发的还是 AI 发的"：

- 消息结构加 `sender_kind: "human"|"agent"`，**发消息 body 可带**（自声明）；
- **信任边界**：该字段是**展示层自声明语义**——服务端只做白名单归一
  （非 `"agent"` 一律存 `"human"`），不校验声明者是否真是 agent。消息归因仍以
  token 反查的 pubkey 为准（谁声明都不影响"是谁发的"）；恶意声明 agent 最多
  换一个前端图标，拿不到任何权限（IM 内无角色体系）。
- WS 广播帧 / 历史列表 / 离线补拉三处全部返回该字段。

### B. @mention 解析与内置助手闭环

- 发消息时**服务端**解析 `@<名字>` 落 `mentions` 列（客户端不传，传了也被覆盖）：
  - 名字规则：`[一-龥A-Za-z0-9_-]{1,42}`（CJK 基本区 + ASCII 字母数字 + `_`/`-`，
    1..=42 字符，超长截断到 42）；
  - `@` 后跟非法字符（空格/标点/@）不算提及；去重保序。
- **内置 agent「NexOS助手」**（名字常量 `NEXOS_ASSISTANT`，前端高亮与外部 agent
  避名用）：
  - 触发：大厅或任意会话的消息 `mentions` 含 `"NexOS助手"`（会话内 @ 同样生效，
    回对应会话）；
  - 执行：`tokio::spawn` 异步任务（**不阻塞发消息的 HTTP 响应**）——取该消息
    除 @ 外文本作为 prompt → reqwest POST 本地推理（OpenAI 兼容
    chat/completions，60s 超时）→ 以 `sender_kind=agent`、`sender_name="NexOS助手"`
    回同会话一条消息（正文 ≤800 字截断 + `"（AI 生成）"` 后缀）；
  - 降级：LLM 不可达/超时/响应畸形 → 固定话术
    `"抱歉，本地推理服务暂时不可用，请稍后再试。（AI 生成）"`;
  - 防自激：`sender_kind=agent` 的消息不再触发助手（助手回复不会触发新回复）。

### C. 文档传输（IM 附件）

- 链上 token 走网关 JSON 通道，**multipart 不可行** → 与 `files.rs` upload 同款
  base64-JSON：`POST /api/v1/im/files {filename, content_base64}`；
- 校验 ≤64MiB（base64 长度前置估算 + 解码后复检，不先解大字符串再拒绝）；
- 落盘 `/tank/im-files/<YYYYMM>/<uuid>-<净化名>`（目录自动建 + 文件名净化：
  路径分隔符/穿越/控制字符一律 `_`，白名单外字符替换，≤120 字符；
  tmp+rename 原子写）；元数据入 `im_files` 表（file_id/filename/size/mime/uploader/path）；
- 下载 `GET /api/v1/im/files/:file_id?token=`：三选一鉴权（Bearer IM token /
  `?token=` IM token / `?token=` 系统 admin token——URL 直链场景 `<img>` 标签
  无法带 Bearer 头）；回传 base64 JSON 信封 + `Content-Disposition`（RFC 5987）；
- 发消息可带 `attachment: {file_id, filename?, size_bytes?, mime?}`——服务端按
  file_id 查落盘记录：**不存在 → 400**；存在 → `filename`/`size_bytes` 以落盘
  真值覆盖（伪造无效），`mime` 自报可精化、回落存储值。

---

## 2. 系统拓扑

### 2.1 全局视图（人类 + 外部 agent + 内置助手 + 附件通道）

```
┌────────────────────────── NexOS 节点（os-api :8080）──────────────────────────┐
│                                                                                │
│  人类用户（Web 前端 Chat.vue）          外部 agent（Windows 演示机 / PPT agent）│
│   ├─ 私钥 localStorage                  ├─ 自有密钥对（一次性生成，本地保存）    │
│   └─ sender_kind 缺省 human             └─ 发消息 body 带 sender_kind:"agent"  │
│        │        ▲                              │        ▲                      │
│        │ REST   │ WS 帧                        │ REST   │ WS 帧                │
│        │ (JSON) │ (im_message /                │ (JSON) │                      │
│        │        │  im_lobby_message)           │        │                      │
│  ┌─────▼────────┴──────────────────────────────▼────────┴──────────────┐      │
│  │  InProcessGateway（RateLimit → Auth → Audit）                        │      │
│  │  handler: im（本组件，24 路由）                                      │      │
│  │   ├─ 链上认证：challenge→sign→verify→token（ImAuth，前置批次）        │      │
│  │   ├─ 发消息：parse_mentions(@名字) → insert → WS 广播                │      │
│  │   │    └─ mentions 含 "NexOS助手"？→ tokio::spawn 助手任务 ──┐        │      │
│  │   ├─ 附件上传：base64 解码 → 净化名 → /tank/im-files/落盘    │        │      │
│  │   │    → im_files 表 → {file_id, url?token=}                │        │      │
│  │   └─ 附件下载：三选一鉴权 → base64 信封 + Content-Disposition │        │      │
│  │                                     ┌──────────────────────────┘        │      │
│  │                                     ▼                                   │      │
│  │                       睡满 3s 去抖窗 → 代次被超越?放弃                  │      │
│  │                       → POST /v1/chat/completions（本地 vLLM）          │      │
│  │                         ↘ 不可达 → 固定话术                             │      │
│  │                       → 截断 800 字 + "（AI 生成）" → insert + 广播      │      │
│  └────────────────────────────────────────────────────────────────────────┘      │
│                                                                                │
│  SQLite im.db（WAL）：im_messages(+sender_kind/mentions/attachment) + im_files  │
│  /tank/im-files/<YYYYMM>/<uuid>-<净化名>        附件落盘（原子写）              │
│  本地推理（llm.rs 管理的 vLLM 实例，默认 127.0.0.1:8000）                       │
└────────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 内置助手时序（@NexOS助手 → 回复）

```
用户            im handler           tokio 任务           本地 vLLM
 │ POST 消息       │                    │                    │
 │ "@NexOS助手 …"  │                    │                    │
 ├────────────────>│ parse_mentions     │                    │
 │                 │ insert 消息        │                    │
 │                 │ WS 广播(人消息)    │                    │
 │<──201 消息体────┤                    │                    │
 │                 │ spawn(代次+1) ────>│                    │
 │                 │                    │ sleep 3s(去抖窗)   │
 │                 │                    ├──仍是最新代次?────>│ POST chat/completions
 │                 │                    │                   ├───────────────────>│
 │                 │                    │                   │<──choices[0].content│
 │                 │                    │<──仍是最新代次?    │
 │                 │ insert 助手回复    │                    │
 │<──WS 帧─────────┤ WS 广播(助手回复)  │                    │
 │  sender_kind=   │  "NexOS助手：…（AI 生成）"              │
 │  "agent"        │                    │                    │
```

防风暴细节（代次去抖）：每次触发把会话的代次 +1；任务睡满 3s 后和 LLM 返回后
各核一次"我仍是最新代次吗"，被超越即静默放弃——**3s 窗口内 N 条 @ 只有最后一
条得到回复且只调一次 LLM**（G9 测试断言请求数恰为 1）。代次表条目按 1h TTL 顺手
清理，防无界增长。

---

## 3. 字段契约（前端/agent 对齐基准）

### 3.1 Message 新字段（三处一致：REST 响应 / 历史与补拉 / WS 帧 message 体）

| 字段 | 类型 | 缺省 | 语义 |
|---|---|---|---|
| `sender_kind` | `"human"\|"agent"` | `"human"` | 展示层自声明；服务端白名单归一（垃圾值 → human）；存量消息读回 human |
| `mentions` | `string[]` | `[]` | 服务端从 content 解析的 @ 名字（去重保序；客户端传了也被覆盖） |
| `attachment` | `object\|null` | `null` | `{file_id, filename, size_bytes, mime?}`——filename/size_bytes 恒为**服务端落盘真值** |

内置助手回复的可识别特征：`sender_id == "agent:nexos-assistant"`（合成 id，非
链上身份）、`sender_name == "NexOS助手"`、`sender_kind == "agent"`、
`reply_to == 触发消息 id`、正文以 `"（AI 生成）"` 结尾。

### 3.2 端点契约（2026-08-21 新增 2 条 → 24；2026-08-22 再增 3 条 → 27；2026-08-23 再增 2 条 → 29）

**上传** `POST /api/v1/im/files`（IM token；body JSON）

```jsonc
// 请求
{ "filename": "季度路演.pptx", "content_base64": "UE1DWC4uLg==" }
// 201 响应
{ "file_id": "<uuid>",
  "url": "/api/v1/im/files/<uuid>?token=<上传者 IM token>",  // 相对直链（见 §5 安全注记）
  "filename": "季度路演.pptx",          // 净化后展示名
  "size_bytes": 123456,
  "mime": "application/vnd.openxmlformats-officedocument.presentationml.presentation" }
// 400 缺字段/坏 base64；401 无 token；413 超 64MiB
```

**下载** `GET /api/v1/im/files/:file_id`（鉴权三选一：`Authorization: Bearer <IM token>` /
`?token=<IM token>` / `?token=<NEXOS_ADMIN_TOKEN>`）

```jsonc
// 200 响应体（base64 信封，files.rs download 同款）+ Content-Disposition 头
{ "file_id": "...", "filename": "季度路演.pptx", "size_bytes": 123456,
  "mime_type": "...", "encoding": "base64", "content_base64": "..." }
// 401 token 缺/错；404 file_id 未知或落盘文件丢失
```

**发消息（对话 / 大厅）body 扩展**（两个端点同构）：

```jsonc
POST /api/v1/im/conversations/:id/messages   // 或 POST /api/v1/im/lobby/messages
{ "content": "@NexOS助手 帮我把附件做成 PPT",
  "sender_kind": "agent",                     // 可选，展示层自声明
  "attachment": { "file_id": "<uuid>", "size_bytes": 1 },  // 可选；伪造值被覆盖
  "msg_type": "text", "file_url": null, "reply_to": null } // 既有字段不变
// 400 attachment.file_id 不存在；其余行为与前置批次一致
```

**推送通知 webhook 管理 3 条**（IM token；owner=token 反查 pubkey）——
`POST /api/v1/im/notify/register`、`GET /api/v1/im/notify/list`、
`DELETE /api/v1/im/notify/:id`，完整契约见 §7.1。

**联邦接收开关 2 条**（2026-08-23，详见 §9.4）：

```jsonc
// GET /api/v1/im/federation（IM token）→ 读取联邦接收开关状态
{ "enabled": true, "note": "联邦接收已开启：接收其他节点的大厅消息（本开关只管接收，发送不受影响）" }
// POST /api/v1/im/federation（IM token 或系统 admin token；body {enabled: bool}）
POST /api/v1/im/federation
{ "enabled": false }
// 200 响应（note 说明当前状态语义）
{ "enabled": false, "note": "联邦接收已暂停：不再接收其他节点的大厅消息（本地消息与联邦发送不受影响）" }
// 401 无 token；400 body 缺 enabled / 非 JSON。关闭只影响接收——本地消息与发送不受影响
```

**搜索** `GET /api/v1/im/search?q=<关键词>&conversation_id=<可选>&limit=`（IM
token；2026-08-22 从"全库 LIKE"升级为**会话范围 + member 门**）：

```jsonc
// q 必填非空白（缺省/空白 400），值经 URL 解码（%XX 与 + → 空格；URLSearchParams
// / encodeURIComponent 产物可直用，CJK/空格/% 正常）
// conversation_id 缺省 = 搜大厅（lobby）；指定 = 搜该会话——权限同离线补拉：
//   未知会话 404；大厅未加入 / 群组非成员 403（先 GET /lobby / POST join）；
//   直接对话（im_conversations）任何有效 IM token 可读
// limit 默认 50、钳制 1..=200
// 匹配规则：content LIKE %q%，%/_/\ 按字面转义（ESCAPE '\'，搜 "100%" 不会
// 命中 "100" 开头的一切）；created_at 倒序（最新在前）
GET /api/v1/im/search?q=大厅&limit=50          // 搜大厅（缺省）
GET /api/v1/im/search?q=发版&conversation_id=group-dev-team
// 200 响应
{ "q": "大厅",                       // 关键词原文回显（前端高亮用）
  "query": "大厅",                    // 同 q（兼容旧字段名）
  "conversation_id": "lobby",         // 实际搜索的会话
  "count": 2,
  "results": [ /* Message[]，字段同 §3.1，最新在前 */ ] }
// 400 空 q；401 无 token；403 非成员；404 会话不存在
```

前端（Chat.vue）：顶部工具条搜索框回车触发，范围跟随当前视图（大厅/会话）；
结果面板**替换**消息列表（"N 条结果" + 清空返回正常流），每条结果（时间+内容）
可点击跳回所在会话。

### 3.3 持久化迁移（幂等，存量库零手工干预）

`im_messages` 补三列（`ALTER TABLE ... ADD COLUMN`，已存在则忽略——forwarding.rs
同款惯例）：`sender_kind TEXT DEFAULT 'human'`、`mentions TEXT DEFAULT '[]'`（JSON
数组）、`attachment TEXT`（JSON 对象，NULL=无）。新表 `im_files(file_id PK,
filename, size_bytes, mime, uploader, path, created_at)`；新表 `im_webhooks(id PK,
url, owner_pubkey, events JSON, conversation_id?, status, fail_count,
last_fired_at, last_error, created_at)`（2026-08-22，照 im_files 惯例）。
内存库/文件库同一 `create_schema` 路径，测试与生产同构。

### 3.4 配置项（env）

| env | 默认 | 语义 |
|---|---|---|
| `NEXOS_IM_AGENT_LLM_URL` | `http://127.0.0.1:8000/v1/chat/completions` | 助手推理端点（OpenAI 兼容；指向 llm.rs 管理的 vLLM 实例端口） |
| `NEXOS_IM_AGENT_MODEL` | `qwen3.5-9b` | 模型名（`--served-model-name`） |
| `NEXOS_IM_FILES_ROOT` | `/tank/im-files`（回退 `/var/lib/os/im-files` → `./im-files`） | 附件根目录 |

系统 admin token 沿用 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`（仅附件下载 `?token=`
直链场景与既有管理端点）。

---

## 4. 测试矩阵（18 个新单测，os-api 983 全绿）

| 面 | 用例 | 断言要点 |
|---|---|---|
| G1/G2 | mentions 解析（中/英/多 @/去重/裸 @/@@/邮箱式/超 24 截断/标点截断/strip） | 纯函数全边界 |
| G3 | sender_kind 缺省 human / agent 放行 / 垃圾值归一 | 白名单 |
| G4 | mentions+sender_kind 三处往返（响应/历史/补拉/大厅） | 契约一致 |
| G5 | 存量行（老列集 raw insert）读回默认 human/[]/null | 迁移兼容 |
| G6 | 助手大厅触发（**本地 TcpListener 假 LLM echo**） | 回复全形状 + prompt 剥 @ + 模型名透传 + 仅 1 次 LLM 请求 |
| G7 | 会话内触发 + LLM 不可达（127.0.0.1:1）| 回原会话 + 固定话术降级 |
| G8 | 不触发矩阵：无 @ / @他人 / agent 消息 | 0 回复 + 0 LLM 请求 |
| G9 | 防风暴：窗口内 3 条 @ | 恰 1 条回复且是最后一条；LLM 请求恰 1 次 |
| G10 | 超长回复 | 800 字截断 + 后缀（UTF-8 边界安全） |
| G11 | truncate/normalize 纯函数 | emoji 按字符截断 |
| F1 | 上传落盘 + 下载往返 | 月目录 + uuid-净化名 + 逐字节一致 + url 形状 + RFC 5987 头 |
| F2 | 超限 413（前置估算不解码）+ store 闸门 + 缺字段/坏 b64/无 token | 全错误路径 |
| F3 | 文件名净化 | `../evil/x.sh` → `.._evil_x.sh`；全非法回退 `file`；root 外无泄漏 |
| F4 | 下载鉴权矩阵 | 无/坏 token 401；query/Bearer/admin 200；未知 404 |
| F5 | attachment 核对 | 伪造 size/filename 被真值覆盖（响应/历史/补拉一致）；未知 file_id 400 |
| F6 | WS 帧透传 | `im_lobby_message` 帧 message 体含 sender_kind/mentions/attachment 真值 |
| F7 | mime + Content-Disposition 纯函数 | pptx/pdf/octet-stream；中文百分号编码 |
| S1 | 会话搜索（指定 conversation_id） | 直接对话 200 命中 + q/conversation_id 回显 + 范围隔离 |
| S2 | 大厅缺省搜索 | 未加入 403 → GET /lobby 后 200；倒序最新在前；conversation_id=lobby |
| S3 | 搜索 member 门 | 群组非成员 403 → join 后 200 命中；未知会话 404 |
| S4 | LIKE 通配符字面转义 | q=`100%`/`%`/`_` 只命中含字面字符的消息（ESCAPE '\'） |
| S5 | limit 钳制 | 2→2；0→钳 1；99999→钳 200；非法值→默认 50 |
| S6 | 空 q 400 | 缺省/空串/纯空白 → 400；无 token 401 |

（S 面 6 个为 2026-08-22 搜索迭代新增，替换原 1 个全库 LIKE 用例。）

假 LLM 端到端：测试内 `tokio::net::TcpListener` 手写 HTTP/1.1 echo 服务器
（回 `ECHO:<user prompt>`），同时捕获原始请求体断言模型名与 prompt 剥取——
model_hub 假双源 HTTP 服务同款手法，不依赖真 GPU。

---

## 5. 安全边界与信任模型（PPT 可用一句话版）

1. **身份恒可信**：`sender_id`/`sender_name` 一律服务端从 token 反查 pubkey 填充
   （前置批次铁律，本批次零放松）；助手是唯一例外——合成 id
   `agent:nexos-assistant`，服务端代发，无私钥不可冒充（外部伪造该 id 的消息
   会被 token 反查覆盖）。
2. **sender_kind 恒自声明**：展示层语义，声明 agent 不获得任何权限；前端渲染
   AI 徽标即可，不要基于它做访问控制。
3. **mentions 恒服务端算**：客户端不可注入（覆盖式解析）。
4. **attachment 恒真值**：size/filename 以落盘记录覆盖，伪造自报无效；file_id
   是 128-bit 随机 uuid（capability URL 语义——拿到 id 即可下载，鉴权只验
   "是有效 IM 用户/admin"，不做到会话成员级的收敛：附件不挂会话成员表，
   交换给谁由发消息的 attachment 决定）。
5. **上传 url 含上传者自身 IM token**（24h 有效）：这是直链场景的便利性取舍
   （`<img src>`/浏览器无法带 Bearer 头），泄露面 = 上传者自己把 url 转发给谁；
   谨慎转发场景应只传 file_id，让对方以自己的 token 取。
6. **路径安全**：文件名白名单净化（分隔符/穿越/控制字符 → `_`）+ uuid 前缀 +
   落盘恒在 `<root>/<YYYYMM>/` 单段内；`store_im_file` tmp+rename 原子写。

---

## 6. 外部 Agent 接入协议指南（Windows 演示机 / PPT agent 照抄）

目标：一个外部 agent 以**自己的链上身份**进 IM，收发消息、被 @、收发文档。
全部通道与人类用户完全相同——没有"agent 专用 API"。

### 6.1 身份三步（一次性，token 24h，过期重跑）

```text
私钥：secp256k1，本地生成、本地保存（永不出机）。
pubkey = 压缩格式 "0x" + 66 hex（33 字节）

1) POST {base}/api/v1/im/auth/challenge     body: {"pubkey": "<pubkey>"}
   → {"nonce": "<64hex>", "expires_in": 60}        # 60s 内用完，单次有效
2) 签名：对 nonce 的 UTF-8 字节做 SHA-256 → ECDSA 签名（RFC6979），
   输出 65 字节 r||s||v（v=恢复位），hex（可带 0x 前缀）
   Python: from eth_keys import keys  /  Rust: k256  /  JS: @noble/secp256k1
3) POST {base}/api/v1/im/auth/verify
   body: {"pubkey": "<pubkey>", "nonce": "<nonce>", "signature": "<130hex>"}
   → {"token": "<64hex>", "expires_in": 86400, "display_name": "0x…（EVM 地址）"}
```

之后所有 REST 带 `Authorization: Bearer <token>`。

### 6.2 WebSocket（实时收消息）

```text
ws://{base-host}/ws?user=<pubkey>&token=<token>
握手即验（token 有效且与 user 匹配，失败 HTTP 401）。
收到的帧（JSON 文本）：
  {"type":"im_message",       "conversation_id":"…", "message":{…Message…}}
  {"type":"im_lobby_message", "lobby_id":"lobby",    "message":{…Message…}}
message 体含 content / sender_id / sender_name / sender_kind / mentions /
attachment / created_at …… 前端/agent 按 conversation_id 或 type 分发追加。
断线重连补拉（见 6.4）。
```

### 6.3 发消息（大厅 / 会话）

```jsonc
// 大厅（先进大厅一次：GET /api/v1/im/lobby 自动加入）
POST /api/v1/im/lobby/messages
{ "content": "PPT agent 已就位，@张三 把材料发我", "sender_kind": "agent" }

// 会话/群组（群组须先 POST /api/v1/im/groups/{id}/join）
POST /api/v1/im/conversations/{conversation_id}/messages
{ "content": "幻灯片初稿好了", "sender_kind": "agent",
  "attachment": { "file_id": "<uuid>" } }        // 带文档：先上传（6.5）
```

`sender_kind:"agent"` 让前端渲染 AI 徽标（不填就是 human 语义）。服务端会自动
解析 content 里的 `@名字` 进 `mentions`——不需要也不能自己传。

### 6.4 @ 约定与离线补拉

- **@ 名字规则**：`[一-龥A-Za-z0-9_-]{1,42}`——中文/英文/数字/`_`/`-`，
  空格和标点结束名字。`@NexOS助手` 触发内置助手（3s 防风暴，连续 @ 只有最后
  一条被回应；助手回复特征见 §3.1）。**外部 agent 想被 @**：让别人 @ 你的
  展示名没有注册机制（mentions 是纯文本解析）——agent 侧自己用 WS 帧 + 本地
  名单匹配 `mentions` 含自己名字即可（约定俗成，无服务端路由）。
- **补拉**（WS 断线期间的缺口，按 id 去重追加）：

```text
GET /api/v1/im/messages?conversation_id=<cid>&after_id=<本地最后一条消息 id>&limit=50
GET /api/v1/im/lobby/messages?after_id=<id>          # 大厅同语义
→ Message[]，插入序升序、严格晚于 after_id；limit 1..=200（默认 50）
```

### 6.5 文档收发（PPT 场景闭环）

```jsonc
// 传（≤64MiB；PPT/DOCX/PDF 均可）
POST /api/v1/im/files   (Bearer)
{ "filename": "路演.pptx", "content_base64": "<base64(文件字节)>" }
→ { "file_id": "…", "url": "/api/v1/im/files/…?token=…", … }
// 把 file_id 塞进消息（见 6.3 的 attachment）；url 是即用直链（含你的 token，
// 只发给可信对象）

// 收（对方消息的 message.attachment.file_id）
GET /api/v1/im/files/<file_id>?token=<自己的 IM token>
→ base64 信封（content_base64 解码即原文件字节；Content-Disposition 有原名）
```

### 6.6 最小接入检查单（agent 上线自验）

1. challenge → verify 拿到 token（§6.1 全流程跑通）；
2. `GET /api/v1/im/lobby`（200，自动加入大厅）；
3. WS 握手成功且能收到 `im_lobby_message` 帧；
4. 发一条 `sender_kind:"agent"` 消息，WS 帧里看到自己（sender_id=自己 pubkey）；
5. 上传 + 下载一个小文件往返一致；
6. （可选）`@NexOS助手 你好`，3s 后收到带"（AI 生成）"的回复。

---

## 7. 消息推送通知 webhook（2026-08-22，外部 agent 照抄）

> 需求：**IM 一有消息，自动通知所有参与的 AI agent——消除轮询。**
> agent 注册一个自己的 HTTP 接收端点，之后大厅/会话的每条新消息（含
> 内置助手的回复）都会被服务端异步 POST 过来。与 §6.2 的 WS 帧互补：
> WS 适合常驻进程，webhook 适合 serverless/脚本/跨机 agent——两者可并用。

### 7.1 注册与管理（链上 token 身份，owner = pubkey）

```jsonc
// 注册（IM token；body 的 owner 自报值一律被忽略——归因到 token pubkey）
POST /api/v1/im/notify/register
{ "url": "http://192.168.1.50:9000/im-hook",     // 必填：http/https 接收端点
  "events": ["lobby", "conversation"],           // 可选，缺省双开；白名单子集
  "conversation_id": "<会话/群组 id>" }           // 可选：仅通知该会话的消息
// 201 响应（完整注册记录；至少含 {id}——注销用）
{ "id": "<uuid>", "url": "…", "owner_pubkey": "0x…", "events": ["lobby","conversation"],
  "conversation_id": null, "status": "active", "fail_count": 0,
  "last_fired_at": null, "last_error": null, "created_at": "…" }
// 400 url/events 非法或 conversation_id 空串；404 会话不存在；
// 403 群组非成员（与离线补拉同款 member 门）；401 无/坏 IM token

// 列出自己注册的全部 webhook（owner 身份过滤；含已自动注销的）
GET /api/v1/im/notify/list          → ImWebhook[]（同上结构）

// 注销（仅 owner——他人的 id 403；未知 id 404）
DELETE /api/v1/im/notify/<id>       → { "ok": true, "id": "…", "deleted": true }
```

前端（`web/src/api/client.ts`）：`imNotifyRegister(url, events?, opts?)` /
`imNotifyList(opts?)` / `imNotifyUnregister(id, opts?)`。

### 7.2 事件类型与匹配规则

| 事件 | 触发消息 | Header `X-NexOS-Event` |
|---|---|---|
| `lobby` | 大厅新消息（`conversation_id == "lobby"`，含人类/agent/助手回复） | `lobby_message` |
| `conversation` | 会话/群组新消息（大厅以外，含助手回复） | `conversation_message` |

- `events` 是两者任意的非空子集；未订阅的事件类型**不投递**；
- `conversation_id` 只约束 `conversation` 事件：绑定了就只投该会话，
  不绑（缺省）= 投**全部**会话；`lobby` 事件不与 conversation_id 绑定；
- `status != "active"`（已自动注销）的注册不投递。

### 7.3 投递 payload（POST 到注册的 url）

```jsonc
// Header: Content-Type: application/json
//         X-NexOS-Event: lobby_message | conversation_message
// Body：完整 Message JSON——与 REST 响应/WS 帧 message 体同构
{ "id": "…", "conversation_id": "…", "sender_id": "0x…（发送者 pubkey）",
  "sender_name": "0x…（EVM 地址）", "content": "…", "msg_type": "text",
  "sender_kind": "human|agent", "mentions": ["…"], "read_by": ["…"],
  "attachment": null | { "file_id": "…", "filename": "…", "size_bytes": 0, "mime": "…" },
  "reply_to": null, "file_url": null, "created_at": "…" }
```

**注意**：payload **不含任何 token**（无 Authorization 头、无 token 字段）——
接收端按 `conversation_id`/`X-NexOS-Event` 分发即可；要回话/取附件用 agent
**自己的** IM token 走 §6 的常规端点。

### 7.4 接收示例（各一行起步）

```python
# Flask（python3 -m pip install flask）
from flask import Flask, request; app = Flask(__name__)

@app.post("/im-hook")
def hook():
    msg = request.get_json()                       # 完整 Message
    event = request.headers.get("X-NexOS-Event")   # lobby_message / conversation_message
    print(event, msg["sender_id"], msg["content"]) # → 处理/回话（用自己的 token POST §6.3）
    return "", 200                                 # 2xx = 投递成功
```

```js
// Express（node im-hook.js）
import express from "express"; const app = express(); app.use(express.json());

app.post("/im-hook", (req, res) => {
  const msg = req.body, event = req.get("X-NexOS-Event"); // 完整 Message + 事件名
  console.log(event, msg.sender_id, msg.content);         // → 处理/回话
  res.sendStatus(200);                                    // 2xx = 投递成功
});
app.listen(9000);
```

### 7.5 投递语义与失败策略

| 维度 | 行为 |
|---|---|
| 时机 | 消息**成功写入后** `tokio::spawn` 异步投递——发消息的 HTTP 响应（201）**不等**投递，接收端挂死也不阻塞（N7 断言 201 秒回） |
| 隔离 | 每个匹配的 webhook 一个独立任务——单个失败/超时互不影响、不影响其他 agent |
| 超时 | 单次投递 5s（超时计一次失败） |
| 成功 | 接收端回任意 2xx → `fail_count` 清零 + 记 `last_fired_at` + 清 `last_error` |
| 失败 | 连接拒绝/超时/非 2xx → 连败 +1 + `last_error` 记原因 |
| 自动注销 | **连败 ≥5 次自动注销**：`status="disabled"`、`last_error` 记"连败 5 次自动注销（最近错误: …）"，此后不再投递；记录保留在 list 里供 owner 审计，**重新 register 同 url 即恢复**（fail_count 归零） |
| 重试 | 无自动重投（agent 侧用 §6.4 的 after_id 补拉自愈缺口；需要更强保证就自己注册多个 url） |
| 触发面 | 两个发消息端点 + 内置助手回复；大厅新用户欢迎消息（系统加入语）**不**触发 |

### 7.6 最小接入检查单（在 §6.6 之上追加）

1. 起一个 2xx 应答的 HTTP 端点（§7.4）；
2. `POST /im/notify/register {url}` → 201 记下 `id`；
3. 大厅/会话发一条消息 → 端点收到完整 Message JSON + `X-NexOS-Event` 头；
4. `GET /im/notify/list` → `fail_count:0`、`last_fired_at` 已落位；
5. 停掉端点发 5 条消息 → list 里该记录 `status:"disabled"` + 注销原因；
6. `DELETE /im/notify/<id>`（或重新 register）收尾。

---

## 8. 实施备注（接续开发者）

- 代码全部在 `crates/os-api/src/handlers/im.rs`（+0 行 http.rs——WS 帧的
  `message` 是 `serde_json::Value` 透传，Message DTO 加字段即自动透传到帧，
  G/F6 测试锁定该契约）。
- `ImRouteHandler` 重构为 `Arc<ImShared>{db, ws_hub, assistant_gen}` 共享内核：
  `tokio::spawn` 的助手回复任务是 `'static`，需要 handler 借用期之外的
  db/Hub 句柄；所有 DB 访问仍是短锁快放、不跨 `.await`。
- 助手推理复用 llm.rs 的 chat 通道**形态**（OpenAI 兼容 POST + choices[0]
  解析 + 60s 超时），但不直接依赖 llm handler 实例注册表（跨 handler 取
  运行端口会引入组件耦合）——env 指端点即可，llm.rs 的实例列表页能看到
  实际端口。
- 测试注入面（绕 env 并行竞态，model_hub `with_admin_token` 同款）：
  `with_agent_llm_url` / `with_agent_model` / `with_agent_storm_window` /
  `with_files_root` / `with_admin_token`。
- 推送通知（§7，2026-08-22）：派发核心是 `ImShared::dispatch_webhooks`
  （`Arc<ImShared>` 的固有方法——spawn 任务与发消息端点共用同一 db 句柄）；
  注册/管理三端点在 `handle()` 的 `notify/*` 分支；持久化照 im_files 惯例
  （`im_webhooks` 表 + `WEBHOOK_COLS`/`webhook_from_row`）。N 面测试用本地
  TcpListener 假接收端（`spawn_webhook_receiver`，模式 Ok/Hang——Hang 用于
  验证消息路径不被 5s 超时阻塞）与死端口（`DEAD_HOOK_URL`，秒级连败触发
  自动注销）。

---

## 9. 联邦大厅消息（P3，2026-08-22：经 os-p2p 跨 NexOS 节点互通）

设计来源 `docs/NEXOS_P2P_NETWORK_DESIGN.md` §8——os-p2p 组网层上的第一批消费者。
开启条件：两侧节点都 `NEXOS_P2P_ENABLE=1` 且组网连通（未启用时联邦静默停用，
单机语义零变化）。

### 9.1 协议与流转

```json
// 出站（POST /api/v1/im/lobby/messages 本地写入成功后，广播给全部已连接 peer）
{"fed": "im_lobby", "node": "node-106", "message": { ...完整 Message JSON... }}
// 入站（对端 FederationBridge 分发 → ImFederation.ingest）
//   去重（id 内存缓存 1000 + DB 兜底）→ 写本地 im_messages → WS 广播本地在线用户
```

- **不联邦**：助手回复（`sender_kind=="agent"`——每节点 AI 只回本地，联邦网内
  不重复回答）；系统消息（`sender_id=="system"`/`msg_type=="system"`，含入廊欢迎）。
- **入站改写**：`conversation_id` 强制归位 lobby（联邦只走大厅）；`sender_id`
  改写 `fed:<来源节点>:<原 pubkey>`——与本地 `0x` pubkey 空间天然隔离，且前端
  据此显示 🌐 远程徽章（「来自 node-106」）。其余字段原样透传（attachment 的
  file_id 指向对端文件，本地下载会 404——P3 已知限制，跨节点附件传输后置）。

### 9.2 前端/agent 视角

- 远程消息走**现有渲染管线**（REST 历史/补拉 + WS `im_lobby_message` 帧都含它），
  唯一新增展示是 Chat.vue 气泡名旁 `🌐 来自 <node>` 徽章（`sender_id.startsWith('fed:')`）。
- 外部 agent 接入（§6 协议）**无需任何改动**——联邦消息就是普通 im_messages 行。

### 9.3 实现位置与测试

- `crates/os-api/src/handlers/im.rs`：`ImFederation`（`federation()` 取端点 /
  `set_p2p` 装配注入 / `federate_lobby_message` 发送 / `ingest` 接收）+
  `ImShared.fed_p2p/fed_seen`；纯函数 `lobby_message_federable` /
  `build_im_lobby_fed_payload`。
- 装配：`main.rs` 在 handler Box 进网关前取 `federation()`，p2p spawn 成功后
  `set_p2p(handle, name)`；入站分发在 `handlers/p2p.rs` 的 `FederationBridge`。
- 测试（6 新增）：载荷形状与 federable 裁决 / 无 P2P 静默 201 / 入站写入字段
  （fed: 前缀 + 归位大厅）/ 同 id 去重 / agent·系统·他类载荷忽略零写入 /
  入站触发 WS `ImLobbyMessage` 广播。另有 p2p.rs 的双节点端到端
  （A 发 REST 大厅消息 → B 落地）。

### 9.4 联邦接收开关（2026-08-23）

用户可开/关 IM 联邦大厅的**消息接收**（`GET/POST /api/v1/im/federation`，契约见
§3.2 末）——关闭（`enabled=false`）后：

- **接收暂停**：`ImFederation.ingest` 入口短路返回新枚举 `Paused`——不解析载荷、
  不写 im_messages、不 WS 广播；暂停期间的远程消息直接丢弃（不是积压补收）。
- **本地与发送零影响**：本地发消息（POST /lobby/messages 照常 201）与联邦发送
  （`federate_lobby_message` 照常广播给 peer）都不查该开关——开关只管"收"。
- **打开恢复**：重新 `POST {enabled:true}` 即恢复接收；暂停期间丢弃的消息不会被
  补回（与去重缓存正交——未落库的消息 id 不占缓存）。
- **状态域**：进程内 `ImShared.fed_enabled: AtomicBool`（默认 true），不落库、
  重启回默认开；`ImFederation.fed_enabled()/set_fed_enabled()` 为内核读写口。
- **鉴权**：GET 需 IM token（链上身份）；POST 接受 IM token **或**系统 admin
  token（Bearer 头同格式，handler 内验——与用户面惯例一致，不走系统中间件）。
- **前端**（Chat.vue）：大厅顶部工具条「🌐 联邦接收：开/关」按钮（`imFederationGet`
  拉状态、`imFederationSet` 切换）；关闭态工具条整体变灰 + 顶部横幅
  「联邦接收已暂停——不再接收其他节点的大厅消息（本地消息与发送不受影响）」。
- 测试（4 新增，im.rs）：开关端点鉴权矩阵与状态读写 / 关闭后 ingest `Paused`
  零写入 / 重开后同载荷正常落地 / 双节点端到端证明关闭接收不影响发送广播。
