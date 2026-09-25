# NexOS 全面优化审查（2026-09，非安全线）

> 审查日期：2026-09-24 · 基线 commit `65f5a69`（2026-09-18，工作树干净）
> 范围：性能与资源 / 架构与代码健康 / 产品与 UX（非视觉）/ 运维。**明确排除安全、鉴权、加密类问题**（另有专线）。
> 方法：MEMORY.md 全量 + 逐维度代码巡检（os-api / os-p2p / os-provision / os-nexhub / web 前端 / apps 三应用 / docs 抽检 / 运行时 DB 只读探查）。
> 性质：**纯只读审查，未实施任何变更**。每项给出：问题 + 证据（文件:行） + 优化建议 + 工作量估 + 优先级（P0 紧急 / P1 高 / P2 中 / P3 低）。

---

## 0. 系统拓扑速览（供零上下文读者定位各节证据）

```
浏览器 ──HTTP──▶ os-api:8558 (axum 单进程)
                   ├─ http.rs（axum Router：静态/SPA + dispatch_handler + WS + git CGI + OpenAI 兼容 + ISO 流式特挂）
                   ├─ gateway_impl.rs dispatch（中间件链→路由匹配 RouteRegistry→30 个 RouteHandler）
                   ├─ handlers/*.rs（im 11722 行 / film_hub 9786 / api_gateway 7683 / …）
                   │     └─ 各持 Mutex<Connection> ↔ /tank/os-data/*.db（15 个 SQLite，WAL）
                   ├─ webui rust-embed（static-dist 3.4MB 原始 / ~700KB gzip）
                   └─ spawn_p2p_if_enabled → os-p2p（meta 引擎 5s tick / 30s gossip digest ≤64KB）
                        └─ 联邦桥 broadcast rx → FederationBridge（IM/NexHub/live/api_market）
前端：crates/os-api/web（Vue3 主前端 33 view）+ apps/{film,qrtransfer,streaming}（独立包，各自 api.ts）
运维：systemd os-api.service（journald）· scripts/release.sh（双架构+dist 三工件+aliyun b64 推送）
```

---

## A. 性能与资源

### A1. os-api 启动路径与内存态

**A1-1【P1】组件级单连接 `std::sync::Mutex<Connection>`，DB 调用直接阻塞 tokio worker**
- 证据：`crates/os-api/src/handlers/im.rs:886-1303`（async handle 内 `self.shared.db.lock().expect("db poisoned")` 十余处）；`film_hub.rs` 与 `api_gateway.rs` 全文件 `spawn_blocking` 计数 = **0**（对照：files.rs/system.rs/share.rs 正确用了 spawn_blocking）。每组件一条连接 + 一把全局互斥锁 ⇒ 该组件**全部读写串行**，且持锁期间阻塞 runtime 线程。IM 是最热的 WS+REST 混合面，消息写入与历史拉取互相排队。
- 建议：短期把重查询（历史消息分页、stats 聚合）包 `spawn_blocking`；中期引 r2d2_sqlite 小连接池（读并发、写仍单连接），或至少读路径改 `OpenFlags::READ_ONLY` + 每请求短连接（WAL 下读不阻塞写）。
- 工作量：短期 0.5-1 天；连接池改造 2-3 天（含回归）。

**A1-2【P1】`HUB_META_LOCK` 进程级全局锁，临界区含同步文件 IO**
- 证据：`film_hub.rs:331`（`static HUB_META_LOCK: Lazy<Mutex<()>>`），10 处上锁点（628/658/708/729/744/2363/2451/6298/9000）；临界区内 `std::fs::read_to_string` + `serde_json::from_str` + 全文件重写（如 `append_activity`：activity.json 环形 200 条，**每条事件全量读+解析+重写**）。锁粒度是**全进程所有 film 项目共用一把**，跨项目互相阻塞；且在 async 上下文持锁做磁盘 IO。
- 建议：① 锁改 per-project（`Mutex<HashMap<String /*root*/, Arc<Mutex<()>>>}`，与 STORY_SOURCE_LOCKS 同款模式）；② 文件读写挪 `spawn_blocking`；③ activity 改 append-only 行文件（JSONL）+ 定期压实，免去全量重写。
- 工作量：① 0.5 天（机械替换）；②③ 1-2 天。

