# FilmStudio 独立化方案（2026-09-08）

> 任务书：影片制作从 NexOS 应用毕业为**独立产品**——Ubuntu 直跑、不依赖
> NexOS、新仓 `film-studio`（NexHub 裸仓已建，当前空仓）。
>
> **版本冻结令**：NexOS 内 film 引擎/应用（主仓 v0.1.44 / 应用 film 0.1.12）
> 不再演进，所有新开发走独立仓。本文是纯调研+架构设计产物，只读源码、
> 唯一写操作即本方案书。
>
> 调研基线：`crates/os-api/src/handlers/film.rs`（8226 行，生产 ~4880）+
> `film_hub.rs`（8867 行，生产 ~5740）+ `film_hub/{git.rs,blender.rs,collab.rs}`
> （~3200 行）+ `film_hub/tests.rs`（8531 行）；前端 `apps/film`（src ~17300
> 行，17 文件 + i18n×4 + nx 组件 7 件）。

---

## 0. 结论速览

- **可行，且代价比预期低**。film 引擎对 os-api 内部件的依赖面**窄而浅**：
  跨 handler 引用只有 5 个来源（llm / api_gateway / media_gen / apps_handler /
  api_market），生产代码合计约 **35 处调用点**，其中真正有语义的不是类型引用
  而是三块执行面（本地 chat 直连、渠道转发、生图内核）——各自的最小重实现
  都在 100~300 行量级。
- **推荐迁移方式：整目录 fork + 定点改造**（不抽共享 crate——冻结令下不动
  NexOS 主仓）。改造点约 **45~60 处**，绝大多数是机械替换（详见 §B.3）。
- **最难的三个依赖**（§B.1 详表）：① api_gateway 渠道面（Channel +
  forward_channel + 计价三单价，触及 model_ref 解析/models.json 路由/fallback
  链/成本记账全链）；② 前端宿主桥+SDK 双载体（协议织入 api.ts/theme/
  SideNav/FilmStudio 四处 + llm/gateway 两宿主端点）；③ http 分发与鉴权
  （68 条路由的网关派发替换为 axum 直起——用 ApiRequest/ApiResponse 适配层
  保 handler 代码零改动化解）。
- **首版基线**：全流程管线 + 定制器 + 模型路由（models.json）+ git 仓 +
  PR/Issues 协作 + Blender 场景源 + 成本记账 + 本地 sd-turbo 生图；砍联邦
  中继（via_node）、能力徽章/降级态（SDK）、NexHub 发布链、本地 vLLM 实例
  生命周期管理（§C）。
- **P0 一页话**见 §D.0。

---

## 1. 引擎依赖图（film.rs + film_hub.rs → os-api 内部件全清单）

先给全貌：film 五文件对 `crate::` / `super::` 的引用统计（生产代码，不含
测试）：

| 依赖来源 | 引用符号 | 调用点 | 性质 |
|---|---|---|---|
| `crate::gateway` | `ApiRequest/ApiResponse/HttpMethod/RouteSpec` | 每文件 1 处 import | **纯类型**（os-common::gateway 的 re-export，~100 行） |
| `crate::error` | `ApiGatewayError` | 3 处（film_hub.rs/git.rs/collab.rs 的 Result 面） | 纯类型（133 行文件） |
| `super::llm` | `LlmRouteHandler`（×4）、`ChatMessage`（×2）、`ChatBody`（×1） | film.rs 7 处 | **执行面**：本地 vLLM chat 直连 |
| `super::api_gateway` | `ApiGatewayRouteHandler`、`Channel` | film.rs 1 处 import + 9 处 `.gateway` 用点 | **执行面**：渠道表 + 转发 |
| `super::media_gen` | `smi_bin / probe_vram_free_mib_with / vram_gate / ensure_imggen_script / imggen_script / imggen_bin / ImageJob / run_imggen_with / summarize_stderr` | film.rs 10 处 + blender.rs 1 处 | **执行面**：sd-turbo 生图内核 |
| `super::apps_handler` | `AppRegistry::is_engine_enabled` | film.rs 2 处（构造注入 + handle() 前置检查） | 门控（独立版整体删除） |
| `crate::handlers::api_market` | `short_node_label` | film.rs 3 处（全部在 via_node 中继错误文案） | 随中继一起砍 |
| （仅测试）`api_market::ApiMarketFedEndpoint` | relay_pair fixture | film.rs 测试 2 处 | 测试工具，独立版换 fixture |
| （传递）`handlers::monitor::read_meminfo` | 统一内存回退（GB10 无独立显存） | media_gen.rs 内部 | ~20 行 /proc/meminfo 解析 |

**结论：没有对 os-storage / os-p2p / os-security / os-core 的直接依赖**；
P2P 中继只经 api_gateway 的 `relay_endpoint()` 间接使用（via_node 路径），
独立版砍掉即断根。任务框架（`Mutex<HashMap<String, FilmTask>>` + tokio
spawn + 环形日志）、SQLite 惯例（rusqlite + WAL + 幂等建表）、git/blender/
ffmpeg 子进程封装——全部**随代码走，零依赖**。

