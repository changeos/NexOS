# AGENT_COORDINATION —— agent 协调组件（agent-coord）

> 2026-08-24 · 组件 `agent-coord`（`crates/os-api/src/handlers/agent_coord.rs`）
>
> 设计来源：NexHub nexos-test 仓库 README §2。测试 agent 诊断「IM 当任务通道」
> 缺四项能力：**定向路由 / 可靠投递 / 在线状态 / 任务状态机**。本组件补齐
> 前三项的最小闭环；任务状态机与 webhook 重试留待下期（见 §7）。

## 1. 组件概述

用户需求原话：「做 agent 协调组件，当 agent 被 at 时给对应的 agent 通讯，
并申明与 agent 交流时要 at 对方」。组件把这条需求落成两条规矩：

1. **@ 即定向投递 + 双写收件箱（不丢 @）**：IM 群消息里 `@<agent 名字>`
   不只靠群广播——协调层为每个被命中的**注册** agent 生成一条定向投递
   记录，且**无论在线离线一律落收件箱**（nexos-test BUG-agentcoord ④：
   新鲜度窗口内强杀客户端（kill -9/断电，无 close 帧）订阅残留仍判在线，
   纯 ws 投递无人消费即丢——双写后重启经 inbox 可回看）：
   - agent 的 pubkey 在 WS hub 有**新鲜**订阅（最后客户端活动距今 ≤
     新鲜度阈值，见 §5；**在线**）→ `delivered=ws`（WS 广播本身已即时
     送达，收件箱记录是重启后的回看凭据——在线消费方处理完应及时
     ack，默认拉取只回未读）；
   - **离线**且配了 `callback_url` → 收件箱 + **webhook 回调**
     （POST，body 见 §4；成功后记录升级 `delivered=webhook`）；
   - **离线无 callback** → 仅收件箱（`delivered=inbox`），agent 侧经
     `GET /api/v1/agents/<name>/inbox?after=<seq>` 增量拉取；
   - **去重**：同 `message_id` 对同 agent 不重复插入（消息重放/挂钩
     重入下收件箱不翻倍）。
2. **协议声明（申明）**：与 agent 交流必须 @ 对方，未 @ 的消息 agent 不认领。
   - 协议全文经 `GET /api/v1/agents/protocol` 供 agent 自举读取（纯 JSON）；
   - agent **注册时**自动向它加入的所有群组（按 pubkey 反查群成员表）发
     一条系统声明消息（`sender_kind="system"`，服务端直插 `im_messages` +
     WS 广播）；**同一 agent 同一群只声明一次**（`declared_groups` 落档）。

投递规则补充：未注册的名字不投递；agent 自己发消息 @ 自己不投递（防自回环）；
system 消息（声明自身）不参与路由。

## 2. 拓扑

```
                       ┌────────────────────────────────────────────────┐
                       │              os-api 网关（main.rs 装配）          │
                       │                                                │
  ┌──────────┐  HTTP   │  ┌──────────────┐   install_hook(内核 Arc)      │
  │ 人类用户  │ ───────▶│  │ im handler   │────────────┐                 │
  │ (浏览器)  │         │  │ (im.rs)      │            ▼                 │
  └──────────┘         │  │  消息落库     │   ┌──────────────────┐        │
                       │  │  WS 广播 ────┼──▶│ on_im_message()  │        │
  ┌──────────┐  HTTP   │  │      │一行挂钩│   │ (进程级钩子,no-op │        │
  │ 外部 agent│ ───────▶│  └──────┼───────┘   │  未装配时零开销)  │        │
  │ (REST)   │         │         │           └────────┬─────────┘        │
  └────▲─────┘         │         │                    │ mentions 命中     │
       │ inbox/ack/    │         │                    ▼   注册表          │
       │ register      │         │           ┌──────────────────┐        │
       │               │         │           │  agent-coord     │        │
       │               │         │           │  CoordCore       │        │
       │               │         │           │  · 注册表(JSON)  │        │
       │               │         │           │  · 收件箱(seq)   │        │
       │               │         │           └───┬──────┬───────┘        │
       │               │         │               │      │                │
       │               │         │   在线判定     │      │ 离线回调        │
       │               │         │   (by_user)   │      │ (3s 超时)      │
       │               │         ▼               ▼      ▼                │
       │               │  ┌────────────┐  ┌─────────┐ ┌──────────┐       │
       │               │  │  WS hub    │  │ (在线即 │ │ webhook  │       │
       │               │  │  by_user───┼─▶│  广播已 │ │ POST     │       │
       │               │  └────────────┘  │  达)    │ │callback_ │       │
       │               │                  └─────────┘ │url       │       │
       │               │  ┌──────────────────────┐    └──────────┘       │
       │               │  │ ImCoordBridge（桥）    │◀── 注册时声明直插      │
       │               │  │ · post_system(群,文案) │    im_messages       │
       │               │  │ · groups_of_member(pk)│    + WS 广播          │
       │               │  └──────────────────────┘                       │
       └───────────────┴────────────────────────────────────────────────┘
```