**A1-3【P2】dispatch 每请求双锁 registry + 克隆 RouteSpec + 重复计算 specificity**
- 证据：`gateway_impl.rs:285-305`（`registry.lock()` 两次：match 与 `all().get(idx).cloned()`——RouteSpec 含 3 String + Vec 每请求克隆）；`routing.rs:329`（`static_routes.get(&(method, norm_path.to_string()))` 每请求分配 String）；`routing.rs:269`（`specificity_of(&r.path)` 每候选每请求重算，可在注册时预存）。
- 建议：路由表 `Arc<Vec<Arc<RouteSpec>>>` 只读快照（注册完成后不再变），dispatch 零锁读；specificity 注册时算好存进桶。路由匹配本身已有静态 O(1) + method 分桶优化（`routing.rs:236-282`），底子好，只差最后一步去锁。
- 工作量：1 天（含 `--check` 与路由冲突测试回归）。

**A1-4【P3】启动路径整体健康**
- 30 个组件顺序 `register_component().await.expect()`（main.rs 249 起）；15 个 DB 构造期 open+建表+seed，毫秒级；P2P spawn 失败降级不阻塞（main.rs:1137）；`probe_node_self_desc()` 的 nvidia-smi 一次性缓存。**无重大启动阻塞**，不需要动。

### A2. SQLite 使用

**A2-1【P1】`/gateway/stats` 全表载入内存聚合；logs 表无保留策略（无界增长）**
- 证据：`api_gateway.rs:2298` `load_all_logs(&conn)`（无 LIMIT 全量）；`total_requests: u64 = logs.iter().map(|_| 1u64).sum()`（等价 `logs.len()`，却是 O(n) 迭代器写法）；全文件无 `DELETE FROM logs`（grep 仅命中 pool.retain/ids.retain）。当前 /tank/os-data/gateway.db logs=29 行（开发机），但**每次网关调用都插一行**，生产节点会持续膨胀，stats 端点随之线性变慢+内存放大（每行结构体）。
- 建议：stats 改 SQL 聚合（`SELECT COUNT(*), SUM(total_tokens), SUM(status='success') FROM logs`）；加日志保留（如 30 天或最近 10 万行，低频清理任务）。
- 工作量：0.5 天。

**A2-2【P2】`busy_timeout` 全仓 0 处**
- 证据：`grep -rn busy_timeout crates --include="*.rs" | grep -v tests` = **0**；11 处 open 均只设 WAL（如 api_gateway.rs:3083、im.rs:4941）。单进程单连接时无碍，但任何外部工具（人工 sqlite3、备份脚本、未来多实例）并发写将直接吃 `SQLITE_BUSY` 立即失败而非等待。
- 建议：统一 helper 里加 `conn.busy_timeout(Duration::from_secs(5))`——一行×11 处，或收敛进 A2-3 的共享模块。
- 工作量：0.5 天。

**A2-3【P2】`open_db + create_schema + seed_if_empty` 样板在 ≥8 个 handler 复制**
- 证据：api_gateway.rs:3080 / api_market.rs:3534 / forwarding.rs:1522 / monitor.rs:578 / nexhub_ci.rs:281 / llm.rs:3496 / tips.rs:174 / apps_handler.rs:382 / media.rs:1289 / model_hub.rs:1584 / im.rs:4941——同构的 open+WAL+建表+降级逻辑各自维护。/tank/os-data 下已积累 **15 个 SQLite 库**。
- 建议：抽 `os-common::sqlite`（open_db(path, migrations) + busy_timeout + WAL + 内存降级），逐个替换。这也是后续统一备份/监控的挂点。
- 工作量：1-1.5 天（纯机械迁移+测试）。

**A2-4【P3】索引现状核查（实测，只读连接）**
- 生产库均小（gateway.logs=29、im_messages=154、ci_runs=28、film_cost_events=12）；im_messages 有 `(conversation_id, created_at)` 复合索引且查询模式吻合（im.rs:5300/5488/5822）；ci_runs 有 repo+created 双索引；film 两表有索引。**当前无急迫缺索引问题**——真正的风险在 logs 无界增长（见 A2-1），不在缺索引。

### A3. HTTP 层

