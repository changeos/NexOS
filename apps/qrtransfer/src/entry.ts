// =============================================================================
// entry.ts —— 应用包单入口（ESM default export = register(ctx)，协议冻结）。
//
// 宿主（crates/os-api/web/src/appRuntime.ts）在以下时点 dynamic import 本文件
// 并调用 register(ctx)：
//   - 桌面启动：GET /api/v1/apps → 逐应用 import /apps-assets/:id/web/entry.js
//   - 应用中心安装成功后的热注册（免刷新）
//
// ctx 契约：
//   ctx.registerApp({id,label,icon,route,category,gradient?,component?})
//   ctx.addRoute({path,name?,component})
//   ctx.addI18n(locale, messages)
//   ctx.api —— 宿主 api client 原语（本包 src/api.ts 消费，不重复实现 HTTP 层）
//
// 本包自带全部资源（组件 / i18n 四语言 / 图标），不依赖主前端内部模块；
// vue / vue-i18n 构建期重写到宿主桥（见 vite.config.ts host-externals）。
// =============================================================================
import QrTransfer from './QrTransfer.vue'
import { QRTRANSFER_ICON } from './icon'
import zhCN from './i18n/zh-CN.json'
import zhTW from './i18n/zh-TW.json'
import enUS from './i18n/en-US.json'
import jaJP from './i18n/ja-JP.json'

/** register 上下文（宿主实现；此处仅类型声明，保持包独立编译）。 */
interface AppCtx {
  registerApp(app: {
    id: string
    label: string
    icon?: string
    route?: string
    category?: string
    gradient?: string
    component?: unknown
  }): void
  addRoute(route: { path: string; name?: string; component: unknown }): void
  addI18n(locale: string, messages: Record<string, unknown>): void
  api: unknown
}

export default function register(ctx: AppCtx): void {
  // 1. 注册桌面应用：窗口组件 + 图标 + 渐变 + 分类（启动台「开发者工具」分组，
  //    与剥离前主前端 APP_CATEGORY.qrtransfer='devtools' 一致）。
  ctx.registerApp({
    id: 'qrtransfer',
    label: '二维码传输',
    icon: QRTRANSFER_ICON,
    route: '/qrtransfer',
    category: 'devtools',
    gradient: 'linear-gradient(135deg, #2C001E 0%, #772953 100%)',
    component: QrTransfer,
  })
  // 2. 注册子路由（直接 URL /qrtransfer → 守卫重定向 /?app=qrtransfer 开浮窗，
  //    与内置应用一致）。
  ctx.addRoute({ path: 'qrtransfer', name: 'qrtransfer', component: QrTransfer })
  // 3. 四语言全量注入（键名 qr.*，宿主 mergeLocaleMessage 合并）。
  ctx.addI18n('zh-CN', zhCN)
  ctx.addI18n('zh-TW', zhTW)
  ctx.addI18n('en-US', enUS)
  ctx.addI18n('ja-JP', jaJP)
}
