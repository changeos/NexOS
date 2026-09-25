// =============================================================================
// toast.ts —— 全局通知模块级单例（v0.1.11 全局操作反馈系统基建）。
//
// 背景：各功能操作缺反馈（点击 / 运行过程 / 成败）——任务中心在工作台底部、
// busy spinner 只在按钮位，均非全页可见。本模块 + NxToast.vue（渲染器，Teleport
// body 右上角堆叠）补全：提交 loading（可持续更新进度文案）→ 终态 success/
// error 一次操作的完整闭环。
//
// 设计：
//   · 模块级 reactive 状态（**单例**）——任何模块 import { toast } 即用，
//     不依赖组件树 provide/inject；NxToast.vue 只是该状态的渲染器（应用根
//     挂一次即可，桌面嵌入与 standalone 两载体同享）。
//   · API：toast.success(msg, title?) / error / warning / info / loading；
//     loading 双载——loading(msg) 建（返回 id 持有），loading(id, msg) 就地
//     更新进度文案（保持 loading 态不重置计时）；update(id, kind, msg) 把
//     loading 翻成终态（success/error/warning，重挂自动关计时）。
//   · 堆叠上限 4 条：超出挤掉最旧一条（新推旧出）。
//   · 自动关：success/warning/info 4s / error 8s（且始终带手动 ×）；
//     loading 不自动关（等终态 update 或手动 dismiss）。
//   · action：可选动作钮（如失败任务的「查看任务」跳任务中心）。
//   · happy-dom 冒烟可直断言 useToastState()（纯状态面，无需渲染器）。
// =============================================================================
import { reactive } from 'vue'

export type ToastKind = 'success' | 'error' | 'warning' | 'info' | 'loading'

/** toast 动作钮（label + 点击回调；如失败任务「查看任务」跳任务中心）。 */
export interface ToastAction {
  label: string
  onClick: () => void
}

/** 单条 toast（模块状态面；渲染器 NxToast.vue 消费）。 */
export interface ToastItem {
  id: number
  kind: ToastKind
  title: string
  message: string
  /** 自动关毫秒数；0 = 不自动关（loading / 手动管理）。 */
  duration: number
  action: ToastAction | null
}

/** 堆叠上限（超出挤掉最旧）。 */
export const TOAST_MAX = 4
/** 常规自动关时长（success/warning/info）。 */
export const TOAST_DURATION = 4000
/** error 自动关时长（翻倍 + 始终可手动关）。 */
export const TOAST_ERROR_DURATION = 8000

/** 模块级单例状态（items[0] = 最新——渲染器列顶）。 */
const state = reactive<{ items: ToastItem[] }>({ items: [] })

let seq = 0
const timers = new Map<number, ReturnType<typeof setTimeout>>()

function clearTimer(id: number): void {
  const timer = timers.get(id)
  if (timer) {
    clearTimeout(timer)
    timers.delete(id)
  }
}

/** 挂自动关计时（duration>0 时）。 */
function arm(item: ToastItem): void {
  clearTimer(item.id)
  if (item.duration > 0) {
    timers.set(
      item.id,
      setTimeout(() => dismiss(item.id), item.duration),
    )
  }
}

/** 移除一条（手动 × / 自动关 / 挤出）。 */
export function dismiss(id: number): void {
  clearTimer(id)
  const idx = state.items.findIndex((x) => x.id === id)
  if (idx >= 0) state.items.splice(idx, 1)
}

/** 落栈：新在顶；超上限挤掉最旧（state.items 末尾）。 */
function push(kind: ToastKind, message: string, opts?: { title?: string; duration?: number; action?: ToastAction }): number {
  const item: ToastItem = {
    id: ++seq,
    kind,
    title: opts?.title ?? '',
    message,
    // loading 不自动关（等 update 翻终态）；error 8s；其余缺省 4s
    duration: opts?.duration ?? (kind === 'loading' ? 0 : kind === 'error' ? TOAST_ERROR_DURATION : TOAST_DURATION),
    action: opts?.action ?? null,
  }
  state.items.unshift(item)
  while (state.items.length > TOAST_MAX) {
    const oldest = state.items[state.items.length - 1]
    dismiss(oldest.id)
  }
  arm(item)
  return item.id
}

/**
 * 就地更新一条（不改 kind）：loading 进度文案刷新（toast.loading(id, msg)）/
 * 标题微调。id 不存在时静默（该条已被关/挤出）。
 */
function patch(id: number, message?: string, title?: string): void {
  const item = state.items.find((x) => x.id === id)
  if (!item) return
  if (message !== undefined) item.message = message
  if (title !== undefined) item.title = title
}

/**
 * 翻终态：loading → success/error/warning（重挂对应自动关计时；action 可换）。
 * id 不存在（被挤出/已关）时按新 kind 重落一条——终态不丢。
 */
function update(
  id: number,
  kind: ToastKind,
  message: string,
  opts?: { title?: string; action?: ToastAction | null },
): void {
  const item = state.items.find((x) => x.id === id)
  if (!item) {
    push(kind, message, { title: opts?.title, action: opts?.action ?? undefined })
    return
  }
  clearTimer(id)
  item.kind = kind
  item.message = message
  if (opts?.title !== undefined) item.title = opts.title
  if (opts?.action !== undefined) item.action = opts.action
  // 翻终态重挂计时（error 8s / 其余 4s）；仍为 loading 则保持不自动关
  item.duration = kind === 'loading' ? 0 : kind === 'error' ? TOAST_ERROR_DURATION : TOAST_DURATION
  arm(item)
}

/** 清空（单测 / 场景重置）。 */
export function clearToasts(): void {
  for (const item of [...state.items]) dismiss(item.id)
}

/** loading 双载签名：建（返回 id）／按 id 更新进度文案（就地，仍 loading 态）。 */
export interface ToastLoadingFn {
  (message: string, title?: string): number
  (id: number, message: string, title?: string): void
}

/** loading 实现（双载分派：首参 number = 更新既有条；否则新建）。 */
const loading = ((a: string | number, b?: string, c?: string): number | void => {
  if (typeof a === 'number') {
    patch(a, b ?? '', c)
    return
  }
  return push('loading', a, { title: b })
}) as ToastLoadingFn

/**
 * toast 单例 API。
 *
 * ```ts
 * const id = toast.loading('已提交：向量化')
 * toast.loading(id, '向量化中 3/17')   // 进度文案持续更新
 * toast.update(id, 'success', '向量化完成：17 块')  // 终态闭环
 * ```
 */
export const toast = {
  success(message: string, title?: string): number {
    return push('success', message, { title })
  },
  error(message: string, title?: string, opts?: { action?: ToastAction }): number {
    return push('error', message, { title, action: opts?.action })
  },
  warning(message: string, title?: string): number {
    return push('warning', message, { title })
  },
  info(message: string, title?: string): number {
    return push('info', message, { title })
  },
  loading,
  /** 翻终态（success/error/warning/info；id 失效时重落一条不丢终态）。 */
  update,
  /** 手动关一条。 */
  dismiss,
  /** 清空。 */
  clear: clearToasts,
}

/** 渲染器 / 冒烟观测面（模块状态 reactive 引用）。 */
export function useToastState() {
  return state
}