**A3-1【P0】请求体 `to_bytes(body, usize::MAX)` 无上限 + JSON 信封大载荷多级拷贝**
- 证据：`http.rs:283` `axum::body::to_bytes(body_axum, usize::MAX)`；`decode_body`（http.rs:291-307）先整包 JSON parse 失败再 UTF-8 字符串。film 源文件导入上限 64MB（NEXOS_FILM_SOURCE_MAX_MB）走 **b64 JSON 信封**（MEMORY 2026-09-08 明言"multipart 不可行，b64 信封唯一通道"）：64MB 文件 → ~85MB body 字节 → serde_json::Value 树（内存 ~2-3×）→ String → base64 解码 → 落盘字节，**单请求峰值内存 ~250MB+ 且全程占用**。响应侧同病：`direct_passthrough_bytes`（http.rs:348）对二进制走 base64-in-JSON-string 再解码，mp4 直传同样 2-3× 内存churn。
- 建议：① 立刻加全局 body 上限（如 128MB，env 可配，413 如实拒绝）——防误用/防打断；② 大载荷端点逐个迁移到已验证的流式特挂模式（**ISO 级流式直传已在 provisioning 做成**：`tokio unfold 256KiB + Range 206 + 白名单`，provisioning.rs 头部文档 §75-80——把同一机制推广到 film 资产上传/下载与 /apps-assets 即可，不是从零造轮子）。
- 工作量：① 0.5 天；② 按端点各 0.5-1 天（film 上传+下载优先）。

**A3-2【P0/快赢】静态资源零缓存头——每次访问全量重下 ~700KB gzip**
- 证据：`http.rs:1672-1760` static_handler 与 `serve_bytes` 只设 Content-Type；全仓 `Cache-Control/ETag/Last-Modified` 仅出现在 git 端点（http.rs:3911）。而 Vite 产物**已经是内容哈希文件名**（`assets/AdminConsole-Dr_56sik.js` 等）——immutable 缓存零风险。
- 建议：`/assets/*`、`/static/*` 返回 `Cache-Control: public, max-age=31536000, immutable`；`index.html` 返回 `no-cache`（保证发版即时生效）。两行 header 的事。
- 工作量：**0.1 天**（+冒烟验证）。内网体感有限，公网 aliyun 节点收益明显。

**A3-3【P3】`decode_body` 对二进制体的双次解析尝试**
- 非 JSON 二进制 body 先走一次注定失败的 `serde_json::from_slice` 全量扫描再降级 UTF-8/Null。有 Content-Type 可判时可以短路。收益小，顺手改。

### A4. P2P

**A4-1【P2】联邦桥观测 eprintln 打印完整 payload**
- 证据：main.rs:1160 附近 `eprintln!("[os-api][p2p] recv from=… payload={}", … m.payload)`——**每条入站消息全文进 journald**；gossip digest 上限 64KB、市场联邦条目广播、IM 大厅消息都在这个通道上。日志噪音 + IO 开销 + journal 磁盘占用三重浪费。
- 建议：payload 截断（前 200 字节 + 总长 + kind），或降为可开关的 debug 级。
- 工作量：0.1 天。

**A4-2【P3】节奏与流量核查——整体合理，无需调**
- ping 5s / refresh 30s / meta_tick 5s（os-p2p/src/api.rs:1819-1820、meta.rs:1052）；gossip 每 6 tick=30s 全量 digest 到所有活跃连接，64KB 截断逐条微调（meta.rs:1282-1320）；TTL 清除 25 分钟节流 + 启动即扫；市场联邦 30 分钟重播 + on-connect 定向补推限幅 100ms/条。**自述四字段搭心跳零新流量**（v0.1.45 设计兑现）。当前节点规模（≤5）下没有优化空间，不建议动。

### A5. 前端

**A5-1【P1】i18n 四语言全量打进首屏 chunk（~235KB 原始）**
- 证据：static-dist/assets/`i18n-BFZFwQRK.js` 235,237B（仅次于 AdminConsole 352KB）；locales 四文件共 270KB 源（en 64K/ja 81K/zh-CN 62K/zh-TW 62K）。用户只_need一种语言。
- 建议：locale 改动态 `import()` 按语言拆包；AdminConsole 352KB 查一下是否被首屏同步加载（路由懒加载应已隔离，需验证首屏关键路径实际只载 entry+router+login 相关）。
- 工作量：i18n 拆包 0.5-1 天；首屏路径核账 0.5 天。

**A5-2【P1】film 应用三套轮询并存 + 逐任务串行请求**
- 证据：`apps/film/src/FilmStudio.vue:844,1498`（POLL_MS=2000，`pollTasks` 对每个活跃任务 **for 循环串行 await** `filmGetTask`——N 任务=每 2 秒 N 个串行 HTTP）；`flow/shotgen.ts:271,438`（3s）；`flow/StoryPage.vue:125,290`（6s `loadSources`，有活跃任务才发——这点做得对）。管线全开时=2s×N + 3s + 6s 三路并发轮询同一后端。
- 建议：① `Promise.all` 并行化（10 行改动）；② 后端加单端点 `GET /film/tasks?ids=a,b,c` 批量查（或 SSE 推送终态——v0.1.11 NxToast 已有任务中心语义，就差服务端推送）；③ 轮询加退避（连续无变化 2s→5s→10s）。
- 工作量：① 0.2 天；② 0.5-1 天；③ 0.5 天。

