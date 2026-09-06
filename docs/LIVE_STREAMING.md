# 直播（Live Streaming v1.1：流媒体中心直播 Tab + 本地/联邦大厅）

> 源码：`crates/os-api/src/handlers/live.rs`（`LiveRouteHandler` + `LiveHub` 扇出引擎
> + `LiveFedEndpoint` 联邦端点，组件名 `live`）· WS 挂载：`crates/os-api/src/http.rs
> ::build_router`（`/ws/live/{room_id}/{publish|view}`）· 前端：
> `crates/os-api/web/src/components/LivePanel.vue`（流媒体中心 `/streaming` 的
> 「直播」Tab 挂载；原独立「直播」桌面应用已移除，旧 `/live` 重定向到
> `/streaming?tab=live`）· 联邦桥：`handlers/p2p.rs::FederationBridge`（live 分支）·
> 登记：2026-08-31（v1 本节点直播；v1.1 并入流媒体中心 + 联邦）

## 1. 功能说明

「流媒体中心 → 直播」Tab：**两段式大厅**——

- **本地大厅**：本节点房间（可开播）。主播浏览器采集（屏幕 / 摄像头）→
  WebSocket 上行 webm 媒体块 → 服务端**内存扇出** → N 个观众 WebSocket 下行 →
  浏览器 MSE（MediaSource + SourceBuffer）播放。
- **联邦大厅**：远端 NexOS 节点的房间（经 os-p2p overlay 的 `live_lobby` 宣告
  合并，显示来源节点名/观看数/状态）。点击观看走**跨节点中继**：观众节点向
  源节点订阅，帧流经 overlay 定向回传注入本地扇出——**观众 WS 仍连本节点**，
  前端无感知差异。

技术红线（沿用 v1 设计定稿）：**纯 Web 技术栈，零原生依赖**——不引入
ffmpeg / WebRTC / mediaserver；跨节点分发复用 os-p2p overlay（加密 + 分块
base64，沿 transfer.rs 手法）。

## 2. 拓扑

### 2.1 本节点（v1 不变）

```
            ┌────────────────────────── 本节点 os-api（单进程） ──────────────────────────┐
            │                                                                             │
 主播浏览器  │   ┌─────────────────────────── 扇出引擎 LiveHub（内存） ──────────────┐     │
 ┌────────┐ │   │  房间表 Mutex<HashMap<room_id, RoomState>>                        │     │
 │屏幕/摄像头│ │   │   · publish_token（创建时签发，仅回传一次）                     │     │
 │MediaRec.│ │   │   · header 缓存（首个 webm chunk = init segment）               │     │
 │vp8+opus │──┼──▶│   · 订阅端注册表 viewer_id → 有界 mpsc(64)                     │     │
 │1s 切片   │WS │   │   · 真实计数：bytes_in/out · viewer_count · dropped/rejected  │     │
 └────────┘pub │   └───────┬──────────────────────────────┬───────────────────────┘     │
   ↑ REST 控制 │           │ try_send（满即丢帧保实时）      │ 连上即先重放 header         │
   │ POST/DEL │   ┌───────▼──────┐  ┌───────▼──────┐  ┌────▼─────┐                     │
   │  rooms   │   │ 观众 1 (WS view) │ │ 观众 2 (WS view) │ │ 观众 N (上限 200)│             │
   │  (admin) │   └───────┬──────┘  └───────┬──────┘  └────┬─────┘                     │
   ▼          │        MSE append        MSE append       MSE append                   │
 GET rooms ───┼── {local, federated} 两段式大厅（公开读，REST 控制面旁路扇出数据面）      │
 (公开)       │                                                                     │
            └─────────────────────────────────────────────────────────────────────────┘
```

### 2.2 联邦大厅 + 跨节点中继（v1.1 新增）

