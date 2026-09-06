# NexOS 全功能再调研报告（第二轮）

> 调研日期：2026-08-20 · 方法：静态代码扫描（只读）+ `git log --since=2026-08-15` 增量核对
> 基线：`docs/FEATURE_SURVEY.md`（2026-08-15，30 应用四维评分 + 10 项 ROI 清单）
> 本轮范围：基线结论逐条复核 + 8 月 15 日后新上线功能（NexHub 货币化/悬赏/链上身份、网关四计费模式+充值订单、转发工具、IM 区块链认证、媒体生成、FileBrowser、模型对话并入、vLLM 监控）的五维评估。
> 说明：`LlmModels.vue` / `client.ts` 前端代理在途改动，本报告对二者只登记现状、不给改动建议。

---

## 1. 执行摘要

### 1.1 三大最值得投入的优化方向

**① 变现"真支付"闭环：NexHub 链上验真 + 网关充值自动核验 + per_image 计费接线（三件一起做）。**
两条变现线当前都停在"自证支付"：NexHub 购买授权的 `verify_payment`（`os-nexhub/src/nexhub_lobby.rs:2104`）只校验货币一致、金额够额、**txid 非空**——任意字符串即可"买下"任意付费条目（前端购买对话框也让用户自填收据，`CodeHub.vue:516`）；网关充值订单是 admin 手动 confirm（`api_gateway.rs` payments 段，Phase 2 RPC 自动核验未做）；`per_image` 计费的扣费入口 `charge_image_call` 挂着 `#[allow(dead_code)]`（`api_gateway.rs:1072`）**零调用方**——`media_gen.rs` 生图端点根本没接网关计费。而验真所需的零件全在库内：区块链模块 7 条链预设 + RPC 节点启停、os-wallet k256/alloy 密钥学、`GATEWAY_MONETIZATION.md` 已写好"一单一址 + 轮询入账"的 Phase 2 设计。变现是本项目当前主叙事，这是唯一"让钱真的进来"的方向，且第一件（NexHub 验真）不做的话付费门禁形同虚设。

**② 链上身份从"两个应用"升级为"系统第三凭证"。**
`os-common/src/chain_auth.rs`（407 行/7 测试）已把挑战-签名内核泛化：IM 与 NexHub 各挂独立实例、密钥对前端共用（`useChainIdentity.ts`/`useImIdentity.ts` 同一 localStorage 私钥）、去自报化彻底（sender/publisher/buyer 全部服务端反查覆盖，`im.rs:341` / `nexhub_lobby.rs:567`）。但覆盖率只有 2/30 应用：**media_gen 生图/生视频仍要求系统 admin token**——大厅身份用户根本用不了"AI 产能"，与"网关=变现通道"定位直接矛盾；blockchain 钱包写操作仍是共享 admin token 无归因；appstore"用户发布"无身份；旧断裂（系统用户登不进 Web、契约 `ApiRequest` 仍无 `auth` 字段，`gateway.rs:82`）原样。让网关 dispatch 识别 `Authorization: Bearer <chain token>` 为第三种 Principal（与 admin token / JWT 并列），媒体生成与区块链写操作即可获得真实调用方归因——这是把已有认证资产复用成系统级信任基础，比新建登录体系便宜得多。

**③ 无人值守韧性三小件：转发断线重连、IM 离线补拉、Files 上传/下载。**
三个天级工作量、高确定性的"最后一公里"：(a) 转发工具持久化与 autostart 都做了，但 `resume_autostart`（`forwarding.rs:465`，经 `spawn_autostart_resume` 在 main.rs 启动时调一次）**只在 os-api 启动时跑**——ssh 隧道半夜断线后状态停在 stopped 直到下次重启，无 watchdog 周期巡检；RDP 转发绑 `0.0.0.0` 无认证、只有累计连接计数（`forwarding.rs:408/1151`），谁连过、传了多少字节全无记录；(b) IM WS 有 5s 固定退避重连（`Chat.vue:422`），但**重连成功后不补拉**当前会话增量与未读数——断线期间的消息缺口要手动切换会话才能看到；(c) 文件管理器浏览体验这轮大幅升级（面包屑/排序/三态/用量懒加载，FileBrowser 组件化复用到存储页），但**上传/下载仍然后端无端点**（`FileBrowser.vue:24` 自己写着 TODO），组件已把 UI 挂点备好。

