// =============================================================================
// appRegistry —— 桌面应用 -> Vue 组件 的映射表 + 运行时注册源。
//
// 两类应用来源：
//   1. 内置应用：静态声明（builtinComponents / builtinApps），随主前端打包；
//   2. 应用包：运行时注册（appRuntime.ts 从 /apps-assets/:id/ 动态 import
//      entry.js 后调 register(ctx) 注入，见 src/appRuntime.ts）。
//
// 暴露形态（对既有消费面零回归）：
//   - appRegistry：key -> 异步视图组件。shallowReactive——运行时应用注册/注销时
//     模板里的 appRegistry[win.id] 读取自动跟随更新（值本身保持原始组件对象，
//     不做深层代理，避免包裹组件对象）。
//   - desktopApps：computed（内置在前 + 运行时在后）。消费方（Launchpad /
//     useDockLayout / router）以 .value 读取。
//   - findApp / getAppName：均读 computed，响应式。
//
// WindowFrame 渲染时：<component :is="appRegistry[win.id]" />
// 路由路径 / 视图文件对应关系请与 router/index.ts 保持同步。
// =============================================================================
import { computed, defineAsyncComponent, reactive, shallowReactive, type Component } from 'vue'

/** 应用 key -> 异步视图组件。key 与 desktopApps[].id 一致（内置应用历史上与
 *  DashboardView 的 app.icon 一致）。运行时可增删（应用包装载/卸载）。 */
export const appRegistry: Record<string, Component> = shallowReactive<
    Record<string, Component>
>({
    storage: defineAsyncComponent(() => import('@/views/Storage.vue')),
    vm: defineAsyncComponent(() => import('@/views/Vms.vue')),
    share: defineAsyncComponent(() => import('@/views/Shares.vue')),
    users: defineAsyncComponent(() => import('@/views/Users.vue')),
    nodes: defineAsyncComponent(() => import('@/views/Nodes.vue')),
    chat: defineAsyncComponent(() => import('@/views/Chat.vue')),
    // modelchat（模型对话）已并入「模型管理」(/llm) 的「对话」Tab，独立窗口/桌面图标移除
    // blehub（BLE mesh 中继，原 IM 应用内 Tab）已并入「网络管理」(/network) 的「BLE Mesh 中继」Tab
    network: defineAsyncComponent(() => import('@/views/Network.vue')),
    provisioning: defineAsyncComponent(() => import('@/views/Provisioning.vue')),
    update: defineAsyncComponent(() => import('@/views/Update.vue')),
    video: defineAsyncComponent(() => import('@/views/Video.vue')),
    music: defineAsyncComponent(() => import('@/views/Music.vue')),
    photo: defineAsyncComponent(() => import('@/views/Photo.vue')),
    // 影片制作已剥离为独立应用包（apps/ 目录，NexHub 仓库）：从应用中心
    // 安装后运行时注册，主前端不再内置。
    backup: defineAsyncComponent(() => import('@/views/Backup.vue')),
    monitor: defineAsyncComponent(() => import('@/views/Monitor.vue')),
    files: defineAsyncComponent(() => import('@/views/Files.vue')),
    downloads: defineAsyncComponent(() => import('@/views/Downloads.vue')),
    containers: defineAsyncComponent(() => import('@/views/Containers.vue')),
    surveillance: defineAsyncComponent(() => import('@/views/Surveillance.vue')),
    cloudsync: defineAsyncComponent(() => import('@/views/CloudSync.vue')),
    notes: defineAsyncComponent(() => import('@/views/Notes.vue')),
    // 流媒体中心已剥离为独立应用包（apps/streaming，含「直播」Tab 的 LivePanel）：
    // 从应用中心安装后运行时注册，主前端不再内置。
    llm: defineAsyncComponent(() => import('@/views/LlmModels.vue')),
    gateway: defineAsyncComponent(() => import('@/views/ApiGateway.vue')),
    blockchain: defineAsyncComponent(() => import('@/views/Blockchain.vue')),
    // modelhub（模型仓库）已并入「模型管理」(/llm) 的一级分组「仓库」（ModelHubPanel
    // 组件：本地模型/在线下载/模型大厅/Spark 专区），独立窗口/桌面图标移除
    appstore: defineAsyncComponent(() => import('@/views/AppStore.vue')),
    agenthub: defineAsyncComponent(() => import('@/views/AgentHub.vue')),
    // 二维码传输已剥离为独立应用包（apps/qrtransfer）：从应用中心安装后运行时注册。
    codehub: defineAsyncComponent(() => import('@/views/CodeHub.vue')),
    devdocs: defineAsyncComponent(() => import('@/views/DevDocs.vue')),
    forwarding: defineAsyncComponent(() => import('@/views/Forwarding.vue')),
    // live（直播）与流媒体中心一并剥离为独立应用包 apps/streaming（「直播」Tab，
    // LivePanel 组件：本地大厅 + 联邦大厅）；/api/v1/live/* 端点后端常开不门控。
    // terminal（管理）：Web 终端——本地 shell + SSH 远程终端（AdminConsole.vue，
    // xterm.js ↔ WS ↔ PTY，docs/ADMIN_CONSOLE.md）。与设置（settings）互不隶属。
    terminal: defineAsyncComponent(() => import('@/views/AdminConsole.vue')),
    settings: defineAsyncComponent(() => import('@/views/Settings.vue')),
})