### 1.1 逐项处置（依赖什么 / 剥离代价 / 最小重实现）

| # | 依赖 | 独立版处置 | 剥离代价 | 最小重实现方案 |
|---|---|---|---|---|
| 1 | **LlmRouteHandler::chat_complete**（静态函数：reqwest POST `127.0.0.1:{port}/v1/chat/completions`，vLLM 思考段双键兼容、NEXOS_VLLM_API_KEY 透传） | **原样拷贝** | 极低——函数自包含（~80 行，连同 ChatBody/ChatMessage/ChatOutcome 三 DTO） | 直接搬 `llm.rs:1869` 起（`pub(crate)` 化的实例调用面），仅改 HTTP client 构造 |
| 2 | **LlmRouteHandler 实例表**（instances_snapshot：找 running 实例的 port+served_model_name；背后是 llm.db + vllm serve 子进程生命周期管理） | **概念替换**：砍实例生命周期，`source:"local"` 解析改为「本地提供方」 | 中——`resolve_local_chat`（~25 行）重写；前端「本地」下拉项数据源换 | 独立版**模型提供方表**（film_providers）：本地手动启动的 vLLM 就是一条 `base_url=http://127.0.0.1:PORT/v1` 的提供方。`model_ref.source="local"` 冻结契约保留，解析为 kind=local 的提供方行（前端零改或微改） |
| 3 | **Channel + channels_snapshot**（gateway.db 渠道表：base_url/api_key/models/enabled/三单价） | **原表裁剪拷贝**：`film_providers` 表（同列，去掉 token/配额/组倍率概念） | 低——Channel 是纯 DTO；resolve_channel（~30 行）改查自有表 | film.db（或独立 providers 表）建 `film_providers`：id/name/base_url/api_key/models/enabled/price_per_call/sec/token/kind(openai-compatible\|local-vllm)。快照 = 每次直查（单进程无共享实例问题） |
| 4 | **forward_channel**（非流式渠道转发：via_node 空=直连 reqwest；非空=api_market relay 经 P2P 代发；usage 解析同款） | **只留直连形态**：独立 OpenAI 兼容 client | 中低——直连分支 = `forward_upstream`（reqwest，~40 行）语义；中继分支整体砍 | 新 `providers.rs`：`forward(provider, suffix, body) -> (text, usage)`——URL=base_url+suffix、Bearer api_key、300s 超时、usage 三元组解析（拷 `parse_usage`）。via_node 字段保留读旧数据但恒走直连+日志提示 |
| 5 | **channel_relay_request + channel_roundtrip_bytes**（tts/music/video 二进制响应的字节面） | **直连重写** | 低——现实现直连分支本就是 film 自有字节面（`film.rs:2062` channel_roundtrip_bytes），只有中继形态借网关组装 | 该函数直连分支保留，中继分支删（错误文案改「独立版不支持中继」） |
| 6 | **media_gen 生图内核**（显存闸门 + 脚本落盘 + spawn；IMGGEN_SCRIPT_PY 自包含 python ~90 行：diffusers sd-turbo、模块级管道缓存、fp16/cuda） | **整块拷贝**（推荐保留本地生图） | 低——纯子进程，天然独立；唯一传递依赖 read_meminfo（~20 行一并拷） | 拷 9 个函数 + ImageJob + 脚本常量（合计 ~350 行）成 `imggen.rs`。备选：v1 砍 local 生图只留渠道（省 ~350 行拷贝，但丢「零 API 依赖可出图」卖点——**不推荐**，拷贝成本低收益高） |
| 7 | **AppRegistry 门控**（未装 nexos-app-film 应用全 404） | **删除** | 极低——1 处构造注入 + handle() 里 1 段检查 | 无（独立版=恒启用；将来要 license 门控再加自有开关） |
| 8 | **short_node_label / relay endpoint** | 随中继砍 | 零 | 无 |
| 9 | **crate::gateway 四类型 + ApiGatewayError** | **vendor**（拷进 server/ 的 `gateway_types.rs`） | 极低 | RouteSpec/ApiRequest/ApiResponse/HttpMethod 从 os-common 拷（~100 行）；错误枚举拷或换 thiserror 自建 |
| 10 | **http 分发 + 鉴权 + main 装配**（gateway_impl.rs 1038 行 radix 派发 + http.rs 4112 行 axum 服务器 + JWT/admin token + SSE 特挂） | **axum 直起 + 适配层** | 中——见 §B.4，用「RouteSpec 表 → axum 路由自动生成 + ApiRequest/ApiResponse 适配器」把 handler 改动压到零 | ~200 行 `server.rs`：启动时拿 `film_routes() + hub_routes()`（两函数现成、返回 Vec<RouteSpec>）逐条注册到 axum Router（:param 路径两端口径同为 Axum 风格）；一个 adapter 把 axum::Request→ApiRequest（含 Bearer→Principal 单用户简化）、ApiResponse→Response。鉴权：`FILMSTUDIO_ADMIN_TOKEN` 精确比对注入 admin（写面 requires_auth 的语义保留）；读公开面沿用 |
| 11 | **api_gateway 计价三单价**（film_cost_events 成本记账读 Channel 的 price_per_call/sec/token） | 随 #3 走（providers 表带三单价列） | 零额外 | channel_prices() 改查 providers 快照（~10 行） |