**A5-3【P2】主前端轮询面广但分散**
- setInterval 分布：Provisioning/Chat/AgentHub 各 6 处、SystemWidget 5、LlmModels/ApiGateway 各 4、InstanceMonitor 4、RepoCiTab 日志滚底 1.5s。各视图各自为政、无统一调度/去重/页面隐藏暂停（`document.visibilityState`）。
- 建议：不必大改——统一一个 `usePolling(fn, ms)` composable（onUnmounted 自动清 + 页面隐藏暂停），新代码用它，旧代码顺手迁。0.5-1 天框架 + 渐进迁移。

**A5-4【P3】无客户端数据缓存层**
- 每个视图 onMounted 独立拉数（如 catalog：AppStore 与 CodeHub Lobby 各拉各的）。当前规模可接受；若后续做任务中心/跨视图联动再考虑轻量 cache composable，暂不建议引库。

---

## B. 架构与代码健康

### B1. 大文件体检（实测行数，非估）

| 文件 | 行数 | 拆分建议 | 优先级 |
|---|---|---|---|
| handlers/im.rs | 11,722 | 按 消息/会话/大厅/文件/webhook/联邦 六域拆 `im/` 目录（film_hub 已开此模式：`film_hub/{blender,collab,git,tests}.rs`） | P1 |
| os-nexhub/nexhub_lobby.rs | 11,255 | 大厅条目/联邦传输/评分分层拆 | P2 |
| handlers/film_hub.rs | 9,786（+tests 8,860） | 继续 `film_hub/` 拆：shotgen/cost/casting/storyboard 各成模块 | P2 |
| handlers/film.rs | 8,276 | 与 film_hub 共享 FilmCtx 已拆一半，任务注册表可独立 | P2 |
| model_hub/api_gateway/api_market/provisioning/llm.rs | 6.6K-7.7K | 每个先抽"路由表+纯函数"与"DB 层"两刀 | P3 |
| web LlmModels.vue | 5,087 | 先抽 SSE 解析（sdk/gateway.ts 已有对齐实现，见 B2-3）与表单区 | P2 |
| web Chat.vue | 5,065 | 消息列表/输入区/文件上传三组件化 | P2 |
| web ApiGateway.vue | 4,766 | 七 Tab 各一子组件（AppStore 1180 行是已 refactor 的好样板） | P2 |
| web api/client.ts | 4,358 | 按域拆 `api/{im,gateway,storage,…}.ts` barrel 导出 | P3 |

- 注：任务书假设的"AppStore.vue 巨型 SFC"**已过时**——现为 1180 行且用 useAppDeploy 组合式收编（2026-09-05 批完成）。当前最大 SFC 是 LlmModels/Chat/ApiGateway。
- 拆分路线统一按"**域子模块目录 + tests 跟迁 + 路由表不动**"模式（film_hub 已验证零行为变化）。每文件 1-2 天，可按需渐进，不必专项。

### B2. 重复实现

**B2-1【P1】五套任务框架并存，无共享抽象**
- 证据：`film.rs:590` FilmTask（内存表+环形日志）；`apps_handler.rs:180,363` AppInstallTask（内存 Vec）；`provisioning.rs:291,387,1853-1856` IsoTask+DeployTask（内存 Vec ×2）；`nexhub_ci.rs` ci_runs（SQLite，唯一持久化的）；前端 trackFilmTask 再造一遍任务中心。grep 全仓无共享 task registry/mod。
- 建议：统一 `os-common::task`（或 os-api 内 task_center.rs）：id/stage/status/progress/环形日志/终态回调 + **SQLite 持久化**（重启后 running→interrupted 如实呈现，解决内存任务重启即丢）。新端点 `GET /api/v1/tasks?kind=&state=` 顺带成为前端统一任务中心的数据源。CI 已有 SQLite 版可作参考实现。
- 工作量：设计+核心 2 天；五处迁移各 0.5-1 天（可分批）。

**B2-2【P2】`demo_iso_tasks()` 在生产构造路径注入假数据**
- 证据：`provisioning.rs:1916` `iso_tasks: Arc::new(Mutex::new(demo_iso_tasks()))`（3787 定义，"OS Standard 0.1.0" 假 ISO、假 sha256）——对照 api_gateway 2026-08-30 已把 demo seed 全撤（"删除的不是真实的数据"事故后），provisioning 这处漏网。
- 建议：`#[cfg(not(test))]` 空表起步（照抄 api_gateway open_db 的处理）。**诚实性问题，P0 级快赢**。
- 工作量：0.2 天。