/**
 * 桌面应用元信息：用于 Dock 栏 / 桌面图标 / 启动台的统一配置。
 * 从 DashboardView 的 apps 数组提炼，避免散落两处。
 */
export interface AppMeta {
    /** 唯一 key，同时作为窗口 id，且为 appRegistry 的 key */
    id: string
    /** 窗口标题 / 图标标签 */
    label: string
    /** 图标名（用于 AppIcon；与 DashboardView 内联 SVG 的 app.icon 一致） */
    icon: string
    /** 图标背景渐变（与 DashboardView 一致） */
    gradient: string
    /** 对应路由路径（兼容直接 URL 访问） */
    route: string
}

/**
 * 运行时注册的应用（应用包）元信息：AppMeta 之上加业务域分类与来源信息。
 * 由 appRuntime.ts 在 register(ctx) 时写入。
 */
export interface RuntimeAppMeta extends AppMeta {
    /** 业务域分类（Launchpad 分组；归一到 AppCategory）。 */
    category: AppCategory
    /** 来源：应用包（区别于内置应用）。 */
    runtime: true
}

/** 全部内置桌面应用（Dock 栏 + 桌面图标共用）。 */
export const builtinApps: AppMeta[] = [
    {
        id: 'storage',
        label: '存储管理',
        icon: 'storage',
        gradient: 'linear-gradient(135deg, #E95420 0%, #2C001E 100%)',
        route: '/storage',
    },
    {
        id: 'vm',
        label: '虚拟机',
        icon: 'vm',
        gradient: 'linear-gradient(135deg, #5e5ce6 0%, #8e8ef0 100%)',
        route: '/vms',
    },
    {
        id: 'share',
        label: '文件共享',
        icon: 'share',
        gradient: 'linear-gradient(135deg, #0E8420 0%, #6ee08a 100%)',
        route: '/shares',
    },
    {
        id: 'users',
        label: '用户管理',
        icon: 'users',
        gradient: 'linear-gradient(135deg, #F99B11 0%, #ffc56b 100%)',
        route: '/users',
    },
    {
        id: 'nodes',
        label: '联邦节点',
        icon: 'nodes',
        gradient: 'linear-gradient(135deg, #bf5af2 0%, #d99bff 100%)',
        route: '/nodes',
    },
    {
        id: 'chat',
        label: 'IM',
        icon: 'chat',
        gradient: 'linear-gradient(135deg, #36d1dc 0%, #5b86e5 100%)',
        route: '/chat',
    },
    // modelchat（模型对话）已并入 llm（模型管理）「对话」Tab，不再单列桌面应用
    {
        id: 'network',
        label: '网络管理',
        icon: 'network',
        gradient: 'linear-gradient(135deg, #30b0c7 0%, #66d6e7 100%)',
        route: '/network',
    },
    {
        id: 'provisioning',
        label: '系统自举',
        icon: 'provisioning',
        gradient: 'linear-gradient(135deg, #64d2ff 0%, #a3e3ff 100%)',
        route: '/provisioning',
    },
    {
        id: 'update',
        label: '更新',
        icon: 'update',
        gradient: 'linear-gradient(135deg, #f7971e 0%, #ffd200 100%)',
        route: '/update',
    },
    {
        id: 'backup',
        label: '备份管理',
        icon: 'backup',
        gradient: 'linear-gradient(135deg, #5ac8fa 0%, #34aadc 100%)',
        route: '/backup',
    },
    {
        id: 'monitor',
        label: '系统监控',
        icon: 'monitor',
        gradient: 'linear-gradient(135deg, #ff375f 0%, #ff7a93 100%)',
        route: '/monitor',
    },
    {
        id: 'files',
        label: '文件管理',
        icon: 'files',
        gradient: 'linear-gradient(135deg, #fa709a 0%, #fee140 100%)',
        route: '/files',
    },
    {
        id: 'downloads',
        label: '下载中心',
        icon: 'downloads',
        gradient: 'linear-gradient(135deg, #30cfd0 0%, #330867 100%)',
        route: '/downloads',
    },
    {
        id: 'containers',
        label: '容器管理',
        icon: 'containers',
        gradient: 'linear-gradient(135deg, #43e97b 0%, #38f9d7 100%)',
        route: '/containers',
    },
    {
        id: 'surveillance',
        label: '监控摄像头',
        icon: 'surveillance',
        gradient: 'linear-gradient(135deg, #5ee7df 0%, #b490ca 100%)',
        route: '/surveillance',
    },
    {
        id: 'cloudsync',
        label: '云同步',
        icon: 'cloudsync',
        gradient: 'linear-gradient(135deg, #84fab0 0%, #8fd3f4 100%)',
        route: '/cloudsync',
    },
    {
        id: 'notes',
        label: '笔记',
        icon: 'notes',
        gradient: 'linear-gradient(135deg, #ffecd2 0%, #fcb69f 100%)',
        route: '/notes',
    },
    // 流媒体中心已剥离为独立应用包（apps/streaming，含「直播」Tab）：安装后经
    // runtimeApps 动态出现在桌面/启动台，不再内置。
    {
        id: 'llm',
        label: '模型管理',
        icon: 'llm',
        gradient: 'linear-gradient(135deg, #a8edea 0%, #fed6e3 100%)',
        route: '/llm',
    },
    {
        id: 'gateway',
        label: 'API 网关',
        icon: 'gateway',
        gradient: 'linear-gradient(135deg, #772953, #E95420)',
        route: '/gateway',
    },
    {
        id: 'blockchain',
        label: '区块链管理',
        icon: 'blockchain',
        gradient: 'linear-gradient(135deg, #2C001E, #772953)',
        route: '/blockchain',
    },
    // modelhub（模型仓库）已并入 llm（模型管理）一级分组「仓库」，不再单列桌面应用
    {
        id: 'appstore',
        label: '应用中心',
        icon: 'appstore',
        gradient: 'linear-gradient(135deg, #E95420, #F99B11)',
        route: '/appstore',
    },
    {
        id: 'agenthub',
        label: 'Agent 集合',
        icon: 'agenthub',
        gradient: 'linear-gradient(135deg, #4f46e5 0%, #06b6d4 100%)',
        route: '/agenthub',
    },
    // 二维码传输已剥离为独立应用包（apps/qrtransfer）：安装后经 runtimeApps
    // 动态出现在桌面/启动台，不再内置。
    {
        id: 'codehub',
        label: 'NexHub',
        icon: 'codehub',
        gradient: 'linear-gradient(135deg, #24292e, #586069)',
        route: '/codehub',
    },
    {
        id: 'devdocs',
        label: '开发者中心',
        icon: 'devdocs',
        gradient: 'linear-gradient(135deg, #141e30, #243b55)',
        route: '/devdocs',
    },
    {
        id: 'forwarding',
        label: '远程转发',
        icon: 'forwarding',
        gradient: 'linear-gradient(135deg, #0d9488, #14b8a6)',
        route: '/forwarding',
    },
    // live（直播）注释条目：live 已随流媒体中心剥离为应用包 apps/streaming
    {
        id: 'terminal',
        label: '管理',
        icon: 'terminal',
        gradient: 'linear-gradient(135deg, #1f2933 0%, #3b4252 100%)',
        route: '/terminal',
    },
    {
        id: 'video',
        label: '影院',
        icon: 'video',
        gradient: 'linear-gradient(135deg, #667eea 0%, #764ba2 100%)',
        route: '/video',
    },
    {
        id: 'music',
        label: '音乐',
        icon: 'music',
        gradient: 'linear-gradient(135deg, #f093fb 0%, #f5576c 100%)',
        route: '/music',
    },
    {
        id: 'photo',
        label: '相册',
        icon: 'photo',
        gradient: 'linear-gradient(135deg, #4facfe 0%, #00f2fe 100%)',
        route: '/photo',
    },
    // 影片制作已剥离为独立应用包：从应用中心安装后动态出现在桌面/启动台。
    {
        id: 'settings',
        label: '设置',
        icon: 'settings',
        gradient: 'linear-gradient(135deg, #5E5C5F, #878789)',
        route: '/settings',
    },
]

