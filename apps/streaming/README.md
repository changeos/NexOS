# nexos-app-streaming —— 流媒体中心（NexOS 应用包）

NexOS 桌面的「流媒体中心」应用包：拉流源管理 / 多机位节目切换 / 转码阶梯
（HLS）/ 推流目标 / 拉流转推流 + **直播**（本地大厅 + 联邦大厅，浏览器采集
→ WS → MSE 播放）。

本包由主前端（`crates/os-api/web`）剥离而来（原 `StreamingCenter.vue` +
`LivePanel.vue`），经 **NexHub 仓库** 分发，用户在 **应用中心** 自行安装；
安装后桌面 / 启动台动态出现，卸载即消失。

## 仓库结构（包根形态：仓库根=应用包根）

```
apps/streaming/
├── manifest.json        # 应用清单（id/name/version/category/icon/entry/engine）
├── package.json         # 构建依赖（vue / vite 等，仅构建期使用）
├── tsconfig.json        # 包独立 TS 配置（vue-tsc 校验）
├── vite.config.ts       # entry.js lib 构建 + 宿主桥重写 + CSS 内联（见下）
├── vite.standalone.config.ts  # standalone-host lib 构建（真宿主，vue 全打包）
├── src/
│   ├── entry.ts         # 单入口：default export register(ctx)（协议冻结）
│   ├── StreamingCenter.vue  # 主组件（7 Tab：直播/拉流源/多机位/转码/转推/推流/总览）
│   ├── LivePanel.vue    # 直播面板（「直播」Tab 挂载；本地大厅 + 联邦大厅）
│   ├── DataTable.vue / data-table.ts  # 通用表格组件（从主前端原样迁入）
│   ├── format.ts        # formatBytes 等格式化工具（原样迁入）
│   ├── api.ts           # /api/v1/streaming/* + /api/v1/live/* 端点封装（宿主桥）
│   ├── icon.ts          # 信号塔图标（SVG 内部标记）
│   └── i18n/            # 四语言（live.* 46 键整体迁入 + streaming.* 壳文案）
├── standalone/
│   ├── standalone.html  # 独立全页入口（构建后并入 web/，经 /apps-assets 服务）
│   └── standalone-host.ts     # 独立运行宿主（宿主桥自给自足，见下）
└── web/                 # 发布产物（构建后并入；/apps-assets/streaming/ 即此目录）
    ├── entry.js         # ESM 单文件：组件 + i18n + 图标 + CSS 全内联（vue 走宿主桥）
    ├── standalone-host.js     # 独立宿主单文件（vue + vue-i18n 完整打包）
    └── standalone.html  # 独立入口页（引用相对 ./web/standalone-host.js）
```

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
插件重写到宿主桥 `globalThis.__NEXOS_HOST__`（主前端 `appRuntime.ts` 注入），
保证与宿主共享同一份 Vue 实例。

## 后端端点

| 端点 | 用途 |
| --- | --- |
| `/api/v1/streaming/sources`（+ `record/start|stop`） | 拉流源 CRUD 与录制 |
| `/api/v1/streaming/program`（+ `switch`） | 多机位节目主输出 |
| `/api/v1/streaming/transcode`（+ `/sources`） | 转码任务与本地输入源 |
| `/api/v1/streaming/outputs`（+ `start|stop`） | 推流目标 |
| `/api/v1/streaming/stats` | 流媒体统计 |
| `/api/v1/live/rooms`（REST 3 条）+ `/ws/live/:id/publish|view` | 直播：本地 + 联邦大厅（overlay `live_lobby` 宣告合并，`live_relay_*` 跨节点观看） |

**live 端点不门控**：直播是联邦能力的 UI——引擎端点留在主应用（后端常开），
UI 随本应用包走；本包 api.ts 正常调用。

## 能力徽章与置灰（@nexos/app-sdk，apps/film 0.1.3 范式）

- 独立模式顶栏：全能力=无徽章 / `degraded`=琥珀「部分能力受限」/
  `offline`=红「离线模式」（`sdk.degraded.state()` 三态）。
- `media.ffmpeg` 缺失或离线 → 转码类按钮（创建转码任务 / 创建转推链路及各自
  提交钮）置灰 + tooltip 指明缺失能力。

## 独立全页运行（standalone）

- 入口：`/apps-assets/streaming/standalone.html`（新浏览器标签页直接打开）；
  应用右上角外链图标（嵌入模式显示、独立模式隐藏，键 `streaming.openStandalone`
  ×4 语言）即 `window.open` 此地址。
- 宿主自给自足：`standalone/standalone-host.ts` 就是真宿主——vue + vue-i18n
  完整打进 `web/standalone-host.js`（自包含，内网离线可跑，不引 CDN），
  api 原语与主前端 client 语义对齐（Bearer token 同 key `os-api-token` 共享、
  15s 超时、401/403 弹 token 条重试）。
- `?tab=` 深链：桌面嵌入模式守卫把 `/streaming?tab=live` 重定向为
  `/?app=streaming&tab=live`，独立模式 `standalone.html?tab=live`——组件统一从
  `location.search` 读取（应用包宿主无 vue-router）。

## 构建方法

```bash
cd apps/streaming
npm install          # 安装构建依赖
npm run build        # entry.js + standalone-host.js + standalone.html + manifest 同步
npm run typecheck    # vue-tsc --noEmit（包独立类型检查）
```

构建产物校验（宿主桥打桩后 entry.js 的 default export 必须是函数）：

```bash
node --input-type=module -e "
globalThis.__NEXOS_HOST__ = { vue: await import('vue'), vueI18n: await import('vue-i18n'), api: {} };
const e = await import('./dist/web/entry.js');
console.log(typeof e.default);   // → function
"
```

## 发布 / 安装

- 发布：本仓库推送到 NexHub（`nexos-app-streaming`，分支 `main`，tag `v0.1.0`），
  源码与 web/ 产物同仓同步提交（仓库根=应用包根）。
- 一键发布（推荐，v0.1.34 起）：主仓 `./tools/publish-app.sh streaming --patch`
  —— 构建 → 同步发布仓（根=包根）→ commit/tag/push → 触发 CI → 安装/升级全链。
- 安装：NexOS 桌面 → 应用中心 → 商店 → 安装
  （`POST /api/v1/apps/install {repo:"nexos-app-streaming"}`）。
- 后端 `GET /apps-assets/streaming/web/entry.js` 提供静态资源；前端运行时
  `dynamic import` + `register(ctx)` 热注册，免刷新桌面可见。

## 版本记录

| 版本 | 日期 | 说明 |
| --- | --- | --- |
| 0.1.0 | 2026-09-05 | 自主前端剥离首发：StreamingCenter.vue（7 Tab）/ LivePanel.vue（直播 Tab 随包，端点常开）/ DataTable / client.ts streaming+live 段 / 图标 / 路由迁入；i18n live.* 46 键×4 整体迁入 + streaming.* 壳文案；SDK 能力徽章三态 + media.ffmpeg 置灰转码类按钮；独立运行外链 + standalone 宿主。 |
