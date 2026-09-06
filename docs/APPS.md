# NexOS 应用开发指南——以 film 影片制作为例

> 面向零上下文读者（人与 AI agent）：读完本文即可从零开发并发布第二个
> NexOS 应用包。贯穿示例是 **film 影片制作**（NexHub 仓库 `nexos-app-film`）——
> 它同时是本文写作动因：2026-09-04 起 film 不再是系统自带功能，而是
> **装了应用才启用** 的独立应用（见 §7 引擎门控）。
> v0.1.28 起应用另有统一能力面 SDK（@nexos/app-sdk——能力快照/联邦大厅/
> 网关/本地 LLM/通知/降级三态），见 §12；film 是第一个吃狗粮的应用。
>
> 本文档位于仓库 `docs/`，是「开发者中心」（DevDocs 应用）的入口文档之一
> （git push 即更新，开发者中心自动收录）。

## 1. 一分钟理解应用包

一个 NexOS 应用 = **一个 git 仓库**，内含：

- `manifest.json` —— 应用身份证（id/名称/版本/入口等 8 个字段）
- `web/` —— 前端静态资源（入口 JS + 按需的 css/json/png/svg/woff2）
- （可选）`engine` 字段声明的**内置引擎**后端能力（如 film 的
  `/api/v1/film/*` 六阶段管线——引擎代码编译在 os-api 二进制内，装应用才开门）

用户视角流程：**应用中心 → 商店 Tab（catalog 自动扫描 NexHub `nexos-app-*`
仓库）→ 一键安装（git clone）→ 桌面出现应用图标 → 点击开窗运行**。卸载即
消失（目录删除 + 注册表注销 + 业务端点即时关闭）。

## 2. 组件拓扑

```text
┌────────────────────────────  NexOS 节点（os-api 网关，axum） ────────────────────────────┐
│                                                                                          │
│  ┌──────────────┐   install/uninstall   ┌───────────────────────────────────────────┐   │
│  │ 应用中心      │ ────────────────────▶ │ AppsRouteHandler（handlers/apps_handler） │   │
│  │ AppStore.vue │ ◀──────────────────── │  · POST /api/v1/apps/install {repo}       │   │
│  └──────┬───────┘   GET /apps /catalog  │  · DELETE /api/v1/apps/:id                │   │
│         │                               │  · GET /api/v1/apps[|/catalog|/tasks]     │   │
│         │                               └──────┬───────────────────────┬───────────┘   │
│         │                                      │ git clone --depth 1   │ SELECT/UPSERT │
│         ▼                                      ▼ (file:// 本机优先)    ▼               │
│  ┌──────────────┐  dynamic import   ┌────────────────────┐   ┌──────────────────┐      │
│  │ 前端运行时    │ ────────────────▶ │ /tank/os-data/apps │   │ apps.db          │      │
│  │ appRuntime.ts│   register(ctx)   │   /<id>/           │   │  apps 表(13 列)  │      │
│  │ （宿主桥 +   │                   │     manifest.json  │   └───────┬──────────┘      │
│  │  注册表）    │ ◀──────────────── │     web/entry.js … │           │ 每请求直查      │
│  └──────┬───────┘   GET /apps-assets/:id/<path>（web/ 下静态托管，防穿越）  │              │
│         │                                                                 ▼              │
│         │ api.get/post…                                   ┌────────────────────────────┐ │
│         │  （走宿主 HTTP client，鉴权同源）                 │ FilmRouteHandler 等引擎     │ │
│         └──────────────────────────────────────────────── │ 未装 film 应用 → 全 404    │ │
│                                                          │ 装了 → /api/v1/film/* 放行 │ │
│                                                          └────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────────────────────────┘
                                        ▲
                                        │ push（manifest.json + web/）
                              ┌─────────┴──────────┐
                              │ NexHub 裸仓库       │
                              │ /tank/git-repos/   │
                              │   nexos-app-*.git  │
                              └────────────────────┘
```

## 3. 应用包规范（冻结契约）

### 3.1 目录结构（以 film 为例，仓库 main 实况 e74068a）

**包根形态**（NexHub `nexos-app-film` main 当前布局——发布根=仓库根）：

```text
nexos-app-film/                 # 仓库根（NexHub 上是裸仓库 nexos-app-film.git）
├── manifest.json               # 必需，仓库根，UTF-8 JSON（见 §3.2）
├── web/                        # 前端静态资源根（唯一对外可服务的目录）
│   └── entry.js                # ESM 单文件入口（组件+i18n+图标+CSS 全内联；vite lib 产物，
                                #   film 工作仓 outDir=dist/web，发布时并入包根 web/——见 §6）
├── src/                        # 源码（entry.ts / FilmStudio.vue / api.ts / i18n/ / icon.ts）
├── vite.config.ts              # 构建配置（host-externals 桥重写 + CSS 内联，见 §4.2）
└── package.json / tsconfig.json / README.md
```

安装后磁盘布局（`/tank/os-data/apps/` 缺省，env 可覆写）——拷贝**发布根下除
`.git` 外全部**（src 等源码文件随包落盘但不参与运行）：