### 1.2 两大最该砍 / 降级的方向

**① 视频生成（media_gen §B）→ UI 降级"配置后启用"，代码保留。**
诚实的框架（任务创建即 failed、不假装排队是优点），但两个后端都是占位：`local` 本地模型未就绪、`external` 读 env `NEXOS_VIDEO_API_URL`（`media_gen.rs:255`）未配置——**当前没有任何路径能成功生成视频**，任务列表恒为 failed，用户点一次挫一次。建议前端在 env 未配置时隐藏生视频入口或折叠为"需配置外部服务"提示；等真实外部服务（或本地视频模型）就绪再开。不要删代码——trait 抽象是对的。

**② 区块链钱包明文私钥 → 变现越近越刺眼，加密前降级写操作。**
旧报告已列（`blockchain.rs:514` `WALLETS_FILE = "/tank/os-data/wallets.json"` 含 `private_key` 明文），本轮**未修**；而 `GATEWAY_MONETIZATION.md` 自己把"钱包私钥明文落盘问题必须先解决"列为 Phase 3 安全前置红线——网关充值、NexHub 悬赏都在往"链上清算"走，清算底座的密钥不能裸奔。建议：钱包写端点（create/import/sign）加二次确认并从默认 Dock 降级，或先做文件权限 0600 + 前端警示，把加密存储（ passphrase/age）排进 Phase 2.5。连带提醒：旧 O9 的 StreamingCenter 合并与 Provisioning ISO/deploy 裁剪两项也均未执行，占位依旧在一级导航。

---

## 2. 旧结论复核表（2026-08-15 建议 → 现状 → 更新判定）

| 旧建议 | 现状（2026-08-20 核实） | 更新判定 |
|---|---|---|
| O1 信任模型闭环（登录页+JWT+契约 auth 透传） | 全局登录/JWT **未做**（全仓仍无 `/api/v1/auth/login`，`http.rs` 仍是 OS_ADMIN_TOKEN 精确匹配→admin Principal）；但被新路线**部分取代**：IM+NexHub 上链上身份（挑战-签名，`chain_auth.rs`），契约 `ApiRequest` 仍无 `auth` 字段（`gateway.rs:82`，doc 改口"handler 自读 Bearer"——绕过而非修复）；Users 建的系统用户依旧登不进 Web | **部分被取代**：缺口从"建登录体系"转移为"ChainAuth 升级为系统第三凭证"（见 §1.1②） |
| O2 最小联邦（os-discover 接 Nodes/IM） | **未动**。`discover.rs` 仍是内存假节点列表（模块头注释原文未改）；os-discover（mDNS/beacon/mTLS）仍零消费；Nodes.vue 189 行未变 | **维持原建议**，优先级让位于变现线 |
| O3 Backup 真执行 + 任务落库 | **未动**（backup.rs 自 8-15 起无功能提交）：任务内存态、`run` 只翻 status（`backup.rs:431` 注释自述）、无恢复路径。注：zfs send/recv 远程复制（`POST /backup/replication`）基线前已真执行，非本轮新增 | **维持原建议**（方案 B 降级 0.5 天仍是快赢） |
| O4 Files 上传/下载/复制/移动 | **上传/下载未做**；新增 `GET /files/usage` 目录用量端点（`files.rs:105`）+ 前端 FileBrowser 组件化（面包屑/排序/三态/懒加载用量，复用到存储页）。上传/下载 TODO 注释在 `FileBrowser.vue:24` 原样 | **浏览面已达标，传输面维持原建议**（UI 挂点已备好，只差后端 2 端点） |
| O5 /tank 与 env 命名收敛 | **未动**：`"/tank` 字面量 .rs 内 168 处；OS_*/NEXOS_* 双命名并存（35 处 admin token 引用） | 维持，机械性工作 |
| O6 CloudSync 持久化 + Monitor history 落库 | **均未动**：cloudsync 任务定义仍内存（`cloudsync.rs:6-8` 模块头自述）；`GET /monitor/history` 仍返回 `demo_history()`（`monitor.rs:411/1176`） | 维持（各 0.5-1 天） |
| O7 UX 基建统一（Dialog/错误/空态） | **部分推进**：`window.confirm` 21→15 文件；FileBrowser 的三态（骨架/空/错误重试）+ 删除两步确认成为新范式；`friendlyError` 仍 17 文件（新增 Forwarding/FileBrowser 用上了，但 Chat/ApiGateway/CodeHub 三个本轮大改页面都没用）；CodeHub 自创 `lobbyWriteErr`（401/403→身份引导文案）是好范式但独一份 | **进行中**，新页面错误文案各自为政（见 §4） |
| O8 轮询→WS 推广 | **未动**：WS 消费者仍只有 Chat；Monitor/Nodes/AppStore/ModelHub/QrTransfer/Forwarding/LlmModels 全在 setInterval | 维持 |
| O9 降级/砍除（Blockchain 移出 Dock+钱包加密；Streaming 合并；Provisioning 裁剪） | **未执行**：blockchain 仍在 Dock（`appRegistry.ts:200`）；钱包明文未修（§1.2②）；Streaming 1,688 行 / Provisioning 1,347 行原样 | 维持并**升紧迫级**（变现红线自认） |
| O10 OS_APPLY_SYSTEM 门禁可见性 | **未见变化**：share/user 写操作响应仍无 `applied` 标记，UI 仍无从感知门禁状态 | 维持 |