```
 节点 A（源：主播所在）                          节点 B（观众所在）
 ═════════════════════                          ═════════════════════
 主播浏览器 ──WS publish──▶ LiveHub(A)
                            │ 房间创建/结束/变更/30s 巡检
                            ├──broadcast─── live_lobby 宣告 ───────────▶ FederationBridge
                            │   (fed_broadcast 一跳)                        │ merge_announcement
                            │                                              ▼（按 fed_room_id 幂等合并，
                            │                                          联邦房间表 TTL 90s）
                            │                                              │
                            │                              B 观众浏览器 ──WS view──▶ LiveHub(B)
                            │                                              │ subscribe 未命中本地房间
                            │                                              ▼ → ensure_shadow_room
                            ◀──────定向──── live_relay_sub ◀─────────────────┤ （建影子房间 + 心跳 30s）
                            │ handle_relay_sub                             │
                            │ （源端中继订阅表；新订阅先重放 header）           │
 主播帧（tap 探针）           │                                              │
   ──▶ 有界通道(64，满丢帧) ──┤                                              │
                            │   分块（1 MiB/块，base64）                     │
                            ├──定向──── live_relay_frame (seq/ci/cn) ───────▶ 重组（集齐 cn 块）
                            │                                              ▼ relay_inject（影子房间扇出）
                            │                                              ▼ MSE append
 退订/结束/心跳超时(90s)：    │                                              │
   live_relay_unsub ─────────▶ 剪除订阅即停帧   ended 帧 ────────────────────▶ 影子房间收尾（观众 {"kind":"ended"}）
```

- **宣告（控制面）**：广播 `live_lobby`；按 `fed_room_id = <源节点短前缀 8 hex>:<room_id>`
  幂等合并（Inserted / Refreshed），`status=ended` 立即出表，TTL 90s 无刷新剔除
  （房间是短暂状态，不同于 NexHub 大厅条目的持久语义——无 DB 无 seed）。
- **中继（数据面）**：每远端订阅一条有界通道（容量 64 帧），满则丢帧保实时
  （与本地观众同语义）；帧 > 1 MiB 按 transfer.rs 分块协议切/重组
  （1 MiB 块经 base64 ≈ 1.4 MiB < overlay 4 MiB 帧上限）；中继投递字节计入
  源房间 `bytes_out`（真实下行流量）。
- **停帧三径**：观众退订（`live_relay_unsub`，含最后一个本地观众离开影子房间）/
  源房间结束（ended 帧收尾）/ 源端心跳超时（90s 无 `live_relay_sub` 刷新即剪除）。

## 3. 端点契约

### 3.1 REST（组件 `live`，`LiveRouteHandler::routes()`）

| 方法 | 路径 | 权限 | 说明 |
|------|------|------|------|
| POST | `/api/v1/live/rooms` | admin | 创建房间，返回房间视图 + publish token；创建即联邦宣告 |
| GET | `/api/v1/live/rooms` | 公开 | 两段式大厅 `{local:[...], federated:[...]}` |
| DELETE | `/api/v1/live/rooms/:id` | admin | 结束直播：踢断主播与全部观众，房间出表；广播 ended 宣告 |

**POST /api/v1/live/rooms**

```json
// 请求（title 必填非空；source_kind ∈ screen | camera）
{ "title": "NexOS 周会", "source_kind": "screen" }

// 响应 201（publish_token 仅此一次下发；GET 不回）
{
  "id": "live-1",
  "title": "NexOS 周会",
  "source_kind": "screen",
  "created_at": "2026-08-31T10:00:00+08:00",
  "publisher_identity": "admin",
  "viewer_count": 0,
  "status": "live",
  "bytes_in": 0,
  "bytes_out": 0,
  "dropped_frames": 0,
  "rejected_frames": 0,
  "publisher_online": false,
  "header_cached": false,
  "publish_token": "lt-9f2c…（64 hex）"
}
```

校验失败 400：`{"error":"title 不可为空"}` / `{"error":"source_kind 必须是 screen 或 camera"}`。

**GET /api/v1/live/rooms**（v1.1 两段式形态）

```json
{
  "local": [ /* LiveRoom 数组（上文房间视图，按创建序；影子房间不在此列）*/ ],
  "federated": [
    {
      "id": "0279be66:live-1",          // fed_room_id（节点短前缀防撞）
      "title": "远端直播",
      "source_kind": "screen",
      "status": "live",
      "node_id": "0x0279be66…（66 hex，中继定向目标）",
      "node_name": "node-a",
      "viewer_count": 3,                // 源节点本地观众数（宣告快照）
      "publisher_online": true,
      "updated_at": "2026-08-31T10:00:30+08:00"
    }
  ]
}
```