```text
/tank/os-data/apps/film/        # = manifest.id
├── manifest.json
├── web/entry.js                # 64,769 B 单文件（film v0.1.0 实测；i18n 内联无独立目录）
└── …（src/ 等随包文件）
```

> **发布根解析（后端自动，两类仓库形态都支持）**：仓库根有合法
> `manifest.json` 且 entry 文件在根下 → 发布根=仓库根；否则若 `dist/`
> 子目录自洽（`dist/manifest.json` + entry 在 dist 内）→ 发布根=`dist/`
> （「源码+dist 双收」形态，只拷贝产物，src 不进安装目录；film 首发提交
> 974da6d 即此形态，e74068a 起对齐为包根形态）。两类都失败 → 400 透出
> 仓库根的校验错误。
>
> `web/` 之外的文件（含 manifest.json、src/）**不可**经 `/apps-assets/`
> 访问——静态托管根是 `<apps_dir>/<id>/web/`，穿越防护见 §5.5。

### 3.2 manifest.json 字段表（8 个）

| 字段 | 类型 | 必需 | 校验规则（不合法 → 安装 400 拒绝） |
|------|------|------|--------------------------------------|
| `id` | string | 是 | `^[a-z0-9-]+$`，≤64 字符；同时是安装目录名、`/apps-assets/:id` 段、apps 表主键 |
| `name` | string | 是 | 非空（trim 后）；桌面显示名 |
| `version` | string | 是 | `x.y.z` 三段数字（≤8 位/段），可带 `-prerelease` / `+build` 后缀；如 `0.1.0`、`1.2.3-beta.1` |
| `category` | string | 否 | 缺省 `custom`；前端 Launchpad 归一到 `media/dev/office/internet/system/...` |
| `icon` | string | 否 | 缺省 `📦`；AppIcon 图标名或 SVG 内部标记字符串 |
| `description` | string | 否 | 一句话简介（应用中心卡片） |
| `entry` | string | 是 | 相对包根（如 `web/entry.js`）；不得以 `/` 开头、不得含 `..`、无空段；**安装时校验该文件真实存在于发布根内** |
| `engine` | string | 否 | 声明启用的内置引擎（如 `film`）；见 §7 |
| `min_os_api` | string | 否 | semver 下限；高于当前 os-api 版本 → 安装 400 拒绝（如 `0.1.25`） |

示例（film 仓库 manifest.json 实文）：

```json
{
  "id": "film",
  "name": "影片制作",
  "version": "0.1.0",
  "category": "media",
  "icon": "clapperboard",
  "description": "AI 影片管线：创意 → 剧本分镜 → 分镜图 → 图生视频 → 配音 → BGM → 合成成片",
  "entry": "web/entry.js",
  "engine": "film"
}
```

## 4. entry.js：register(ctx) 协议（冻结实况）

入口模块 **`export default function register(ctx)`**（同步或 async 均可，宿主
会 await）。装载时序：前端运行时（`crates/os-api/web/src/appRuntime.ts`）
`GET /api/v1/apps` → 逐个 `import(/apps-assets/<id>/web/entry.js)`（10s 超时，
失败显示占位卡可重试）→ 调 `register(ctx)` → 桌面/窗口/路由/i18n 就绪。
**安装/卸载免刷新热生效**（重装会先清旧注册再装新；缓存击穿用
`?v=<installed_at>`）。

### 4.1 ctx 的 TS 签名（与 appRuntime.ts 逐字一致）

```ts
/** 应用包 registerApp 声明 */
interface DesktopAppDecl {
    id: string            // 必需；= manifest.id；与内置应用 id 冲突会被拒绝
    label: string         // 窗口标题 / 桌面图标标签
    icon?: string         // AppIcon 图标名；或 SVG 内部标记字符串——含 '<' 即视为
                          // SVG，注册为运行时图标（film 的场记板图标即此形态）
    route?: string        // 直接 URL 访问的路由路径；缺省 /<id>
    category?: string     // Launchpad 分组；未知归 system
    gradient?: string     // 图标背景渐变；缺省品牌渐变
    component?: unknown   // 应用主组件（Vue Component，窗口内容）；缺省占位卡
}

/** 应用包 addRoute 声明（挂到父路由 'layout' 下，path 归一为相对段） */
interface AppRouteDecl {
    path: string          // 如 'film'（'/' 前缀会被剥离）
    name?: string
    component: unknown
}

/** 传给 register(ctx) 的上下文（冻结契约） */
interface AppRegisterContext {
    registerApp(app: DesktopAppDecl): void    // 必须调用且 component 非空，否则视为装载失败
    addRoute(route: AppRouteDecl): void       // 挂为 'layout' 子路由，并建立 /path → /?app=<id>
                                              // 守卫映射（直接 URL → 桌面浮窗，与内置应用一致）
    addI18n(locale: string, messages: Record<string, unknown>): void  // 宿主 mergeLocaleMessage 合并
    api: {                                   // 宿主 api client **原语**（不是完整 client）：
        get<T>(path: string): Promise<T>     // 鉴权/超时/ApiError 全走宿主，应用不重复实现 HTTP 层
        post<T>(path: string, body?: unknown): Promise<T>
        del<T>(path: string): Promise<T>
        request<T>(path: string, opts?: { method?: string; body?: unknown }): Promise<T>
    }
    sdk: NexosSdk                            // @nexos/app-sdk 就绪实例（v0.1.28 起，
                                             // 协议版本 sdk.version='0.1'；§12——能力快照/
                                             // 联邦大厅/网关/本地 LLM/通知，降级三态）
}

// entry 形态（apps/film/src/entry.ts 实况）
export default function register(ctx: AppRegisterContext): void { ... }
```

