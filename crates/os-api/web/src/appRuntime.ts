// =============================================================================
// appRuntime —— 应用包运行时（docs/APPS.md 协议的实现侧）。
//
// 职责：
//   1. 宿主桥（host bridge）：在 window.__NEXOS_HOST__ 上暴露主前端的
//      vue / vue-i18n 模块命名空间 + api client 原语（get/post/del/request）
//      + @nexos/app-sdk 就绪实例（sdk——v0.1.28，应用能力面 SDK，docs/APPS.md
//      「应用 SDK」章；实例同时携带 createSdk 工厂面供构建期重写解构）。
//      应用包 entry.js 构建时把 `from 'vue'` / `from 'vue-i18n'` 重写到该桥
//      （应用包 vite.config.ts 的 host-externals 插件），从而：
//        - 应用包与宿主共享同一份 Vue（响应式系统 / getCurrentInstance 正常）；
//        - 应用包与宿主共享同一份 vue-i18n（useI18n / mergeLocaleMessage）；
//        - 应用包不重复实现 HTTP 层（鉴权 token / 超时 / ApiError 全走宿主）。
//   2. 启动装载：GET /api/v1/apps → 逐个 dynamic import(/apps-assets/:id/entry)
//      → 调 register(ctx)（10s 超时；失败记录占位卡，不崩桌面）。
//   3. 热装卸：安装成功后免刷新走一遍 import+register；卸载后从注册表 /
//      路由 / 窗口 / Dock 移除。
//
// register(ctx) 契约（冻结，docs/APPS.md）：
//   ctx.registerApp({id,label,icon,route,category,gradient?,component?})
//   ctx.addRoute({path,name?,component})
//   ctx.addI18n(locale, messages)
//   ctx.api = { get, post, del, request }（主前端 client 原语）
// =============================================================================
import * as vueNamespace from 'vue'
import * as vueI18nNamespace from 'vue-i18n'
import type { Component } from 'vue'
import { shallowReactive } from 'vue'
import i18n from '@/i18n'
import router, { addAppRoute } from '@/router'
import {
    appRegistry,
    runtimeApps,
    registerRuntimeIcon,
    unregisterRuntimeApp,
    type AppCategory,
    type RuntimeAppMeta,
} from '@/appRegistry'
import { useWindowManager } from '@/composables/useWindowManager'
import { useDockLayout } from '@/composables/useDockLayout'
import {
    appsList,
    get,
    getApiToken,
    post,
    del,
    request,
    type InstalledApp,
} from '@/api/client'
import { useToast } from '@/composables/useToast'
import { createSdk, type NexosSdk } from '@/sdk'

// =============================================================================
// 类型
// =============================================================================

/** 应用包 registerApp 声明（冻结契约；见文件头注释）。 */
export interface DesktopAppDecl {
    /** 应用 id（= 窗口 id = /apps-assets/:id；与内置应用不可冲突）。 */
    id: string
    /** 显示名（窗口标题 / 桌面图标标签）。 */
    label: string
    /** 图标：AppIcon 体系图标名，或 SVG 内部标记字符串（含 '<' 视为后者）。 */
    icon?: string
    /** 路由路径（直接 URL 访问兼容；缺省 /<id>）。 */
    route?: string
    /** 业务域分类（Launchpad 分组；归一到 AppCategory，未知归 system）。 */
    category?: string
    /** 图标背景渐变（缺省用中性渐变）。 */
    gradient?: string
    /** 应用主组件（窗口内容）。缺省时窗口显示"未注册应用"占位。 */
    component?: unknown
}

/** 应用包 addRoute 声明（冻结契约）。 */
export interface AppRouteDecl {
    path: string
    name?: string
    component: unknown
}

