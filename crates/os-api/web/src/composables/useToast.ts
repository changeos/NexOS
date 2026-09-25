// =============================================================================
// useToast —— 全局 Toast 提示 composable（Ubuntu Yaru 风真实浮层）。
//
// 维护一个模块级 reactive 队列 `toasts`，success/error/info 往队列 push 一条
// 提示，3 秒后自动移除。ToastContainer.vue 直接导入 `toasts` 渲染为右上角
// 浮层（彩色左边框 + Ubuntu 圆角 + 轻阴影 + 淡出动画）。
//
// Shares.vue / Users.vue / Settings.vue 等通过 useToast() 取得 toast，
// 调用 toast.success(msg) / toast.error(msg) / toast.info(msg)。
// =============================================================================
import { reactive } from 'vue'

export type ToastType = 'success' | 'error' | 'info'

export interface ToastItem {
    id: number
    type: ToastType
    message: string
    visible: boolean
}

/** 全局 toast 队列（供 ToastContainer.vue 直接渲染）。 */
export const toasts = reactive<ToastItem[]>([])

/** 自动移除延迟（毫秒）。 */
const AUTO_REMOVE_MS = 3000
/** 淡出动画时长（毫秒），到时后真正从 DOM 摘除。 */
const FADE_MS = 300

let nextId = 1

/** 推送一条 toast；3 秒淡出后从队列移除。 */
function push(type: ToastType, message: string): void {
    const id = nextId++
    toasts.push({ id, type, message, visible: true })
    // 先标记不可见以触发淡出过渡，再延时摘除。
    window.setTimeout(() => {
        const item = toasts.find((t) => t.id === id)
        if (item) item.visible = false
    }, AUTO_REMOVE_MS)
    window.setTimeout(() => {
        const idx = toasts.findIndex((t) => t.id === id)
        if (idx >= 0) toasts.splice(idx, 1)
    }, AUTO_REMOVE_MS + FADE_MS)
}

/** 手动关闭某条 toast（点击关闭按钮用）。 */
export function dismissToast(id: number): void {
    const idx = toasts.findIndex((t) => t.id === id)
    if (idx >= 0) toasts.splice(idx, 1)
}

export interface Toast {
    success(msg: string): void
    error(msg: string): void
    info(msg: string): void
}

/** 返回 toast 句柄；同时可从模块导入 `toasts` / `dismissToast` 渲染容器。 */
export function useToast(): Toast {
    return {
        success(msg: string): void {
            push('success', msg)
        },
        error(msg: string): void {
            push('error', msg)
        },
        info(msg: string): void {
            push('info', msg)
        },
    }
}