### 4.2 宿主桥：应用包**不打包** vue / vue-i18n（重要）

宿主在 `window.__NEXOS_HOST__ = { vue, vueI18n, api, sdk }` 暴露主前端的模块
命名空间与 api 原语（`sdk` = @nexos/app-sdk 就绪实例，§12）。应用**源码正常写**
`import { ref } from 'vue'` / `import { useI18n } from 'vue-i18n'` /
`import { createSdk } from '@nexos/app-sdk'`；**构建期**由应用包 vite 配置的
host-externals 插件把这些导入重写到桥（输出形如
`const __m = globalThis.__NEXOS_HOST__.vue || {}; export const { ref, … } = __m;`）。

为什么必须共享单一 Vue 实例（前端实测结论，打包方案已否决）：打进第二份
Vue 会导致**组件响应式失活**，且 `useI18n()`/`getCurrentInstance()` 因跨
实例注入而抛错。参考实现：**`apps/film/vite.config.ts`**（`hostExternals()`
插件——从真实模块命名空间取导出名清单，随宿主版本演进自动覆盖新 API；
配 `inlineCss()` 把 SFC 样式内联进 entry.js 头部，产出无外部依赖的 ESM
单文件）。无构建直写 entry.js 时，从 `globalThis.__NEXOS_HOST__` 取用即可。

### 4.3 register 全流程实例（apps/film/src/entry.ts 实文精简）

```ts
import FilmStudio from './FilmStudio.vue'
import { FILM_ICON } from './icon'          // SVG 内部标记字符串
import zhCN from './i18n/zh-CN.json'        // 构建时内联进 entry.js 单文件

export default function register(ctx) {
    // 1. 注册桌面应用（窗口组件 + SVG 图标 + 渐变 + 启动台「媒体」分组）
    ctx.registerApp({
        id: 'film',
        label: '影片制作',
        icon: FILM_ICON,                    // 含 '<' → 运行时 SVG 图标
        route: '/film',
        category: 'media',
        gradient: 'linear-gradient(135deg, #f7971e 0%, #f12711 100%)',
        component: FilmStudio,
    })
    // 2. 子路由：直接 URL /film → 守卫重定向 /?app=film 开浮窗（与内置应用一致）
    ctx.addRoute({ path: 'film', name: 'film', component: FilmStudio })
    // 3. 四语言全量注入（键名 film.*）
    ctx.addI18n('zh-CN', zhCN)
    // … zh-TW / en-US / ja-JP 同款
}
```

film v0.1.0 产物：`web/entry.js` **64,769 B** ESM 单文件（组件 + 四语言
i18n + SVG 图标 + CSS 全内联，无独立资源目录），已推 NexHub `nexos-app-film`
main（布局对齐提交 e74068a；tag `v0.1.0`＝974da6d）。

## 5. 后端 API 契约（component="apps"，读公开 / 写 admin）

后端实现：`crates/os-api/src/handlers/apps_handler.rs`（6 条路由）。

### 5.1 GET /api/v1/apps —— 已装列表

```json
{"apps":[{"id":"film","name":"NexOS 影片制作","version":"0.1.0","category":"media",
  "icon":"🎬","description":"…","entry":"web/entry.js","dir":"/tank/os-data/apps/film",
  "installed_at":"2026-09-04T10:00:00+08:00",
  "repo":"nexos-app-film","engine":"film","min_os_api":"","updated_at":"…"}]}
```

前 9 个字段为冻结契约面；`repo/engine/min_os_api/updated_at` 为扩展字段。

### 5.2 POST /api/v1/apps/install —— 安装（admin，同步完成）

请求体 `{"repo":"nexos-app-film"}`。源解析：本机
`/tank/git-repos/<repo>.git` 优先（`git clone --depth 1 file://…`，超时 300s）；
repo 也可传完整 http(s) URL 直连克隆。仓库名合法性：`[A-Za-z0-9][A-Za-z0-9._-]*`
（可带 `.git` 后缀）。

流程：clone 到临时目录 → 发布根解析（§3.1，仓库根优先 / dist 双收回退）→
manifest 校验（§3.2 全规则 + entry 文件存在 + min_os_api 下限）→ 拷贝发布根
（除 `.git` 外全部）到 `<apps_dir>/<id>/` → apps 表登记。**同步完成即返回**
（appstore 安装任务框架同款：任务记录即时终态，可经 `GET /api/v1/apps/tasks`
观测）。**本实现的响应形态是「同步返回」**——前端 `waitForInstalled` 同时
兼容「同步 `{app}`」与「任务态轮询 `/api/v1/apps` 出现该 id（≤60s/2s 间
隔）」两种，二者都工作：