/** 传给 entry.js register(ctx) 的上下文（冻结契约）。 */
export interface AppRegisterContext {
    registerApp(app: DesktopAppDecl): void
    addRoute(route: AppRouteDecl): void
    addI18n(locale: string, messages: Record<string, unknown>): void
    /** 主前端 api client 原语（鉴权/超时/错误处理全由宿主提供）。 */
    api: {
        get<T>(path: string): Promise<T>
        post<T>(path: string, body?: unknown): Promise<T>
        del<T>(path: string): Promise<T>
        request<T>(path: string, opts?: { method?: string; body?: unknown }): Promise<T>
    }
    /**
     * @nexos/app-sdk 就绪实例（v0.1.28 起；协议版本 sdk.version='0.1'）。
     * 能力快照 / 联邦大厅 / 网关 / 本地 LLM / 通知——应用也可经
     * `import { createSdk } from '@nexos/app-sdk'`（构建期重写到桥）自建。
     */
    sdk: NexosSdk
}

/** 应用包加载失败条目（桌面占位卡数据源）。 */
export interface AppLoadFailure {
    id: string
    name: string
    error: string
}

// =============================================================================
// 宿主桥（必须在任何 /apps-assets 动态 import 之前就位）
// =============================================================================

/** 桥上暴露的宿主能力（应用包构建期重写 'vue'/'vue-i18n'/'@nexos/app-sdk' 到这里）。 */
interface NexosHostBridge {
    vue: typeof vueNamespace
    vueI18n: typeof vueI18nNamespace
    api: AppRegisterContext['api']
    /** @nexos/app-sdk：既是就绪实例（ctx.sdk 同一对象）也携带工厂面
     *  （sdk.createSdk / sdk.SDK_VERSION——host-externals 虚拟模块的导出源）。 */
    sdk: NexosSdk
}

/** 应用 SDK（宿主 api + 主前端 toast + 全局令牌读取注入；模块级单例）。 */
let hostSdk: NexosSdk | null = null

/** 安装宿主桥（幂等）。 */
export function ensureHostBridge(): void {
    if (!hostSdk) {
        // 嵌入模式通知走主前端全局 toast（useToast 队列 → ToastContainer 渲染）；
        // 令牌读取与 client.ts 同源（网关 SSE 裸 fetch 的鉴权口径一致）。
        const toast = useToast()
        hostSdk = createSdk({ get, post, del, request }, {
            getToken: getApiToken,
            notify: (title, body) =>
                toast.info(body ? `${title}：${body}` : title),
        })
    }
    const g = globalThis as { __NEXOS_HOST__?: NexosHostBridge }
    g.__NEXOS_HOST__ = {
        vue: vueNamespace,
        vueI18n: vueI18nNamespace,
        api: { get, post, del, request },
        sdk: hostSdk,
    }
}

// 模块加载即安装（MainLayout 引入本模块必然先于应用包 import）。
ensureHostBridge()

// =============================================================================
// 运行时状态
// =============================================================================

/** 后端报告的已安装应用包（GET /api/v1/apps 结果缓存，热装时更新）。 */
export const installedApps = shallowReactive<InstalledApp[]>([])

/** 加载失败的应用（桌面占位卡 + 重试）。 */
export const appLoadFailures = shallowReactive<AppLoadFailure[]>([])

/** 应用注册的路由名（卸载时 router.removeRoute 用）。 */
const appRouteNames = new Map<string, string[]>()

/** entry.js 动态 import 超时（毫秒）。 */
const IMPORT_TIMEOUT_MS = 10_000

// =============================================================================
// 装载
// =============================================================================

/** 带超时的 dynamic import（超时抛错，模块本身继续加载由浏览器缓存接管）。 */
function importWithTimeout(url: string): Promise<Record<string, unknown>> {
    return new Promise<Record<string, unknown>>((resolve, reject) => {
        const timer = setTimeout(
            () => reject(new Error(`加载超时（${IMPORT_TIMEOUT_MS / 1000}s）`)),
            IMPORT_TIMEOUT_MS,
        )
        import(/* @vite-ignore */ url)
            .then((m: Record<string, unknown>) => {
                clearTimeout(timer)
                resolve(m)
            })
            .catch((e: unknown) => {
                clearTimeout(timer)
                reject(e instanceof Error ? e : new Error(String(e)))
            })
    })
}