### 1.2 前端依赖面（apps/film）

**宿主桥协议**：`window.__NEXOS_HOST__ = {vue, vueI18n, api, sdk}` 四键。
`standalone/standalone-host.ts`（318 行）已自包含 vue + vue-i18n + api
client（fetch 原语：同源 JSON/Bearer localStorage `os-api-token`/15s 超时/
401 弹 token 条重试）+ SDK 实例（`createSdk(api)`，SDK 源经 vite alias 指向
主前端 `crates/os-api/web/src/sdk/` 直连打包）——**独立可行已被双载体构建证
明**。桥在应用源码内的触点：

| 触点 | 位置 | 独立版处置 |
|---|---|---|
| `api()` 取桥（`__NEXOS_HOST__.api`） | `src/api.ts` 1 处函数体 | 直接 import 自有 client（从 standalone-host.ts 抽成 `src/api/client.ts`） |
| `hostSdk()` 取桥（SDK 实例/旧宿主回退） | `src/api.ts` 1 处 | **删 SDK**（见下） |
| SDK 四面消费 | `sdk.llm.instances()` / `sdk.gateway.channels()`（模型源下拉）、`sdk.degraded.state()` + `sdk.capabilities.cached()/subscribe()`（能力徽章） | llm/gateway 两面 → 改调自有 `/api/v1/film/providers`（api.ts 4 个函数 + ModelsPage.vue 数据源）；degraded/capabilities → **砍**（独立版无联邦/降级概念；ffmpeg/blender 可用性已有 `GET /api/v1/film/tools`，徽章数据源换成它） |
| `__NEXOS_STANDALONE__` 标记 | 3 处（theme.ts / SideNav.vue / FilmStudio.vue——控制深浅主题与外链图标显隐） | 恒真化或删（独立版只有一种形态） |
| 嵌入载体构建（host-externals/inline-css/lib 模式） | `vite.config.ts` | **删**——只留 standalone 形态 |
| register 协议（registerApp/addRoute/addI18n） | `src/entry.ts` + standalone-host 的 ctx 适配器 | 直连：新 `src/main.ts` 直接 `createApp(FilmStudio) + createI18n + mount`（standalone-host.ts 主体照搬） |
| 宿主通用端点 | `GET /api/v1/llm/instances`、`GET/POST /api/v1/gateway/channels`（模型设置页「+添加 API」直填建渠道，POST 后链式设默认） | server 提供等价面：`GET/POST /api/v1/film/providers`（前端 api.ts ~6 处路径改；或 server 先挂兼容别名 `/api/v1/gateway/channels` 降前端改动——**推荐前者**，一次改干净） |
| vue-router | 无依赖（应用是 FilmStudio.vue 单组件内部视图态切换，SideNav 切换不经路由） | v1 不引；深链 ?p=&view= 已用 query 实现。引 vue-router 留作 P3 可选 |
| @nexos/app-sdk 其余面（notify/lobby/capabilities/degraded 共 1088 行） | 应用源码未用（notify 三档策略只在 standalone-host 注释里提及；toast 是自研 nx/toast） | 无需 vendor——裁剪 SDK 的问题直接消失（**整删**） |

### 1.3 数据与资产

| 数据 | 现状 | 独立版 |
|---|---|---|
| film.db（env NEXOS_FILM_DB，/tank/os-data/film.db → /var/lib/os/film.db） | 表：film_projects / film_characters / film_cost_events | **schema 原样沿用**；缺省根改 `/var/lib/filmstudio/film.db`（用户级安装 `~/.filmstudio/film.db`），env 指回旧路径即收编存量 |
| hub 树 + 产物目录（env NEXOS_FILM_DIR，/tank/os-data/film/<project-id>/） | `<dir>/hub/`（story/storyboard/casting/collab/...冻结契约）+ `<dir>/repo/`（git 仓，**仓随项目走、零绝对路径配置**）+ 项目根产物（shot-N.png 等） | 目录布局**原样沿用**（git 仓设计本就为多节点搬目录而生）；缺省根 `/var/lib/filmstudio/film`（或 ~/.filmstudio/film）；env 覆写支持指向 /tank/os-data/film 平滑接管 |
| gateway.db（渠道表） | Channel 含 via_node/计价三单价 | 独立版 film_providers 自有表 + 一次性迁移脚本（gateway.db channels → providers JSON 导入；模型设置页「+添加 API」直填能力在独立版天然具备） |
| llm.db（实例表） | vLLM 实例生命周期 | 不迁（砍实例管理；本地 vLLM=一条 provider 行） |
| ci.db / NexHub 发布链（nexos-app-film 仓、应用中心、apps-assets 托管） | 应用包发布/安装/引擎门控 | **整链砍**（独立产品自有发布：GitHub release / 官方二进制） |
| 模型路由 models.json / ownership / activity / collab | hub 树内文件即真值 | 原样走 |