- 新装成功 → `201 {"ok":true,"action":"install","app":{…}}`
- 覆盖升级（同 id 同 repo 异版本）→ `201 {"ok":true,"action":"upgrade","app":{…}}`
- 同版本重复装（幂等 no-op）→ `200 {"ok":true,"action":"noop","app":{…}}`
- 失败 → 400（manifest 校验）/ 404（仓库不存在）/ 409（同 id 已装但来源
  repo 不同——拒绝覆盖）/ 500（clone 或落盘失败），body `{"error":"…"}`
  且任务记录 `status:"failed"`

### 5.3 DELETE /api/v1/apps/:id —— 卸载（admin）

删 `<apps_dir>/<id>/` 目录（防御：登记目录必须在 apps_dir 下）+ apps 表
注销。`200 {"ok":true,"id":"film","dir":"…","action":"uninstall"}`；未装 404。
卸载后：静态资源、桌面入口、引擎门控**即时**失效。

### 5.4 GET /api/v1/apps/catalog —— 商店目录（公开）

扫描 `<repos_dir>/nexos-app-*.git`（缺省 `/tank/git-repos`，与 code_repo /
`/git/*` CGI 同源），逐仓 `git --git-dir=<bare> show HEAD:manifest.json`：

```json
{"apps":[
  {"repo":"nexos-app-film","id":"film","name":"NexOS 影片制作","version":"0.1.0",
   "category":"media","icon":"🎬","description":"…","engine":"film",
   "installed":true,"installed_version":"0.1.0"},
  {"repo":"nexos-app-broken","installed":false,
   "error":"manifest 不可读（空仓库或未推送 manifest.json）"}]}
```

manifest 字段全可选（`id?`）；不可读的仓库**如实**列出并带 `error`，不假
成功。`installed` 按 repo 匹配（回退按 id）。

### 5.5 GET /apps-assets/:id/<path> —— 静态资源（公开）

从 `<apps_dir>/<id>/web/<path>` 读文件。`<path>` 相对 `web/` 目录；**兼容
剥前导 `web/` 段**（前端把 manifest.entry 原样拼 URL，`/apps-assets/film/
web/entry.js` 与 `/apps-assets/film/entry.js` 等价同指 `web/entry.js`）。

- MIME 白名单（按扩展名）：`js/mjs→text/javascript`、`css→text/css`、
  `json/map→application/json`、`svg→image/svg+xml`、`png→image/png`、
  `woff2→font/woff2`，未知 → `application/octet-stream`
- 文本类 MIME 原文直传（浏览器 `<script src>`/`fetch().json()` 需要原文，
  非 JSON 引号包裹）；png/woff2 base64 装载、网关直传层解码回原始字节
- 穿越防护三道闸：id 白名单（`^[a-z0-9-]+$`）→ 子路径拒 `..`/空段/反斜杠
  → canonicalize 后必须仍在 `<apps_dir>/<id>/web/` 内（符号链接逃逸也拦）；
  违规一律 404。`web/` 之外的包根文件（manifest.json）不可达。

### 5.6 GET /api/v1/apps/tasks —— 安装任务列表（公开，观测面）

`{"tasks":[{"id":"app-task-3","app_id":"film","repo":"nexos-app-film",
"action":"install","status":"completed","log_tail":"…","created_at":"…"}]}`

## 6. 发布分发流程（NexHub → 应用中心；film 实况走查）

1. **建仓**：在 NexHub（代码仓库中心，或直接 `git init --bare
   /tank/git-repos/nexos-app-<name>.git` + `git symbolic-ref HEAD
   refs/heads/main`）创建 `nexos-app-` 前缀裸仓库——catalog 只认这个前缀。
2. **开发**：应用工作仓（`apps/<name>/`）——`manifest.json` + 源码 `src/`
   + `vite.config.ts`（拷 `apps/film/vite.config.ts` 改 id/outDir/styleId）；
   `npm run build` 产出 `web/entry.js` ESM 单文件。
3. **推送**：commit 并 `git push <bare> HEAD:main`（源码与产物同仓提交，
   发布根=包根；或只推产物/dist 双收形态——后端两种都认，§3.1）。打 tag
   留版本锚点（film：`v0.1.0`）——**安装源取 HEAD（main），tag 仅作发布
   标记不参与安装选版**。
4. **可见**：商店 Tab（catalog）立即可见——HEAD 有 manifest 即出条目；
   空仓库会列出但带 error 提示。
5. **安装**：用户一键装（§5.2）→ `GET /api/v1/apps` 出现该应用 → 前端
   运行时热注册桌面图标（免刷新）。
6. **升级**：仓库 bump manifest.version 再 push → 用户再点安装 → 覆盖升级
   （`action:"upgrade"`）；前端用 `?v=<installed_at>` 击穿模块缓存。
7. film 实况：`nexos-app-film` main 已对齐包根布局（`web/entry.js` 64,769 B
   在根，e74068a），tag `v0.1.0`；后端已对真实仓库端到端冒烟（catalog 可
   见 → 安装 201 → `/apps-assets/film/entry.js` 200 → `/api/v1/film/*`
   开门 → 卸载即时 404）。
