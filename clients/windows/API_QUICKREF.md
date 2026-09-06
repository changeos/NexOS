# API 速查表（Windows 客户端 MVP）

> 依据 PLANNING.md §3 契约整理。`BASE = http://192.0.2.106:8080`（= ub2604:8080）。
> 认证：写操作（POST/DELETE）加 `Authorization: Bearer change-me-admin-token`；标注"免认证"的不需要。
> 响应：成功 = 裸 JSON（列表是数组）；失败 = `{"error": "<msg>"}` + 对应 HTTP 状态码。
> 字段细节与请求/响应示例见 PLANNING.md 对应小节。

## 通用（3）

| 端点 | 认证 | 说明 / 关键响应字段 |
|---|---|---|
| GET `/healthz` | 免 | 连通探针 `{"status":"ok"}` |
| GET `/status` | 免 | `{hostname, version, capacity{used_bytes,total_bytes}, health, node_count, cpu_virt, uptime}`（capacity 为占位 0） |
| GET `/api/v1/version` | 免 | `{"name":"os-api","version":"0.1.0"}` |

## 仪表盘 Monitor（4 核心 + 2 可选）

| 端点 | 认证 | 说明 / 关键响应字段 |
|---|---|---|
| GET `/api/v1/monitor/metrics` | 免 | 全量指标：`hostname, uptime_secs, load_avg[3], cpu_usage, cpu_cores, mem_total/used/available_bytes, swap_total/used_bytes, disk_total/used_bytes, net_rx/tx_bytes, processes, kernel_version`（CPU 是采样差值，轮询间隔 ≥1s；net_* 为累计值需自行差分） |
| GET `/api/v1/monitor/stats` | 免 | 一次聚合：`cpu_usage, cpu_cores, mem_used_ratio, disk_used_ratio, load_avg_1, uptime_secs, processes, alerts_total, alerts_unacked, zpools_total, zpools_healthy, hostname` |
| GET `/api/v1/monitor/services` | 免 | `[{name, status(running/stopped/unknown), pid}]` |
| GET `/api/v1/monitor/zpools` | 免 | `[{name, state(ONLINE/…), size_bytes, allocated_bytes, free_bytes, healthy}]` |
| GET `/api/v1/monitor/alerts` | 免（可选） | 最近 100 条 `[{id, level(info/warning/critical), message, source, timestamp, acked}]` |
| POST `/api/v1/monitor/alerts/:id/ack` | 需（可选） | body `{}`；404=告警不存在 |

## IM 大厅（4）— lobby id 恒为 `"lobby"`；在线窗口 60s，客户端心跳 30s

| 端点 | 认证 | 说明 / 关键点 |
|---|---|---|
| GET `/api/v1/im/lobby?user=<id>` | 免 | `{id:"lobby", name, member_count, online_count, last_message}`。**带 user= 即心跳；首次自动加入大厅并广播欢迎系统消息（加入的唯一方式）** |
| GET `/api/v1/im/lobby/messages?user=<id>` | 免 | 最近 50 条消息数组（时间正序）；user= 计心跳 |
| POST `/api/v1/im/lobby/messages` | 需 | body `{user_id?, content, sender_name?}` → 201+消息对象。未加入大厅 → **403**；空 content → 400；成功即 WS 全员广播 |
| GET `/api/v1/im/lobby/members` | 免 | `{lobby_id, member_count, online_count, members:[{user_id, display_name, last_seen, joined_at, online}]}` |

## IM 会话/群组（6）

| 端点 | 认证 | 说明 |
|---|---|---|
| GET `/api/v1/im/groups` | 免 | `[{id, name, owner, kind:"group", members[], last_activity, created_at}]` |
| POST `/api/v1/im/groups` | 需 | body `{name, owner?, members?}` → 201 |
| GET `/api/v1/im/conversations` | 免 | `[{id, name, is_group, created_by, created_at}]` |
| POST `/api/v1/im/conversations` | 需 | body `{name, created_by?}` → 201 |
| GET `/api/v1/im/conversations/:id/messages` | 免 | 全量消息（时间正序）；`:id` 可为会话或群组 id；不存在返回 `[]` |
| POST `/api/v1/im/conversations/:id/messages` | 需 | body `{content, sender_id?, sender_name?, msg_type?, file_url?, reply_to?}`（sender_id 缺省 "me"）→ 201 + WS 广播 `im_message`；对话不存在 → 404 |