---

## A. 目标形态

```text
film-studio/                     # NexHub 裸仓已建（/tank/git-repos/film-studio.git，空仓待灌）
├── server/                      # Rust 独立 bin（axum 直起，非 os-api 组件）
│   ├── Cargo.toml               # 依赖极简：axum/tokio/rusqlite/reqwest/serde/once_cell/chrono/base64
│   └── src/
│       ├── main.rs              # filmstudio serve --addr :8600；RouteSpec 表→Router 装配
│       ├── gateway_types.rs     # vendored：ApiRequest/ApiResponse/HttpMethod/RouteSpec/Error
│       ├── providers.rs         # 模型提供方表 + OpenAI 兼容直连 client（forward/usage/计价）
│       ├── imggen.rs            # vendored：sd-turbo 内核 + vram 闸门 + /proc/meminfo 回退
│       ├── film/                # 整目录 fork：mod.rs(原 film.rs) hub/ git.rs blender.rs collab.rs
│       └── tests/               # 随迁：film 内嵌测试 + tests.rs 8531 行（fixture 换）
├── web/                         # 前端主仓（以 standalone 形态为基础）
│   ├── vite.config.ts           # 标准 SPA build（lib 模式/host-externals 全删）
│   ├── index.html               # 原 standalone.html 升格
│   └── src/                     # 原 apps/film/src + standalone-host 抽 main.ts/api client
└── install.sh                   # Ubuntu 一键（见 §D P3）
```

- **部署**：`filmstudio serve --addr :8600`（缺省 8600，避开 os-api 8600 段
  需实测——若冲突改 :8610/:9090，P0 定）；web/ 产物由 server 静态托管
  （`GET /` 起前端，`/api/v1/*` 走引擎——同源，前端 client 零配置）。
- **env 前缀：定 `FILMSTUDIO_`**（`FS_` 过短易撞——文件系统语义/别的工具；
  长前缀 grep 友好）。映射表（17 项，P1 批量替换 + 兼容期读旧 `NEXOS_*`
  回退可选——**不做**，一次切干净）：

| NexOS | FilmStudio | 缺省 |
|---|---|---|
| NEXOS_FILM_DB | FILMSTUDIO_DB | /var/lib/filmstudio/film.db（用户级 ~/.filmstudio/） |
| NEXOS_FILM_DIR | FILMSTUDIO_DIR | /var/lib/filmstudio/film |
| NEXOS_FILM_EXPORT_BASE | FILMSTUDIO_EXPORT_BASE | 空（同语义） |
| NEXOS_FILM_SOURCE_MAX_MB / NEXOS_FILM_ASSET_MAX_MB | FILMSTUDIO_SOURCE_MAX_MB / ASSET_MAX_MB | 64 / 50 |
| NEXOS_FILM_VIDEO_TIMEOUT_SECS / COMPOSE_TIMEOUT_SECS | FILMSTUDIO_VIDEO_TIMEOUT_SECS / COMPOSE_TIMEOUT_SECS | 600 / 600 |
| NEXOS_FILM_REF_STRENGTH / NEXOS_FILM_TTS_VOICE | FILMSTUDIO_REF_STRENGTH / TTS_VOICE | 0.5 / alloy |
| NEXOS_FILM_GIT_BIN / NEXOS_FFMPEG_BIN / NEXOS_BLENDER_BIN | FILMSTUDIO_GIT_BIN / FFMPEG_BIN / BLENDER_BIN | git / 探测链 / 探测链 |
| NEXOS_IMGGEN_BIN/_SCRIPT/_TIMEOUT_SECS、NEXOS_SMI_BIN、NEXOS_SD_MODEL | FILMSTUDIO_IMGGEN_* / SMI_BIN / SD_MODEL | 同 media-gen 缺省 |
| NEXOS_VLLM_API_KEY | FILMSTUDIO_VLLM_API_KEY | 空 |
| NEXOS_ADMIN_TOKEN（鉴权） | FILMSTUDIO_ADMIN_TOKEN | 生成式：缺省启动时随机生成打印（首登粘贴） |
| NEXOS_P2P_ENABLE 等中继系 | 无 | 砍 |

---

## B. 剥离策略

### B.1 依赖处置总表 + 最难三项深析

§1.1 已给逐项表。**剥离代价总评——最难的三个**：

1. **api_gateway 渠道面（最广）**。它不是单点函数而是横切概念：FilmCtx 两
   字段（gateway/llm 注入）、resolve_channel/channel_prices/channel_model_of
   三读取面、chat/image/video/tts/music 五能力位的 channel 分支、
   models.json 路由 + 执行类错误 fallback 链（16 端点接入）、成本记账三单
   价。**拆法**：Channel DTO 原样保留（含 via_node 字段——旧数据兼容，
   恒直连），`ApiGatewayRouteHandler` 换成自有 `ProviderStore`（同
   channels_snapshot/forward_channel/channel_relay_request 三个方法签名），
   调用点即零改或微改。净新增 ~250 行（表 + 直连 client + usage 解析），
   修改 ~15 处。