**复核总结：10 项旧建议 0 项完成、1 项部分推进（O7）、1 项被新路线部分取代（O1）。** 五天窗口的全部产能投在了变现叙事的新功能线上（网关计费、NexHub 货币化、链上身份、转发、媒体生成），旧债一分未还——这个取舍本身成立，但意味着新功能清单（§3）里"收尾/接线"类条目密度会很高。

---

## 3. 新功能优化清单（按 ROI 排序）

| # | 做什么 | 证据（file:line） | 工作量 | 依赖 |
|---|---|---|---|---|
| N1 | **NexHub 购买授权链上验真**：`verify_payment` 接 os-wallet ChainAdapter——RPC 按 txid 查交易真实存在 + 金额 ≥ price + 收款地址匹配收款方；查不到 → 402（可先只支持 EVM/BTC 主流，其余币种维持 admin 复核） | `nexhub_lobby.rs:2104-2124`（txid 非空即过）；前端自填报单 `CodeHub.vue:516-542`；模块头自认"二期接入点 `verify_payment_onchain` 仅预留钩子" | 3~5 天 | blockchain RPC（已有） |
| N2 | **生图接网关计费（per_image 接线）**：`POST /api/v1/media/image` 认 gateway sk-os- key（或网关加 `/gateway/v1/images/generations` 代理转发到 media_gen），成功处调 `charge_image_call`，移除 `#[allow(dead_code)]` | `api_gateway.rs:1072-1073`（dead_code + 注释自述"生图端点尚不存在故无调用方"——同批 media_gen.rs 已存在但未接）；`media_gen.rs` 全文无 gateway 引用 | 2~3 天 | N6（非 admin 调用方身份）或 sk-os- 直通 |
| N3 | **网关充值自动核验（EVM 一单一址）**：订单创建时经 os-wallet 派生唯一收款地址（弃静态 env 地址），后台轮询入账（金额+确认数阈值）自动 confirm | `api_gateway.rs` payments 段（手动 confirm/reject，幂等已做好）；`GATEWAY_MONETIZATION.md` §Phase 2 设计完整；收款地址现为 env 静态三币种（前端还专门识别占位地址并红字警示 `ApiGateway.vue:145`） | 5~8 天 | os-wallet（已有） |
| N4 | **转发守护**：后台 watchdog 周期巡检（对 running 隧道 try_wait/kill -0，死亡且 autostart → 退避重启）；RDP 加连接日志表（peer/时间/字节数/时长） | `forwarding.rs:465` `resume_autostart` 仅 main.rs:523 启动时调用一次；`forwarding.rs:1151-1163` accept 只 `bump_rdp_connections` 计数，无日志表（建表仅 ssh_tunnels/rdp_forwards 两个，forwarding.rs:1338/1356） | 1~2 天 | 无 |
| N5 | **IM 离线消息补拉**：WS onopen（含重连）后拉当前会话增量 + 刷新 unreadMap；重连退避加抖动/上限 | `Chat.vue:422-429` scheduleReconnect 只重连不补拉；onmessage 只处理增量（Chat.vue:377-410），断线窗口消息丢失展示；30s 心跳只刷大厅在线（Chat.vue:304-311） | 0.5~1 天 | 无 |
| N6 | **ChainAuth nonce 桶过期清扫 + 挑战限速**：定期清 60s 过期 nonce（或容量上限 LRU）；challenge/verify 按源限速防刷桶 | `chain_auth.rs:144-172`：`create_nonce` 只覆盖同 pubkey，**无人认领的过期 nonce 永不删除**（HashMap 只增不减）；`take_nonce` 过期路径也不 remove | 0.5 天 | 无 |
| N7 | **RDP 转发收敛暴露面**：listen 地址可配置（默认 127.0.0.1，显式选 0.0.0.0 才对外）+ 连接来源白名单选项 | `forwarding.rs:408` 无条件 `TcpListener::bind(("0.0.0.0", port))`，LAN 内任意主机可借道直连内网 Windows，无认证无日志 | 0.5~1 天 | 无 |
| N8 | **media_gen 状态持久化 + 视频降级**：recent 环形/视频任务落 SQLite（现全内存重启丢）；视频入口在 env 未配置时 UI 折叠 | `media_gen.rs`（内存 `Mutex<Vec>` + 环形 Vec，模块头自述）；`media_gen.rs:255` external 后端 env 未配必失败 | 0.5~1 天 | 无 |
| N9 | **Files 上传/下载**（旧 O4 复位）：`GET /files/download`（Range）+ `POST /files/upload`（multipart）；FileBrowser 已留按钮位与 client 方法清单 | `FileBrowser.vue:24` TODO 写明端点名与 UI 设计；`files.rs` 路由表 6 条无传输端点 | 2~3 天 | 无 |
| N10 | **vLLM 监控并入 Monitor 告警**：metrics 异常（unreachable/队列暴涨）走 Monitor 告警引擎 + history 落库（顺手修旧 O6 的 history 占位） | `llm.rs` metrics 按需采集+5s 缓存+速率换算已完成（模块头 §轻量监控），但仅前端 5s 轮询消费、无告警无历史；`monitor.rs:1176` demo_history 仍占位 | 1~2 天 | 无 |
| N11 | **GET /gateway/payments 收敛**：订单含收款地址/金额/txid，改需身份或脱敏；media_gen recent 同理评估 | `api_gateway.rs:1256`（公开读，测试断言"GET /payments 公开读"是有意为之，但 txid+地址组合是给攻击者的对账信息） | 0.5 天 | 无 |
| N12 | **模型对话历史落库**：localStorage → 后端会话表（跨设备/换浏览器不丢） | `LlmModels.vue:467-484`（`os-model-chat-history` localStorage，自并入后逻辑未变） | 1 天 | 前端代理让路后再动（在途文件） |