**兼容说明（破坏性变更，调用方已随 v1.1 迁移）**：v1 返回裸 `LiveRoom[]` 数组；
v1.1 起改为 `{local, federated}` 对象。仓库内数组形态调用方为前端
`liveListRooms()`（唯一消费方 LivePanel.vue）与本文件 handler 测试，均已随本
变更迁移；无 CLI/MCP 外部调用方（全仓 grep 核对）。联邦表条目随 GET 顺手做
TTL 剔除。

**DELETE /api/v1/live/rooms/:id** → `200` 返回结束时刻快照（`status:"ended"`，
`viewer_count:0`）；房间不存在 `404`。对联邦形态 id 的 DELETE 是对**本节点
影子房间**操作（收尾本地观众；源房间不受影响）。

### 3.2 WebSocket（`http.rs::build_router` 始终挂载，升级前校验——拒绝时客户端拿到 HTTP 状态码而非 WS 空转）

| 路径 | 鉴权 | 帧协议 |
|------|------|--------|
| `/ws/live/:id/publish?token=<publish token>` | token 与创建响应精确一致（401）；房间不存在 404；**影子房间拒绝 publish（404）** | 上行：二进制帧（MediaRecorder webm chunk）+ 文本控制帧 `{"kind":"stop"}`；下行：文本 `{"kind":"error","msg":…}`（超限拒收提示） |
| `/ws/live/:id/view` | 公开（房间不存在 404；订阅满 429） | 下行：二进制帧（先缓存 header 重放，再实时转发）+ 文本 `{"kind":"ended"}`；上行忽略 |

**联邦形态房间 id 同一端点**：`/ws/live/0279be66:live-1/view`——首次订阅时
服务端自动建影子房间 + 向源节点发中继订阅（`ensure_shadow_room`），帧到达后
注入扇出；观众侧协议与本地房间完全一致（前端无感知差异）。

语义细则（v1 沿用）：

- **header 缓存与重放**：主播每个流的首个二进制 chunk（webm init segment）缓存
  在房间态；观众连上时**先入队 header 再收实时帧**（MSE 顺序 append 前提）。
  主播重连/顶号时 header 重置（新流 = 新 init segment）。**新中继订阅同样先
  重放源房间缓存 header**（远端观众中途加入语义一致）。
- **慢消费者**：每观众一条有界通道（容量 64 帧）；`try_send` 满即丢帧保实时
  （不阻塞主播），计数进 `dropped_frames`；通道关闭（观众已断）则移除订阅端。
- **上行限流**：单帧 > 上限（默认 2 MiB）拒收——不扇出、不计 `bytes_in`，
  记 `rejected_frames`，并回文本错误帧提示主播。观端重组后帧同样受此限
  （病态远端载荷丢弃并计数）。
- **主播生命周期**：`{"kind":"stop"}` 控制帧 / WS 断开 / 被 `DELETE` 踢断 /
  被新主播顶号（同 token 重连，代际递增，旧连接收到通道断开即自断）均视为
  下播：`status → ended`、观众收 `{"kind":"ended"}` 收尾、联邦广播 ended 宣告。
- **房间回收**：本地房间无主播且观众清零即出表；影子房间本地观众清零即出表
  并退订中继（停帧）。**重启即全部清空**（本地表与联邦表都是内存态）。
- **每房间订阅上限**：默认 200，超出握手 429。

### 3.3 联邦协议（os-p2p overlay fed 载荷；`payload.fed` 区分）

| kind | 方向 | 载荷要素 | 语义 |
|------|------|----------|------|
| `live_lobby` | 广播（源 → 全部已连接 peer，一跳） | `node`（源节点名）、`node_id`（`0x`+66 hex）、`room{room_id, fed_room_id, title, source_kind, status, viewer_count, publisher_online, updated_at}` | 房间宣告：接收端按 `fed_room_id` 幂等合并进联邦表（Inserted/Refreshed）；`status=ended` 立即出表；TTL 90s 无刷新剔除。触发时机：创建 / 主播接入 / 下播 / DELETE / 30s 巡检刷新（viewer_count 漂移随之收敛） |
| `live_relay_sub` | 观众节点 → 源节点（定向） | `node`、`node_id`、`room_id`（fed 形态） | 中继订阅 + **心跳**（同一载荷）：源端为该节点建中继订阅（有界通道 + 发送任务；新订阅先重放缓存 header）；已存在则仅刷新 last_seen。房间不存在 → 回 ended 帧让观众端立即收尾。观众节点每 30s 重发（影子房间存续期间） |
| `live_relay_frame` | 源节点 → 观众节点（定向） | `node`、`room_id`（fed）、`seq`（帧序号，源端递增）、`ci`/`cn`（块索引/块数）、`bytes`（块 base64）、`ended`（bool） | 中继帧：帧按 `RELAY_CHUNK_BYTES`（1 MiB）切块，接收端按 `(room_id, seq)` 重组，集齐 `cn` 块注入影子房间扇出。`ended=true` 为收尾控制帧（无 bytes）→ 影子房间结束 |
| `live_relay_unsub` | 观众节点 → 源节点（定向） | `node`、`node_id`、`room_id`（fed） | 退订：源端剪除该节点的中继订阅（drop 通道 → 发送任务退出即停帧） |

