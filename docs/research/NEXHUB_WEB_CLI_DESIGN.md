# NexHub 网页优先重排 · nexhub CLI · 应用一键部署 —— 调研与设计方案

> 状态：设计稿（纯调研产出，未动任何源码）
> 日期：2026-09-05　基线版本：NexOS v0.1.31（/s/codehub 全屏独立路由已上线）
> 调研范围：`crates/os-api/web/src/views/CodeHub.vue`（2957 行全读）、`router/index.ts`、`api/client.ts`、
> `os-nexhub/src/code_repo.rs`、`os-api/src/http.rs`（git_http_handler）、`handlers/apps_handler.rs`、
> `handlers/provisioning.rs`（install.sh 先例）、`/tank/git-repos/nexos-app-*.git` 实仓 manifest。

---

## 0. 一句话推荐路线

**以既有后端端点为地板做"网页产品化"重构（P1：GitHub 风三层布局 + 仓库详情页 + README markdown 渲染 + nexos-app-\* 应用卡一键部署，零新增端点）；CLI（P2）复用 provisioning install.sh 的动态脚本端点先例，只新增 `GET /api/v1/coderepo/cli.sh` 一个端点；CodeHub.vue 按 Tab 域拆成 ~7 个子组件以承载重构。**

核心判断：**后端能力已经齐了**——仓库 CRUD/文件树/commits/Issues/PR、Smart Git HTTP、应用 catalog/install/热注册、引擎门控全部就绪；三件升级的瓶颈全部在前端呈现层与 CLI 分发壳，不需要新的业务端点。

---

## 1. 现状审计

### 1.1 CodeHub.vue 页面结构（crates/os-api/web/src/views/CodeHub.vue，2957 行）

| # | Tab | 数据源（client.ts） | 交互 | 现状问题 |
|---|-----|--------------------|------|---------|
| 1 | 仓库列表 | `GET /api/v1/coderepo/repos` | 卡片网格 `minmax(300px,1fr)` + 创建/导入/删除弹窗 | 无搜索/过滤/排序；无 URL 态；删除用原生 `confirm()`；卡片 5 个动作按钮并列，全屏下信息密度低 |
| 2 | 大厅 | `GET /api/v1/nexhub/lobby(?:q=&tag=&sort=)` + `/stats` | 本地/联邦二级 Tab、行展开详情、打赏 TipButton、发布/克隆/购买弹窗 | 最成熟的一屏（有搜索/tag facet/排序/响应式契约）；但 README 摘要是 `<pre>` 纯文本；行内 6+ 按钮拥挤 |
| 3 | 代码浏览 | `GET …/repos/:name/contents`、`…/file?path=` | 仓库下拉/手输名 + 280px 文件树 + 内容面板 | **无 README 渲染**（纯 `<pre>`）；文件树默认全收起、无面包屑；仓库靠下拉选择而非导航；无 URL 态 |
| 4 | 提交历史 | `GET …/repos/:name/commits` | 仓库下拉 + 列表（最近 20 条） | 与"浏览"割裂——同一仓库的信息被拆成两个平级 Tab |
| 5 | Issues | coderepo issues 端点组 | 仓库下拉 + 状态过滤 + 行展开 + 评论 | 同上：必须先在下拉选仓库 |
| 6 | Pull Requests | coderepo pulls 端点组 | 同上 + merge/close 权限显隐 | 同上 |
| 7 | AI 会话 | `GET/POST /api/v1/coderepo/sessions` | 时间线 + 归档弹窗 | 独立性好，问题少 |
| 8 | 接入说明 | 纯前端静态 | curl 三步上架指南 | **硬编码 IP（192.0.2.106 / 203.0.113.2）与开发期 token 文案**，应改为按 `window.location.host` 动态生成 |

横切现状：
- **Tab 用 `v-show` 切换**（8 个 panel 全部常驻渲染），Tab 状态不进 URL——`/s/codehub` 全屏打开后**无法深链**到某个仓库/某 Tab，刷新即回仓库列表。
- **i18n 缺失**：全文件中文硬编码，唯二 `useI18n` 用点是 `t('apps.openStandalone')`；i18n locales（zh-CN/en-US/ja-JP/zh-TW）里没有 codehub 域。
- **无仓库详情页概念**："浏览/提交/Issues/PR"都是从仓库卡片**跳 Tab + 改另一个 Tab 的 ref**，一个仓库的生命周期信息散落在 4 个 Tab 里。
- **standalone 全屏模式与桌面模式唯一差异**：`isStandalone` 时隐藏右上角外链按钮。没有利用全屏做任何排版增益——这就是用户"更符合网页打开"诉求的直接痛点。