> N1+N2+N3 = §1.1① 的完整变现闭环；N4+N5+N9 = §1.1③；N6+N7 是本轮新发现的安全修补（见 §5 隐患计数）。

### 链上身份覆盖率专项（任务重点问询）

| 功能 | 身份现状 | 判定 |
|---|---|---|
| IM 全部写端点 | `caller()` 反查 token→pubkey，body 自报 sender 一律忽略（`im.rs:341` 起，每个写分支 `let Some(caller)` 守卫）；WS 握手强制 `?user=<pubkey>&token=` 匹配（`chain_auth.rs:210`） | ✅ 已闭环 |
| NexHub publish/purchase/bounty 全链 | `caller()`（`nexhub_lobby.rs:567`）链上 token 优先、admin token 回落；publisher 字段已从请求体移除由服务端归因（`CodeHub.vue:148` 注释）；悬赏 approve 校验 poster 所有权（`nexhub_lobby.rs:1293`） | ✅ 身份闭环；❌ 支付自证（N1） |
| media_gen 生图/生视频 | `requires_auth+admin`——**只能 admin token 调用**，链上身份用户被挡在外面 | ❌ 与变现定位冲突（N2/N6 前置） |
| blockchain 钱包/节点写操作 | `requires_auth+admin` 共享 token，无个人归因 | ❌ 自报/无归因残留 |
| appstore 用户发布 | publisher 硬编码 `"用户发布"`（`app_store.rs:1008`），无身份 | ❌ 无归因（低危，管理面） |
| users / QR / streaming 等 | 不涉及对外身份语义（users 旧断裂见 §2-O1） | — |
| 网关 sk-os- 令牌 | 令牌即身份（计费归账可用），但与链上身份零关联（Phase 3"钱包即身份"规划中） | 🟡 规划已写未动工 |

