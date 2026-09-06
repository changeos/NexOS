/* ============================================================
   Vue Router — OS System 路由表
   / → Dashboard, /storage, /vms, /shares, /users, /nodes, /settings
   所有页面共享 MainLayout（Ubuntu Yaru 风窗口化布局）。
   应用路径直接访问时由下方 beforeEach 守卫重定向到 /?app=<id>
   （桌面打开对应浮窗，不再全屏）。
   /s/<appId> → 内置应用「独立全屏」路由（STANDALONE_APPS 映射表，
   桌面壳之外全屏渲染，见下方说明块）。
   ============================================================ */

import { createRouter, createWebHistory, type RouteRecordRaw } from 'vue-router'
import MainLayout from '@/layouts/MainLayout.vue'
import { appRegistry, desktopApps, findApp } from '@/appRegistry'
import type { Component } from 'vue'

// ============================================================
// 内置应用「独立全屏」注册表（/s/<appId> —— 桌面壳之外）
//
// 复刻 apps/film 的「右上角外链 → 新标签页独立打开」交互，但 NexHub 等
// 内置应用无独立包 / standalone.html，改为走内置应用全屏路由：映射表里
// 登记一行（appId → 视图组件懒加载器），下方 routes 即自动展开出顶层
// 记录 /s/<appId>（MainLayout 之外渲染，无 Dock / Launchpad / 窗口框）。
// 同源 localStorage（os-api-token 等）与桌面共享，独立页内 API 天然可用。
// 将来其它内置应用独立化：在 STANDALONE_APPS 加一行即可复用整条链路。
//
// v0.1.32 children 扩展：条目值除组件加载器外，还可用 { component, children }
// 形态声明子路由（children 挂在该顶层记录下，渲染于应用视图内的
// <RouterView>）——NexHub 用其承载仓库/Tab 深链（/s/codehub/r/:name 等），
// 刷新不丢状态。未登记的 /s/* 兜底回首页（同既有机制）；/s/ 前缀路径不进
// 桌面浮窗重定向守卫（routeAppIds 仅登记桌面路径，天然不拦）。
// ============================================================
/** standalone 应用条目：组件加载器，或 { component, children }（子路由深链）。 */
type StandaloneAppEntry =
    | (() => Promise<Component>)
    | {
          component: () => Promise<Component>
          children?: RouteRecordRaw[]
      }

function standaloneComponent(entry: StandaloneAppEntry): () => Promise<Component> {
    return typeof entry === 'function' ? entry : entry.component
}

function standaloneChildren(entry: StandaloneAppEntry): RouteRecordRaw[] {
    return typeof entry === 'function' ? [] : (entry.children ?? [])
}

const STANDALONE_APPS: Record<string, StandaloneAppEntry> = {
    codehub: {
        component: () => import('@/views/CodeHub.vue'),
        // NexHub 网页化子路由（v0.1.32）：Tab/仓库状态进 URL，刷新不丢。
        children: [
            {
                path: '',
                name: 'standalone-codehub-home',
                component: () => import('@/views/nexhub/views/RepoExplorePage.vue'),
                meta: { title: 'NexHub' },
            },
            {
                path: 'lobby',
                name: 'standalone-codehub-lobby',
                component: () => import('@/views/nexhub/views/LobbyPage.vue'),
                meta: { title: 'NexHub Lobby' },
            },
            {
                path: 'sessions',
                name: 'standalone-codehub-sessions',
                component: () => import('@/views/nexhub/views/SessionsPage.vue'),
                meta: { title: 'NexHub Sessions' },
            },
            {
                path: 'onboarding',
                name: 'standalone-codehub-onboarding',
                component: () => import('@/views/nexhub/views/OnboardingPage.vue'),
                meta: { title: 'NexHub Onboarding' },
            },
            {
                // 仓库详情（Code/Commits/Manifest/Issues/PR 内嵌 Tab 经 ?tab= 深链）
                path: 'r/:name',
                name: 'standalone-codehub-repo',
                component: () => import('@/views/nexhub/views/RepoDetailPage.vue'),
                props: true,
                meta: { title: 'NexHub Repo' },
            },
        ],
    },
}