/** 构造传给 entry.js 的注册上下文。 */
function makeRegisterContext(app: InstalledApp): AppRegisterContext {
    return {
        registerApp(decl: DesktopAppDecl): void {
            registerRuntimeApp(app, decl)
        },
        addRoute(route: AppRouteDecl): void {
            if (!route || typeof route.path !== 'string' || route.path === '') return
            try {
                const name = addAppRoute({
                    path: route.path,
                    name: route.name ?? `${app.id}-route`,
                    component: route.component as Component,
                    appId: app.id,
                })
                if (name) {
                    const list = appRouteNames.get(app.id) ?? []
                    list.push(name)
                    appRouteNames.set(app.id, list)
                }
            } catch {
                // 路由注册失败不影响窗口使用（直接 URL 兼容降级），吞掉。
            }
        },
        addI18n(locale: string, messages: Record<string, unknown>): void {
            if (typeof locale !== 'string' || !messages) return
            i18n.global.mergeLocaleMessage(locale, messages)
        },
        api: { get, post, del, request },
        sdk: hostSdk as NexosSdk,
    }
}

/** registerApp 落地：写 runtimeApps / appRegistry / runtimeIcons。 */
function registerRuntimeApp(app: InstalledApp, decl: DesktopAppDecl): void {
    if (!decl || typeof decl.id !== 'string' || decl.id === '') return
    // id 与内置应用冲突：拒绝（防止覆盖内置窗口组件）。
    const isRuntime = runtimeApps.some((a) => a.id === decl.id)
    if (!isRuntime && appRegistry[decl.id]) return
    // 同 id 重复注册（重装/重试）：先清旧条目。
    unregisterRuntimeApp(decl.id)
    const iconRaw = typeof decl.icon === 'string' ? decl.icon : ''
    const iconName = iconRaw.includes('<') ? decl.id : iconRaw || app.icon || decl.id
    if (iconRaw.includes('<')) registerRuntimeIcon(decl.id, iconRaw)
    const meta: RuntimeAppMeta = {
        id: decl.id,
        label: decl.label || app.name || decl.id,
        icon: iconName,
        gradient:
            decl.gradient ||
            'linear-gradient(135deg, #772953 0%, #E95420 100%)',
        route: decl.route || `/${decl.id}`,
        category: normalizeCategory(decl.category || app.category),
        runtime: true,
    }
    runtimeApps.push(meta)
    if (decl.component) {
        appRegistry[decl.id] = decl.component as Component
    }
    // 清除该应用的失败占位（重试成功路径）。
    const fIdx = appLoadFailures.findIndex((f) => f.id === decl.id)
    if (fIdx >= 0) appLoadFailures.splice(fIdx, 1)
}

/** 分类归一（未知 / 缺省归 system）。 */
function normalizeCategory(cat: string): AppCategory {
    switch (cat) {
        case 'backup':
        case 'monitor':
        case 'media':
        case 'security':
        case 'files':
        case 'devtools':
        case 'power':
        case 'system':
            return cat
        default:
            return 'system'
    }
}

/** 记录加载失败（桌面占位卡展示）。 */
function recordFailure(app: InstalledApp, err: unknown): void {
    const message = err instanceof Error ? err.message : String(err)
    const existing = appLoadFailures.findIndex((f) => f.id === app.id)
    const entry: AppLoadFailure = { id: app.id, name: app.name || app.id, error: message }
    if (existing >= 0) appLoadFailures.splice(existing, 1, entry)
    else appLoadFailures.push(entry)
}

/**
 * 加载单个应用包：dynamic import entry.js → register(ctx)。
 * - 已注册（同 id 且未要求强制）时直接返回；
 * - bust：缓存击穿参数（重装新版本时用 installed_at 区分模块 URL）。
 */
export async function loadAppEntry(
    app: InstalledApp,
    opts?: { bust?: string },
): Promise<void> {
    if (!app || typeof app.id !== 'string' || app.id === '') return
    if (runtimeApps.some((a) => a.id === app.id)) return
    const entry = String(app.entry || 'web/entry.js').replace(/^\/+/, '')
    const bust = opts?.bust ?? app.installed_at ?? ''
    const url =
        `/apps-assets/${encodeURIComponent(app.id)}/${entry}` +
        (bust ? `?v=${encodeURIComponent(bust)}` : '')
    const mod = await importWithTimeout(url)
    const register = mod?.default as unknown
    if (typeof register !== 'function') {
        throw new Error('entry.js 缺少 default export 函数（register(ctx)）')
    }
    const ctx = makeRegisterContext(app)
    const result = (register as (c: AppRegisterContext) => unknown)(ctx)
    // register 支持 async（返回 Promise）：等待完成再校验。
    if (result && typeof (result as Promise<unknown>).then === 'function') {
        await result
    }
    if (!appRegistry[app.id]) {
        throw new Error('register(ctx) 未注册应用（缺 registerApp 调用或 component）')
    }
}

