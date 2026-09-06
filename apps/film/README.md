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
│   ├── FilmStudio.vue   # 主组件（列表页 + 工作室：左侧选项卡 + 五区工作台）
│   ├── PreviewMonitor.vue  # 预览监视器（ratio 适配画面区 + 字幕条 + 播放控制）
│   ├── TimelineTracks.vue  # 底部多轨时间轴（四轨 + 播放头：点击/拖动 seek）
│   ├── previewEngine.ts    # 预览播放引擎（段映射/时钟/媒体驱动/懒加载 composable
│   │                       # + finalName 版本预览：合成页 dist/final-v*.mp4 指定装载）
│   ├── api.ts           # /api/v1/film/* 端点封装（底层走宿主桥 HTTP 层；FilmHub
│   │                    #   流程化契约：story/storyboard/casting/audio-bgm/cache/
│   │                    #   files/cost/export/import + 写端点 author 字段）
│   ├── icon.ts          # 场记板图标（SVG 内部标记）
│   ├── i18n/            # 四语言全量文案（zh-CN / zh-TW / en-US / ja-JP，键 film.*）
│   └── flow/            # FilmHub 流程化（v0.1.7）
│       ├── SideNav.vue        # 左侧竖向选项卡（五阶段+工作台+设置；阶段徽章读 README）
│       ├── FlowPageHead.vue   # 流程页页头（分区负责人/待认领+认领；多人提示条）
│       ├── CostBadge.vue      # 成本徽章（调用数+估算费用）+ 只读面板 by stage/channel
│       ├── StoryPage.vue      # ① 剧情：原文导入/列表/pre + story.md 正稿（AI 写/字数/来源）
│       ├── StoryboardPage.vue # ② 分镜：从剧情生成分镜 + 镜头卡横排网格 + 去工作台细调
│       ├── CastingPage.vue    # ③ 定妆：AI 提取六类 + Tab + 对象卡 + 多视图五槽位 +
│       │                       #   card.md/voice 编辑 + 对象级认领
│       ├── AudioPage.vue      # ④ 音频：BGM 库（trigger/mood/时长/有无 track）+ 导入/AI 生成/删除
│       ├── ComposePage.vue    # ⑤ 合成：BGM 选择 + dist 成品版本列表（下载/预览切监视器）
│       │                       #   + cache 半成品区（确认采用 commit / 丢弃）
│       ├── SettingsPage.vue   # 设置/成员：成员管理 + 分区认领 + 活动流（多人分工 v1）
│       ├── flowTypes.ts       # 阶段/视图类型 + README frontmatter stage 解析（纯函数）
│       ├── flowFiles.ts       # files 树派生（dist/cache/sources）+ frontmatter/字数/
│       │                       #   视图槽位匹配 + BGM 表单校验（纯函数）
│       ├── collab.ts          # 多人分工：ownership.json（分区+对象级认领）/ activity.json /
│       │                       #   操作人 localStorage（纯函数 + b64 编解码）
│       ├── flowContext.ts     # FlowContext（provide/inject：项目/模型源/任务中心/阶段/协作态）
│       └── flow.css           # 流程页共享样式（fh-* 命名空间，全局类）
├── scripts/
│   ├── preview-smoke.mjs       # previewEngine 冒烟（esbuild + happy-dom，47 断言）
│   ├── flow-smoke.mjs          # 流程化组件冒烟（vite 构建 harness + happy-dom 挂载，51 断言）
│   ├── flow-smoke-harness.ts   # 冒烟 harness（真实 .vue 挂载 + mock 宿主桥 api/fixtures）
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

## 工作室布局（v0.1.7 FilmHub 流程化）

```
┌────────────────────────────────────────────────────────────────────┐
│ ← 返回  标题 [16:9] [风格] ⚙   …  [我是:操作人▾] [💰成本徽章] [↗] │ 顶栏
├──────────┬─────────────────────────────────────────────────────────┤
│ ① 剧情 ✓ │  当前选项卡页面（独立组件，FlowContext provide/inject）   │
│ ② 分镜 ✓ │   · StoryPage      左原文区 | 右 story.md 正稿           │
│ ③ 定妆 ● │   · StoryboardPage 生成条 + 镜头卡横排网格               │
│ ④ 音频 ○ │   · CastingPage    提取报告 + 六类 Tab + 对象详情五槽位   │
│ ⑤ 合成 ○ │   · AudioPage      BGM 库 + 导入/生成表单                │
│ ──────── │   · ComposePage    BGM 选择 + dist 版本 + cache 半成品   │
│ 🛠 工作台 │   · SettingsPage   成员/分区认领/活动流                  │
│ ⚙ 设置   │   · 工作台 = 原五区（左镜头列 24% + 任务条 / 中面板 40% + │
│          │     角色 / 右监视器 36% + 合成区 + 底部多轨时间轴）        │
└──────────┴─────────────────────────────────────────────────────────┘
```

