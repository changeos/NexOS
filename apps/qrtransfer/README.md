# nexos-app-qrtransfer —— 二维码传输（NexOS 应用包）

NexOS 桌面的「二维码传输」应用包：文件 ⇄ QR 视频编解码（文件 → 跳动 QR 视频，
每帧一个二维码 → 解码回文件）+ 文本 ⇄ QR 图片即时互转。适合离线 / 物理隔离
（air-gap）场景——数据只经摄像头/屏幕与 QR 编码流动，不走网络。

本包由主前端（`crates/os-api/web`）剥离而来，经 **NexHub 仓库** 分发，
用户在 **应用中心** 自行安装；安装后桌面 / 启动台动态出现，卸载即消失。

## 仓库结构（包根形态：仓库根=应用包根）

```
apps/qrtransfer/
├── manifest.json        # 应用清单（id/name/version/category/icon/entry/engine）
├── package.json         # 构建依赖（vue / vite 等，仅构建期使用）
├── tsconfig.json        # 包独立 TS 配置（vue-tsc 校验）
├── vite.config.ts       # entry.js lib 构建 + 宿主桥重写 + CSS 内联（见下）
├── vite.standalone.config.ts  # standalone-host lib 构建（真宿主，vue 全打包）
├── src/
│   ├── entry.ts         # 单入口：default export register(ctx)（协议冻结）
│   ├── QrTransfer.vue   # 主组件（编码/解码/文本三 Tab，从主前端原样迁入）
│   ├── api.ts           # /api/v1/qr/* 端点封装（底层走宿主桥 HTTP 层）
│   ├── clipboard.ts     # 剪贴板工具（兼容 HTTP 非安全上下文，原样迁入）
│   ├── icon.ts          # QR 码图标（SVG 内部标记）
│   └── i18n/            # 四语言 chrome 文案（zh-CN/zh-TW/en-US/ja-JP，键 qr.*）
├── standalone/
│   ├── standalone.html  # 独立全页入口（构建后并入 web/，经 /apps-assets 服务）
│   └── standalone-host.ts     # 独立运行宿主（宿主桥自给自足，见下）
└── web/                 # 发布产物（构建后并入；/apps-assets/qrtransfer/ 即此目录）
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
插件把 `import ... from 'vue' / 'vue-i18n'` 重写到宿主桥
`globalThis.__NEXOS_HOST__.vue / .vueI18n`（主前端 `appRuntime.ts` 注入），
保证与宿主共享同一份 Vue 实例（响应式系统 / `useI18n` 注入正常）。

## 后端端点（`/api/v1/qr/*`，QrTransferRouteHandler）

| 端点 | 用途 |
| --- | --- |
| `POST /qr/encode` + `GET /qr/encode/:id` + `GET /qr/encode/:id/video` | 文件 → QR 视频（任务制）+ 视频下载/流式播放 |
| `POST /qr/decode` + `GET /qr/decode/:id` + `GET /qr/decode/:id/file` | QR 视频/图片 → 文件（任务制）+ 结果下载 |
| `GET /qr/stats` | 编解码统计 |
| `POST /qr/encode-text` + `POST /qr/decode-text` | 文本 ⇄ QR 图片（即时，多码自动分块） |

引擎随 os-api 编译、端点常开（本应用未做引擎门控）；主前端内唯一跨应用
消费方 BleHub（mesh 连接 QR）直连同端点。

## 独立全页运行（standalone）

- 入口：`/apps-assets/qrtransfer/standalone.html`（新浏览器标签页直接打开）；
  应用右上角的外链图标按钮（嵌入模式显示、独立模式隐藏，键 `qr.openStandalone`
  ×4 语言）即 `window.open` 此地址。
- 宿主自给自足：`standalone/standalone-host.ts` 就是真宿主——
  `vite.standalone.config.ts` 构建（**不挂 host-externals**）把
  vue + vue-i18n 完整打进 `web/standalone-host.js`（应用包完全自包含，
  内网离线可跑，不引 CDN），然后置 `window.__NEXOS_STANDALONE__ = true`、
  以极简 ctx 适配器调 `register`；api 原语与主前端 client 语义对齐
  （Bearer token 同 key `os-api-token` 共享、15s 超时、401/403 弹 token 条重试）。
- 能力徽章：本应用无重能力依赖（QR 编解码全在服务端本地），不强塞 SDK 置灰
  （apps/film 0.1.3 范式按需采纳——qr 无 ffmpeg/LLM 依赖，不显示徽章）。

## 构建方法

```bash
cd apps/qrtransfer
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

- 发布：本仓库推送到 NexHub（`nexos-app-qrtransfer`，分支 `main`，tag `v0.1.0`），
  源码与 web/ 产物同仓同步提交（仓库根=应用包根）。
- 一键发布（推荐，v0.1.34 起）：主仓 `./tools/publish-app.sh qrtransfer --patch`
  —— 构建 → 同步发布仓（根=包根）→ commit/tag/push → 触发 CI → 安装/升级全链。
- 安装：NexOS 桌面 → 应用中心 → 商店 → 安装
  （`POST /api/v1/apps/install {repo:"nexos-app-qrtransfer"}`）。
- 后端 `GET /apps-assets/qrtransfer/web/entry.js` 提供静态资源；前端运行时
  `dynamic import` + `register(ctx)` 热注册，免刷新桌面可见。

## 版本记录

| 版本 | 日期 | 说明 |
| --- | --- | --- |
| 0.1.0 | 2026-09-05 | 自主前端剥离首发：QrTransfer.vue / client.ts qr 段端点 / 图标 / 路由迁入；i18n chrome（qr.* ×4）+ 独立运行外链（照 film 0.1.2 范式）；standalone 宿主。 |