/**
 * 启动装载（MainLayout onMounted 调用一次）：
 * GET /api/v1/apps → 并发装载全部已装应用包。
 * 后端未就绪 / 接口失败：静默（桌面照常，仅无应用包）。
 */
export async function bootstrapApps(): Promise<void> {
    let list: InstalledApp[] = []
    try {
        const resp = await appsList()
        list = Array.isArray(resp?.apps) ? resp.apps : []
    } catch {
        return
    }
    installedApps.splice(0, installedApps.length, ...list)
    await Promise.all(
        list.map((a) =>
            loadAppEntry(a).catch((e: unknown) => {
                recordFailure(a, e)
            }),
        ),
    )
}

/** 桌面占位卡「重试」：重装失败应用的 entry。 */
export async function retryApp(id: string): Promise<void> {
    let app = installedApps.find((a) => a.id === id)
    if (!app) {
        app = { id, name: id, version: '', category: '', icon: '', description: '', entry: 'web/entry.js', dir: '', installed_at: '' }
    }
    // 先清失败条目（重试中不显示旧错误）；再失败会重新记录。
    const fIdx = appLoadFailures.findIndex((f) => f.id === id)
    if (fIdx >= 0) appLoadFailures.splice(fIdx, 1)
    try {
        await loadAppEntry(app, { bust: `retry-${Date.now()}` })
    } catch (e: unknown) {
        recordFailure(app, e)
    }
}

/** 忽略某失败占位卡（本次会话不再显示）。 */
export function dismissAppFailure(id: string): void {
    const fIdx = appLoadFailures.findIndex((f) => f.id === id)
    if (fIdx >= 0) appLoadFailures.splice(fIdx, 1)
}

// =============================================================================
// 热装卸
// =============================================================================

/**
 * 安装成功后的热注册（免刷新）：登记已装清单 + import + register。
 * bust 用 installed_at（或版本）避免旧模块缓存。
 */
export async function hotRegisterApp(app: InstalledApp): Promise<void> {
    // 更新已装清单（去重替换）。
    const idx = installedApps.findIndex((a) => a.id === app.id)
    if (idx >= 0) installedApps.splice(idx, 1, app)
    else installedApps.push(app)
    // 重装场景：先卸干净旧注册再装新。
    if (runtimeApps.some((a) => a.id === app.id)) {
        removeAppRegistration(app.id)
    }
    await loadAppEntry(app, { bust: app.installed_at || app.version || String(Date.now()) })
}

/**
 * 卸载后的本地清理（API DELETE 成功后调用）：
 * 关窗口 → 出 Dock → 移除路由 → 注销注册表 → 清失败占位。
 */
export function removeAppRegistration(id: string): void {
    const wm = useWindowManager()
    wm.closeWindow(id)
    const dockLayout = useDockLayout()
    dockLayout.moveToDesktop(id) // 仅移出 dockAppIds（localStorage 同步清理）
    for (const name of appRouteNames.get(id) ?? []) {
        try {
            router.removeRoute(name)
        } catch {
            // 路由不存在等：忽略
        }
    }
    appRouteNames.delete(id)
    unregisterRuntimeApp(id)
    const fIdx = appLoadFailures.findIndex((f) => f.id === id)
    if (fIdx >= 0) appLoadFailures.splice(fIdx, 1)
    const iIdx = installedApps.findIndex((a) => a.id === id)
    if (iIdx >= 0) installedApps.splice(iIdx, 1)
}

/** 从已装清单 / catalog 刷新（AppStore 卸载成功后调用 removeAppRegistration）。 */
export function appInstalledVersion(id: string): string {
    return installedApps.find((a) => a.id === id)?.version ?? ''
}
