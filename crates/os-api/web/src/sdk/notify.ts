// =============================================================================
// notify —— 通知面（@nexos/app-sdk 的 sdk.notify(title, body)）。
//
// 三档策略（按宿主形态自动选择）：
//   1. 宿主注入（createSdk opts.notify）——嵌入模式由 appRuntime.ts 注入主
//      前端全局 toast（useToast 队列，ToastContainer 渲染）；宿主有全局
//      toast 时 SDK 不自绘。
//   2. 独立模式（__NEXOS_STANDALONE__=true）且 Notification 权限已授予 →
//      系统 Notification API。**只在已 granted 时用**——不主动弹权限询问
//      （不打扰），无权限降级第 3 档。
//   3. SDK 自绘迷你 toast DOM（零依赖：fixed 右下角，3.5s 自动消失，堆叠
//      上限 4 条）——宿主无 toast / 独立模式无通知权限的兜底。
// =============================================================================

/** 通知函数签名（sdk.notify 可直接调用）。 */
export type NotifyFn = (title: string, body?: string) => void

/** 迷你 toast 的自动消失时长（毫秒）。 */
const MINI_TOAST_MS = 3_500
/** 同屏迷你 toast 上限（超出移除最旧的）。 */
const MINI_TOAST_MAX = 4

/** 独立模式标记（standalone-host.ts 置位）。 */
function isStandalone(): boolean {
    return (
        (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__ === true
    )
}

/** 系统通知（权限已授予才走；返回是否成功发出）。 */
function systemNotification(title: string, body?: string): boolean {
    if (typeof Notification === 'undefined') return false
    if (Notification.permission !== 'granted') return false
    try {
        new Notification(title, { body: body ?? '' })
        return true
    } catch {
        return false
    }
}

/** 自绘迷你 toast（DOM；测试/无头环境 document 缺失时静默跳过）。 */
function miniToast(title: string, body?: string): void {
    if (typeof document === 'undefined') return
    const doc = document
    let host = doc.getElementById('nexos-sdk-mini-toasts')
    if (!host) {
        host = doc.createElement('div')
        host.id = 'nexos-sdk-mini-toasts'
        host.style.cssText = [
            'position:fixed', 'right:14px', 'bottom:14px', 'z-index:2147483647',
            'display:flex', 'flex-direction:column', 'gap:8px', 'max-width:320px',
            'pointer-events:none',
        ].join(';')
        doc.body.appendChild(host)
    }
    const item = doc.createElement('div')
    item.style.cssText = [
        'background:#1f2028', 'color:#e6e4e9', 'border-left:3px solid #E95420',
        'border-radius:8px', 'padding:9px 12px', 'font:13px/1.5 system-ui, sans-serif',
        'box-shadow:0 4px 16px rgba(0,0,0,.4)', 'opacity:0', 'transition:opacity .25s',
        'word-break:break-word',
    ].join(';')
    const t = doc.createElement('strong')
    t.textContent = title
    item.appendChild(t)
    if (body) {
        const b = doc.createElement('div')
        b.textContent = body
        b.style.color = '#b9bac4'
        item.appendChild(b)
    }
    host.appendChild(item)
    // 超限移除最旧
    while (host.childElementCount > MINI_TOAST_MAX && host.firstElementChild) {
        host.firstElementChild.remove()
    }
    requestAnimationFrame(() => {
        item.style.opacity = '1'
    })
    setTimeout(() => {
        item.style.opacity = '0'
        setTimeout(() => item.remove(), 300)
    }, MINI_TOAST_MS)
}

/**
 * 构造通知函数（index.ts 装配用；应用一般不直接调）。
 * 优先级：宿主注入 > 独立模式系统通知 > 迷你 toast。
 */
export function createNotifier(opts?: { notify?: NotifyFn }): NotifyFn {
    if (opts?.notify) return opts.notify
    return (title: string, body?: string): void => {
        if (isStandalone() && systemNotification(title, body)) return
        miniToast(title, body)
    }
}