2. **前端宿主桥 + SDK 双载体（最碎）**。协议触点分散（api.ts/theme/SideNav/
   FilmStudio/entry/两份 vite config），且 llm/gateway 两宿主端点是模型源
   下拉的数据底座。**拆法**：standalone-host.ts 已证明自包含可行，P2 把它
   从「第二载体」升为「唯一载体」——main.ts 直连化 + api client 抽包 +
   SDK 整删（degraded/capabilities 徽章换 /film/tools 数据源）+ 两端点改
   providers。修改 ~25 处（集中在 api.ts 与 ModelsPage.vue）。
3. **http 分发与鉴权（最容易低估）**。68 条路由（film.rs 21 + film_hub 47）
   现经 gateway_impl radix 派发 + http.rs 鉴权中间件；天真做法是给每个
   handler 改 axum extractor 签名——那将是几百处改动。**拆法**：不改
   handler，写一个 ~200 行适配层（axum Request → ApiRequest；handler 的
   ApiResponse → axum Response；RouteSpec 表在启动时自动注册成 Router，
   :param 风格两侧同源）。鉴权从 JWT/admin 双轨简化为单 token（独立产品
   单用户）：`FILMSTUDIO_ADMIN_TOKEN` Bearer 精确比对 → 注入 admin
   Principal；无 token 的写面 401 文案带设置指引。SSE 特挂面 film 不用
   （任务面是轮询不是流）——零负担。

**不难但量大的**：tests.rs 8531 行随迁。fixture 依赖三件：AppRegistry::
with_paths（门控测试——随门控删）、api_market::ApiMarketFedEndpoint
relay_pair（中继互连测试——随中继删）、mock 注入框架（with_gateway/
with_llm/with_imggen_mock 等 builder——**全部随 FilmCtx 改造为 with_
providers 等价物，测试主体断言零改**）。估计删 ~15% 用例（门控+中继），
其余 85% 平移。

### B.2 代码迁移方式：整目录 fork（推荐）vs os-api 抽共享 crate

| | 整目录 fork + 定点改造（推荐） | os-api 抽 film-engine 共享 crate |
|---|---|---|
| NexOS 主仓改动 | 零（冻结令友好） | 需把 llm/api_gateway/media_gen 的 pub(crate) 面再抽层——**违反冻结令** |
| 双向耦合 | 断根：独立版自由演进（换鉴权/换 env/砍中继） | 共享 crate 成为第二冻结面，独立版演进反过来锁主仓 |
| 代价 | 拷贝 ~1.6 万行（生产+测试）+ ~350 行 vendored 内核 + ~250 行 providers | 表面省拷贝，实付协调成本 + 主仓回归测试义务 |
| 升级回流 | 不回流（冻结令本就禁止 NexOS 侧演进） | 无意义 |

**推荐 fork + 改造**，理由：冻结令使「共享」的收益归零（主仓不再吃 film
改动），而成本（抽层回归）全在主仓侧。fork 后独立仓立即获得自由删改权
（删门控/删中继/换 env 正是第一批提交）。

### B.3 改造点清单（fork 后的 diff 面，估 45~60 处）

| 类别 | 处 | 说明 |
|---|---|---|
| import 替换 | 5 文件 × ~3 处 | crate::gateway/crate::error/super::* → crate 内模块路径 |
| FilmCtx 字段 | 2 字段 + 构造/ctx() 快照 2 处 | gateway/llm → providers: Arc<ProviderStore> |
| resolve_local_chat | 1 函数（~25 行重写） | 实例表 → kind=local-vllm 提供方（port 从 base_url 解析） |
| resolve_channel/channel_prices/channel_model_of | 3 函数微改 | channels_snapshot() → providers.snapshot()（签名不变则调用点零改） |
| forward_channel 调用面 | ~8 处（五能力 channel 分支 + emb） | 方法签名保持 `forward(&ch, suffix, &body)` → 调用点零改 |
| channel_roundtrip_bytes 中继分支 | 1 处删 | 错误文案「独立版不支持中继渠道」 |
| chat_complete/ChatBody/ChatMessage | 7 处 | 拷贝后同路径引用（改 super::llm:: → crate::providers::） |
| media_gen 九函数 | 11 处 | 拷贝后同路径引用 |
| read_meminfo | 1 处（media_gen 内） | 随 imggen.rs vendor |
| AppRegistry 门控 | 2 处删（注入 + handle 检查段） | 连同测试 ~4 例删 |
| api_market short_node_label | 3 处删（随中继） | |
| env 改名 | ~17 常量名 × 多处引用 | 机械 sed（NEXOS_FILM_→FILMSTUDIO_ 等，§A 映射表） |
| main.rs 装配 | 新写 ~200 行 | serve 子命令 + Router 装配 + 静态托管 + token 鉴权 |
| eprintln! 前缀 | ~几十处 `[filmhub]` | 可留（日志前缀无害）或统一 [filmstudio]（P3 打磨） |

### B.4 server 骨架关键设计（P0 的 ~500 行）

