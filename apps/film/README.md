# nexos-app-film —— 影片制作（NexOS 应用包）

NexOS 桌面的「影片制作」应用包：AI 影片管线（参考 LibTV）——
创意 → 剧本（分镜）→ 分镜图 → 图生视频 → 配音 → BGM → 合成成片（final.mp4）。
每个生成阶段可选模型源（本地 LLM / 本地 sd-turbo / 网关渠道，含 🌐 联邦中继）。

本包由主前端（`crates/os-api/web`）剥离而来，经 **NexHub 仓库** 分发，
用户在 **应用中心**（AppStore）自行安装；安装后桌面 / 启动台动态出现，卸载即消失。

## 仓库结构（包根形态：仓库根=应用包根）

```
apps/film/
├── manifest.json        # 应用清单（id/name/version/category/icon/entry/engine）
├── package.json         # 构建依赖（vue / vite 等，仅构建期使用）
├── tsconfig.json        # 包独立 TS 配置（vue-tsc 校验）
├── vite.config.ts       # entry.js lib 构建 + 宿主桥重写 + CSS 内联（见下）
├── vite.standalone.config.ts  # standalone-host lib 构建（真宿主，vue 全打包）
├── src/
│   ├── entry.ts         # 单入口：default export register(ctx)（协议冻结）
│   ├── FilmStudio.vue   # 主组件（FilmHub 大厅 HubLobby + 工作室：左侧选项卡 + 五区工作台）
│   ├── PreviewMonitor.vue  # 预览监视器（ratio 适配画面区 + 字幕条 + 播放控制）
│   ├── TimelineTracks.vue  # 底部多轨时间轴（四轨 + 播放头：点击/拖动 seek）
│   ├── previewEngine.ts    # 预览播放引擎（段映射/时钟/媒体驱动/懒加载 composable
│   │                       # + finalName 版本预览：合成页 dist/final-v*.mp4 指定装载）
│   ├── api.ts           # /api/v1/film/* 端点封装（底层走宿主桥 HTTP 层；FilmHub
│   │                    #   流程化契约：story/storyboard/casting/audio-bgm/cache/
│   │                    #   files/cost/export/import + 写端点 author 字段）
│   ├── icon.ts          # 场记板图标（SVG 内部标记）
│   ├── i18n/            # 四语言全量文案（zh-CN / zh-TW / en-US / ja-JP；键 film.* + filmhub.*）
│   └── flow/            # FilmHub 流程化（v0.1.7）+ 显性大厅（v0.1.1）
│       ├── HubLobby.vue        # 🎬 FilmHub 大厅（应用首页：品牌栏/搜索/丰富模式/项目卡网格）
│       ├── HubBrowse.vue       # 项目 Hub 浏览（第八视图：文件树+内容区+活动/成本/接入指南 Tab）
│       ├── SideNav.vue        # 左侧竖向选项卡（🎬FilmHub 回大厅+五阶段+工作台+Hub 浏览+设置）
│       ├── FlowPageHead.vue   # 流程页页头（分区负责人/待认领+认领；多人提示条）
│       ├── CostBadge.vue      # 成本徽章（调用数+估算费用）+ 只读面板弹窗（表格=CostPanel）
│       ├── CostPanel.vue      # 只读成本面板展示件（by stage/channel 两表；徽章弹窗与 Hub 浏览共用）
│       ├── StoryPage.vue      # ① 剧情：原文导入/列表/pre + story.md 正稿（AI 写/字数/来源）
│       ├── StoryboardPage.vue # ② 分镜：从剧情生成分镜 + 镜头卡横排网格 + 去工作台细调
│       ├── CastingPage.vue    # ③ 定妆：AI 提取六类 + Tab + 对象卡 + 多视图五槽位 +
│       │                       #   card.md/voice 编辑 + 对象级认领 + pendingCastSelect 互跳选中
│       ├── AudioPage.vue      # ④ 音频：BGM 库（trigger/mood/时长/有无 track）+ 导入/AI 生成/删除
│       ├── ComposePage.vue    # ⑤ 合成：BGM 选择 + dist 成品版本列表（下载/预览切监视器）
│       │                       #   + cache 半成品区（确认采用 commit / 丢弃）
│       ├── SettingsPage.vue   # 设置/成员：成员管理 + 分区认领 + 活动流（多人分工 v1）
│       ├── flowTypes.ts       # 阶段/视图类型（含 hub 第八视图）+ README frontmatter stage 解析
│       ├── flowFiles.ts       # files 树派生（dist/cache/sources）+ hub 纯函数（buildHubTree/
│       │                       #   hubFileIcon/hubTargetView/hubCastSelect/deriveStageFromProject）
│       ├── collab.ts          # 多人分工：ownership.json（分区+对象级认领）/ activity.json /
│       │                       #   操作人 localStorage（纯函数 + b64 编解码）
│       ├── flowContext.ts     # FlowContext（provide/inject：项目/模型源/任务中心/阶段/协作态/
│       │                       #   pendingCastSelect——Hub 浏览→定妆页对象选中互跳）
│       └── flow.css           # 流程页共享样式（fh-*/hub-* 命名空间，全局类）
├── scripts/
│   ├── preview-smoke.mjs       # previewEngine 冒烟（esbuild + happy-dom，47 断言）
│   ├── flow-smoke.mjs          # 流程化组件冒烟（vite 构建 harness + happy-dom 挂载，53 断言）
│   ├── flow-smoke-harness.ts   # 流程化冒烟 harness（真实 .vue 挂载 + mock 宿主桥 api/fixtures）
│   ├── hub-smoke.mjs           # 显性大厅冒烟（vite 构建 harness + happy-dom，50 断言）
│   ├── hub-smoke-harness.ts    # 大厅冒烟 harness（HubLobby/HubBrowse/CastingPage/SideNav + 降级 fixtures）
│   └── sdk-verify.mjs          # SDK 装载校验
├── standalone/
│   ├── standalone.html  # 独立全页入口（构建后并入 web/，经 /apps-assets 服务）
│   └── standalone-host.ts     # 独立运行宿主（宿主桥自给自足，见下）
└── web/                 # 发布产物（构建后并入；/apps-assets/film/ 即此目录）
    ├── entry.js         # ESM 单文件：组件 + i18n + 图标 + CSS 全内联（vue 走宿主桥）
    ├── standalone-host.js     # 独立宿主单文件（vue + vue-i18n 完整打包）
    └── standalone.html  # 独立入口页（引用相对 ./web/standalone-host.js，
                          #   经静态托管剥 web/ 段规则命中同目录产物）
```