数据面三条通道（对应 nexos-test 诊断的三缺口）：

| 缺口 | 本组件补法 |
|------|-----------|
| 定向路由 | im 消息 `mentions`（服务端解析）命中注册表 → 按 agent 生成投递记录（不再只靠群广播） |
| 可靠投递 | 收件箱持久化（递增 seq + `after` 增量 + `ack` 确认）+ 可选 webhook（离线推） |
| 在线状态 | `GET /api/v1/agents` 的 `online` = pubkey 在 WS hub `by_user` 有**新鲜**订阅（订阅最后客户端活动距今 ≤ 阈值，缺省 120s，env 可调；半开/僵死连接的订阅残留不再误判在线，见 §5「新鲜度阈值」） |

## 3. 端点契约（component=`agent-coord`，6 条）

| method | path | 鉴权（开发期） | 请求 | 响应要点 |
|--------|------|------|------|---------|
| POST | `/api/v1/agents/register` | 公开；**admin Bearer 得覆盖特权**（见下三级语义） | `{name, pubkey?, callback_url?}` | 201 新建 / 200 覆盖；`declared_now`=本次声明群组数；**注册即触发协议声明**；畸形 JSON body **400**（客户端错误） |
| GET | `/api/v1/agents` | 公开 | — | `{agents:[{name, pubkey, online, callback_url(脱敏), declared_groups, created_at}], count}` |
| GET | `/api/v1/agents/protocol` | 公开 | — | `{component, version, protocol, endpoints}`（协议全文，agent 自举） |
| GET | `/api/v1/agents/:name/inbox?after=<seq>&include_acked=1` | 公开（发版前收紧 agent token） | `after` 缺省 0 | `{agent, after, include_acked, count, records}`，seq 严格大于 after，**升序（新在后）**；**默认只回未读**（`acked_at` 为空），`include_acked=1` 显式开历史；未知 agent 404 |
| POST | `/api/v1/agents/:name/ack` | 同上 | `{seq}` | `{agent, acked, seq}`——seq≤给定值的未读置 `acked_at`，**历史保留不删**（`include_acked=1` 可回看）；畸形 body 400 |
| DELETE | `/api/v1/agents/:name` | 公开（发版前收紧 admin） | — | `{deleted}`——连带收件箱；未知 404 |

字段校验：

- `name`：唯一键，`[a-z0-9-]`，1..=32 字符（im @mention 字符集的 ASCII 子集，
  保证 `@<name>` 一定能被 im.rs `parse_mentions` 解析出）；非法 400。
- `pubkey`：`0x` + 恰好 66 hex（33 字节压缩 secp256k1 公钥**格式校验**；
  协调层只拿它对 WS 订阅键精确匹配，不做点校验）；非法 400。
- `callback_url`：http/https、无空白、≤2048（复用 im webhook url 规则）；非法 400。

投递记录（`records[]` 行）：

```json
{
  "seq": 3,                       // 递增游标（inbox after / ack 用）
  "agent_name": "dev-agent",
  "message_id": "<im_messages.id>",
  "group_id": "<conversation_id>", // 触发消息所在群/会话
  "content": "@dev-agent ...(≤120 字摘要)",
  "delivered": "ws | webhook | inbox",
  "acked_at": null,                // POST ack 后置位
  "created_at": "RFC3339"
}
```