// =============================================================================
// 运行时注册源（应用包）——由 src/appRuntime.ts 写入，本文件只维护容器与合并视图。
// =============================================================================

/** 已注册的运行时应用（应用包）。shallowReactive：增删项触发消费方更新。 */
export const runtimeApps = shallowReactive<RuntimeAppMeta[]>([])

/** 运行时图标库：图标名 -> SVG 内部标记（路径/形状字符串）。
 *  AppIcon 渲染时 builtin ICONS 优先，找不到再查这里（应用包自带图标）。 */
export const runtimeIcons = shallowReactive<Record<string, string>>({})

/** 合并视图：内置应用（静态，顺序稳定）+ 运行时应用（应用包）。 */
export const desktopApps = computed<AppMeta[]>(() => [
    ...builtinApps,
    ...runtimeApps.map((a) => ({
        id: a.id,
        label: a.label,
        icon: a.icon,
        gradient: a.gradient,
        route: a.route,
    })),
])

/** 通过 id 查找应用元信息（内置 + 运行时）。 */
export function findApp(id: string): AppMeta | undefined {
    return desktopApps.value.find((a) => a.id === id)
}

/** 应用业务域分类（用于 Launchpad 分组）。 */
export type AppCategory =
    | 'backup' // 备份
    | 'monitor' // 监控
    | 'media' // 媒体
    | 'security' // 安全
    | 'files' // 文件
    | 'devtools' // 开发者工具
    | 'power' // 电源 / 基础设施
    | 'system' // 系统 / 设置