## FilmHub 显性大厅 + 工作室布局（v0.1.1）

```
┌────────────────────────────────────────────────────────────────────┐
│ 🎬 FilmHub          影片项目中心 · AI 像写代码一样创作               │
│ [🔍 搜索（标题/创意）] [🌿丰富模式] [↻刷新] [+新建项目] [↗]        │ 大厅顶栏
├────────────────────────────────────────────────────────────────────┤
│ ┌──────────────┐ ┌──────────────┐ ┌──────────────┐                 │
│ │ 星际快递员 16:9│ │ 深海灯塔 16:9 │ │ 无题草稿 16:9 │                 │
│ │ idea 两行截断… │ │ …            │ │ …            │                 │
│ │ ✓✓✓✓● 五阶段点 │ │ ✓✓●○○        │ │ ●○○○○        │                 │
│ │ 👤小明 👤小红  │ │ 👤阿礁        │ │（404→素卡）   │                 │
│ │ 🕒更新 💰12次  │ │ 🕒更新        │ │ 🕒更新        │                 │
│ │ ▸小红 story…  │ │（降级无活动行）│ │              │                 │
│ │ [打开][🗂Hub][删]│ │ …            │ │ …            │                 │
│ └──────────────┘ └──────────────┘ └──────────────┘                 │
└────────────────────────────────────────────────────────────────────┘
        │打开（按 README 阶段定缺省页）/ Hub 浏览（直达文件树）
        ▼
┌──────────┬─────────────────────────────────────────────────────────┐
│ 🎬FilmHub│  当前选项卡页面（独立组件，FlowContext provide/inject）    │
│ ──────── │   · StoryPage      左原文区 | 右 story.md 正稿            │
│ ① 剧情 ✓ │   · StoryboardPage 生成条 + 镜头卡横排网格               │
│ ② 分镜 ✓ │   · CastingPage    提取报告 + 六类 Tab + 对象详情五槽位   │
│ ③ 定妆 ● │   · AudioPage      BGM 库 + 导入/生成表单                │
│ ④ 音频 ○ │   · ComposePage    BGM 选择 + dist 版本 + cache 半成品   │
│ ⑤ 合成 ○ │   · HubBrowse（第八视图，v0.1.1）项目文件树 + 内容区：    │
│ ──────── │     左树（目录折叠/类型图标）| 右内容（pre/图/音/视频）     │
│ 🛠 工作台 │     + 四 Tab（文件/活动/成本/AI 接入指南 curl 三段）       │
│ 🗂 Hub浏览│     +「在工作台打开」路径→流程页互跳（casting 带对象选中） │
│ ⚙ 设置   │   · SettingsPage   成员/分区认领/活动流                  │
│          │   · 工作台 = 原五区（左镜头列 24% + 任务条 / 中面板 40% + │
│          │     角色 / 右监视器 36% + 合成区 + 底部多轨时间轴）        │
└──────────┴─────────────────────────────────────────────────────────┘
```