webhook 回调（离线 + 配了 callback_url 时，`X-NexOS-Event: agent_mention`，
reqwest 3s 超时，失败仅记日志**不重试**——重试下期）：

```json
{
  "type": "agent_mention",
  "agent": "dev-agent",
  "message": { ...完整 Message JSON（含 sender_kind/mentions/attachment，不含任何 token） },
  "inbox_url": "/api/v1/agents/dev-agent/inbox?after=2"
}
```

注册同名三级语义（nexos-test BUG-agentcoord ① 的安全版——文档原「同名即
覆盖」与代码 409 不符，现以代码语义为基准收紧并补运维通道）：

1. **同 pubkey**（或原条目未绑 pubkey）→ 幂等覆盖：`pubkey`/`callback_url`
   更新（未传字段清空），`created_at` 与 `declared_groups` 保留——重复
   注册**不重发声明**，200；
2. **异 pubkey 且无 admin** → **409 拒绝**（防重名劫持，先到先得不退）；
3. **异 pubkey + admin**（`Authorization: Bearer <NEXOS_ADMIN_TOKEN>`，经
   AuthMiddleware 解析为 Admin Principal）→ **覆盖换绑**：pubkey/callback
   换新，**declared_groups 与收件箱保留**、已声明群组不重发——私钥丢失/
   换机时管理员代为换绑的运维通道，200。

畸形 JSON body（缺字段/非对象）→ **400**（客户端错误，不污染 5xx 监控
口径，BUG-agentcoord ②）；register 与 ack 同口径。

## 4. 协议全文（`GET /api/v1/agents/protocol`）

> NexOS agent 协作协议：与 agent 交流必须 @对方（@\<name\>）。未 @ 的消息
> agent 不认领。@ 即定向投递：在线 WS 即时送达（收件箱同步留痕，消费后
> 应及时 ack——默认拉取只回未读），离线进收件箱
> （GET /api/v1/agents/\<name\>/inbox?after=）+ 可选 webhook。执行完成回帖
> 群内并引用原任务。

注册时直插群组的系统声明消息（`sender_kind="system"`）：

> 📢【agent 协作协议】\<name\> 已注册。与它交流请 @\<name\>——被 @ 才会定向
> 投递（在线 WS / 离线收件箱+webhook），未 @ 的消息 agent 不认领。

## 5. 持久化与环境变量

| env | 缺省 | 说明 |
|-----|------|------|
| `NEXOS_AGENTS_FILE` | `/tank/os-data/agents.json` | 注册表 + 收件箱 JSON（原子写：先 `.tmp` 再 rename；目录自动创建；缺失/损坏 → 空表降级） |
| `NEXOS_AGENTS_ONLINE_STALE_SECS` | `120` | online 判定**新鲜度阈值**（u64 秒；缺失/非法/0 回落缺省）。订阅最后**客户端→服务端**活动距今超过阈值 → `online=false`（半开/僵死 WS 连接兜底：客户端 ping_interval=25s + ~5 倍余量）。只影响判定**不删订阅**（清理仍归断连路径）；WS 读循环收到任意客户端帧（含协议层 Ping/Pong）即刷新；出向发送不计（半开 TCP 下写进发送缓冲仍"成功"，不构成对端存活证据）。构造时读定，改动需重启进程 |

**新鲜度阈值**的来历：BUG-dev-standby-ws-silent-drop「关联」段——半开/僵死
连接会让 WS hub 的 `by_user` 订阅长期残留，对端实际已死而服务端视角仍
`online:true`（dev-standby 静默失联期间实测在线误报）。客户端已修
（ping_timeout=10），服务端以订阅级过期兜底。多租户影响：时间戳挂在通用
WS hub 上，IM 网页端等既有订阅者只在读路径多一次锁内纳秒级字段赋值，
广播/定向推送语义不变。

## 6. 与 im 组件的耦合（最小侵入 + 注入式）