接收端入口：`handlers/p2p.rs::FederationBridge::dispatch`（live 分支）→
`LiveFedEndpoint::dispatch(from, payload)` 按 kind 分发。装配顺序红线（与
im/nexhub 同款）：`set_p2p`（发送端注入 + hub 钩子回填，同步锁写）**先于**
入站消费 task 启动与网关对外服务。

## 4. 大厅语义（本地 / 联邦）

| | 本地大厅（local） | 联邦大厅（federated） |
|---|---|---|
| 数据源 | 本节点 `LiveHub` 房间表（真实计数） | 远端 `live_lobby` 宣告合并（快照字段） |
| 可开播 | 是（主播面板，admin） | 否（远端房间只读；影子房间拒绝 publish） |
| 观看 | 本节点扇出 | 跨节点中继 → 本节点影子房间扇出（WS 同一 view 端点） |
| viewer_count | 本节点观众真实连接数 | 源节点本地观众数（中继观众计入各自节点影子房间） |
| 生命周期 | 无主播且观众清零回收 | TTL 90s 无刷新剔除 / ended 立即出表（宣告驱动） |
| 持久化 | 无（重启清空） | 无（内存表，重启清空；无 seed） |

自回路防护：宣告/订阅的 `node_id` == 本机 NodeID（同私钥多 OS 实例）→
不入联邦表 / 不建中继（本地直连即可，P2P 自回路只会重复扇出）。

## 5. 环境变量

| env | 缺省 | 说明 |
|-----|------|------|
| `NEXOS_LIVE_MAX_FRAME_BYTES` | `2097152`（2 MiB） | 单个上行帧上限（字节）；非法值回默认并 `[live]` 前缀日志告警 |
| `NEXOS_LIVE_MAX_VIEWERS` | `200` | 每房间观众（订阅端）上限 |

联邦常量（`live.rs`，暂无 env 覆盖）：宣告/巡检周期 30s、联邦表 TTL 90s、
中继心跳 30s / 订阅超时 90s、中继分块 1 MiB、中继通道容量 64 帧、观端待重组
seq 上限 8。联邦启用随 P2P：`NEXOS_P2P_ENABLE=1`（未启用时联邦静默停用，
本地大厅不受影响）。除此之外**无新增 DB、无配置文件**。

## 6. 真实数据声明

- **无 seed 无演示房间**：本地房间表与联邦房间表启动均为空；联邦条目只来自
  真实网络宣告。
- `viewer_count`（本地）：真实 WS 观众连接数（subscribe +1 / unsubscribe -1 /
  通道死端即时修正），**不是**估算值。联邦条目的 viewer_count 是源节点宣告
  快照（其本地观众）。
- `bytes_in` / `bytes_out`：服务端逐帧累加的真实字节（out 只计成功投递，
  **含中继投递**：帧长 × 成功投递的远端订阅数）。
- `dropped_frames` / `rejected_frames`：真实丢帧/拒收计数（中继背压丢帧记日志，
  不与本地观众计数混同）。
- `status` / `publisher_online`：由真实连接事件驱动（主播 WS 接入/断开），
  非定时器推测；联邦条目为源节点宣告快照。
- **重启即清空**：内存态无持久化——服务重启后两个大厅都为空、进行中的推流
  断开（主播端需重新创建房间推流）。这是 v1.x 明确取舍，见 §7。

## 7. v1.1 边界与后续路线

**当前边界（明确不做）**：

- **中继单跳**：观众节点必须与源节点 overlay 直连可达（fed_broadcast 广播
  范围 = 当前已连接 peer 一跳；接收方不转播）。多跳中继 / 边缘级联进后续。