8. 更多发布案例（同款流程走查，2026-09-05）：**二维码传输**
   （`nexos-app-qrtransfer`，tag `v0.1.0`，entry.js 41,848 B——自主前端剥离，
   文件/文本 ⇄ QR）与 **流媒体中心**（`nexos-app-streaming`，tag `v0.1.0`，
   entry.js 117,290 B——自主前端剥离，含「直播」Tab 的 LivePanel；
   `/api/v1/live/*` 端点常开不门控）。两包均含 standalone 独立运行宿主
   （`web/standalone-host.js` + `web/standalone.html`），安装 201 →
   `/apps-assets/<id>/web/entry.js` 200 → catalog `installed:true` 全链通过。

### 6.9 应用发布流程：一键脚本 publish-app（v0.1.34 起）

上述 1–7 步的手工序列，主仓 `tools/publish-app.sh` 一条命令全链代跑：

```bash
cd /home/oem/NexOS
./tools/publish-app.sh qrtransfer --patch        # 版本 +0.0.1 → 构建 → 发布 → CI → 安装
./tools/publish-app.sh film                      # 用当前版本重发布（tag 强制重打）
./tools/publish-app.sh streaming --no-install    # 只发布与触发 CI，不装本机
```

脚本六步（任一步失败即停 `set -euo pipefail`；全程步骤化日志）：

| 步 | 动作 | 与手动流程对照 |
| --- | --- | --- |
| 1 | 前置检查：`apps/<name>/`、manifest.json 可读（jq 校验）、node/npm/git/rsync、裸仓 `/tank/git-repos/nexos-app-<name>.git` 存在 | §6.1 建仓 + §6.2 前提的机器检查 |
| 2 | 版本管理：`--patch` 则 manifest.json + package.json 同步 +0.0.1；无 flag 用当前版本（重发布） | §6.6 手动 bump |
| 3 | 构建：`apps/<name>` 下 node_modules 缺则 `npm install --no-audit --no-fund`，`npm run build`（scripts 声明 `build:standalone` 则再补跑；当前三应用并入 build） | 各应用 README「构建方法」 |
| 4 | 发布仓同步：clone 裸仓到 `/tmp` 临时目录 → rsync——`dist/web/* → web/`（`--delete` 清陈旧）、`dist/standalone.html → web/`、src/standalone/scripts/manifest/README/package*/tsconfig/vite 配置 → 根（**仓库根=包根铁律**，§3.1） | §6.3 手动拷贝推送 |
| 5 | commit（`v<版本>: <一行>`，`-c user.name=oem -c user.email=oem@ub2604`）+ tag `v<版本>`（重发布 `tag -f`）+ push main 与 tag（file:// 路径直推） | §6.3 手动 git 序列 |
| 6 | CI 触发 + 安装：POST `/api/v1/coderepo/repos/<repo>/ci`（file:// 直推不经 git-http push 钩子，脚本显式触发，不等终态只打印查询命令）；除 `--no-install` 外 POST `/api/v1/apps/install {"repo"}` 并打印 action（install/upgrade/noop）。token：`NEXHUB_TOKEN` env → `~/.config/nexhub/credentials` 的 `TOKEN=` 行；都缺则打印手动 curl 命令（发布本体已成功） | §6.5 手动安装 + CI Tab 手动触发 |

env 覆盖：`NEXHUB_API`（缺省 `http://127.0.0.1:8558`）、`NEXOS_GIT_REPOS_DIR`
（缺省 `/tank/git-repos`，与 os-nexhub 同名同义）。手动流程（§6.1–6.7 原文）
仍完全有效——脚本只是它的机械固化，CI 侧由内置 CI 的 **monorepo 骨架注入**
（v0.1.34，`nexhub_ci`）兜底：应用仓 tsconfig/vite 的 `../../crates/.../sdk`
相对引用在 CI 工作目录内自动可达（env `NEXOS_CI_MONOREPO` 缺省
`/home/oem/NexOS`）。

## 7. 引擎门控：引擎内置、应用按装启用

**概念**：重能力后端（如 film 的六阶段 AI 影片管线：本地 vLLM 分镜 /
sd-turbo 关键帧 / 渠道图生视频 / TTS / BGM / ffmpeg 合成）继续**编译在
os-api 二进制内**（引擎内置——不随应用下发任意 Rust 代码，安全边界清晰），
但其业务端点被门控：**未安装声明该 engine 的应用 → 一律 404**：

```json
{"error":"应用「film」未安装：可在 应用中心 → 商店 安装"}
```

**实现**：`AppRegistry::is_engine_enabled(engine)` 按
`SELECT COUNT(*) FROM apps WHERE id=? OR engine=?` 判定；film handler
（`handlers/film.rs`）在 `handle()` 顶部每请求直查（无缓存），安装/卸载
任务落库后**即时生效**；apps 表损坏/锁失败按未启用处理（fail-closed）。

**已门控引擎清单**（main.rs 注册处 `with_app_registry` 注入共享单例；film
于 v0.1.26 批次，qrtransfer / streaming 于 v0.1.30 批次）：

| engine 键 | 应用 | handler | 门控范围（未装全 404） |
|-----------|------|---------|------------------------|
| `film` | film（影片制作）| `handlers/film.rs` | `/api/v1/film/*` 全部 21 条 |
| `qrtransfer` | 二维码传输 | `handlers/qr_transfer.rs` | `/api/v1/qr/*` 全部 9 条 |
| `streaming` | 流媒体中心 | `handlers/streaming.rs` | `/api/v1/streaming/*` 全部 18 条 |