**B2-3【P2】前端三套 API 层 + SSE 双实现**
- 证据：主前端 `api/client.ts` 4358 行（自有 request/超时/token）；`sdk/`（应用 SDK，gateway.ts 注释自述：主前端 LlmModels.vue 内联 SSE 解析"无法抽出"故 SDK **独立再实现一遍**语义对齐版）；apps/film/src/api.ts 1570 行（第三套 request 封装）。apps 三仓各带一份。
- 建议：不求合并 SDK 与 client（宿主/独立双形态是有意设计），但主前端内部应统一：client.ts 按域拆 + SSE 解析抽 `utils/sse.ts` 一处实现（LlmModels.vue 与 sdk 各自引用）。
- 工作量：SSE 抽取 0.5 天；client.ts 域拆 1 天。

**B2-4【P3】两套 catalog 拉取**
- AppStore 走 `useAppDeploy`（GET /api/v1/apps/catalog）+ 本机 installed apps；CodeHub LobbyPage 另拉 repo 列表聚合同类信息。语义有差（包目录 vs 仓列表），非纯重复；等 B2-1 任务中心/统一注册表落地时顺带收敛即可，不单独立项。

### B3. os-api 与 os-provision 双轨同构

**【P2】网络装机域三文件在 os-api 侧整段重写**
- 证据：os-provision `pxe.rs`(501)+`netboot.rs`(638)+`iso9660.rs`(493)=1,632 行纯逻辑；os-api `handlers/provisioning.rs` 文档自述"本节为 HTTP 侧**自包含实现**——PXE 域'搬自 pxe.rs'同款双轨"（§75-80）、"内置 ISO9660 只读提取（**os-provision::iso9660 的 os-api 自包含同款**）"（§1182）。os-api 的 Cargo.toml **不依赖** os-provision（grep use os_provision = 0 处）；os-provision 仅剩 os-integration（集成测试）在用。
- 建议：短期接受双轨（历史决策：os-api 装机面想零依赖下沉），但**把 os-provision 三文件标注为"契约参考实现"**或反向让 os-api 依赖 os-provision（纯逻辑 crate 无重依赖，加依赖成本低）。至少 ISO9660 提取器这类 500 行纯算法不该两份维护——改 bug 要改两处。
- 工作量：os-api 加依赖并删自包含副本 0.5-1 天（测试已双侧都有）；仅文档标注 0.1 天。

### B4. 错误处理一致性

**B4-1【P2】日志三套机制并存，os-p2p 的 tracing 静默丢弃**
- 证据：os-api binary 全文件 **0 处 tracing**（无 subscriber）；os-p2p 大量 `tracing::debug!`（如 meta.rs:1319 截断日志）——**journald 里无声**（meta.rs:1089 注释自认"tracing 在 journald 里无声"故用 eprintln）；os-api 侧 eprintln 400 处（非测试），前缀风格 20+ 种（[os-api]/[filmhub]/[ci]/[live-fed]/[nettest]…大体是组件名但无规范），37 处裸 eprintln 无前缀。
- 建议：不急着全量换 tracing。最小动作：① os-api main 装 `tracing_subscriber::fmt().with_writer(std::io::stderr)`（一行，立刻激活 os-p2p 既有的 debug/info 事件，且带级别/时间戳）；② 定前缀规范 `[component][module]` 补齐 37 处裸调。
- 工作量：① 0.2 天；② 0.5 天。

**B4-2【P2】用户面错误文案中英混杂**
- 证据：http 层英文（"method not allowed"/"read body failed"/"not found"/"rate limited"——http.rs:257/285/1677/1765、gateway_impl.rs:267/296）；组件层中文（"该文件正在执行〈…〉任务"film_hub.rs:6595、"扫描已触发" media.rs:532）。前端展示直接裸出。
- 建议：定一条铁律——**用户面 error 字段统一中文（或统一走 error code + 前端翻译）**。网关层 4 处英文改成中文即可对齐现状主流（101 处 "error": 中文占绝对多数）。
- 工作量：0.3 天（含 i18n 键）。

---

## C. 产品与 UX（非视觉）

### C1. 端点一致性