1. `main.rs`：clap 三态——`serve --addr`（缺省 [::]:8610 待定）/`version`/
   `migrate`（存量收编：--from-nexos /tank/os-data 指回或拷贝）。
2. 路由装配：`FilmRouteHandler::routes()` 与 `hub_routes()` 已返回
   Vec<RouteSpec>（path 即 Axum 风格 `:param`）——启动时 zip 成
   `axum::Router::route(path, any(adapter))`；同时保留一张 (method,path)→
   分发闭包表（handler 内部本就是按 req.method+path match 的统一入口
   `handle(ApiRequest)`，见 film.rs:3866）——**适配层只需一个 any-method
   handler 调 handle()**，RouteSpec 的 requires_auth/required_roles 由
   adapter 前置执行（单 token 模式：写面要 admin）。
3. 静态托管：rust-embed 或 tower-http::services::ServeDir（web/dist），
   `/api/v1/*` 优先匹配。
4. ProviderStore：SQLite film_providers 表 + `snapshot()/forward()/
   roundtrip_bytes()/prices()/model_of()`（对应现 FilmCtx 消费的五个面）。
5. 健康面：`GET /healthz`（端口/版本/工具探测/提供方计数）。

### B.5 前端去桥（P2 的具体动作序）

1. `vite.standalone.config.ts` → 升格为唯一 `vite.config.ts`：lib 模式改
   SPA（rollupOptions.input=index.html），删 postProcess 的 NODE_ENV 替换
   hack（SPA 模式 define 生效）与 CSS 内联 hack（正常产物引用 css）。
2. 新 `src/main.ts`：standalone-host.ts 的 main() 直连化——去 register 协议
   中转（createApp(FilmStudio) + createI18n(四语言 import) + injectBaseStyles
   照搬）；`src/entry.ts` 删。
3. `src/api/client.ts`：standalone-host 的 request/ApiError/token 条抽出为
   模块（导出 get/post/del/request）；api.ts 的 `api()` 改 import（1 处）。
4. SDK 删：hostSdk() 删；`sdkLlmInstances/sdkGatewayChannels` 改调
   `/api/v1/film/providers`（4 函数）；FilmStudio.vue initCaps/徽章改
   `/api/v1/film/tools` 数据源（或 v1 直接砍徽章，工具态已有指引文案）；
   ModelsPage.vue 直填建渠道 → POST /api/v1/film/providers（~6 处）。
5. `__NEXOS_STANDALONE__` 3 处恒真化（或改编译期 `__STANDALONE__` define）。
6. 深链 ?p=&view= 保持 query 形态（不引 router）；P3 视需要引 vue-router
   （九视图 → 路由化，收益是浏览器前进后退）。

---

## C. 首版功能基线（从 v0.1.44 功能清单裁剪）

**保留（=独立版 1.0 全量）**：

- 全流程管线：hub 建项目 → 剧情页（txt/小说导入[64MB/GBK 转码]→清理→分章
  →人物梳理→入库锁定）→ 向量化/语义搜索（emb 能力位）→ 分镜（chapter_range
  按章生成）→ 定妆（六类对象 + 多视图 + CastCustomizer 定制器[六类部件模板/
  主槽版本化]）→ 动作/运镜（motion/camera 注入链）→ BGM（高频/场景触发式）
  → 生成（cache/commit 半成品分离）→ compose（两遍 ffmpeg、版本化 dist、
  导出路径）。
- 模型路由组件：models.json 八能力位 + routes 按 task 路由（精确>前缀>兜底）
  + 执行类错误 fallback 链；模型提供方管理页（直填 base_url/key/models/
  三单价）。
- 本地生图：sd-turbo 内核（显存闸门/统一内存回退）——独立卖点之一。
- git 仓化 + PR/Issues 协作层（13 端点全留：issues 三态/PR merge/no-ff/
  from-task 一键提 Issue/activity 冲突自愈）。
- Blender 场景源（LLM 产 bpy 脚本 + 危险 token 剥离 + headless 渲染）。
- 成本记账（film_cost_events + 提供方三单价 + 三维聚合）。
- 多人分工/留名（ownership/activity/author）、任务中心（202+轮询+toast
  进度）、i18n 四语言、深浅主题。

**砍（独立版明确不做）**：

| 砍项 | 理由 | 替代 |
|---|---|---|
| 联邦中继渠道（via_node / api_market relay / P2P） | 中继依赖 os-p2p overlay 组网——独立产品无此拓扑 | 直连提供方（多提供方 + fallback 链已覆盖大部分场景） |
| 能力徽章/降级态（sdk.capabilities/degraded） | 联邦能力面板概念 | `/api/v1/film/tools` 本地工具探测（ffmpeg/blender/git）+ 提供方列表即真实能力面 |
| NexHub 发布链（nexos-app-film 应用包/应用中心/apps-assets/manifest/engine 门控） | 应用包机制整个不存在 | 自有发布：git tag + 预编译二进制 + install.sh |
| 嵌入载体（entry.js/host-externals/register 协议/appRuntime） | 无宿主 | 唯一形态=SPA |
| 本地 vLLM 实例生命周期（启停/健康/GPU 探测的 llm 组件面） | 重依赖（vllm serve 子进程编排非 film 核心） | 文档指引手动起 vLLM + provider 行接入；P2+ 可做轻量「进程看护」 |
| @nexos/app-sdk（联邦/大厅/降级/通知全套） | 面向 NexOS 宿主生态 | 自有 api client + nx/toast（已自研） |