---

## 4. 横向一致性缺口（新页面 × 错误/空态/i18n/窄窗）

统计口径：grep 计数（friendlyError 使用 / 静默 `catch {}` / `window.confirm` / `@media` / `$t()`）。

| 新/大改页面 | 错误处理 | 空态 | i18n | 窄窗 @media | 备注 |
|---|---|---|---|---|---|
| Forwarding.vue（979 行） | ✅ friendlyError×12 | ✅ 空列表引导 | ❌ 0 | 1 处 | 本轮新页面里规范最好的 |
| ApiGateway.vue（1,828 行） | ❌ 0 friendlyError，静默 catch×3 | ✅ 计费/订单空态 | ❌ 0 | ❌ 0 | 五 Tab 大改无窄窗适配 |
| CodeHub.vue（1,749 行） | 🟡 自建 lobbyWriteErr（401/403→引导文案，好范式） | ✅ | ❌ 0 | 1 处 | 错误范式应提炼进 client.ts |
| Chat.vue（2,031 行） | ❌ 0 friendlyError，静默 catch×7，window.confirm×1 | ✅ 身份卡引导 | ❌ 0 | 1 处 | |
| LlmModels.vue（2,018 行） | 🟡 friendlyError×10，静默 catch×8，confirm×2 | ✅ | ❌ 0 | 1 处 | 前端代理在途，仅登记 |
| FileBrowser.vue（747 行） | ✅ friendlyError×5 + 三态（骨架/空/错误重试） | ✅ 范式级 | ❌ 0 | ❌ 0（靠宿主页面） | 三态+两步删除确认可作全仓模板 |
| useChainIdentity/useImIdentity | 🟡 静默 catch×5/×6（认证失败降级，语义可接受） | — | — | — | |

横向结论：
1. **i18n 假覆盖继续扩大**：全部新页面 `$t()` 计数为 0，硬编码中文随新增 4,469 行 web 代码继续膨胀；三语言 locale 仍只有 shell 层消费者。
2. **错误处理两种新范式并存未收敛**：FileBrowser 三态 + CodeHub lobbyWriteErr 都是好的局部解，但 Chat/ApiGateway（本轮改动量最大的两页）仍是裸 message 拼接；`lobbyWriteErr` 的"401→引导初始化身份"模式是链上身份推广后所有页面都会需要的，应下沉到 client.ts。
3. **窄窗适配随意**：ApiGateway 五 Tab 与 FileBrowser 组件 0 处 @media；Forwarding/CodeHub/Chat/LlmModels 各 1 处（约等于"有最小努力"）。
4. 旧缺口沿用：monitor history 占位、cloudsync 内存任务、WS 单消费者（Chat）、9 页面 setInterval 轮询。

---

## 5. 数据支撑

### 5.1 增量总览（2026-08-15 基线 → 2026-08-20）

| 指标 | 基线 | 现在 | 增量 |
|---|---|---|---|
| os-api handlers 行数 | 37,940（28 文件） | 45,626（29 文件：+forwarding +media_gen） | **+7,686（+20%）** |
| views 行数 | 26,150（31 文件） | 28,604（31 文件） | +2,454 |
| web src 总行数（vue+ts） | 32,484 | 36,953 | +4,469 |
| handlers 目录测试数 | 632 | 697 | +65 |
| workspace `#[test]`+`#[tokio::test]` | 4,172 | 4,299 | +127 |
| 静态 RouteSpec 总数 | 304 | **333**（os-api 303 + os-nexhub 30） | +29 |
| git 增量（8-15 起，crates/） | — | 219 files，+23,368 / −4,099 | 全仓 +27,412 / −5,405 |

### 5.2 各新功能代码量 / 测试 / 路由