- 阶段徽章读项目 README.md frontmatter `stage`（story→storyboard→casting→audio→
  compose；已过=✓、当前=橙实心序号）；进入项目按阶段定缺省页。
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
npm run smoke        # previewEngine 冒烟 47 断言 + 流程化组件冒烟 51 断言
npm run smoke:flow   # 仅流程化组件冒烟（vite 构建 harness + happy-dom 真实挂载 .vue）
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
| 0.1.7 | 2026-09-06 | FilmHub 流程化 UI（v0.1.35 前端件）：工作室改**左侧竖向选项卡**（五流程阶段 ①剧情②分镜③定妆④音频⑤合成 + 工作台 + 设置/成员；NexHub 导航形态，可折叠，阶段徽章读 README frontmatter stage）+ 流程页独立组件 src/flow/（剧情：原文导入/story.md AI 写/pre+字数来源；分镜：从剧情生成+镜头卡横排网格+去工作台细调；定妆：AI 提取六类报告+Tab+对象卡+多视图五槽位（AI 生成/导入双按钮）+card.md/voice+**对象级认领**；音频：BGM 库 trigger/mood/时长/有无 track+导入+AI 生成；合成：BGM 选择+dist/final-v\* 成品版本列表（下载+预览成片切监视器 finalName 版本装载）+cache 半成品确认采用 commit）；**多人分工 v1**（ownership.json 分区+对象级认领 / activity.json 活动流 / 写端点 author 字段 / 顶栏「我是」localStorage 记忆 / 成员≥2 提示条）；成本徽章+只读面板（by stage/channel）；api.ts 契约端点 20+（含 files 树/b64 信封编解码/cost/export/import）；i18n 163 键×4（341 键/语言）；happy-dom 组件冒烟 51 断言（npm run smoke:flow，vite 构建 harness 真实挂载 .vue）；entry.js 289,016B（gzip 60.16kB）。 |
| 0.1.6 | 2026-09-05 | 预览监视器与时间轴播放头（v0.1.35 前端件）：五区布局（左镜头卡 24% + 紧凑任务条 / 中镜头面板 40% 含角色库 / 右监视器 36% + 紧凑合成区，窄屏堆叠监视器置顶）；previewEngine.ts 播放引擎（storyboard 分镜连播：单 video 切 src / Ken Burns 图片段 / 黑底占位 + line-N.mp3 段同步 + bgm 循环 0.35 混播 + final.mp4 整播模式）；TimelineTracks 播放头（标尺点击/拖动 seek + 播放匀速移动 + 当前段红色高亮）；素材 ±1 预加载；i18n 17 键×4；happy-dom 冒烟 47 断言（npm run smoke）。 |
| 0.1.4 | 2026-09-04 | 角色库与一致性：角色 CRUD（name/description/voice）+ 定妆图上传/生成 + 镜头绑定 chips（PUT script.characters）+ 参考图导入；生成注入（channel reference_images+strength / local prompt 档）+ TTS voice 按角色透传；字段名对齐后端（shot/desc/artifacts）+ 任务中心 stage/status 映射修复；i18n 45 键×4。 |
| 0.1.3 | 2026-09-04 | SDK 吃狗粮（模型源走 @nexos/app-sdk、standalone 能力徽章三态、生成按钮按缺失能力置灰）。 |
| 0.1.2 | 2026-09-04 | 独立解耦运行：standalone 宿主（vue 全打包自包含）+ 独立入口页 + 右上角外链图标（film.openStandalone ×4）。 |
| 0.1.1 | 2026-09-04 | （布局/修订过渡，无功能变更。） |
| 0.1.0 | 2026-09-04 | 自主前端剥离首发：组件 / API / i18n（四语言）/ 图标迁入，宿主桥运行时。 |
