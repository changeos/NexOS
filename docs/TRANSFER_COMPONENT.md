# 传输组件（transfer）：迅雷式多源下载管理 + P2P 网状分发

> 2026-08-25 新增。用户定调（原话拆解）：「增加传输组件，类似迅雷或其他的
> CDN，传输不能限于公网 ip，**不要用公网 ip 做分发点**」。
>
> 落地：分发完全走 **os-p2p 叠加层消息通道**（直连 → TCP 打洞 → 中继信箱的
> 连接阶梯）——两个 NAT 后节点互传不需要任何一端有公网 IP，也不需要知道
> 对方的 underlay 地址。这同时是仓库 / 大文件分发的架构答案（如 aliyun 云
> 节点无法直连 106 内网地址的场景：两者只要在同一 os-p2p 网内即可互传）。

## 1. 架构

```text
  ┌─────────────── 提供方（种子节点 A）───────────────┐
  │ 本地文件 /tank/x.iso                              │
  │   │ POST /api/v1/transfer/publish {path}          │
  │   ▼                                               │
  │ TransferManifest（分块清单）                       │
  │   {transfer_id:"tr_<sha256[:16]>", sha256,        │
  │    size, chunk_size:1MiB, chunks:[sha256×N]}      │
  │   │ 入种子注册表（.transfer-registry.json 持久化） │
  │   ▼                                               │
  │ TransferService::ingress（订阅 on_msg 常驻任务）   │
  │   ├─ transfer_query  → 查注册表 → transfer_offer   │
  │   └─ transfer_chunk  → 定位读+自校验 → chunk_data  │
  └────────────────┬──────────────────▲───────────────┘
                   │ overlay 消息通道（Handle::send）
                   │ （直连 / TCP 打洞 / 中继信箱——全加密，免公网 IP）
  ┌────────────────▼──────────────────┴───────────────┐
  │ 消费方（节点 B）：fetch {sha256} → 任务引擎         │
  │   ① query 扇出连接的 peer → 收 offer（源列表）     │
  │   ② 分块拉取：≤4 块在途（背压），批内多源轮转      │
  │   ③ 逐块 sha256 校验 → 坏块重试 2 次（换源）       │
  │   ④ 完成块位图持久化（.progress.json 断点续传）    │
  │   ⑤ 全部到齐 → 整文件 sha256 复核 → .part 落名    │
  │   ⑥ 自动登记注册表 → B 成为新源（CDN 式 swarming） │
  └───────────────────────────────────────────────────┘
```

代码落点（红线的「地盘」）：

| 层 | 文件 | 职责 |
|---|---|---|
| 协议 + 引擎 | `crates/os-p2p/src/transfer.rs` | 清单/注册表/进度位图纯函数 + `TransferService`（提供方 ingress + 消费方任务引擎）+ 16 组测试 |
| REST 适配 | `crates/os-api/src/handlers/transfer.rs` | `TransferRouteHandler`（component=transfer，8 组测试）；装配在 `main.rs::spawn_p2p_if_enabled` |
| 前端 | `crates/os-api/web/src/views/Downloads.vue` | 「下载中心」双 Tab：🌐 HTTP/BT（aria2，原功能不动）+ 🔗 P2P 传输（任务表/发布/拉取/清单） |
| API 客户端 | `crates/os-api/web/src/api/client.ts` | `transfer*` 十个端点封装 |

**不改** `downloads.rs` / `im.rs` / `meta.rs`（独立组件，无协议常量冲突：
transfer 载荷用自有 tag `payload.transfer`，与联邦桥的 `payload.fed` 互不感知）。

## 2. 协议帧表（overlay send 载荷，tag = `payload.transfer`）

复用 `im_lobby_query` 的请求-应答模式：每个载荷带 `req_id`，应答方回帧时
原样回显，消费方用 req_id 在 pending 表里唤醒对应的 oneshot 等待者（应答
同时携带来源 NodeID，`offer` 的应答方即真实源）。

| 帧 | 方向 | 载荷字段 | 语义 |
|---|---|---|---|
| `transfer_query` | 消费→各 peer | `req_id, sha256?, transfer_id?` | 「你有这个文件吗」（扇出探测） |
| `transfer_offer` | 提供方→消费 | `req_id, manifest` | 有——回完整清单（未命中保持沉默，消费方按 8s 窗口聚合） |
| `transfer_chunk` | 消费→提供方 | `req_id, transfer_id, index` | 拉取第 index 块 |
| `transfer_chunk_data` | 提供方→消费 | `req_id, transfer_id, index, bytes(base64), sha256` | 块字节 + 摘要 |
| `transfer_error` | 提供方→消费 | `req_id, reason` | 明确否定（未知 ID / 块越界 / 本地文件失配拒供） |

### 分块大小裁决：1 MiB（而非 4 MiB）

勘察 `transport.rs`：overlay 帧 = 长度前缀 JSON 信封（AES-GCM 加密后同限），
单帧上限 `MAX_FRAME_LEN` = 4 MiB。4 MiB 原始块经 base64（×4/3 ≈ 5.6 MiB）
+ 信封/密文开销**必然超限断连**——缺省块定为 1 MiB（线上 ≈ 1.4 MiB）。
清单自带 `chunk_size` 字段，协议不锁死。

