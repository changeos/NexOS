// =============================================================================
// useWindowManager —— 桌面多浮窗管理器（单例）。
//
// 核心职责：
//   - 维护当前所有打开窗口的状态数组 windows（reactive 单例，全局共享）
//   - 维护当前聚焦窗口 activeId
//   - 提供 open/close/focus/minimize/maximize/restore/move/resize 等方法
//
// 设计要点：
//   - 模块级单例（state 抬到模块作用域），useWindowManager() 仅返回引用，
//     保证 MainLayout / Dock / DashboardView 多处调用拿到同一份状态。
//   - z-index 全局递增：focusWindow 时新 zIndex = 当前最大 + 1。
//   - 连续打开同位置窗口时使用级联偏移（+24px），超出可视宽度自动重置到起点。
//   - 最大化前用 prevRect 保存上一份位置/尺寸，便于还原。
// =============================================================================
import { reactive, ref } from 'vue'

/** 窗口矩形（位置 + 尺寸）。 */
export interface WindowRect {
    x: number
    y: number
    width: number
    height: number
}

/** 单个窗口的完整状态。 */
export interface WindowState {
    /** 应用 key（appRegistry 的 key），同时是窗口唯一 id */
    id: string
    /** 窗口标题 */
    title: string
    /** 图标名（app.icon） */
    icon: string
    /** 位置 x（相对桌面区域） */
    x: number
    /** 位置 y（相对桌面区域） */
    y: number
    /** 宽 */
    width: number
    /** 高 */
    height: number
    /** 层叠顺序 */
    zIndex: number
    /** 是否最小化（从桌面隐藏，Dock 仍显示） */
    minimized: boolean
    /** 是否最大化（占满桌面区域） */
    maximized: boolean
    /** Aero Snap 吸附方向（`left`/`right`/`maximized`/null）。null=未吸附 */
    snapped?: 'left' | 'right' | 'maximized' | null
    /** 最大化前的位置/尺寸，用于还原 */
    prevRect?: WindowRect
}

/** openWindow 入参（位置/尺寸可选，缺省用默认值 + 级联偏移）。 */
export interface OpenWindowOptions {
    id: string
    title: string
    icon: string
    x?: number
    y?: number
    width?: number
    height?: number
}

/** 默认窗口尺寸。 */
const DEFAULT_WIDTH = 960
const DEFAULT_HEIGHT = 640
/** 窗口最小尺寸（resize 时强制约束）。 */
export const MIN_WIDTH = 320
export const MIN_HEIGHT = 240
/** 级联偏移步长。 */
const CASCADE_STEP = 28
/** 级联最大偏移数（超出后重置回起点）。 */
const CASCADE_MAX = 8
/** Aero Snap 边缘吸附阈值（px）：窗口拖到距边缘 < 此值时触发半屏/全屏吸附。 */
const SNAP_THRESHOLD = 10

// =============================================================================
// 模块级单例状态
// =============================================================================
const windows = reactive<WindowState[]>([])
const activeId = ref<string | null>(null)
/** 全局 z-index 计数器，单调递增。 */
let zCounter = 10
/** 每个应用的级联计数（用于决定下一次打开时的偏移）。 */
const cascadeIndex = new Map<string, number>()
/** Aero Snap 预览方向（拖拽过程中实时更新，供 UI 显示半透明高亮区）。null=无预览。 */
const snapPreview = ref<'left' | 'right' | 'maximized' | null>(null)

/** 取桌面可视区域尺寸（用于级联/边界计算）。 */
function viewport(): { vw: number; vh: number } {
    // 桌面区域为内容区，去除 sidebar（移动端折叠为 0）后取整。
    // 退化到 window 尺寸，保证 SSR / 测试环境不报错。
    const el = typeof document !== 'undefined' ? document.querySelector('.desktop-area') : null
    if (el) {
        const r = el.getBoundingClientRect()
        return { vw: Math.floor(r.width), vh: Math.floor(r.height) }
    }
    const w = typeof window !== 'undefined' ? window.innerWidth : 1280
    const h = typeof window !== 'undefined' ? window.innerHeight : 800
    return { vw: w, vh: h }
}