### 1.2 路由现状（router/index.ts）

| 路径 | 行为 |
|------|------|
| `/codehub` | MainLayout 子路由；`beforeEach` 守卫查 `routeAppIds` → 重定向 `/?app=codehub` 桌面浮窗（WindowFrame 内渲染，宽度受窗口限制） |
| `/s/codehub` | `STANDALONE_APPS` 注册表展开的**顶层全屏路由**（无 Dock/Launchpad/窗口框）；同源 localStorage 共享 token；未登记的 `/s/*` 兜底回首页 |
| 机制 | 内置应用独立化 = 在 `STANDALONE_APPS` 加一行即可复用整条链路；运行时应用包走 `addAppRoute()`（appRuntime 热注册） |

### 1.3 数据流与身份

- 所有请求走 `api/client.ts` 的 `endpoints.codeRepoXxx` / `endpoints.nexhubLobbyXxx`（读公开、写双通道：链上身份 `useChainIdentity` 的 challenge→sign→verify 24h token，或全局 admin token 回落）。
- `AppStore.vue` 已有完整的**应用包安装先例**：`appsCatalog()` → `appsInstall(repo)` → 轮询 `GET /api/v1/apps` 出现该 id → `hotRegisterApp()` 免刷新桌面可见（appRuntime 动态 import `/apps-assets/:id/entry`）。CodeHub 的部署按钮可直接照抄此流程。
- markdown 渲染依赖已就位：`marked@^18` 在 package.json，`DevDocs.vue` / `LlmModels.vue` 已用 `marked.setOptions({gfm:true, async:false})` + `v-html` 直渲（信任模型：内容为可信上游，无 dompurify）——README 渲染直接复用同一模式。

---

## 2. 后端能力盘点（全部既有，无需新增业务端点）

### 2.1 code_repo（os-nexhub/src/code_repo.rs，CodeRepoRouteHandler）