const routes: RouteRecordRaw[] = [
    {
        path: '/',
        // 命名父路由：运行时应用包经 addAppRoute() 挂到该父记录下（子路由）。
        name: 'layout',
        component: MainLayout,
        children: [
            {
                path: '',
                name: 'dashboard',
                component: () => import('@/views/DashboardView.vue'),
                meta: { title: 'Dashboard', icon: 'dashboard' },
            },
            {
                path: 'storage',
                name: 'storage',
                component: () => import('@/views/Storage.vue'),
                meta: { title: '存储', icon: 'storage' },
            },
            {
                path: 'vms',
                name: 'vms',
                component: () => import('@/views/Vms.vue'),
                meta: { title: '虚拟机', icon: 'vm' },
            },
            {
                path: 'shares',
                name: 'shares',
                component: () => import('@/views/Shares.vue'),
                meta: { title: '共享', icon: 'share' },
            },
            {
                path: 'users',
                name: 'users',
                component: () => import('@/views/Users.vue'),
                meta: { title: '用户', icon: 'users' },
            },
            {
                path: 'chat',
                name: 'chat',
                component: () => import('@/views/Chat.vue'),
                meta: { title: '聊天', icon: 'chat' },
            },
            // 「模型对话」已并入「模型管理」(/llm) 的「对话」Tab；旧链接重定向不断
            {
                path: 'modelchat',
                redirect: '/llm',
            },
            {
                path: 'nodes',
                name: 'nodes',
                component: () => import('@/views/Nodes.vue'),
                meta: { title: '节点', icon: 'nodes' },
            },
            {
                path: 'network',
                name: 'network',
                component: () => import('@/views/Network.vue'),
                meta: { title: '网络', icon: 'network' },
            },
            // 「BLE mesh 中继」已并入「网络管理」(/network) 的「BLE Mesh 中继」Tab；旧链接重定向不断
            {
                path: 'blehub',
                redirect: '/network',
            },
            {
                path: 'provisioning',
                name: 'provisioning',
                component: () => import('@/views/Provisioning.vue'),
                meta: { title: '系统自举', icon: 'provisioning' },
            },
            {
                path: 'update',
                name: 'update',
                component: () => import('@/views/Update.vue'),
                meta: { title: '更新', icon: 'update' },
            },
            {
                path: 'forwarding',
                name: 'forwarding',
                component: () => import('@/views/Forwarding.vue'),
                meta: { title: '远程转发', icon: 'forwarding' },
            },
            // 「直播」已随流媒体中心剥离为独立应用包 apps/streaming（安装后经
            // addRoute 注册 /streaming 子路由 + /?app=streaming 守卫映射；未安装
            // 时旧 /live 链接经兜底回首页）
            // 「管理」桌面应用：Web 终端（本地 shell / SSH 远程，xterm.js ↔ WS ↔ PTY）
            {
                path: 'terminal',
                name: 'terminal',
                component: () => import('@/views/AdminConsole.vue'),
                meta: { title: '管理', icon: 'terminal' },
            },
            {
                path: 'video',
                name: 'video',
                component: () => import('@/views/Video.vue'),
                meta: { title: '影院', icon: 'video' },
            },
            {
                path: 'music',
                name: 'music',
                component: () => import('@/views/Music.vue'),
                meta: { title: '音乐', icon: 'music' },
            },
            {
                path: 'photo',
                name: 'photo',
                component: () => import('@/views/Photo.vue'),
                meta: { title: '相册', icon: 'photo' },
            },
            // 影片制作已剥离为独立应用包（apps/ 目录 → NexHub 仓库）：路由 / 注册表 /
            // 图标 / i18n 由应用包安装后运行时注册（appRuntime.registerApp+addRoute）。
            {
                path: 'backup',
                name: 'backup',
                component: () => import('@/views/Backup.vue'),
                meta: { title: '备份', icon: 'backup' },
            },
            {
                path: 'monitor',
                name: 'monitor',
                component: () => import('@/views/Monitor.vue'),
                meta: { title: '监控', icon: 'monitor' },
            },
            {
                path: 'files',
                name: 'files',
                component: () => import('@/views/Files.vue'),
                meta: { title: '文件管理', icon: 'files' },
            },
            {
                path: 'downloads',
                name: 'downloads',
                component: () => import('@/views/Downloads.vue'),
                meta: { title: '下载中心', icon: 'downloads' },
            },
            {
                path: 'containers',
                name: 'containers',
                component: () => import('@/views/Containers.vue'),
                meta: { title: '容器管理', icon: 'containers' },
            },
            {
                path: 'surveillance',
                name: 'surveillance',
                component: () => import('@/views/Surveillance.vue'),
                meta: { title: '监控摄像头', icon: 'surveillance' },
            },
            {
                path: 'cloudsync',
                name: 'cloudsync',
                component: () => import('@/views/CloudSync.vue'),
                meta: { title: '云同步', icon: 'cloudsync' },
            },
            {
                path: 'notes',
                name: 'notes',
                component: () => import('@/views/Notes.vue'),
                meta: { title: '笔记', icon: 'notes' },
            },
            // 流媒体中心已剥离为独立应用包（apps/streaming → NexHub 仓库）：路由 /
            // 注册表 / 图标 / i18n 由应用包安装后运行时注册（appRuntime.registerApp+addRoute）。
            {
                path: 'llm',
                name: 'llm',
                component: () => import('@/views/LlmModels.vue'),
                meta: { title: '模型管理', icon: 'llm' },
            },
            {
                path: 'gateway',
                name: 'gateway',
                component: () => import('@/views/ApiGateway.vue'),
                meta: { title: 'API 网关', icon: 'gateway' },
            },
            {
                path: 'blockchain',
                name: 'blockchain',
                component: () => import('@/views/Blockchain.vue'),
                meta: { title: '区块链管理', icon: 'blockchain' },
            },
            // 「模型仓库」已并入「模型管理」(/llm) 的一级分组「仓库」（ModelHubPanel：
            // 本地模型/在线下载/模型大厅/Spark 专区）；旧链接重定向不断（带 tab
            // 深链直达仓库分组）
            {
                path: 'modelhub',
                redirect: { path: '/llm', query: { tab: 'repo' } },
            },
            {
                path: 'appstore',
                name: 'appstore',
                component: () => import('@/views/AppStore.vue'),
                meta: { title: '应用中心', icon: 'appstore' },
            },
            {
                path: 'agenthub',
                name: 'agenthub',
                component: () => import('@/views/AgentHub.vue'),
                meta: { title: 'Agent 集合', icon: 'agenthub' },
            },
            // 二维码传输已剥离为独立应用包（apps/qrtransfer → NexHub 仓库）：路由 /
            // 注册表 / 图标 / i18n 由应用包安装后运行时注册（appRuntime.registerApp+addRoute）。
            {
                path: 'codehub',
                name: 'codehub',
                component: () => import('@/views/CodeHub.vue'),
                meta: { title: 'NexHub', icon: 'codehub' },
            },
            {
                path: 'devdocs',
                name: 'devdocs',
                component: () => import('@/views/DevDocs.vue'),
                meta: { title: '开发者中心', icon: 'devdocs' },
            },
            {
                path: 'settings',
                name: 'settings',
                component: () => import('@/views/Settings.vue'),
                meta: { title: '设置', icon: 'settings' },
            },
        ],
    },
    // 内置应用独立全屏路由：由 STANDALONE_APPS 展开（/s/<appId> 字面路径，
    // 顶层记录——不在 'layout' 父级之下，独立页全屏渲染应用视图本身）。
    // children 扩展：声明的子路由挂为该顶层记录的 children（应用视图内
    // RouterView 渲染，承载 /s/codehub/r/:name 等深链）。
    // 未登记的 /s/xxx 不匹配任何记录，落入兜底重定向回桌面首页。
    ...Object.entries(STANDALONE_APPS).map(([appId, entry]) => ({
        path: `/s/${appId}`,
        name: `standalone-${appId}`,
        component: standaloneComponent(entry),
        children: standaloneChildren(entry),
        meta: { title: findApp(appId)?.label ?? appId },
    })),
    // 兜底：未知路径回首页
    { path: '/:pathMatch(.*)*', redirect: '/' },
]