### JSON+base64 起步的代价与二进制化路线

- 代价：约 **+33% 传输体积** + JSON 编解码 CPU；且 `on_msg` 是 broadcast——
  每个订阅者克隆一份载荷（os-api 部署里联邦桥 + 本服务 = 2 份/块）。
- 路线：给 `FrameKind` 加专用**二进制帧**（`TRANSFER_DATA`，帧体直接是
  `req_id‖index‖raw bytes`，免 base64/JSON 双重开销），块可提至 3 MiB；
  双端按 `PROTOCOL_VERSION` +1 同步升级（既有「无明文回落」语义保证灰度
  期互不干扰）。本期不做——先以正确性换带宽，节点间叠加层已加密。

## 3. REST 端点（component=transfer）

| method | path | 鉴权 | 语义 |
|--------|------|------|------|
| POST | `/api/v1/transfer/publish` | admin | `{path, name?}` 本地文件发布为可传输 → 201 `{transfer_id, sha256, chunks…}` |
| GET | `/api/v1/transfer/manifests` | 公开 | 本机已发布清单（含本地路径） |
| DELETE | `/api/v1/transfer/manifests/:id` | admin | 下架（:id = transfer_id 或 sha256） |
| POST | `/api/v1/transfer/fetch` | admin | `{sha256 \| transfer_id, name?}` 发起 P2P 拉取 → 202 任务视图 |
| GET | `/api/v1/transfer/tasks` | 公开 | 任务列表（phase/进度块位图/速度/源节点短 ID） |
| GET | `/api/v1/transfer/tasks/:id` | 公开 | 单任务详情 |
| POST | `/api/v1/transfer/tasks/:id/pause` | admin | 暂停（保留进度） |
| POST | `/api/v1/transfer/tasks/:id/resume` | admin | 继续（断点续传） |
| POST | `/api/v1/transfer/tasks/:id/cancel` | admin | 取消（进度文件保留，重新 fetch 续传） |
| GET | `/api/v1/transfer/stats` | 公开 | 统计（清单数/任务数/做种供出量） |

任务 `phase`：`querying → downloading → paused ⇄ / completed / failed /
cancelled`；`status` 输出与 downloads 任务同词表（pending/downloading/
paused/completed/error），前端 Tab 聚合展示无需适配。

**启用条件**：`NEXOS_P2P_ENABLE=1`（transfer 与 p2p 组网同生——没有 overlay
就没有分发通道）。未启用时全部端点 503 + 引导文案。

**环境变量**：`NEXOS_TRANSFER_DIR`（落地目录，缺省 `/tank/downloads`）；
`NEXOS_TRANSFER_REGISTRY`（注册表持久化，缺省 `<dir>/.transfer-registry.json`）；
`NEXOS_TRANSFER_CHUNK`（分块大小，缺省 1 MiB，压小仅建议测试用）。

## 4. 与 aria2 / downloads 的分工

| | downloads（aria2） | transfer（本组件） |
|---|---|---|
| 通道 | 公网 HTTP/FTP/SFTP 直链、BitTorrent（magnet/.torrent）、ED2K | os-p2p 叠加层（NAT 后节点互传，**免公网 IP**） |
| 寻址 | URL / info-hash | 内容寻址：文件 sha256（或 `tr_…` transfer_id） |
| 做种 | BT 任务下完即停（`seed-time=0`） | **完成即做种**（下载完成自动登记注册表——CDN 式 swarming） |
| 断点 | aria2 内建 | 块位图 `.progress.json` 持久化 |
| 进度 | aria2 字节进度 | 块位图（`chunks_done/chunks_total` + 字节换算） |

前端 Downloads.vue 双 Tab 聚合：🌐 HTTP/BT（aria2）+ 🔗 P2P 传输。

## 5. CDN 式再分发语义（swarming）

1. A `publish` → A 的注册表落一条 `{manifest → /tank/x.iso}`；
2. B `fetch {sha256}` → 从 A 分块拉取、逐块校验落地；
3. B 完成 → **自动**登记 B 的注册表 `{manifest → B 的落地文件}`；
4. C `fetch {sha256}` → query 扇出，A 和 B 都应答 offer → C 的源列表
   `[A, B]`，批内轮转从两源并发拉取（带宽分摊），坏块自动换源重试；
5. 任何节点 `DELETE /transfer/manifests/:id` 即退出做种（下架不影响已
   传播的副本——内容寻址，其他源照常应答）。

提供方还有一道**自校验防线**：供块前对读出的字节重算 sha256 对清单，不符
即拒供（文件在发布后被改动 → 防污染扩散）；消费方逐块校验 + 整文件复核
双层兜底。

## 6. NAT 免公网 IP 的达成方式

- 所有帧经 `Handle::send` / `on_msg`（os-p2p SEND 路由）：**直连优先 →
  我是其 relay（信箱投递）→ 经 relay 转发 → lookup 重试**；