/** 计算新窗口的级联起始位置。 */
function nextCascadePos(id: string): { x: number; y: number } {
    const { vw } = viewport()
    const idx = (cascadeIndex.get(id) ?? -1) + 1
    cascadeIndex.set(id, idx % (CASCADE_MAX + 1))
    const step = (idx % (CASCADE_MAX + 1)) * CASCADE_STEP
    // 居中略偏左上，叠加级联偏移；不超出可视右下角。
    const baseX = Math.max(40, Math.floor((vw - DEFAULT_WIDTH) / 2))
    const x = Math.min(baseX + step, Math.max(40, vw - DEFAULT_WIDTH - 40))
    const y = 48 + step
    return { x, y }
}

/** 内部：计算并返回当前最大 zIndex + 1。 */
function nextZ(): number {
    zCounter += 1
    return zCounter
}

/** 内部：将指定 id 的窗口置顶。 */
function bringToFront(id: string): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    win.zIndex = nextZ()
    activeId.value = id
}

/**
 * 打开窗口：已开则聚焦（同时取消最小化），未开则创建新窗口并置顶。
 */
function openWindow(opts: OpenWindowOptions): void {
    const existing = windows.find((w) => w.id === opts.id)
    if (existing) {
        // 已存在：若最小化则还原，并聚焦。
        existing.minimized = false
        bringToFront(existing.id)
        return
    }
    const pos = opts.x != null && opts.y != null ? { x: opts.x, y: opts.y } : nextCascadePos(opts.id)
    const win: WindowState = {
        id: opts.id,
        title: opts.title,
        icon: opts.icon,
        x: pos.x,
        y: pos.y,
        width: opts.width ?? DEFAULT_WIDTH,
        height: opts.height ?? DEFAULT_HEIGHT,
        zIndex: nextZ(),
        minimized: false,
        maximized: false,
    }
    windows.push(win)
    activeId.value = win.id
}

/** 关闭窗口。 */
function closeWindow(id: string): void {
    const idx = windows.findIndex((w) => w.id === id)
    if (idx === -1) return
    windows.splice(idx, 1)
    if (activeId.value === id) {
        // 聚焦剩余中 zIndex 最大的（且未最小化）。
        const visible = windows.filter((w) => !w.minimized)
        if (visible.length > 0) {
            const top = visible.reduce((a, b) => (a.zIndex > b.zIndex ? a : b))
            activeId.value = top.id
        } else {
            activeId.value = null
        }
    }
}

/** 聚焦窗口（z-index 置顶）。 */
function focusWindow(id: string): void {
    bringToFront(id)
}

/** 最小化窗口。 */
function minimizeWindow(id: string): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    win.minimized = true
    if (activeId.value === id) {
        const visible = windows.filter((w) => !w.minimized)
        if (visible.length > 0) {
            const top = visible.reduce((a, b) => (a.zIndex > b.zIndex ? a : b))
            activeId.value = top.id
        } else {
            activeId.value = null
        }
    }
}

/** 最大化窗口（保存 prevRect）。已最大化则还原。 */
function maximizeWindow(id: string): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    if (win.maximized) {
        // 还原
        if (win.prevRect) {
            win.x = win.prevRect.x
            win.y = win.prevRect.y
            win.width = win.prevRect.width
            win.height = win.prevRect.height
            win.prevRect = undefined
        }
        win.maximized = false
        win.snapped = null
    } else {
        win.prevRect = { x: win.x, y: win.y, width: win.width, height: win.height }
        win.maximized = true
        win.snapped = 'maximized'
    }
    bringToFront(id)
}

/** 从最小化恢复（取消最小化 + 聚焦）。 */
function restoreWindow(id: string): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    win.minimized = false
    bringToFront(id)
}

/**
 * 移动窗口（拖拽更新位置）。
 *
 * Aero Snap 行为：
 *   - 拖动已 snap / maximized 的窗口 → 先还原（恢复 prevRect 尺寸，清除 snapped 标记），
 *     再跟随光标移动（解除吸附）。
 *   - 拖动过程中检测边缘，实时更新 snapPreview（供 UI 显示半透明高亮预览区）；
 *     真正吸附在松手时由 commitSnap 完成。
 *
 * 约束：窗口至少保留 40px 在桌面可视区域内（防止拖丢）。
 */