| 方法 | 端点 | 鉴权 | 说明 |
|------|------|------|------|
| GET | `/api/v1/coderepo/repos` | 公开 | 扫描 `/tank/git-repos/*.git`（NEXOS_GIT_REPOS_DIR 可覆盖），返回 name/description/size/last_commit/branch_count/commit_count/**clone_url_ssh** |
| POST | `/api/v1/coderepo/repos` | admin | 创建裸仓库（git init --bare，HEAD→main）；409 已存在 |
| DELETE | `/api/v1/coderepo/repos/:name` | admin | rm -rf 裸仓库 |
| GET | `/api/v1/coderepo/repos/:name/contents` | 公开 | 文件树 + branches + default_branch |
| GET | `/api/v1/coderepo/repos/:name/file?path=` | 公开 | 文件内容（**README.md 渲染的数据源**） |
| GET | `/api/v1/coderepo/repos/:name/commits` | 公开 | 最近 20 条 |
| POST | `/api/v1/coderepo/repos/:name/clone-url` | admin | 双通道 clone URL（SSH + Smart HTTP） |
| POST | `/api/v1/coderepo/repos/:name/import` | admin | 目录导入（git init+add+commit+push） |
| GET/POST | `/api/v1/coderepo/sessions(/…/end)` | 读公开/写 admin | AI 会话归档 |
| GET | `/api/v1/coderepo/stats` | 公开 | 汇总 |
| 12 条 | `…/repos/:name/issues|pulls/…`（issues.rs） | 读公开/写 handler 内自验 | 项目协作层（关闭/重开/评论/merge 权限分级） |

### 2.2 Smart Git HTTP（os-api/src/http.rs `git_http_handler`，`/git/*`）

- 系统 `git-http-backend` CGI；**读匿名放行 / push 必须 token**（401 触发 git 凭据提示，Basic 密码 = admin token）。
- 路径穿越防护 + 端点白名单；仓库根 `state.git_repos_root` 或回退 `os_nexhub::repos_dir()`。
- **CLI/网页 clone 的地基已就绪**：`git clone http://<host>:8558/git/<repo>.git` 今天就能用。

### 2.3 应用包运行时（handlers/apps_handler.rs，六端点）

| 方法 | 端点 | 鉴权 | 说明 |
|------|------|------|------|
| GET | `/api/v1/apps` | 公开 | 已装列表（含 repo/version/installed_at） |
| POST | `/api/v1/apps/install` {repo} | admin | **同步**安装/升级：clone→manifest 校验（id/version/entry/min_os_api）→ 拷贝 → 登记；action=`install`/`upgrade`/`noop`；同 id 异源 409 |
| DELETE | `/api/v1/apps/:id` | admin | 卸载 |
| GET | `/api/v1/apps/catalog` | 公开 | **扫 `nexos-app-*` 前缀裸仓库**（`CATALOG_REPO_PREFIX`），逐仓 `git show HEAD:manifest.json`；返回 repo/id/name/version/category/icon/description/engine/installed/installed_version/error |
| GET | `/api/v1/apps/tasks` | 公开 | 安装任务列表（同步终态 + 可观测记录） |
| GET | `/apps-assets/:id/*` | 公开 | 应用静态资源（防穿越；text 直传/二进制 base64） |

- catalog 实仓验证（`git show HEAD:manifest.json`）：nexos-app-film（id=film v0.1.4，category=media，icon=clapperboard，engine=film）、nexos-app-qrtransfer（v0.1.0，icon=内联 SVG，engine=qrtransfer）、nexos-app-streaming（v0.1.0，icon=内联 SVG，engine=streaming）。manifest 字段：id/name/version/category/icon/description/entry/engine/sdk/min_os_api。
- **引擎门控联动**（main.rs）：film / streaming / qr_transfer 引擎内置但每请求直查 apps 表——未安装对应应用包时引擎 API 被门控，**装机即解锁引擎**，卸载即关。

### 2.4 动态 shell 脚本端点先例（handlers/provisioning.rs）——CLI 分发的样板

- `GET /api/v1/provisioning/install.sh`：**公开、按请求动态生成、原文直传**。
- 机制要点（cli.sh 可全套复用）：
  1. 安装源 URL 由 **HTTP Host 头推导**（`source_base_url()`：Host → `NEXOS_GIT_ADVERTISE_HOST` env → 127.0.0.1 兜底）——任一节点都能当分发源；
  2. handler 返回 `body: Value::String(script)` + `content-type: text/x-shellscript`，http.rs `direct_passthrough_bytes()` 对 `text/*` 做**原文直传**（不走 JSON 信封）；
  3. 路由声明 `requires_auth=false`（NAT 新机无 token 可达）。

### 2.5 nexhub 大厅（os-nexhub/src/nexhub_lobby.rs，与 CLI 映射相关）

`/api/v1/nexhub/auth/challenge|verify`（链上 token 签发）、`GET /lobby(?:q=&tag=&sort=)`、`GET /lobby/stats|entitlements|:name`、`POST /lobby/publish|:name/clone|:name/purchase|:name/federate`、`DELETE /lobby/:name`、bounty 8 条、lobby 级 pulls/releases。

---

## 3. 方案 A：网页优先重排

### 3.1 设计原则（业界参照 + 本地约束）

参照 GitHub/Gitea 仓库页与 Primer 布局指南的成熟模式：**顶栏全局导航 + 仓库列表页（visitor-first，搜索/分类 facet 前置）+ 仓库详情页（header 行为区 + Code tab 主内容 + README 渲染 + 右侧 About 侧栏）**。社区复盘（community#204347 等）的共识是"访客优先、README 主角、文件树退居导航"，NexHub 全屏模式应照此办理。差异化：NexHub 的仓库里有一类特殊公民 `nexos-app-*`（NexOS 应用包），需要 GitHub 没有的**应用卡 + 一键部署**形态（参照 Coolify/CapRover 的 one-click apps 目录 + Deploy to Netlify 的跨站一键部署按钮 pattern，但部署目标是本节点，无需跨站授权跳转）。

### 3.2 新布局线框（ASCII）

#### ① 全局顶栏（standalone 常驻；桌面窗口模式隐藏、由窗口标题栏替代）

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ ◆ NexHub    [ 🔍 搜索仓库 / 项目 / 应用…            ]      身份🪪  · 节点名  │
│                          Repos │ Apps │ 大厅 │ Issues │ AI 会话 │ 接入指南   │ ← 一级导航(原 8 Tab 归组)
└──────────────────────────────────────────────────────────────────────────────┘
```

#### ② 仓库列表页 `/s/codehub`（GitHub 风 Explore：搜索 + facets + 最近动态）

```
┌ 顶栏 ────────────────────────────────────────────────────────────────────────┐
│ Repos      [全部(12)] [应用 nexos-app-*(3)] [普通仓库(9)]     [创建▾][导入]  │
│  ┌ 搜索：_____________  类别: [全部][media][devtools][…]  排序: [最近提交▾] ┐│
│  └ 语言/标签 chips:  #rust ×4  #media ×2  #cli ×1（点击过滤）───────────── ┘│
├──────────────────────────────────────────────┬───────────────────────────────┤
│ 仓库行列表（单列，行 = 名称+描述+元数据+动作） │  右侧栏（全屏≥1100px 显示） │
│ ┌──────────────────────────────────────────┐ │  统计：仓库 12 · 3.2 GB      │
│ │ 📦 nexos-app-film        [应用] [⬇ 部署] │ │  AI 会话 8 · 累计提交 214    │
│ │    AI 影片管线：创意→…   ⎇2 ·◷57 · 12MB  │ │  最近动态：                  │
│ │    更新于 3 小时前        v0.1.4         │ │   · film push main 3h 前     │
│ ├──────────────────────────────────────────┤ │   · nexos 新仓建 1d 前       │
│ │ 📁 zbox-vm-tools        [⬇ 克隆][浏览]   │ │   · issue #3 评论 2d 前      │
│ │    （描述）              ⎇1 ·◷12 · 2MB  │ │  快速开始：                  │
│ ├──────────────────────────────────────────┤ │  $ curl -fsSL http://<host>: │
│ │ 📁 nexhub-guide   …                      │ │    8558/api/v1/coderepo/     │
│ └──────────────────────────────────────────┘ │    cli.sh | sh    [复制]     │
└──────────────────────────────────────────────┴───────────────────────────────┘
（窄屏/窗口模式：右侧栏折叠为顶部统计条，行动作收进 ⋯ 菜单）
```

#### ③ 仓库详情页 `/s/codehub/:repo`（GitHub 风：header 行为区 + tab + README 主角）

```
┌ 顶栏 ────────────────────────────────────────────────────────────────────────┐
│ ← 返回列表   📁 nexos-app-film   [应用 v0.1.4]  公开                          │
│ AI 影片管线：创意 → 剧本分镜 → …                                              │
│ [ ⭐HTTPS ⌄ http://host:8558/git/nexos-app-film.git 📋复制 ]                  │
│                [⬇ 部署到本节点]  [⇄ PR]  [💬 Issue]  [🗑]                     │ ← 行为区
│ ┌─ Code ─ Manifest ─ Commits(57) ─ Issues(3) ─ PR(1) ─ Sessions ─┐            │
├──────────────────────────────────────────────┬───────────────────────────────┤
│ ⎇ main ▾   nexos-app-film /                  │  About（右栏）                │
│  ▸ apps/  ▸ docs/   📄 manifest.json         │  ──────────────               │
│  📄 README.md   📄 web/                      │  版本 v0.1.4 · media          │
│ ┌──────────────────────────────────────────┐ │  引擎 film · sdk ^0.1         │
│ │  文件列表（名称/最近提交/时间，GitHub 风）│ │  克隆 ⬇ 12 · 大小 12MB        │
│ │  → 下方 README.md 渲染区（marked GFM）   │ │  Tags: #media #film           │
│ │  # 影片制作 …（标题/表格/代码块全渲染）   │ │  ┌─────────────────────────┐ │
│ └──────────────────────────────────────────┘ │  │ ⬇ 部署到本节点           │ │
│                                              │  │ 已装 v0.1.3 → 升级可用   │ │
│                                              │  └─────────────────────────┘ │
└──────────────────────────────────────────────┴───────────────────────────────┘
```

#### ④ 应用仓库页（nexos-app-\* 专属变体 = 详情页 + 应用卡头）

```
┌──────────────────────────────────────────────────────────────────────────────┐
│ [icon]  影片制作  nexos-app-film v0.1.4   category: media   引擎: film       │
│         AI 影片管线：创意 → 剧本分镜 → 分镜图 → 图生视频 → …                  │
│ ┌────────────────────┐  ┌────────────────────────────────────────────────┐  │
│ │  截图位（manifest   │  │ ⬇ 部署到本节点（未装） / ⬆ 升级到 v0.1.4   │  │
│ │  screenshots 字段   │  │ 已装 v0.1.4 ✓ —— 桌面 / Launchpad 可见     │  │
│ │  扩展，缺省占位图） │  │ 安装后解锁 film 引擎 API（引擎门控联动）    │  │
│ └────────────────────┘  └────────────────────────────────────────────────┘  │
│   同详情页 Code/Manifest/Commits/… Tab（README 默认渲染）                     │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 3.3 路由与 URL 方案（解决"不可深链"）

`/s/codehub` 改造为**子路由父节点**（`STANDALONE_APPS` 机制保留，CodeHub.vue 内部改用嵌套 `<RouterView>`，或把 STANDALONE_APPS 展开逻辑改为支持 children 数组——推荐后者，一处改动全局复用）：

| URL | 视图 | 对应现状 |
|-----|------|---------|
| `/s/codehub` | 仓库列表（Explore） | Tab1 |
| `/s/codehub/apps` | 应用目录（nexos-app-\* 卡片墙） | 新增（数据 = appsCatalog） |
| `/s/codehub/:repo` | 仓库详情（默认 Code tab，README 渲染） | Tab3+4 合体 |
| `/s/codehub/:repo/(tree|blob)/:path*` | 文件树/文件（可后续加，P1 只做 repo 级） | Tab3 深链 |
| `/s/codehub/:repo/manifest` | Manifest 渲染（应用仓库） | 新增视图，数据既有 |
| `/s/codehub/:repo/(commits\|issues\|pulls\|sessions)` | 详情页内嵌 tab | Tab4/5/6/7 |
| `/s/codehub/onboarding` | 接入指南（IP 动态化） | Tab8 |

- 桌面浮窗模式（`/?app=codehub` → WindowFrame）：**同一套组件树**，非 standalone 时顶部导航隐藏（窗口标题栏承担）、URL 由内存 route 模拟或直接忽略——组件以 `props.mode: 'web' | 'window'` 区分，避免两套布局。
- 返回桌面入口：standalone 顶栏左侧加 `← 桌面`（现全屏模式无回桌面路径，是网页体验缺项）。

### 3.4 桌面窗口 vs 全屏：双布局适配策略

**同布局自适应**（不做两套模板），断点按"容器宽度"而非视口（浮窗宽度 ≠ 视口宽度，CSS 容器查询 `@container` 或 JS ResizeObserver 注入 `data-size` 属性）：

| 容器宽 | 布局行为 |
|--------|---------|
| ≥1100px（全屏常态） | 三栏全开：列表页右栏/详情页 About 栏常驻；仓库行动作按钮全展开 |
| 700–1099px（中窗） | 右侧栏收起为顶部统计条；行动作收进 `⋯` 溢出菜单 |
| <700px（小窗/移动） | 单列；大厅行响应式契约（已实现：标签折叠→描述隐藏→按钮换行）全站推广 |

既有大厅 Tab 的响应式契约（项目名永不截断、让位优先级注释）是现成资产，直接提升为全局规范。

### 3.5 其他重排要点

- **README markdown 渲染**：详情页 Code tab 默认拉 `codeRepoFile(repo, 'README.md')` → `marked.parse()` → `v-html`（信任模型同 DevDocs.vue：NexHub 仓库为本节点托管的可信内容；P2 可再加 dompurify）。大厅行摘要保留纯文本 excerpt，但展开详情升级为 markdown 渲染。
- **弹窗依赖降级**：删除/合并等破坏性操作从原生 `confirm()` 换为站内确认弹层（Toast 体系已有）；创建/导入/发布保留 modal 但统一为右侧抽屉（全屏模式下更"网页"）。
- **i18n 补课**：新增 `codehub` i18n 域，重排时同步把硬编码文案迁入 locales 四语言（现状 0 覆盖，随拆组件一次性做，避免二次翻动）。
- **接入指南动态化**：`192.0.2.106` / `203.0.113.2` / `change-me-admin-token` 硬编码改为 `window.location.host` 推导 + 从 `/api/v1/coderepo/cli.sh` 的安装命令替代手写 curl 教程。

---

## 4. 方案 B：nexhub CLI 规格

### 4.1 分发方式（复用 install.sh 先例，一个新端点）

```
curl -fsSL http://<node>:8558/api/v1/coderepo/cli.sh | sh
```

- 新端点 **`GET /api/v1/coderepo/cli.sh`**（公开 `requires_auth=false`，挂 CodeRepoRouteHandler）：
  - handler 内按请求动态渲染脚本常量（照 `render_install_script` 先例），安装源/默认服务器 URL 由 **HTTP Host 头推导**（Host → `NEXOS_GIT_ADVERTISE_HOST` → 127.0.0.1 兜底）；
  - 返回 `body: Value::String` + `content-type: text/x-shellscript; charset=utf-8` → http.rs `direct_passthrough_bytes()` 原文直传（机制现成，零网关改动）；
  - 脚本行为：探测 `~/.local/bin`（PATH 内则用之，否则 `/usr/local/bin`，无权限时提示 sudo）→ 写入 `nexhub` 主脚本 → `chmod +x` → 打印 `nexhub login` 引导。**单文件自包含**（主程序就是被安装的 shell 脚本本身，无二进制下载），安装 = 下载一份脚本，升级 = `nexhub self-update` 重新拉同端点。
- 凭据：`nexhub login <token>` 写 `~/.config/nexhub/credentials`（`chmod 0600`；内容 `server=<base>\ntoken=<t>`），环境变量 `NEXHUB_SERVER` / `NEXHUB_TOKEN` 优先（CI 场景）。token 三选一：admin token（读写全量）或链上 token（challenge→sign→verify 产物，写操作归因 pubkey）——CLI 不做签名，链上 token 由用户从 Web/IM 侧取得后 login。

### 4.2 命令面（逐一映射既有端点，零新业务端点）

| 命令 | 端点映射 | 说明 |
|------|---------|------|
| `nexhub login <token>` | —（本地） | 存 server+token 到 `~/.config/nexhub/credentials`（0600）；`--server` 指定节点 |
| `nexhub whoami` | `GET /api/v1/coderepo/stats` + 一次带 token 的轻写探测（或 `GET /api/v1/apps`） | 验证 token 有效 + 打印 server/身份形态（admin/chain） |
| `nexhub ping` | 同上 | 纯连通性（退出码 0/1，供脚本） |
| `nexhub repo list` | `GET /api/v1/coderepo/repos` | 表格输出（`--json` 原样） |
| `nexhub repo create <name> [-d desc]` | `POST /api/v1/coderepo/repos` | 201 成功 / 409 已存在 |
| `nexhub repo delete <name>` | `DELETE /api/v1/coderepo/repos/:name` | 需 `--yes` 跳过确认 |
| `nexhub repo info <name>` | `GET …/contents` + `GET …/commits`（组合） | 分支/默认分支/文件数/最近提交 |
| `nexhub clone <repo>` | 输出 Smart HTTP URL → **有 git 则 `exec git clone http://<host>:8558/git/<repo>.git`**（匿名读），无 git 则仅打印 URL | push 凭据提示：用户名任意、密码=token（401 触发 git 自带凭据提示，与网页"接入说明"一致） |
| `nexhub push <repo> [branch]` | （薄封装）校验远端存在后 `git remote add hub <url> && git push hub <branch>` | 可选，P2.5 |
| `nexhub apps list` | `GET /api/v1/apps/catalog` | 含 installed/installed_version 列；`--all` 连未装一起列（默认即全列） |
| `nexhub apps deploy <repo>` | `POST /api/v1/apps/install {repo}` → 同步终态 | 输出 action=install/upgrade/noop + 版本；failed 打印 error；`--wait` 轮询 `GET /api/v1/apps`（当前后端同步完成，wait 仅防御性） |
| `nexhub apps remove <id>` | `DELETE /api/v1/apps/:id` | 需 `--yes` |
| `nexhub apps installed` | `GET /api/v1/apps` | 已装列表 |
| `nexhub lobby list [q] [--tag] [--sort]` | `GET /api/v1/nexhub/lobby` | 可选，P2.5 |
| `nexhub self-update` | `GET /api/v1/coderepo/cli.sh` | 重新下载覆盖自身（`$0` 定位自路径） |
| `nexhub help / <cmd> --help` | — | 纯文本（避免引入 man 体系） |

### 4.3 脚本形态建议

- **单文件 POSIX bash**（`#!/usr/bin/env bash` + `set -eu`；避免依赖 bash 4 特性以兼容 macOS 自带 bash 3.2）：依赖 **curl**（必选）+ **jq**（首选）→ 缺失时降级 **python3 -c 'json…'**（再缺失则仅支持 `--json` 原样输出并告警）。
- 输出纪律：默认人读表格；`--json` 透传原始响应；错误统一 stderr + 退出码（0 成功 / 1 远端错误 / 2 参数错——与 os-cli 约定一致）。
- 安全：token 仅从 env/credentials 读取，不进 argv（避免 ps 泄漏；curl `-H @file` 或 `--config` 注入）。
- **与既有 `os` CLI（crates/os-cli，Rust clap 二进制：status/pool/vm/share/user/discover + `--output text|json|yaml`）的关系**：定位互斥不重叠——`os` 管 OS 基础设施（Rust 编译分发），`nexhub` 管代码托管/应用分发（shell 脚本随节点分发、零构建）。若未来合并，`nexhub` 命令面可平移为 `os nexhub` 子命令树；本期不建议（Rust 交叉编译分发成本 > shell 脚本价值）。

---

## 5. 方案 C：应用一键部署（NexHub 网页）

### 5.1 资格判定

- 仓库列表/详情页对每个 repo 计算 `isApp = name.startsWith('nexos-app-')`（与后端 `CATALOG_REPO_PREFIX` 同约定）+ catalog 返回的 manifest 可读性（`error` 字段为空 → manifest 校验通过；`error` 非空 → 显示"缺 manifest.json 或校验失败"，按钮禁用并提示原因——**不假成功**，与 catalog 的 error 透传语义一致）。
- 数据源：详情页进入时并行拉 `GET /api/v1/apps/catalog`（含 installed/installed_version，一次请求同时拿到资格与安装态）。

### 5.2 UI 状态机（详情页行为区 + 应用卡 + 右栏 About 复用同一组件 `<DeployButton :entry>`）

```
未安装 ──点击──▶ 安装中(spinner) ──201──▶ 已装 vX.Y.Z ✓（"桌面 / Launchpad 可见"）
                   │                        ▲
                   ├─400 manifest 错误──────┘ → 红字提示原因
catalog 版本 > installed_version ──▶ [⬆ 升级到 vX.Y.Z] ──201 action=upgrade──▶ 已装新版本
同版本重复点击 ──▶ 200 action=noop（提示"已是最新"）
```

- 安装为**同步请求**（后端 clone+校验+拷贝在请求内完成），前端 loading 态 + 超时兜底后轮询 `GET /api/v1/apps/tasks` 观测（client.ts `AppsInstallResp` 已做同步/任务双态宽松容错）。
- 完成提示链：`action=install/upgrade` → 绿 toast「已安装 vX —— 桌面可见（appRuntime 热注册，免刷新）」；引擎门控应用（film/streaming/qrtransfer）追加说明「film 引擎 API 已解锁」。
- 已装但来源不同仓库 → 后端 409（同 id 异源拒绝覆盖），前端原样透传。
- 应用目录页（`/s/codehub/apps`）= catalog 卡片墙，卡内含 icon（名字或内联 SVG，`appRegistry` 已有 runtime 图标渲染先例）/name/version/category/description/部署按钮/「查看源码」链到详情页。

### 5.3 降级与门控

- **未登录/token 无效**：部署按钮不禁用（读 catalog 公开），点击后 401 → 按钮变「登录后部署」，引导至顶栏身份入口（桌面模式下引导 `/?app=` 身份面板）；同现有 `lobbyWriteErr` 的 401 文案模式。
- **权限**：`POST /apps/install` 需 admin——非 admin 链上身份点击 → 403 提示「应用部署需节点管理员（admin token）」。是否放宽为链上身份可装：开放问题 §7.3。
- **引擎联动说明**（写进应用页文案）：部署后 film/streaming/qrtransfer 对应引擎 API 立即可用（每请求查 apps 表，无需重启）；卸载即门控回关。

---

## 6. 方案 D：契约草案与实施计划

### 6.1 端点增量清单

| Phase | 端点 | 类型 | 说明 |
|-------|------|------|------|
| P1 | （无） | — | 重排 + 一键部署全部用既有端点；README 用 `…/file?path=README.md`；部署用 catalog/install/tasks |
| P1（可选增强） | `GET /api/v1/coderepo/repos` 响应加 `default_branch` / `description` 已有；**`category` 需从 catalog join** | 前端 join | 列表页 facet 由前端合并 repos × catalog 计算，不加端点 |
| P2 | **`GET /api/v1/coderepo/cli.sh`** | **唯一新增** | 公开、动态生成、Host 头推导、text/x-shellscript 直传（照 provisioning/install.sh 样板，路由声明 requires_auth=false + 直传 content-type） |
| P2（可选） | manifest.json 约定扩展 `screenshots: []` | 约定，非端点 | 应用卡截图位；缺省占位 |

结论：**最小增量 = 1 个新端点（cli.sh）**。

### 6.2 CodeHub.vue 拆分建议（现 2957 行）

按"Tab 域 → 子组件 + 组合式函数"拆，一个 PR 内完成结构性拆分（不改行为）后再做重排，保证可回退：

```
web/src/views/codehub/
├── CodeHubPage.vue          // 壳：顶栏/导航/路由出口（web|window 双模式）
├── views/
│   ├── RepoListPage.vue     // Explore：搜索+facets+行列表（原 Tab1+stats 右栏）
│   ├── AppCatalogPage.vue   // 应用卡片墙（appsCatalog 驱动）
│   ├── RepoDetailPage.vue   // 详情壳：header 行为区 + 内嵌 tab 路由
│   │   ├── RepoCodeTab.vue      // 文件树+列表+README 渲染（marked）
│   │   ├── RepoManifestTab.vue  // manifest 渲染（应用仓库）
│   │   ├── RepoCommitsTab.vue   // 原 Tab4
│   │   ├── CollabTabs.vue       // Issues/PR（原 Tab5/6，内部再拆 CollabList/CollabDetail）
│   │   └── RepoSessionsTab.vue  // 原 Tab7
│   └── OnboardingPage.vue   // 接入指南（IP/token 动态化 + cli.sh 快速开始）
├── components/
│   ├── DeployButton.vue     // §5.2 状态机（复用 AppStore 轮询+热注册逻辑）
│   ├── CloneUrlBar.vue      // 双通道 clone 地址复制
│   └── IdentityCard.vue     // 链上身份卡（原大厅 Tab 内提为通用）
└── composables/
    ├── useNexhubRepos.ts    // repos/commits/contents/file 拉取
    ├── useLobby.ts          // 大厅状态（原 Tab2 逻辑整体迁移）
    └── useAppDeploy.ts      // catalog+install+轮询+热注册（从 AppStore.vue 抽公用）
```

- `useAppDeploy.ts` 从 AppStore.vue 抽出后由 AppStore 与 CodeHub 共用（消除两处 install 流程重复）。
- i18n：拆分时每组件同步引入 `useI18n`，文案入 locales（新 `codehub.*` 域）。

### 6.3 分阶段实施建议

| Phase | 内容 | 规模感 | 依赖 |
|-------|------|--------|------|
| **P0（半天）** | CodeHub.vue 无行为拆分（§6.2 目录骨架，v-show→组件挂载但 URL 仍单页） | 纯重构 | 无 |
| **P1（2–4 天）** | 网页重排：/s/codehub 子路由深链、三层布局线框落地、README markdown 渲染、应用目录页 + DeployButton 一键部署（含引擎解锁提示）、接入指南动态化、i18n 补课 | 前端为主，零端点 | P0 |
| **P2（1–2 天）** | `GET /api/v1/coderepo/cli.sh` 端点 + nexhub 脚本（login/repo/apps deploy/whoami/self-update）+ 接入指南与文档接入 CLI 快速开始 | 后端 1 端点 + 1 个 ~400 行脚本 | 无（可与 P1 并行） |
| P2.5（按需） | `nexhub push/lobby list`、文件级深链 `tree/blob`、manifest screenshots、dompurify | 增强 | P1/P2 |

### 6.4 风险与开放问题

1. **README/manifest 渲染 XSS 面**：marked v-html 无 dompurify（DevDocs 先例接受"可信上游"；NexHub 仓库内容可被任何有 push token 的 agent 写入——信任边界比 DevDocs 弱）。建议 P1 至少对 README 渲染加 dompurify，或接受风险并记录。
2. **STANDALONE_APPS 子路由改造**：现机制只展开单条顶层记录；改 children 需要兼容"未登录/未装应用"等兜底路径，注意 `/s/codehub/:repo` 的 `:repo` 段与其他未来 standalone 应用无冲突（仅 codehub 自己的子树，无冲突）。
3. **install 权限模型**：`POST /apps/install` 仅 admin；网页"一键部署"的产品预期是"节点用户都能装"。放宽到链上身份（装自家发布的 app）是否引入越权/资源滥用，需产品决策（开放问题）。
4. **catalog 实时性**：catalog 每次全量 `git show HEAD:manifest.json` 扫描（当前 3 个应用无压力）；应用多了以后需要缓存/事件失效（低风险，记入 TODO）。
5. **cli.sh 在 Windows/Git Bash 的兼容**：install.sh 已踩过 Git Bash 引号坑（接入指南有提示）；nexhub 脚本遵循同样规避（`--data-binary @file`、避免内联 JSON），Windows 目标用户 P2 暂不支持 powershell 版。
6. **桌面浮窗模式的 URL 同步**：浮窗内不做 URL 同步（window.history 属于宿主页），仅 standalone 深链；两模式组件一致、状态经 props/store 传递——需在 P0 拆分时定死接口，避免 P1 返工。
7. **硬编码 IP/token 文案外泄**：接入指南当前明文开发期 token（change-me-admin-token）与公网 IP——重排时改为动态推导的同时，需确认生产是否仍要展示该 token（安全开放问题）。

---

## 附：调研依据文件索引

- 前端：`crates/os-api/web/src/views/CodeHub.vue`、`web/src/router/index.ts`、`web/src/api/client.ts`（codeRepo/apps 段）、`web/src/appRuntime.ts`、`web/src/views/AppStore.vue`（install 先例）、`web/src/views/DevDocs.vue`（marked 先例）
- 后端：`crates/os-nexhub/src/code_repo.rs`（路由表 L776–846）、`os-api/src/http.rs`（git_http_handler L947+、direct_passthrough_bytes L335）、`os-api/src/handlers/apps_handler.rs`（六端点 + AppRegistry.install/scan_catalog）、`os-api/src/handlers/provisioning.rs`（install.sh L1406+、source_base_url L505）、`os-api/src/main.rs`（组件注册 + 引擎门控注释 L444–653）
- 仓库实况：`/tank/git-repos/nexos-app-{film,qrtransfer,streaming}.git` HEAD:manifest.json
- 业界参照：GitHub 仓库页/Primer 布局（github.blog 2025 changelog、community#204347）、gh CLI 命令面（cli.github.com/manual/gh_repo）、自托管 one-click apps（Coolify/CapRover/Dokploy）