- **显性大厅**（v0.1.1，HubLobby 组件）：进入影片制作第一眼=🎬 FilmHub 品牌大厅
  （NexHub Explore 仓库大厅对等形态），项目像「仓库」一样浏览：搜索（标题/idea）、
  五阶段进度点（列表无 README stage——`deriveStageFromProject` 按产物启发式推导，
  进入项目后以 README 为准）、丰富模式（缺省开，localStorage 记忆可关；并发 ≤12
  项目轻读 ownership/activity/cost，任一失败静默降级素卡）、卡片操作（打开 /
  🗂 Hub 浏览 / 删除确认）。
- 阶段徽章读项目 README.md frontmatter `stage`（story→storyboard→casting→audio→
  compose；已过=✓、当前=橙实心序号）；进入项目按阶段定缺省页；SideNav 顶部
  「🎬 FilmHub」品牌项回大厅（导航层级：FilmHub 大厅 → 项目（流程页|Hub 浏览））。
- **Hub 浏览页**（HubBrowse 组件，第八视图 + 大厅卡「Hub 浏览」直达）：GET :id/files
  平铺清单 → buildHubTree 嵌套树（目录在前按名排序，根级目录缺省展开，可折叠；
  图标 md📝/json🧾/图🖼/音🎵/视频▶/余📄）；内容区文本等宽 pre（mime 优先 +
  扩展名兜底）/图片 data URL 直显/audio/video 标签/二进制提示；顶部路径面包屑 +
  「在工作台打开」（hubTargetView：story.md→剧情 / storyboard.json→分镜 /
  casting/*→定妆（pendingCastSelect 选中对象）/ audio/*→音频 / dist·cache·final*→
  合成 / 其余→工作台）；四 Tab：文件 / 活动（activity.json 时间线）/ 成本
  （CostPanel by stage/channel 两表，与成本徽章弹窗共用）/ AI 接入指南
  （curl 三段：GET files 树 / PUT storyboard.json / POST storyboard generate +
  应用说明——agent 改文件后走对应流程页或 POST /film/projects/import）。
- 多人分工 v1：ownership.json（members + sections 分区认领 + casting_objects
  对象级认领，键 `<type>/<name>`）与 activity.json（环形 200 条流水）走通用
  files 面（GET/PUT）；写端点 body 带 `author`（顶栏「我是」选择器，
  localStorage 记忆，缺省 anonymous）；未认领对象卡显示「待认领」+ 认领按钮，
  新建对象创建人自动成为 owner；v1 软约束无锁——「多人同时编辑以后保存为准」。
- 剧情正稿 v1 纯 pre 等宽展示（不引 marked）；cache「丢弃」仅本地隐藏
  （后端暂无删除端点）。

## entry.js 协议（冻结，docs/APPS.md）

```ts
export default function register(ctx: {
  registerApp(app: {id, label, icon, route, category, gradient?, component?}): void
  addRoute(route: {path, name?, component}): void
  addI18n(locale: string, messages: Record<string, unknown>): void
  api: { get, post, del, request }   // 宿主 api client 原语（鉴权/超时/错误统一）
}): void
```

应用包自带全部资源（组件 / i18n / 图标），不依赖主前端内部模块。
`vue` 与 `vue-i18n` **不打进包**：构建期由 `vite.config.ts` 的 `host-externals`
插件把 `import ... from 'vue' / 'vue-i18n'` 重写到宿主桥
`globalThis.__NEXOS_HOST__.vue / .vueI18n`（主前端 `appRuntime.ts` 注入），
保证与宿主共享同一份 Vue 实例（响应式系统 / `useI18n` 注入正常）——
打进第二份 Vue 会导致组件状态失活与 `useI18n()` 报错，故弃用打包方案。

## 独立全页运行（standalone，v0.1.2 起）

桌面嵌入模式之外，本包支持**脱离 NexOS 桌面壳的独立全页运行**：

- 入口：`/apps-assets/film/standalone.html`（新浏览器标签页直接打开）；
  应用 UI 右上角的外链图标按钮（嵌入模式显示、独立模式隐藏，键
  `film.openStandalone` ×4 语言）即 `window.open` 此地址。
- 宿主自给自足：`standalone/standalone-host.ts` 就是真宿主——
  `vite.standalone.config.ts` 构建（**不挂 host-externals**）把
  vue + vue-i18n 完整打进 `web/standalone-host.js`（~316KB，应用包完全
  自包含，内网离线可跑，不引 CDN），然后置
  `window.__NEXOS_STANDALONE__ = true`、以极简 ctx 适配器调 `register`：
  `registerApp → createApp 挂 #app 全屏`（暗色 tokens 近似样式底）、
  `addRoute → noop`、`addI18n → 独立 i18n 实例`（locale 判定
  localStorage `os.locale` → navigator.language → zh-CN）。
- api 原语与主前端 client 语义对齐：同源路径、JSON、Bearer token
  （localStorage `os-api-token`，与桌面「设置 → API 令牌」同 key 同源共享）、
  15s 超时、非 2xx 抛 `ApiError`（message/status/path）；写操作 401/403 时
  弹极简 token 输入条（记住后自动重试一次）。

## 构建方法

```bash
cd apps/film
npm install          # 安装构建依赖
npm run build        # entry.js + standalone-host.js + standalone.html + manifest 同步
npm run typecheck    # vue-tsc --noEmit（包独立类型检查）
npm run smoke        # previewEngine 冒烟 47 断言 + 流程化组件冒烟 53 断言 + 显性大厅冒烟 50 断言
npm run smoke:flow   # 仅流程化组件冒烟（vite 构建 harness + happy-dom 真实挂载 .vue）
npm run smoke:hub    # 仅显性大厅冒烟（HubLobby/HubBrowse/CastingPage/SideNav + 丰富模式降级）
```

构建产物校验（宿主桥打桩后 entry.js 的 default export 必须是函数）：

```bash
node --input-type=module -e "
globalThis.__NEXOS_HOST__ = { vue: await import('vue'), vueI18n: await import('vue-i18n'), api: {} };
const e = await import('./dist/web/entry.js');
console.log(typeof e.default);   // → function
await import('./dist/web/standalone-host.js');  // 模块完整性（无 DOM 环境 mount 前止步属正常）
"
```

## 发布 / 安装

- 发布：本仓库推送到 NexHub（`nexos-app-film`，分支 `main`，tag `v0.1.4`），
  源码与 web/ 产物同仓同步提交（仓库根=应用包根）。
- 一键发布（推荐，v0.1.34 起）：主仓 `./tools/publish-app.sh film --patch` ——
  构建 → 同步发布仓（根=包根）→ commit/tag/push → 触发 CI → 安装/升级全链。
- 安装：NexOS 桌面 → 应用中心 → 商店 → NexOS 应用包 → 安装
  （`POST /api/v1/apps/install {repo:"nexos-app-film"}`；同 id 异版本为覆盖升级）。
- 后端 `GET /apps-assets/film/web/entry.js` 提供静态资源；前端运行时
  `dynamic import` + `register(ctx)` 热注册，免刷新桌面可见。

## 版本记录

| 版本 | 日期 | 说明 |
| --- | --- | --- |
| 0.1.1 | 2026-09-06 | **FilmHub 显性大厅**（进入影片制作第一眼=🎬 FilmHub，NexHub Explore 对等形态）：① **大厅**（HubLobby 组件重造项目列表页=应用首页）：品牌栏（影片项目中心 · AI 像写代码一样创作）+ 搜索（标题/idea 过滤）+ 丰富模式开关（缺省开 localStorage 记忆；并发 ≤12 项目轻读 files/ownership+activity+cost，任一失败**静默降级素卡**）+ 项目卡网格（NexHub 仓库卡风格：标题+ratio pill+五阶段进度点——列表无 README stage，deriveStageFromProject 按产物启发式推导（final*→compose / bgm·line-\*→audio / shot-\*.png·casting→casting / 有分镜→storyboard / 余 story），详情以 README 为准；idea 两行截断/更新时间/成员 chips/最近活动一句/成本小字）+ 卡片操作（打开/🗂 Hub 浏览/删除确认）；新建项目弹窗保留。② **项目 Hub 浏览页**（HubBrowse 组件，第八视图+大厅卡直达）：左文件树（GET :id/files→buildHubTree 嵌套，目录在前可折叠根级缺省展开，图标 md📝/json🧾/图🖼/音🎵/视频▶/余📄）+ 右内容区（文本等宽 pre（mime 优先+扩展兜底）/图片 data URL 直显/audio/video 标签/二进制提示）+ 路径面包屑 +「在工作台打开」（hubTargetView 互跳映射；casting/* 经 FlowContext.pendingCastSelect 定妆页切类+选中对象）+ 四 Tab（文件/活动流 activity.json 时间线/成本 CostPanel by stage·channel 两表（从 CostBadge 抽出共用）/AI 接入指南——curl 三段（GET files 树/PUT storyboard.json/POST storyboard generate）+ agent 改文件后走流程页或 POST /film/projects/import 说明）。③ **导航整合**：SideNav 顶部「🎬 FilmHub」品牌项回大厅 +「🗂 Hub 浏览」第八选项卡（9 项）；层级=FilmHub 大厅→项目（流程页\|Hub 浏览）；外链/独立模式逻辑保留。④ i18n 新增 filmhub.* 34 键×4（zh-TW：影片中樞/檔案樹/活動流/接入指南）；hub-smoke 冒烟 50 断言（npm run smoke:hub）；flow-smoke 更新为 9 选项卡 53 断言。 |
| 0.1.7 | 2026-09-06 | FilmHub 流程化 UI（v0.1.35 前端件）：工作室改**左侧竖向选项卡**（五流程阶段 ①剧情②分镜③定妆④音频⑤合成 + 工作台 + 设置/成员；NexHub 导航形态，可折叠，阶段徽章读 README frontmatter stage）+ 流程页独立组件 src/flow/（剧情：原文导入/story.md AI 写/pre+字数来源；分镜：从剧情生成+镜头卡横排网格+去工作台细调；定妆：AI 提取六类报告+Tab+对象卡+多视图五槽位（AI 生成/导入双按钮）+card.md/voice+**对象级认领**；音频：BGM 库 trigger/mood/时长/有无 track+导入+AI 生成；合成：BGM 选择+dist/final-v\* 成品版本列表（下载+预览成片切监视器 finalName 版本装载）+cache 半成品确认采用 commit）；**多人分工 v1**（ownership.json 分区+对象级认领 / activity.json 活动流 / 写端点 author 字段 / 顶栏「我是」localStorage 记忆 / 成员≥2 提示条）；成本徽章+只读面板（by stage/channel）；api.ts 契约端点 20+（含 files 树/b64 信封编解码/cost/export/import）；i18n 163 键×4（341 键/语言）；happy-dom 组件冒烟 51 断言（npm run smoke:flow，vite 构建 harness 真实挂载 .vue）；entry.js 289,016B（gzip 60.16kB）。 |
| 0.1.6 | 2026-09-05 | 预览监视器与时间轴播放头（v0.1.35 前端件）：五区布局（左镜头卡 24% + 紧凑任务条 / 中镜头面板 40% 含角色库 / 右监视器 36% + 紧凑合成区，窄屏堆叠监视器置顶）；previewEngine.ts 播放引擎（storyboard 分镜连播：单 video 切 src / Ken Burns 图片段 / 黑底占位 + line-N.mp3 段同步 + bgm 循环 0.35 混播 + final.mp4 整播模式）；TimelineTracks 播放头（标尺点击/拖动 seek + 播放匀速移动 + 当前段红色高亮）；素材 ±1 预加载；i18n 17 键×4；happy-dom 冒烟 47 断言（npm run smoke）。 |
| 0.1.4 | 2026-09-04 | 角色库与一致性：角色 CRUD（name/description/voice）+ 定妆图上传/生成 + 镜头绑定 chips（PUT script.characters）+ 参考图导入；生成注入（channel reference_images+strength / local prompt 档）+ TTS voice 按角色透传；字段名对齐后端（shot/desc/artifacts）+ 任务中心 stage/status 映射修复；i18n 45 键×4。 |
| 0.1.3 | 2026-09-04 | SDK 吃狗粮（模型源走 @nexos/app-sdk、standalone 能力徽章三态、生成按钮按缺失能力置灰）。 |
| 0.1.2 | 2026-09-04 | 独立解耦运行：standalone 宿主（vue 全打包自包含）+ 独立入口页 + 右上角外链图标（film.openStandalone ×4）。 |
| 0.1.1 | 2026-09-04 | （布局/修订过渡，无功能变更。） |
| 0.1.0 | 2026-09-04 | 自主前端剥离首发：组件 / API / i18n（四语言）/ 图标迁入，宿主桥运行时。 |
