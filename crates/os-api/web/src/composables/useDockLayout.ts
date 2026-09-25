// =============================================================================
// useDockLayout —— 桌面 / Dock 互斥布局管理（单例）。
//
// 核心职责：
//   - 维护"在 Dock 里"的应用 id 集合（dockAppIds），其余应用在桌面。
//   - 桌面与 Dock 互斥：同一应用要么在桌面，要么在 Dock，不能同时存在。
//   - 持久化 dockAppIds 到 localStorage（key: STORAGE_KEY）。
//
// 设计要点：
//   - 模块级单例（state 抬到模块作用域），useDockLayout() 仅返回引用，
//     保证 MainLayout / DashboardView 多处调用拿到同一份状态。
//   - 初始 Dock 为空；应用默认都在桌面，由用户自行拖入 Dock。
//   - 设置入口 = 顶栏右上齿轮（MainLayout goToSettings），Dock 不再放设置，
//     settings 也不出现在桌面（DashboardView 的 allApps 本就不含 settings）。
// =============================================================================
import { computed, reactive } from 'vue'
import { desktopApps, findApp, type AppMeta } from '@/appRegistry'

/** localStorage 持久化 key。 */
const STORAGE_KEY = 'os-dock-layout'

/** 初始 Dock 应用 id 列表（空：设置入口在顶栏右上齿轮，Dock 不再放设置）。 */
const DEFAULT_DOCK_IDS: string[] = []

// =============================================================================
// 模块级单例状态
// =============================================================================
/**
 * 在 Dock 里的应用 id 集合（数组，保持插入顺序）。
 * reactive 让 computed（dockApps / desktopApps）自动跟随更新。
 */
const dockAppIds = reactive<string[]>([])

/** 是否已从 localStorage 初始化过（避免重复加载覆盖用户运行时改动）。 */
let initialized = false

/** 安全读 localStorage（解析失败回退默认）。 */
function loadStoredDockIds(): string[] {
    try {
        const raw = localStorage.getItem(STORAGE_KEY)
        if (!raw) return [...DEFAULT_DOCK_IDS]
        const parsed = JSON.parse(raw) as unknown
        if (!Array.isArray(parsed)) return [...DEFAULT_DOCK_IDS]
        // 仅保留合法字符串且当前已注册（内置 + 运行时应用包）的 id。
        // 运行时应用异步注册：装载完成前校验会丢弃其 Dock 记录——这里放行
        // 未知 id（findApp 落空只是 Dock 临时不显示，应用注册后 dockApps 恢复），
        // 避免应用包装载慢导致 Dock 固定项被清。
        const ids = parsed.filter((x): x is string => typeof x === 'string')
        // 存量迁移：老版本会把 'settings' 钉在 Dock（localStorage 里可能还存着），
        // 设置入口已改为顶栏齿轮，这里一次性过滤掉，其余 Dock 项保持不变。
        return ids.filter((id) => id !== 'settings')
    } catch {
        return [...DEFAULT_DOCK_IDS]
    }
}

/** 写 localStorage（静默失败）。 */
function persist(): void {
    try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(dockAppIds))
    } catch {
        // 配额满 / 隐私模式：静默失败，不影响功能。
    }
}

/** 模块加载时初始化一次。 */
function ensureInit(): void {
    if (initialized) return
    initialized = true
    const stored = loadStoredDockIds()
    dockAppIds.splice(0, dockAppIds.length, ...stored)
}

// =============================================================================
// 业务方法
// =============================================================================

/** 应用是否在 Dock。 */
function isOnDock(id: string): boolean {
    return dockAppIds.includes(id)
}

/** 应用是否在桌面（不在 Dock）。 */
function isOnDesktop(id: string): boolean {
    return !dockAppIds.includes(id)
}

/**
 * 把应用从桌面移到 Dock。
 * - 已在 Dock 则无操作（幂等）。
 */
function moveToDock(id: string): void {
    ensureInit()
    if (dockAppIds.includes(id)) return
    const meta = findApp(id)
    if (!meta) return
    dockAppIds.push(id)
    persist()
}

/**
 * 把应用从 Dock 移到桌面（settings 不再特殊，任何 Dock 项都可移出）。
 */
function moveToDesktop(id: string): void {
    ensureInit()
    const idx = dockAppIds.indexOf(id)
    if (idx === -1) return
    dockAppIds.splice(idx, 1)
    persist()
}

// =============================================================================
// 派生 computed（应用元信息列表）
// =============================================================================

/** Dock 里的应用元信息列表（顺序 = dockAppIds 顺序）。 */
const dockApps = computed<AppMeta[]>(() => {
    ensureInit()
    const list: AppMeta[] = []
    for (const id of dockAppIds) {
        const meta = findApp(id)
        if (meta) list.push(meta)
    }
    return list
})

/**
 * 桌面里的应用元信息列表。
 * 顺序遵循 desktopApps（内置在前 + 运行时应用包在后），过滤掉所有当前在 Dock 的应用。
 */
const desktopAppsMeta = computed<AppMeta[]>(() => {
    ensureInit()
    return desktopApps.value.filter((a) => !dockAppIds.includes(a.id))
})

/** 返回布局管理句柄（单例，多次调用共享同一状态）。 */
export function useDockLayout() {
    return {
        dockAppIds,
        isOnDock,
        isOnDesktop,
        moveToDock,
        moveToDesktop,
        dockApps,
        desktopApps: desktopAppsMeta,
    }
}

export type DockLayout = ReturnType<typeof useDockLayout>