> live 例外：P2P 联邦直播 `/api/v1/live/*` 是独立组件（`handlers/live.rs`，
> 注册名 `live`），属联邦基础能力**常开、不门控**。streaming 引擎不代理任何
> live 路由（其 transcode 的 `mode=live` 只是任务模式字符串，非联邦直播
> 端点），故 `/api/v1/streaming/*` 整表门控、无例外。

**架构取舍与理由**：

- 为什么引擎内置而非应用自带后端？应用包是纯静态前端（web/）+ manifest，
  不含可执行代码——用户审计面 = 前端 JS + 声明的 engine 名；引擎升级随
  os-api 走 A/B 更新与安全补丁，不需要每个应用自带一套 Rust/Python 运行时。
- 为什么装了才开？语义对齐手机系统服务+应用：出厂系统含定位服务内核，
  但没有地图应用就无人可调。film 不再占用每个节点的攻击面与 UI 入口；
  不用的节点零暴露（404 而非 403——对未装应用，入口本身就不存在）。
- 代价：引擎代码闲置体积（编译进二进制但不初始化业务状态；门控查询是
  单行索引 SELECT，每请求开销微秒级）；换取装卸即时生效与零缓存一致性。

## 8. apps 表结构（SQLite apps.db）

```sql
CREATE TABLE IF NOT EXISTS apps (
    id TEXT PRIMARY KEY,          -- manifest.id
    name TEXT NOT NULL,
    version TEXT NOT NULL,        -- 当前安装版本
    category TEXT NOT NULL DEFAULT 'custom',
    icon TEXT NOT NULL DEFAULT '📦',
    description TEXT NOT NULL DEFAULT '',
    entry TEXT NOT NULL,          -- 如 web/entry.js
    repo TEXT NOT NULL,           -- 来源仓库（幂等/升级/409 判定依据）
    engine TEXT NOT NULL DEFAULT '',      -- 声明的内置引擎（门控键）
    min_os_api TEXT NOT NULL DEFAULT '',
    dir TEXT NOT NULL,            -- 绝对安装目录
    installed_at TEXT NOT NULL,   -- 首装时间（升级不变）
    updated_at TEXT NOT NULL      -- 最后升级时间
);
```

打开失败降级内存库（不挡启动，如实 eprintln）。升级走
`INSERT … ON CONFLICT(id) DO UPDATE`，`installed_at` 保持首装值。

## 9. env 清单

| env | 缺省 | 作用 |
|-----|------|------|
| `NEXOS_APPS_DIR` | `/tank/os-data/apps` | 应用安装根目录（`<id>/` 子目录） |
| `NEXOS_APPS_DB` | `/tank/os-data/apps.db` | apps 注册表 SQLite 路径 |
| `NEXOS_GIT_REPOS_DIR` / `OS_GIT_REPOS_DIR` | `/tank/git-repos` | NexHub 裸仓库根（catalog 扫描 + file:// clone 源；os-nexhub `repos_dir()` 同源共用） |
| `NEXOS_FILM_DIR` | `/tank/os-data/film` | film 引擎产物目录（引擎自身 env，非应用运行时新增） |
| `NEXOS_FILM_DB` | `/tank/os-data/film.db` | film 项目表（同上） |

测试隔离：`AppRegistry::with_paths(db, apps_dir, repos_dir)` 显式注入临时
目录，不读 env（防并行测试互踩）。

## 10. 开发第二个应用：Checklist（照 film 复盘）

1. 规划：应用 id（小写连字符）、是否需要内置引擎（多数纯前端应用填
   `engine` 为空即可，只做 REST 消费）。
2. 起步：`cp -r apps/film apps/<name>`（或仅拷 `vite.config.ts` 改
   `outDir`/`styleId`/别名），`manifest.json` 八字段照 §3.2 改；源码正常写
   `import { ref } from 'vue'`——**不要**把 vue/vue-i18n 设为 external 或
   打进包，host-externals 插件会接管（§4.2）。
3. 入口：`src/entry.ts` default export `register(ctx)`，必调
   `registerApp`（带 component）——照 §4.3 film 实文结构。
4. 构建：`npm install && npm run build` → `web/entry.js` 单文件；校验
   `node --input-type=module -e "import('./web/entry.js').then(m=>console.log(typeof m.default))"`
   输出 `function`。
5. 本地自测：推到 NexHub 裸仓库，`POST /api/v1/apps/install` 装真机，桌面
   看图标/开窗/直接 URL（/path → /?app=<id> 浮窗）/调 API。
6. 校验失败排查：安装 400 的 error 文案指明哪个字段（id 非法 / 版本格式 /
   entry 不存在 / min_os_api 超前）；`GET /api/v1/apps/tasks` 有 failed
   记录与 log_tail。
7. 若需要引擎门控：在 os-api 侧给对应 handler 加
   `with_app_registry(Arc<AppRegistry>)` 注入 + handle() 顶部门控断言
   （复刻 film.rs 的 10 行模式），并在本文 §7 补一行应用名。
8. 升级：只 bump manifest.version 推送，重装即升级；不要改 id。

## 11. 测试与质量（本运行时的守护网）