**争议项定夺**：本地 sd-turbo 生图——**保留**（~350 行 vendored 成本换「无
任何 API key 也能出图」的独立产品底线能力）；若 P1 进度紧可临时禁用入口
（env 开关），内核代码照拷不留债。

---

## D. 分批实施

### D.0 P0 范围一页话

> **一周内可跑的最小闭环**：server/ 骨架（axum 适配层 + 单 token 鉴权 +
> 静态托管 ~500 行）+ film.rs/film_hub.rs 整目录 fork + 最小三替换
> （ProviderStore 换渠道面、chat_complete 拷贝、门控删）+ web 以现有
> standalone 构建原样托管——**验收**：Ubuntu 裸机 `./filmstudio serve` +
> 浏览器打开：建项目 → 导入小说 → 「+添加 API」填一个 OpenAI 兼容提供方 →
> 剧情生成 → 分镜 → 定妆视图（渠道生图）→ 假 ffmpeg compose 报错带安装指
> 引 → git log 有提交。中继/徽章/SDK/embed/blender 全部不进 P0。
> P0 结束即冻结「film-studio 仓 v0.1.0 骨架」，此后主仓 NexOS 侧只剩
> bugfix 级维护。

### D.1 批次表（人日为单人全职估算）

| 批 | 范围 | 交付/验收 | 估 |
|---|---|---|---|
| **P0 可跑骨架** | 仓库初始化（server/web/install.sh 占位）；gateway_types + adapter + main 装配；film.rs+film_hub.rs fork + 三替换；providers 表 + 直连 client；web standalone 原样托管 | §D.0 验收链全绿；`cargo test`（fork 子集，删门控/中继用例后）通过 | 4~5 人日 |
| **P1 引擎全量** | git.rs/blender.rs/collab.rs/tests.rs 随迁；emb/search；models.json 路由+fallback 链接入 providers；成本记账；imggen 内核 vendor（sd-turbo 真机出图）；env 前缀全量切换；`migrate` 子命令（存量 /tank/os-data/film 收编 + gateway.db 渠道导入） | 68 端点全绿；tests.rs ~100 例平移通过；真机：Blender 渲染一张 scene、git 分支/PR merge、compose 真片 | 5~7 人日 |
| **P2 前端去桥** | vite 配置升格 SPA；main.ts 直连化；api client 抽包；SDK 删 + providers 页对接；徽章换 tools；主题恒定 | `npm run build` 单产物；typecheck + smoke 脚本（flow/hub/toast 三套随迁）通过；嵌入模式彻底删除 | 3~4 人日 |
| **P3 打磨发布** | install.sh（预编译二进制下载优先/cargo build 兜底 + systemd 单元 filmstudio.service + git/ffmpeg/blender 可选探测与安装指引）；README/部署文档；版本 1.0.0 + tag；GitHub release（用户明说才公开） | 全新 Ubuntu 22.04/24.04 VM 一键装起；升级路径（数据目录版本化迁移说明） | 3~4 人日 |

合计 ~15~20 人日（单人）。风险缓冲：tests.rs 平移的 fixture 改造（估
+1 人日）、axum 版本 API 差异（workspace 现用 axum 直接沿用即可）。

### D.2 install.sh 要点

1. root 装 /opt/filmstudio + systemd（User=filmstudio 或 root 单机直跑），
   非 root 装 ~/.filmstudio + 用户级 systemd/--user 或 nohup。
2. 探测（缺了不阻断，装完 healthz 如实报）：git（apt git）、ffmpeg
   （apt ffmpeg）、blender（apt 或官方 tar——记录 jumpbox 拉 4.4.3 先例）、
   python3+torch+diffusers（仅 local 生图要，探测到即提示 sd 模型路径
   FILMSTUDIO_SD_MODEL）。
3. 首启生成随机 FILMSTUDIO_ADMIN_TOKEN 写 EnvironmentFile（600 权限）并
   打印一次性展示。
4. 二进制分发：`install.sh` 检测 glibc 匹配则下载 release 产物，否则
   `cargo build --release`（记录 Rust 工具链版本）。

---

## E. 与 NexOS 的关系

1. **冻结共存**：NexOS 主仓 film 引擎（v0.1.44 内置 + 门控）与应用包
   （nexos-app-film 0.1.12）**保留可用、不再演进**——主仓不删代码（删了
   反而破坏 68 端点与 CHANGELOG 语义），apps 仓封版。两边数据面按 §1.3
   可互相收编（film.db schema 与 hub 树完全同构；独立版 migrate 子命令
   负责单向收编 + 渠道导入）。