**C1-1【P2】分页三式并存 + 信封不统一**
- 证据：分页参数 `limit`（im.rs:1419 等）、`limit+offset`（个别）、游标 `after_id`（im.rs:5828）、尾部 `tail`（llm.rs:3164）；列表响应有裸数组/`{apps:[...]}`/`{root,files:[...]}` 信封多形态（files 信封曾致前端 e.filter 崩——MEMORY 2026-09-06 已修但防御式归一 hubFileEntries 三态仍在四处消费，正是漂移的持续成本）。
- 建议：立约定文档（docs/ 里加 API_CONVENTIONS.md）：列表统一 `{items, total?}` 信封、分页统一 `limit+cursor`、时间戳统一 ms 或 ISO 二选一。**新端点强制、旧端点遇改则迁**，不做一次性大迁移。
- 工作量：约定文档 0.5 天；渐进执行。

**C1-2【P3】三代路由风格并存**
- 早期无命名空间 `/api/v1/pools /disks /vms /shares`；中期组件前缀 `/api/v1/{film,llm,im,…}`；film 域内动词后缀 RPC 风（`…/generate|extract|commit|import|merge`）与新 collab 资源风（`…/issues/:iid/comments`）同文件混居。家族内部自洽、跨家族漂移。**不建议改造**（470+ 路由重命名破坏面大），只在约定文档里记录"新端点用资源风 + 动作子资源"。

### C2. 任务系统统一度

（详见 B2-1）结论：**五套框架、四套纯内存**——film 任务重启即丢（running 态蒸发）、apps 安装任务同、provisioning 部署任务同；唯一持久化的是 ci_runs。前端任务中心（v0.1.11 NxToast + 顶栏指示器）已经是统一皮，缺的是统一后端。这是产品线"可信度"问题：任务中断后用户看不到任何痕迹。**建议列为 P1 专项**。

### C3. 文档漂移（5 份抽检）

| 文档 | 最后更新 | 漂移点 |
|---|---|---|
| docs/DEPLOYMENT.md | 08-21 | 标题"22 crate"正文打补丁到 26（实际 **29**）；未提 scripts/release.sh（现行真发版流程）；§2-4 osd 全家桶与 §9 实跑形态并存无"以 §9 为准"置顶声明（有但埋得深） |
| docs/ARCHITECTURE.md | 08-21 | "27 crate"（实际 29）；§8.1 路由统计口径 304/330 与 MEMORY "470+" 三种说法并存 |
| README.md | — | "26 crate / ~330 路由 / 31 view"（实际 29 / 470+ / 33 view）——README 是对外门面，数字最该准 |
| docs/PERFORMANCE_BASELINE.md | 08-05 | 基线 HEAD 67e014d 距今 2 个月、5 crate criterion 基线未随路由优化（routing.rs 静态 O(1) 改造已落地）复跑——基线本身声明了用途，属"应复跑"而非错误 |
| docs/NET_BOOT.md / FILM_STUDIO.md | 09-18 / 09-09 | 抽检 env 清单与实现一致（NEXOS_FILM_VIDEO_TIMEOUT_SECS/SOURCE_MAX_MB 均在）——**新鲜无漂移** |

- 建议：① README/ARCHITECTURE 数字一次校准（29 crate、路由数用 `os-api --check` 实测口径）；② DEPLOYMENT.md §0 加"当前发版=scripts/release.sh"指向；③ PERFORMANCE_BASELINE 排一次复跑（A1-3 改造后正好需要对照）。
- 工作量：①② 0.5 天；③ 0.5 天（跑 criterion 记数）。

### C4. 国际化完整性

**【P1】主前端硬编码中文残留面大（film 已修，主前端未跟）**
- 证据（模板/脚本字面量，排除注释）：ApiGateway.vue 120 行（Tab 标签 `'渠道'/'实例'/'令牌'` 等 L291-301、计费 hint L350-351）、Provisioning 82、Storage 75、LlmModels 62、Chat 58、Network 55、Downloads 36、Blockchain 35、Surveillance 34、Forwarding/Backup 28/28、AgentHub 27…（同文件内又混用 261 处 `t()`——ApiGateway 单文件即中英斑驳）。
- 建议：按视图逐个迁 i18n 键（film 2026-09 批 62 键×4 的作业模式可直接复用）；优先高频页 ApiGateway/Storage/Chat。可与 A5-1 懒加载同批做（反正要动 i18n 装载）。
- 工作量：每视图 0.3-0.5 天 ×12 视图 ≈ 4-6 天（可拆给多代理并行）。

---

## D. 运维

### D1. 日志

（详见 B4-1）现状：eprintln → journald（systemd 承接轮转，这点 OK），但无级别、无结构化、os-p2p tracing 静默、入站 p2p payload 全文打印（A4-1）。最小改造 = 装 tracing subscriber + payload 截断 + 前缀规范，合计 **<1 天**，收益：日志可 grep 级别、journald 体积可控。