## IM 辅助（可选）

| 端点 | 认证 | 说明 |
|---|---|---|
| POST `/api/v1/im/groups/:id/join` / `…/leave` | 需 | body `{member}` |
| POST `/api/v1/im/messages/:id/read` | 需 **admin 角色** | body `{user_id?}`（缺省 me） |
| GET `/api/v1/im/conversations/:id/unread?user=` | 免 | `{conversation_id, user, unread}` |
| GET `/api/v1/im/search?q=` | 免 | `{query, count, results:[消息]}` |
| GET `/api/v1/im/status` | 免 | `{ready, conversations, groups, peers, messages}` |
| GET/POST `/api/v1/im/peers` | 读免/写需 | Federation 节点记录（仅记录不真连，MVP 可不做） |

## NexHub 大厅（6）— 付费字段：`price_sats`（0=免费）+ `currency`（free/btc/nex/usdc/eth）

| 端点 | 认证 | 说明 |
|---|---|---|
| GET `/api/v1/nexhub/lobby?q=&tag=&sort=recent\|downloads` | 免 | 裸数组 `LobbyEntry[]`：`repo_name, description, tags[], publisher, source_url, homepage_node, commit_count, size_bytes, default_branch, last_commit, last_commit_date, readme_excerpt, download_count, published_at, price_sats, currency` |
| GET `/api/v1/nexhub/lobby/stats` | 免 | `{published_count, total_downloads, top_tags:[{tag,count}]}` |
| GET `/api/v1/nexhub/lobby/:name` | 免 | 条目 + `clone_url_ssh` + `clone_url_http`（本机 git clone 用 http 通道）；404=不存在 |
| POST `/api/v1/nexhub/lobby/:name/clone` | 需 admin | 免费条目 body `{}`；付费条目必须 body `{"buyer":"<id>"}`（需先 purchase）→ `{ok, name, cloned, source_url, local_path, download_count, clone_url_ssh, clone_url_http}`。**服务端**克隆到 /tank/git-repos；付费无授权 → **402**；git 失败 → 502 |
| POST `/api/v1/nexhub/lobby/:name/purchase` | 需 | body `{buyer, txid, chain?, amount_sats?, currency?}` → `{ok, repo_name, buyer, chain, txid, amount_sats, currency, paid_at, note}`。免费条目 → 400；货币不符/金额不足/txid 空 → **402** |
| GET `/api/v1/nexhub/lobby/entitlements?repo=&buyer=` | 需 | 授权记录 `[{repo_name, buyer, chain, txid, amount_sats, currency, paid_at}]` |

## WebSocket（1 条通道）

| 项 | 值 |
|---|---|
| URL | `ws://<host>:8080/ws?user=<我的id>`（**握手免认证**；user 缺省 "anonymous"；https 环境用 wss） |
| 方向 | **服务端只推不收**；客户端上行帧被忽略；发消息走 HTTP POST |
| 心跳 | 无 WS 层心跳——保活靠 HTTP `GET /im/lobby?user=` 每 **30s**（在线窗口 60s）；断线 5s 重连 |
| IM 帧 | `{"type":"im_message","conversation_id":"…","message":{…}}`；`{"type":"im_lobby_message","lobby_id":"lobby","message":{…}}`（message = 完整消息 DTO，按 `message.id` 去重） |
| 忽略帧 | `event` / `progress` / `notification` / `error` |
| 消息 DTO | `{id, conversation_id, sender_id, sender_name, content, msg_type(text/file/image/system), file_url, reply_to, created_at(RFC3339), read_by[]}`；`sender_id=="system"` 或 `msg_type=="system"` → 居中灰显 |

## Git Smart HTTP（本机克隆用）

| 项 | 值 |
|---|---|
| URL | `http://<host>:8080/git/<name>.git`（详情端点的 `clone_url_http` 直接给全） |
| 认证 | Basic（用户名任意，**密码 = admin token**）或 Bearer；token 未配置 → 503；错误 → 401 + `WWW-Authenticate: Basic realm="NexHub Git"` |
| 示例 | `git clone http://oem:change-me-admin-token@192.0.2.106:8080/git/nexos.git`（push 同一认证） |