`cargo test -p os-api` 覆盖（真实 git 裸仓库 fixture，全临时目录隔离，
不碰真实 `/tank`）：

- 校验纯函数：id/version/entry/repo 规则、semver 下限比较、mime 白名单
- 安装/卸载回路：201 落盘（.git 不拷贝）、GET /apps 冻结字段齐、目录删除、
  二次卸载 404
- 幂等与升级：同版本 200 noop、异版本 201 upgrade、同 id 异 repo 409、
  仓库缺失 404、非法名 400
- **发布根解析**：源码+dist 双收仓库（根 entry 只在 dist/）回退 dist/ 装
  成功、src 不进安装目录、静态托管命中
- manifest 校验拒绝：坏版本/坏 entry/坏 id/超前 min_os_api 全 400 且任务
  记录 failed
- catalog：好仓库条目、空仓库如实 error、非 `nexos-app-` 前缀仓库不出现、
  安装后 installed/installed_version 翻转
- 静态托管：两种 URL 写法等价命中、mime 正确、未知应用 404、四类穿越
  攻击全 404；**axum 全栈 oneshot 端到端**（组件注册→build_router→直传层）
- 门控切换：未装 film → `/api/v1/film/*` 全 404（含写端点不落库）→ 安装
  → 200 → 卸载 → 404（文案逐字断言）

另有对**真实 NexHub 仓库**的隔离冒烟（env 指向 /tmp，不碰真实
`/tank/os-data`）：catalog 扫到 `nexos-app-film` 真实 manifest → 安装 201 →
幂等重装 200 noop → 门控开（film 端点 200）→ `entry.js` 200
（64,769 B、`text/javascript`、两 URL 写法字节一致）→ 穿越四连（含
`%2e%2e` 编码）全 404 → 卸载 → 门控/资源即时 404。

## 12. 应用 SDK（@nexos/app-sdk，v0.1.28 起）

应用不必手拼 REST 端点：**@nexos/app-sdk** 是 NexOS 的应用能力面 SDK——
联邦大厅 / API 网关（sk-os- 鉴权 + SSE 流式）/ 本地 LLM / 通知 / 能力快照
与降级三态，一个对象全带走。film 是第一个吃狗粮的应用（模型选择器数据源
与降级徽章即走 SDK）。

### 12.1 三行接入

```ts
import { createSdk } from '@nexos/app-sdk'   // 构建期重写到宿主桥，零打包
const sdk = ctx.sdk ?? createSdk(ctx.api)    // 宿主已备好就绪实例则直用
const caps = await sdk.capabilities.get()    // 能力快照（5s 缓存）
```

### 12.2 双载体注入（同一协议版本 `sdk.version='0.1'`）

```text
┌─────────────── 载体 1：桌面嵌入（主前端 appRuntime.ts）────────────────┐
│ createSdk(宿主 api 原语, {notify: useToast, getToken: getApiToken})   │
│   → window.__NEXOS_HOST__.sdk = 就绪实例（ctx.sdk 同一对象）          │
│ 应用 entry.js 的 `import { createSdk } from '@nexos/app-sdk'` 构建期  │
│ 经 host-externals 重写为 `__NEXOS_HOST__.sdk` 解构（零打包）          │
└──────────────────────────────────────────────────────────────────────┘
┌─────────────── 载体 2：独立运行（standalone-host.ts）─────────────────┐
│ createSdk(独立 api 原语, {getToken}) → __NEXOS_HOST__.sdk（ctx.sdk）  │
│ SDK 源码经 vite resolve.alias 指向唯一事实源打进 standalone-host.js   │
│ （自包含；通知走 Notification API，无权限降级 SDK 自绘迷你 toast）    │
└──────────────────────────────────────────────────────────────────────┘
```

**跨包复用机制（唯一事实源 = `crates/os-api/web/src/sdk/*.ts`）**：应用包
不复制源码——类型检查经 tsconfig `paths` 把 `@nexos/app-sdk` 映射到主前端
源文件（`apps/film/tsconfig.json`）；构建期两分支见上（嵌入=桥重写零打包、
独立=alias 打包进宿主）。改 SDK 只动一处，应用下次构建即随。

桥协议（`__NEXOS_HOST__.sdk`）：**既是就绪实例又是工厂载体**——实例上挂
`createSdk` / `SDK_VERSION`（host-externals 虚拟模块从该对象解构导出），
应用 `import { createSdk }` 照常编译；需要自定义 opts（getToken/notify/
fetchImpl）的应用可再自建实例。

### 12.3 能力面 API 表