/** 内置 id -> 业务域（Launchpad 分组依据）。 */
export const APP_CATEGORY: Record<string, AppCategory> = {
    storage: 'files', vm: 'devtools', share: 'files', users: 'security', nodes: 'power',
    chat: 'system', network: 'devtools', provisioning: 'power', update: 'power', forwarding: 'devtools',
    terminal: 'system',
    backup: 'backup', monitor: 'monitor', files: 'files', downloads: 'files',
    containers: 'devtools', surveillance: 'monitor', cloudsync: 'files', notes: 'system',
    // streaming/qrtransfer 已剥离为应用包：分类随注册声明（media/devtools），不在内置表
    llm: 'devtools', gateway: 'devtools', blockchain: 'devtools',
    appstore: 'system', agenthub: 'devtools', codehub: 'devtools',
    devdocs: 'devtools', video: 'media', music: 'media', photo: 'media',
    settings: 'system',
}

/** 合并分类查询：内置查表；运行时应用读注册时声明的 category（归一，未知归 system）。 */
export function getAppCategory(id: string): AppCategory {
    const builtin = APP_CATEGORY[id]
    if (builtin) return builtin
    const rt = runtimeApps.find((a) => a.id === id)
    if (rt) {
        const known = CATEGORY_META.some((c) => c.key === rt.category)
        return known ? rt.category : 'system'
    }
    return 'system'
}