- **出向**（im → agent-coord）：im.rs 在消息落库 + WS 广播后**一行**调用
  `crate::handlers::agent_coord::on_im_message(&msg)`（共两处挂钩：会话/群
  发消息 `POST /conversations/:id/messages`、大厅发消息 `POST /lobby/messages`）。
  钩子是进程级单例，main.rs 装配 agent-coord 时 `install_hook(core)` 注入；
  未装配时 no-op（单测/独立 im 部署零开销）。
- **入向**（agent-coord → im）：`im::ImRouteHandler::coord_bridge()` 产出
  轻量桥 `ImCoordBridge`（`Arc<ImShared>` 句柄，`federation()` 同款手法），
  只暴露两个最小能力：`post_system(cid, content)`（声明系统消息直插 +
  广播）与 `groups_of_member(pubkey)`（群成员反查）。handler 之间不直接
  持引用，全部经 main.rs 装配注入。

main.rs 装配（摘要）：

```rust
let im_coord_bridge = im_handler.coord_bridge();      // Box 前取桥
// ... im 注册 ...
let agent_coord = AgentCoordRouteHandler::new()
    .with_ws_hub(gw.ws_hub())                         // 在线判定
    .with_im_bridge(im_coord_bridge);                 // 声明直插
os_api::handlers::agent_coord::install_hook(agent_coord.core()); // im 挂钩
gw.register_component("agent-coord", Box::new(agent_coord)).await;
```

## 7. 与 nexos-test README §2 设计的对应关系

| nexos-test 诊断缺口 | 本期状态 | 实现 |
|---------------------|---------|------|
| 定向路由 | ✅ 已做 | `mentions` 命中注册表 → 投递记录（`route_message`，经 im 挂钩触发） |
| 可靠投递 | ✅ 已做（最小闭环） | 收件箱持久化（**在线/离线双写**——强杀场景 @ 不丢）+ `after` 增量 + 默认只回未读（`include_acked=1` 开历史）+ `ack` 确认 + 可选 webhook（离线推，不阻塞消息路径） |
| 在线状态 | ✅ 已做 | `online` = pubkey 在 WS hub `by_user` 有**新鲜**订阅（`GET /api/v1/agents` 列表；最后客户端活动 ≤ 阈值内，缺省 120s，env 覆盖） |
| 任务状态机 | ⛔ 未做（下期） | 无任务领取/进行中/完成状态流转；本期仅投递 + 确认 |
| webhook 重试 | ⛔ 未做（下期） | 失败仅记日志（连败降级/退避重试沿用 im notify 的设计下期接） |

## 8. 测试（`cargo test -p os-api --lib agent_coord`，25 个）

覆盖：路由形状 / 注册幂等覆盖 / **同名三级语义**（同 pubkey 幂等、异 pubkey
409、admin Principal 覆盖换绑保 declared_groups+收件箱）/ **畸形 JSON 400**
（register+ack）/ 非法 name·pubkey·callback 400 / 在线判定
（造 WS 订阅 + **新鲜度阈值**：订阅刚 touch 在线、伪造 last_active 老于阈值
离线且订阅不删、`NEXOS_AGENTS_ONLINE_STALE_SECS` env 覆盖生效、无 pubkey
恒离线）/ 投递三态（在线 ws、离线+callback 经 **std TcpListener mock
HTTP listener** 收到 POST 并升级 webhook、离线无 callback 仅 inbox）+
**双写·去重·ack 过滤**（在线 @ 落收件箱 delivered=ws、同 message_id 不重复
插入、ack 后默认不回 include_acked=1 回看）/ 未注册·自提及·system 不投递 /
inbox after 增量 + **默认只回未读·include_acked=1 开历史** / ack 历史保留 /
协议文本端点 / 声明同群一次（重复注册不重发，经真实 im 链上身份登录建群
断言）/ 无 pubkey 跳过声明 / 删除连带收件箱 / JSON 持久化重启读回（原子写
不留 .tmp）/ im 挂钩端到端（install_hook 后经 im REST 发消息 → 收件箱命中）。