const router = createRouter({
    history: createWebHistory(),
    routes,
})

// ============================================================
// 应用路径 → 桌面浮窗重定向守卫
//
// 直接 URL 访问 /chat、/storage、/settings 等应用路径时不再进入
// MainLayout 的全屏 fallback（整页全屏、无窗口管理，用户被"困住"），
// 而是重定向到桌面根并携带 ?app=<id>；MainLayout 监听到该参数后
// 自动打开对应浮窗并清除参数（见 MainLayout.vue 的 watch）。
//
// route → app id 映射取自 desktopApps（/vms→vm、/shares→share 等
// 非一一对应的路径也能正确映射）；appRegistry 未注册的应用不重定向，
// 继续走子路由全屏 fallback（保留"返回桌面"按钮兜底）。
// 运行时应用包注册路由时经 addAppRoute() 把映射补充进来（守卫读 live Map）。
// ============================================================
const routeAppIds = new Map<string, string>()
for (const app of desktopApps.value) {
    if (appRegistry[app.id]) routeAppIds.set(app.route, app.id)
}

/**
 * 运行时应用包注册路由（appRuntime.addRoute 调用）：
 * - 挂为 'layout' 父路由的子路由（path 归一为相对段，'/' 前缀剥离）；
 * - 登记重定向映射（直接 URL → /?app=<id> 桌面浮窗，与内置应用一致）；
 * - 返回路由名（卸载时 router.removeRoute 用）；入参非法返回 ''。
 */
export function addAppRoute(decl: {
    path: string
    name?: string
    component: Component
    appId: string
}): string {
    if (!decl || typeof decl.path !== 'string' || decl.path === '') return ''
    const childPath = decl.path.replace(/^\/+/, '')
    if (childPath === '') return ''
    const name = decl.name || `app-${decl.appId}`
    try {
        router.addRoute('layout', {
            path: childPath,
            name,
            component: decl.component,
            meta: { title: decl.appId },
        })
    } catch {
        return ''
    }
    routeAppIds.set(`/${childPath}`, decl.appId)
    return name
}

router.beforeEach((to) => {
    if (to.path === '/') return true
    const appId = routeAppIds.get(to.path)
    if (appId) return { path: '/', query: { ...to.query, app: appId } }
    return true
})

// 设置文档标题
router.afterEach((to) => {
    const title = (to.meta?.title as string) || 'OS'
    document.title = `${title} · NexOS`
})

export default router