### D2. 备份

**【P1】15 个 SQLite + film hub 数据树无任何默认备份任务**
- 证据：backup.rs 能力 = ZFS 快照任务（spawn_blocking 真实 zfs snapshot + 保留策略 + auto_snapshot 调度），**但任务表用户创建、无预置**——/tank/os-data（全部 DB + film 项目树 + 身份账本 identity-ledger.json）不在任何快照计划内；113 的六仓 mirror 是 09-18 人工灾备一次性动作；gateway.db-shm/wal 常驻（WAL 未 checkpoint 也无所谓，但快照 SQLite 需 `VACUUM INTO`/`.backup` 或容忍 WAL 一致性窗口）。
- 建议：① 预置一条默认备份任务（每日 zfs snapshot tank/os-data，保留 7 天）——backup.rs 现成能力，只差 seed；② SQLite 追加 `VACUUM INTO /tank/backups/xx-YYYYMMDD.db` 式逻辑导出（每日一个 DB 几十 KB-几 MB，成本低可靠高）；③ 网络装机 5.8G ISO 仓不进备份（可重建，sidecar 有 sha256）。
- 工作量：① 0.3 天；② 1 天（走统一任务框架更好，可挂 B2-1）。

### D3. 监控

**现状好于任务书假设**：monitor.rs 已有告警引擎（cpu/mem/disk/service 四类阈值 + 5 分钟同源去重 + ack 端点 + history，`spawn_alert_engine` 常驻）。
缺口：
- **【P2】任务积压无告警**：CI queued 堆积（Semaphore(2) 满载）、film 任务 running 超时悬挂不产生 alert；
- **【P2】告警无出站通道**：只进 DB+UI，无 webhook/邮件——人不在页面上等于没告警。IM 组件就在本进程（发 IM 通知零成本）或复用 ads/资讯栏机制；
- 【P3】P2P peer 数/联邦中继失败率不在指标内。
- 工作量：积压告警 0.5 天（挂 monitor check_thresholds 加两条规则）；出站通道 1 天。

### D4. 升级链

**【P1】release.sh 已覆盖 80%，但三处缺口各有实伤史**
- 现状（scripts/release.sh，86 行）：bump→fmt/clippy/test→双架构构建→tag/push→dist 三工件刷新→aliyun 经 Files API 同步。已脚本化的部分是好的。
- 缺口：
  1. **不跑 `npm run build`**——前端改动若忘手动构建，rust-embed 嵌入旧 web（MEMORY 记载两类事故：显示旧版、"dist 三件必刷"）；0.2 天补一行 + static-dist 新旧 diff 检查。
  2. **从工作树构建**——违反自家"部署构建污染纪律"（v0.1.43 铁律：release 必须从 `git archive <tag>` 纯净导出树构建，曾发生"含未提交代码报旧版本"污染 aliyun 公网源）；脚本应改 tag 导出构建，0.5 天。
  3. **aliyun 同步走 JSON b64 信封传整个二进制**（release.sh ⑤c：数十 MB release 二进制 base64 后过 /files/upload 的 usize::MAX body 通道）——慢且吃内存（对照 A3-1），scp/rsync 或分块上传更稳；顺带 113 部署（scp+static-dist tar+restart）与部署后 `/healthz` 探活未进脚本。
- 建议终态：`scripts/release.sh 0.1.x` 一条命令含 前端构建→纯净导出构建→dist→双节点部署→healthz 探活→回滚提示。合计 1-1.5 天。

---

## E. 快赢清单（一天内能做、收益明显，Top 10）