- **中继链路带宽/延迟**：帧流 JSON+base64 过 overlay（约 +33% 传输体积 +
  信封开销），且 overlay 帧上限 4 MiB——大帧按 1 MiB 分块。MSE 路径本就
  ~2-5s 延迟，中继叠加同量级；对延迟敏感场景仍是后续 WebRTC 路线。
- **不落盘**：无录制、无回放，媒体数据只在内存中转发（本地与中继同）。
- **不转码**：webm (vp8/opus) 原样透传，编码参数由主播端 MediaRecorder 决定。
- **联邦无鉴权裁决**：任何已连接 peer 的宣告都合并（信任边界 = overlay 握手
  验签节点身份，同 im/nexhub 联邦先例）；payload 净化（title/room_id/node_id
  形态校验 + 限幅）防病态值，不做内容审核。

**后续路线（文档预留，均未实现）**：

1. **多跳中继 / 边缘级联**：B 中继给 C（树状分发），复用分块协议 + 环检测。
2. **录制落 tank**：主播帧旁路写 `/tank` 上的 webm 文件（追加 cluster 即可
   播放），媒体库（media 组件）直接索引回放。
3. **WebRTC 降延迟**：MSE 路径延迟 ~2-5s；对延迟敏感场景引入浏览器端
   WebRTC（仍零服务端原生依赖）。
4. **中继二进制化**：给 overlay FrameKind 加专用二进制帧（免 base64/JSON
   双重开销，块可提至 3 MiB，见 transfer.rs 同款预留）。

## 8. 测试

`crates/os-api/src/handlers/live.rs` 尾部 `#[cfg(test)]`（22 个用例；mock 只进
cfg(test)：channel 注入 fake 订阅端 + fake 互连 overlay + 真实 TcpListener 起
axum serve + tungstenite WS 握手，同 terminal WS / p2p 测试手法）：

- 房间生命周期：路由声明权限（POST/DELETE admin、GET 公开）、创建/两段式列表/
  结束/404、title 与 source_kind 校验。
- 扇出逻辑：header 重放与帧序（中途观众先收 init segment）、慢消费者丢帧
  计数且快消费者零丢失、viewer_count 增减与房间回收、主播断开通知观众
  ended、顶号代际（旧代 detach 不清新主播）。
- 上行限流：超限帧拒收（不计 bytes_in、不下发、计数 +1）、边界值放行；
  订阅上限拒绝与空位恢复。
- **联邦（F1–F6 + WS e2e）**：
  - F1 `fed_room_id` 前缀防撞 + `local_room_id` 回程解析；
  - F2 宣告/中继帧载荷序列化（kind/node/room 全要素、fed_room_id 派生、
    分块字段 base64、ended 无 bytes）；
  - F3 联邦表幂等合并（Inserted/Refreshed 不重复）、ended 立即出表、
    TTL 91s 剔除、非法载荷与自回路忽略；
  - F4 GET /rooms 两段式契约（顶层对象 + local/federated 数组——兼容迁移断言）；
  - F5 中继闭环（两 LiveHub 实例 + fake 互连 overlay）：A 播（header +
    1.5 MiB 双块帧）→ B 订（影子房间 + sub）→ B 观众收 header + 重组帧
    （字节级一致）→ 中继计 bytes_out → 退订剪除 → 重订（header 重放）→
    A 下播 → B 观众收 Ended + 联邦条目出表；
  - F6 中继订阅心跳超时剪除（91s 合成 now）；
  - WS e2e（联邦形态）：真 axum + tungstenite——宣告合并 → 观众 WS 连
    联邦 id（101）→ sub 载荷断言 → dispatch 驱动中继帧（header/双块重组/
    ended 控制帧）→ 观众收帧与 `{"kind":"ended"}` → 断开发 unsub。
- WS e2e（本地）：错 token 401 / 房间不存在 404 / 未知动作 404（升级前拒绝）；
  publish→view 全链路（token 握手 101 → header 重放 → 实时帧 → stop 控制帧
  → ended → 房间回收）。

`FederationBridge` 的 live 分支分发由 `handlers/p2p.rs` 既有桥测试矩阵覆盖
（live 端点缺省 None 时载荷静默让路）；main.rs 装配为 `set_p2p` 注入 +
bridge live 字段（与 im/nexhub 完全同构）。