| 方法 | 端点/实现 | 降级行为（独立/受限时） |
|------|-----------|--------------------------|
| `sdk.capabilities.get()`（5s 缓存）/ `refresh()` / `cached()` / `subscribe()` | `GET /api/v1/capabilities`（读公开，秒回零探测） | 探测失败抛错——降级判定交给 `degraded` |
| `sdk.degraded.state()` / `refresh()` / `subscribe()` | 快照派生（纯函数 `missingOf`） | 探测连败 3 次判 `offline`（`missing=['capabilities']`），**永不抛错** |
| `sdk.lobby.list({q?,scope?})` | `GET /api/v1/api-market` | 空数组=大厅无条目（快照 `lobby.entries=0` → missing `lobby`） |
| `sdk.lobby.chat(entryRef,{messages,stream,onDelta,onDone,onError})` | entryRef→网关渠道映射（§12.4）→ 网关 chat | 无匹配渠道 → 抛错引导「导入为渠道」 |
| `sdk.gateway.channels()` | `GET /api/v1/gateway/channels` | 空=渠道面不可用（missing `gateway`） |
| `sdk.gateway.chat(model,messages,{stream?})` | `POST /api/v1/gateway/v1/chat/completions`（sk-os- Bearer，`stream:true` 走 SSE） | 401/402 等错误 onError 通知 + reject；无 token 时诚实 401 |
| `sdk.llm.instances()` / `running()` | `GET /api/v1/llm/instances` | 空数组（missing `llm`） |
| `sdk.llm.chat(instanceRef,messages,{max_tokens?,temperature?,chat_template_kwargs?})` | `POST /api/v1/llm/instances/:id/chat`（本地实例直连，film local chat 同语义） | 实例非 running 由服务端报错 |
| `sdk.notify(title, body?)` | 嵌入=宿主 toast（主前端 useToast）；独立=Notification API（已授权才用，不主动弹权限询问）；兜底=SDK 自绘迷你 toast DOM | 永不抛错 |

`gateway.chat` 流式回调语义：`onDelta` 逐帧（正文+思考段双键兼容）、
`onDone` 流末聚合、`onError` 失败通知——**Promise 始终反映最终结果**
（resolve/reject），回调是通知不是替代 catch。

### 12.4 能力快照字段（`GET /api/v1/capabilities` 实况）

服务端 `handlers/capabilities.rs`：聚合既有 handler 内存态/缓存
（`instances_snapshot` / `channels_snapshot` / `listings_snapshot` /
`AppRegistry::installed_apps` / p2p `Handle::peers` + film `detect_ffmpeg`），
**秒回、零主动探测联邦、零出站请求**：

```json
{
  "sdk_version": "0.1", "generated_at": "…",
  "llm":     { "instances": 1, "running": ["llm-5"] },
  "gateway": { "channels": 1, "enabled": 1, "relay_channels": 0 },
  "lobby":   { "entries": 1, "last_sync_at": "…心跳缓存最新一条|null", "reachable": "任一条目心跳≤60s" },
  "media":   { "ffmpeg_available": true },
  "p2p":     { "enabled": false, "peers_connected": 0 },
  "apps": ["film"]
}
```

### 12.5 降级矩阵

| 快照信号 | missing 键 | film 的消费（参考实现） |
|----------|-----------|--------------------------|
| `llm.running=[]` | `llm` | 生成剧本按钮置灰（本地 LLM 缺） |
| `gateway.enabled=0` | `gateway` | video/tts/music 按钮置灰（渠道转发缺） |
| `lobby.entries=0` | `lobby` | （SDK 层标记；film 不置灰任何控件） |
| `media.ffmpeg_available=false` | `media.ffmpeg` | 合成按钮置灰 + 安装指引 tooltip |
| `p2p.enabled=false` | **不计入**（部署缺省关，非能力缺失；快照仍透出） | — |
| 快照探测连败 3 次 | `capabilities`（mode=offline） | 顶栏红徽章「离线模式」，全部生成入口停用 |

三态徽章（film 顶栏实装）：全能力=无徽章；`degraded`=琥珀「部分能力受限」
（tooltip 列 missing）；`offline`=红「离线模式」。missing 键与人类可读名
的对照由应用自行映射（film 用 `film.capsMissingTip` i18n 键透出技术键名）。

### 12.6 应用接入 Checklist（SDK 增量，接 §10）

1. `package.json` 无需依赖——tsconfig `paths` 加
   `"@nexos/app-sdk": ["../../crates/os-api/web/src/sdk/index.ts"]`；
   vite 配置照抄 film（嵌入=hostExternals 的 BRIDGE/EXPORT_NAMES 加一行；
   standalone=resolve.alias 加一行）。
2. 组件里 `hostSdk()`（读 `__NEXOS_HOST__.sdk`，见 film `api.ts`）取就绪
   实例；null = 旧宿主，自行降级。
3. 启动先 `sdk.degraded.state()` → 徽章/置灰（§12.5 矩阵对号入座）；
   数据源一律走 `sdk.llm.*` / `sdk.gateway.*`，不再手拼端点。

## 13. 未尽事项（如实记录）

- 安装是同步执行（file:// 克隆亚秒级可接受）；超大应用包（网络直连）应
  改 202 + 任务轮询——前端 `waitForInstalled` 已兼容任务态，切换成本低。
- 安装选版恒取 HEAD（main）；tag 不参与（版本治理留给后续）。
- 包根形态下 src/ 等源码文件随包拷进安装目录（拷贝规则=除 .git 外全部）；
  想只装产物就发 dist 双收形态或纯产物仓——两种都支持。
- catalog 仅扫本机 NexHub 仓库；联邦（跨节点）应用分发未做。
- `min_os_api` 只在安装时校验一次；os-api 降级运行不回滚已装应用。
- 无应用级权限模型（应用经宿主 api 面调 REST，权限 = 登录用户自身）。