function moveWindow(id: string, x: number, y: number): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    const { vw, vh } = viewport()
    // 拖动已 snap/maximized 的窗口 → 自动还原（恢复尺寸 + 清除标记）
    let justRestored = false
    if (win.snapped || win.maximized) {
        if (win.prevRect) {
            win.width = win.prevRect.width
            win.height = win.prevRect.height
        }
        win.snapped = null
        win.maximized = false
        win.prevRect = undefined
        justRestored = true
    }
    const KEEP = 40
    const nx = Math.min(Math.max(x, -(win.width - KEEP)), vw - KEEP)
    const ny = Math.min(Math.max(y, 0), vh - KEEP)
    win.x = nx
    win.y = ny
    // 更新 snap 预览（仅正常拖动；刚还原的那一帧不触发，避免在吸附位置立即重显预览）
    if (justRestored) {
        snapPreview.value = null
        return
    }
    if (ny < SNAP_THRESHOLD) {
        snapPreview.value = 'maximized'
    } else if (nx < SNAP_THRESHOLD) {
        snapPreview.value = 'left'
    } else if (nx + win.width > vw - SNAP_THRESHOLD) {
        snapPreview.value = 'right'
    } else {
        snapPreview.value = null
    }
}

/**
 * 显式吸附窗口到指定方向（半屏 / 全屏）。
 *
 * - `left` / `right` → 占左/右半屏（宽 50%，高满屏）。
 * - `maximized` → 全屏（同时置 maximized=true，复用既有 CSS）。
 *
 * 吸附前保存 prevRect，供后续拖动 / 还原使用。已吸附状态下重复调用会覆盖。
 */
function snapWindow(id: string, direction: 'left' | 'right' | 'maximized'): void {
    const win = windows.find((w) => w.id === id)
    if (!win) return
    // 仅在「未吸附且未最大化」时记录 prevRect，避免覆盖原始尺寸
    if (!win.snapped && !win.maximized) {
        win.prevRect = { x: win.x, y: win.y, width: win.width, height: win.height }
    }
    const { vw, vh } = viewport()
    if (direction === 'left') {
        win.x = 0
        win.y = 0
        win.width = Math.floor(vw / 2)
        win.height = vh
        win.snapped = 'left'
        win.maximized = false
    } else if (direction === 'right') {
        win.x = Math.ceil(vw / 2)
        win.y = 0
        win.width = Math.floor(vw / 2)
        win.height = vh
        win.snapped = 'right'
        win.maximized = false
    } else {
        win.x = 0
        win.y = 0
        win.width = vw
        win.height = vh
        win.snapped = 'maximized'
        win.maximized = true
    }
    snapPreview.value = null
    bringToFront(id)
}

/**
 * 提交 snap（松手时调用）：若 snapPreview 非空，则吸附到预览方向。
 */
function commitSnap(id: string): void {
    const dir = snapPreview.value
    snapPreview.value = null
    if (dir) snapWindow(id, dir)
}

/**
 * 缩放窗口（更新尺寸）。强制最小尺寸；缩放即清除 snap 状态。
 */
function resizeWindow(id: string, width: number, height: number): void {
    const win = windows.find((w) => w.id === id)
    if (!win || win.maximized) return
    // 拖动 resize 手柄 → 退出 snap（用户主动改尺寸，半屏/全屏吸附不再适用）
    if (win.snapped) {
        win.snapped = null
        win.prevRect = undefined
    }
    win.width = Math.max(MIN_WIDTH, Math.round(width))
    win.height = Math.max(MIN_HEIGHT, Math.round(height))
}

/** 判断某应用窗口是否已打开（不论最小化）。 */
function isOpen(id: string): boolean {
    return windows.some((w) => w.id === id)
}

/**
 * Dock 点击行为：
 *   - 未开 -> 打开
 *   - 已开且最小化 -> 还原
 *   - 已开且当前聚焦 -> 最小化（点击 Dock 切走）
 *   - 已开但非聚焦 -> 聚焦
 */
function toggleFromDock(id: string, meta: { title: string; icon: string }): void {
    const win = windows.find((w) => w.id === id)
    if (!win) {
        openWindow({ id, title: meta.title, icon: meta.icon })
        return
    }
    if (win.minimized) {
        restoreWindow(id)
        return
    }
    if (activeId.value === id) {
        minimizeWindow(id)
    } else {
        focusWindow(id)
    }
}

/** 返回窗口管理器句柄（单例，多次调用共享同一状态）。 */
export function useWindowManager() {
    return {
        windows,
        activeId,
        snapPreview,
        openWindow,
        closeWindow,
        focusWindow,
        minimizeWindow,
        maximizeWindow,
        restoreWindow,
        moveWindow,
        resizeWindow,
        snapWindow,
        commitSnap,
        isOpen,
        toggleFromDock,
    }
}

export type WindowManager = ReturnType<typeof useWindowManager>