- NAT 后节点的可达性由 os-p2p 既有机制解决：TCP 打洞（观测端点 + 同时
  打开）或任意可达节点中继（store-and-forward 信箱，100 条/节点）；
- 传输协议层**完全不知道 underlay 地址的存在**——没有「分发点 IP」这个
  概念可违反；公网锚点节点若在网内，只是普通 peer（可自愿做源/中继），
  不是必需组件。

## 7. 断点续传细节

- 进度文件：`<落地目录>/<name>.<sha256 前 8>.progress.json`——内容
  `{manifest, done:[已完块下标]}`（清单随进度持久化，续传时不依赖网络
  即可恢复几何）；
- 半成品：`<name>.<sha256 前 8>.part`——**定位写**（按块偏移 seek+write），
  续传只补缺失块；
- 一致性防线：续传装载位图时校验 `.part` 实际长度——超出文件长度的
  「已完成」块作废重拉（防上次写盘中断留下的假位图）；`.part` 整个缺失
  则全量重拉；
- 完成：整文件 sha256 复核 → 原子 rename `.part` → 终名（同名占用退让
  `name.2.ext`，不覆盖）→ 删进度文件 → 登记做种。
- 取消/暂停/失败均**保留**进度文件——重新 `fetch` 同键即续传。

## 8. 测试（24 组）

`cargo test -p os-p2p transfer`（16）：

| # | 测试 | 验证点 |
|---|---|---|
| 1 | chunk_geometry_covers_whole_file | 块数/偏移/末块取余/空文件 0 块纯函数 |
| 2 | manifest_chunking_and_hashes | 分块摘要、整文件摘要、`tr_`+sha256 前 16 确定性 ID、MIME |
| 3 | manifest_rejects_directory_and_missing | 目录/缺文件拒绝发布 |
| 4 | sanitize_filename_blocks_traversal | 清单 name 路径穿越清洗 |
| 5 | registry_publish_persist_reload_unpublish | 发布幂等/自动登记去重/持久化重载/双键下架 |
| 6 | progress_roundtrip_preserves_bitmap | 位图持久化往返 + 损坏降级 |
| 7 | offer_query_roundtrip_over_overlay | **双节点 spawn**：query→offer 经 overlay 送达，应答方=真实源，transfer_id 命中 |
| 8 | fetch_end_to_end_two_nodes_byte_identical | **双节点端到端**：A 发布 B 拉取字节级一致 + 落地目录 + 自动做种 + 进度清理 |
| 9 | fetch_bad_chunk_exhausts_retries_and_fails | 清单摘要被篡改 → 坏块重试 2 次耗尽 → 任务失败 |
| 10 | fetch_resume_from_saved_progress | 预置 6/13 块进度 → 只补 7 块（供块数断言）；位图/`.part` 不匹配 → 全量重拉 |
| 11 | fetch_backpressure_caps_inflight | 20 块任务在途块峰值 ≤ 4 |
| 12 | task_state_machine_control_paths | 无源失败/查询中取消/终态拒操作/暂停→继续→完成 |
| 13 | fetch_lands_in_configured_dir_with_name_override | transfer_id 发起 + 名称覆盖 + 落地目录 |
| 14 | publish_validates_source_path | 服务层路径校验 + 下架后无应答 |
| 15 | inbound_ignores_non_transfer_payloads | 非 transfer 帧静默让路（与联邦桥共存） |
| 16 | fetch_empty_file_boundary | 0 块空文件直接整文件校验完成 |

`cargo test -p os-api --lib handlers::transfer`（8）：路由声明鉴权矩阵、
未启用 503、publish 校验（.. 穿越/目录）、publish→manifests→DELETE 全链、
fetch 键格式校验、任务观察面字段、**双节点 REST 端到端**（A 经 REST 发布 →
B 经 REST fetch → 完成 + 终态 409）、兜底 404。

双节点测试与 im.rs/meta.rs 同款模式：`P2pNode::spawn`（127.0.0.1:0 随机
端口 + `Timing::testing()`）+ `dial` 建链，走真实 overlay 帧路径。

## 9. 后续路线

1. **多源并行分块调度**：当前批内轮转选源已支持多源，进一步做「按源实时
   速度加权」的块分配（慢源少给块）；
2. **二进制帧**（见 §2）：`FrameKind::TRANSFER_DATA` 免 base64/JSON，块
   提至 3 MiB，吞吐约 ×1.33；
3. **仓库同步复用**：NexHub 裸仓库打包（`git bundle`/tar）→ publish →
   各节点 fetch → 解包——「aliun 无公网 IP 拉内网 106 仓库」的落地形态；
4. **大文件流式清单**：publish 目前整文件读一遍建清单（1 TiB ≈ 分钟级），
   可改增量登记（追加块边写边记）；
5. **与更新通道打通**：`os-update` 的更新源检查（NexHub git tag）可叠加
   transfer 拉取镜像层（省 HTTP 出口带宽，节点间就近取）。