/** 业务域展示顺序与中文名（Launchpad 表头）。 */
export const CATEGORY_META: { key: AppCategory; label: string }[] = [
    { key: 'backup', label: '备份' },
    { key: 'monitor', label: '监控' },
    { key: 'media', label: '媒体' },
    { key: 'security', label: '安全' },
    { key: 'files', label: '文件' },
    { key: 'devtools', label: '开发者工具' },
    { key: 'power', label: '电源 / 基础设施' },
    { key: 'system', label: '系统' },
]

// =============================================================================
// 应用改名（自定义名称，localStorage 持久化）
//
// 存储格式：{ storage: '存储管理', vm: '虚拟机2.0', ... }
// 所有显示应用名的地方（桌面图标 / Dock / 窗口标题 / 文件夹内标签）统一走
// getAppName(id)，reactive 让改名后各处自动跟随更新。
// =============================================================================

/** localStorage 持久化 key。 */
const CUSTOM_NAMES_KEY = 'os-app-names'

/** appId -> 自定义名称（reactive 单例，模块加载即共享）。 */
const customNames = reactive<Record<string, string>>({})

/** 是否已从 localStorage 初始化（避免重复加载覆盖运行时改动）。 */
let namesInitialized = false

/** 安全读 localStorage（解析失败静默回退默认名）。 */
function ensureNamesInit(): void {
    if (namesInitialized) return
    namesInitialized = true
    try {
        const raw = localStorage.getItem(CUSTOM_NAMES_KEY)
        if (!raw) return
        const parsed = JSON.parse(raw) as unknown
        if (typeof parsed !== 'object' || parsed === null) return
        for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
            if (typeof v === 'string' && v.trim() !== '') customNames[k] = v
        }
    } catch {
        // 解析失败：静默回退默认 label
    }
}

/** 写 localStorage（配额满 / 隐私模式静默失败）。 */
function persistNames(): void {
    try {
        localStorage.setItem(CUSTOM_NAMES_KEY, JSON.stringify(customNames))
    } catch {
        // 静默失败
    }
}

/** 获取应用显示名：自定义名 ?? 原始 label ?? id。 */
export function getAppName(id: string): string {
    ensureNamesInit()
    const custom = customNames[id]
    if (custom && custom.trim() !== '') return custom
    return findApp(id)?.label ?? id
}

/** 设置自定义名（空串/纯空白 = 恢复默认名）。 */
export function setAppName(id: string, name: string): void {
    ensureNamesInit()
    const trimmed = name.trim()
    if (trimmed === '') {
        resetAppName(id)
        return
    }
    customNames[id] = trimmed
    persistNames()
}

/** 删除自定义名，恢复默认 label。 */
export function resetAppName(id: string): void {
    ensureNamesInit()
    if (id in customNames) {
        delete customNames[id]
        persistNames()
    }
}

/** 是否设置了自定义名（用于右键菜单"恢复默认名称"项的显隐）。 */
export function hasCustomName(id: string): boolean {
    ensureNamesInit()
    return Boolean(customNames[id] && customNames[id].trim() !== '')
}

// =============================================================================
// 运行时注册表写入 API（供 appRuntime.ts 调用；组件不直接使用）
// =============================================================================

/**
 * 注销一个运行时应用（应用包卸载）：从注册表 / 图标库移除。
 * 不关窗口不清 Dock（调用方 appRuntime 负责——需要 wm / dockLayout 协作）。
 */
export function unregisterRuntimeApp(id: string): void {
    const idx = runtimeApps.findIndex((a) => a.id === id)
    if (idx >= 0) runtimeApps.splice(idx, 1)
    delete appRegistry[id]
    delete runtimeIcons[id]
}

/** 注册运行时图标（SVG 内部标记，AppIcon 24x24 stroke 风格）。 */
export function registerRuntimeIcon(name: string, svgInner: string): void {
    if (typeof svgInner === 'string' && svgInner.trim() !== '') {
        runtimeIcons[name] = svgInner
    }
}