2. **将来 NexOS 引擎门控应用是否指向独立服务**（开放）：两条路——
   a) os-api 反代：`/api/v1/film/*` → `http://127.0.0.1:8610`（NexOS 桌面
   的 film 应用包前端本就消费同路径 REST，理论零改；但鉴权双轨 token 需
   桥接，且 NexOS 冻结令下这个反代本身也是主仓改动）；b) 不指——NexOS
   用户想用新版就独立装 FilmStudio，主仓内置版当「车载基础版」。
   **建议维持 b 直至有真实双节点需求**；a 留作技术验证 spike（半天可测：
   nginx/caddy 反代即可验证前端兼容性，不必动 os-api）。
3. **品牌/命名**：产品名 FilmStudio（仓 film-studio）；REST 路径保持
   `/api/v1/film/*`（前端零改 + 与 NexOS 版语义同源）；binary 名
   `filmstudio`。

---

## F. 开放问题（移交决策）

1. **端口**：:8600 与 os-api 常用端口段是否冲突需实测；建议 :8610 定案。
2. **鉴权形态**：v1 单 admin token（本方案）够不够？若独立产品要多用户
   （工作室多人），providers/项目属主/ownership 的 author 模型要提前定
   （现 ownership.json 是自由字符串 author——多人鉴权需要 Principal 贯穿，
   改动面大，建议 v1 明确单管理员+多署名）。
3. **NexOS 反代（§E.2a）**是否立项 spike。
4. **本地生图的模型分发**：sd-turbo 权重（/tank/models/sd-turbo）独立版
   是否提供下载脚本/首启拉取（~几 GB，影响 install.sh 体验）。
5. **vue-router 引入时机**（P3 可选：九视图路由化换浏览器历史栈）。
6. **ci.db/NexHub CI**：独立仓是否复用 NexHub 的 CI 裸仓机制（现
   film-studio.git 就是 NexHub 裸仓）——若独立产品发布走 GitHub，NexHub
   仓仅作内部镜像即可，不需 CI 接线。

---

## 附：调研事实底账（关键行号索引）

- 依赖注入装配：`crates/os-api/src/main.rs:570-582`（with_gateway/
  with_llm/with_app_registry 三注入）。
- FilmCtx 结构：`handlers/film.rs:1748-1768`（db/tasks/gateway/llm/
  local_chat/imggen/smi_bin/ffmpeg_bin/blender_bin/ref_strength/tts_voice）。
- 渠道解析/计价：`film.rs:1772-1822`（resolve_channel/channel_prices/
  channel_model_of）；本地 chat 解析 `film.rs:1826-1847`。
- chat 统一入口（local/channel 双分支 + usage）：`film.rs:1859` 起。
- 字节面：`film.rs:2062 channel_roundtrip_bytes`（直连 reqwest 自有）。
- local.image 内核复用：`film.rs:2153-2190`（闸门+脚本落盘+spawn）。
- 统一入口 handle()：`film.rs:3866`；routes()：`film.rs:3830`；hub_routes()
  ：`film_hub.rs:6232`（47 条）。
- 引擎门控：`film.rs:3874`（is_engine_enabled("film")）。
- llm 实例调用面：`handlers/llm.rs:1869 chat_complete`（静态，自包含）；
  `llm.rs:1591 instances_snapshot`；DTO 608/621/635。
- 渠道转发面：`handlers/api_gateway.rs:1230 forward_channel`（直连/中继
  双形态）、`1187 channel_relay_request`；Channel DTO `276-315`（含三单价）。
- 生图内核：`handlers/media_gen.rs:374 vram_gate / 461-494 env 注入点 /
  504 ImageJob / 521 probe_vram_free_mib_with / 558 ensure_imggen_script /
  578 run_imggen_with / 142 IMGGEN_SCRIPT_PY`；统一内存回退依赖
  `handlers/monitor.rs:789 read_meminfo`。
- git 仓化：`film_hub/git.rs` 模块头（--git-dir/--work-tree 每命令注入、
  零绝对路径、降级不阻断）。
- Blender：`film_hub/blender.rs` 模块头（LLM 产 bpy + 危险 token 行级剥离
  + factory-startup 隔离；spike 实测 3/3 管道成功）。
- 协作层：`film_hub/collab.rs` 模块头（issues/prs 文件即真值 + merge 语义）。
- 宿主桥自包含证明：`apps/film/standalone/standalone-host.ts`（318 行全量）；
  双载体构建 `apps/film/vite.config.ts`（host-externals）/`vite.standalone.
  config.ts`（SDK alias 直连主前端源）。
- 前端桥触点：`src/api.ts:37-61`（hostSdk/api 取桥）、`src/api.ts:458-520`
  （llm/instances、gateway/channels + 直填建渠道）；`FilmStudio.vue:243-258`
  （degraded/capabilities 徽章）。
- 数据：film.db 三表（film_projects `film.rs:1423` / film_characters
  `film.rs:1435` / film_cost_events `film_hub.rs:854`）；env 清单
  `docs/FILM_STUDIO.md §6`。
- 版本冻结基线：主仓 Cargo.toml `version = "0.1.44"`；应用
  `apps/film/manifest.json` `0.1.12`。