| 新功能 | 后端 | 前端 | 路由 | 测试 |
|---|---|---|---|---|
| NexHub 货币化+悬赏+链上身份 | nexhub_lobby.rs 4,622（较基线 ≈+2,900）；chain_auth.rs 407 | CodeHub.vue +220；useChainIdentity.ts 282 | 18（含 2 auth + 8 bounty，全 requires_auth=false 自验） | 50 + 7 |
| 网关四计费+充值订单 | api_gateway.rs 4,998（基线 3,661，+1,337） | ApiGateway.vue 1,243→1,828 | 31（+4：payments×4、redeem 系） | 58→75 |
| 转发工具 SSH/RDP | forwarding.rs 2,447（全新，SQLite 持久化） | Forwarding.vue 979（全新）+桌面图标 | 13（全新） | 16 |
| IM 区块链认证 | im.rs 3,124（≈+1,600：删自报路径+auth 端点+全端点 caller 守卫） | Chat.vue 1,636→2,031；useImIdentity.ts 348→373 | 21（用户面自验，POST /peers 留 admin） | 43 |
| 媒体生成 | media_gen.rs 1,489（全新：sd-turbo 真管线+视频框架） | LlmModels.vue 生成区（并入页） | 5（全新） | 27 |
| 文件/存储浏览 | files.rs 689→936（+usage 端点） | FileBrowser.vue 747 新组件；Files.vue 490→52 | 6（+1） | 15 |
| 模型对话并入模型管理 | —（无后端变化） | LlmModels.vue 911→2,018；ModelChat.vue 445 删除；/modelchat 重定向 /llm | 0 | — |
| vLLM 监控 | llm.rs ≈2,050→2,554（metrics 按需采集+5s 缓存+Counter 速率+模拟模式） | LlmModels.vue 监控区 | 12（+1 /metrics） | 48 |

### 5.3 新发现安全隐患（本轮新增 6 项，另有 1 项旧患未修）

| # | 隐患 | 位置 | 等级 |
|---|---|---|---|
| S1 | NexHub 付费门禁可白嫖：自证收据 txid 任意非空字符串即发授权——**2026-08-31 状态更新：dApp 一期缓解（RPC 可用时核验）**：purchase/approve 在自证面校验后接力真实 EVM RPC 核验（`chain_verify.rs` 核验本体 + `nexhub_lobby.rs` ChainPayGate 接线，网关 PaymentOrder confirm 复用）；eth/evm 货币且链/收款地址可定位时伪造 txid 被 409/400 拒绝，链上事实（块高/实付 wei）落库；RPC 故障降级放行为可用性权衡（见 docs/NEXHUB_LOBBY_DESIGN.md §10.6） | nexhub_lobby.rs（verify_payment → check_chain_payment；原 2104-2124） | 高→中（RPC 可用时不白嫖；故障窗口恢复自证语义；ERC-20/BTC 一期不核） |
| S2 | RDP 转发 0.0.0.0 无认证无连接日志，LAN 任意主机可借道打内网 Windows | forwarding.rs:408 / 1151 | 中 |
| S3 | 链上身份私钥明文存浏览器 localStorage（XSS 即失窃；设计取舍"服务器不存私钥"正确，但 Web 端存储无保护） | useImIdentity.ts:40（`os-im-privkey`） | 中 |
| S4 | ChainAuth nonce 桶无过期清扫无限速：公网狂刷 challenge 可使 HashMap 只增不减 | chain_auth.rs:144-172 | 低-中 |
| S5 | GET /gateway/payments 公开暴露订单（收款地址+金额+txid，给攻击者的对账清单） | api_gateway.rs:1256 | 低 |
| S6 | media_gen recent 公开读生成记录（prompt 内容可含隐私） | media_gen.rs 路由表（读端点公开） | 低 |
| S7 | tips（统一打赏）txid 自报不验真：任意非空字符串即可在账本登记"链上凭证"（仅展示层凭证；amount 为站内积分，不产生授权/提现，危害有限；与 S1 同类，按惯例只记录不处理，详见 docs/TIPS.md §7） | handlers/tips.rs（handle_create，txid 仅长度校验） | 低 |
| 旧 | 钱包私钥明文 /tank/os-data/wallets.json（基线已列，未修，变现红线自认） | blockchain.rs:514 | 高 |

---

## 附：本轮未覆盖项

- `LlmModels.vue` / `client.ts` 在途改动后的最终形态（前端代理负责，本报告只登记截至今日状态）。
- docs/ 目录其他文档的在途改动（文档代理负责）。
- 运行时行为（全部结论来自静态代码与模块自述文档；forwarding/gateway/im 等 .db 文件在仓库根存在，说明有实机运行史但未实测）。