| # | 事项 | 文件:证据 | 工作量 | 收益 |
|---|---|---|---|---|
| 1 | 静态资源 `Cache-Control: immutable`（/assets/* 哈希名已具备条件；index.html 除外） | http.rs:1672 serve_bytes | 0.1d | 每次访问省 ~700KB gzip 重下，公网节点体感明显 |
| 2 | 请求体上限（usize::MAX → env 可配 128MB，413 如实拒） | http.rs:283 | 0.5d | 堵住内存放大入口（A3-1①） |
| 3 | `/gateway/stats` 改 SQL 聚合 + logs 保留策略（30 天清理） | api_gateway.rs:2298 | 0.5d | 消除无界表增长+全表载入（A2-1） |
| 4 | `demo_iso_tasks()` 生产路径移除（cfg(not(test)) 空表） | provisioning.rs:1916 | 0.2d | 假数据诚实性（B2-2） |
| 5 | p2p recv 日志 payload 截断 200B | main.rs:1160 | 0.1d | journald 噪音/磁盘大降（A4-1） |
| 6 | `busy_timeout(5s)` 进 11 处 open_db | api_gateway.rs:3080 等 | 0.5d | 防 SQLITE_BUSY 立败（A2-2） |
| 7 | film pollTasks 并行化 Promise.all + 空闲退避 | apps/film/.../FilmStudio.vue:1508 | 0.3d | N 任务时轮询延迟 N×RTT→1×RTT（A5-2①） |
| 8 | tracing subscriber 一行装上（激活 os-p2p 既有日志+级别） | main.rs | 0.2d | 日志可分级可 grep（B4-1①） |
| 9 | HUB_META_LOCK 改 per-project 锁 | film_hub.rs:331 | 0.5d | 跨项目互阻塞消除（A1-2①） |
| 10 | release.sh 补 `npm run build` + tag 纯净导出 + 部署后 healthz 探活 | scripts/release.sh | 0.5d | 消灭两类已发生过的事故（D4-1/2） |

---

## F. Top 5 高价值优化（非快赢，需立项）

1. **DB 访问层收敛（连接策略 + spawn_blocking + 共享 helper）**——解锁 IM/film/gateway 组件并发，消除 async 内同步阻塞；约 2-3 天。
2. **大载荷流式通道推广**——把 ISO 直传特挂（已验证的 256KiB unfold+Range 模式）推广到 film 上传/下载与应用资产，替代 b64 JSON 信封；按端点 0.5-1 天/个，先 film 源导入与 mp4 直传。
3. **统一任务框架（持久化 + /api/v1/tasks 聚合端点）**——收编五套任务实现，重启不丢、前端任务中心单一数据源；核心 2 天 + 迁移分批。
4. **主前端 i18n 补齐 + locale 懒加载**——12 视图硬编码中文迁键 + 四语言拆包省 ~200KB 首屏；4-6 天（可并行拆代理）。
5. **巨型 handler/SFC 拆分（im.rs / nexhub_lobby.rs / LlmModels.vue / Chat.vue / ApiGateway.vue）**——按 film_hub 已验证的域子模块模式渐进；每文件 1-2 天，长期可维护性与 AI 代理协作效率双收。

## G. 风险提示（实施时注意）

- **A1-2 锁改造**：per-project 化要保持"同项目多元文件读改写原子性"（activity/assets/ownership 同锁语义），建议同批加并发测试再切。
- **A3-2 缓存头**：index.html 必须 no-cache，否则发版不生效；/apps-assets 走独立路由需同批覆盖。
- **A3-1② 流式迁移**：前端消费是 b64 信封假定，改流式要前后端同版本发布（或保留旧端点过渡一个版本）。
- **B2-1 任务持久化**：内存任务语义（重启丢）改为持久后，前端对 404 的兜底逻辑要同步（film api.ts:451 注释明确"未知 id 抛 404"）。
- **D2 备份**：ZFS 快照抓 WAL 态 DB 可能拿到中间事务，逻辑导出（VACUUM INTO）更稳，二选一或叠加。
- **release.sh 改 git archive 构建**：会改变构建目录习惯（target 缓存冷），首次构建时间变长，属预期。

---

## 附：本次审查数据快照（2026-09-24 实测）

- Rust 大文件 TOP：im.rs 11,722 / nexhub_lobby.rs 11,255 / film_hub.rs 9,786 / film_hub/tests.rs 8,860 / film.rs 8,276 / model_hub.rs 7,685 / api_gateway.rs 7,683 / api_market.rs 7,609 / provisioning.rs 6,656 / llm.rs 6,640
- 前端大文件 TOP：LlmModels.vue 5,087 / Chat.vue 5,065 / ApiGateway.vue 4,766 / client.ts 4,358 / Storage.vue 2,982 / Provisioning.vue 2,865
- static-dist：3.40MB 原始 / ~701KB gzip（估算 gzip -9 全量）；最大 chunk：AdminConsole 352KB、i18n 235KB、LlmModels 141KB、ApiGateway 100KB、router 90KB
- eprintln（非测试）400 处、前缀 20+ 种、裸调用 37 处；busy_timeout 0 处；spawn_blocking：files/system/share/network_exit 有，im/film_hub/api_gateway/api_market/llm **无**
- 生产 DB（/tank/os-data，只读探查）：15 库；gateway.logs=29、im_messages=154、ci_runs=28、film_projects=1——当前量级小，问题在结构不在存量
- 轮询汇总：film 2s(任务,串行/N)+3s(shotgen)+6s(源列表)；主前端 1.5s(CI 日志)~15s 不等 20+ 处 setInterval
