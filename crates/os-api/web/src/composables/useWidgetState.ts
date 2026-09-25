/**
 * useWidgetState —— 运行信息磁贴卡片的位置 / 展开状态持久化。
 *
 * localStorage key `os-widget-state` 存 { x, y, expanded }。
 * 默认 x 为 9999 哨兵（右缘真实坐标依赖视口宽度，不能静态写死）：
 * 挂载 / window resize 时 ensureVisible() 把越界坐标贴回右侧边缘并持久化，
 * 保证胶囊始终可见（曾因哨兵值直接渲染 left:9999px 而整体在视口外）。
 *
 * 单例模式：跨 SystemWidget 组件与未来设置页共享同一份状态。
 */
import { ref } from 'vue'

export interface WidgetState {
    x: number
    y: number
    expanded: boolean
}

const WIDGET_KEY = 'os-widget-state'

/**
 * 默认状态：x=9999 是"未初始化"哨兵——首次由 ensureVisible() 贴到右侧边缘
 * （innerWidth - 宽 - 12）并持久化，避免写死与视口相关的坐标。
 * y=70 避开顶栏；默认展开。
 */
function defaultState(): WidgetState {
    return { x: 9999, y: 70, expanded: true }
}

/** 单例状态。 */
const state = ref<WidgetState>(defaultState())
let initialized = false

function loadFromStorage(): WidgetState {
    const base = defaultState()
    try {
        const raw = window.localStorage.getItem(WIDGET_KEY)
        if (!raw) return base
        const parsed = JSON.parse(raw) as Partial<WidgetState>
        return {
            x: typeof parsed.x === 'number' && Number.isFinite(parsed.x) ? parsed.x : base.x,
            y: typeof parsed.y === 'number' && Number.isFinite(parsed.y) ? parsed.y : base.y,
            expanded: typeof parsed.expanded === 'boolean' ? parsed.expanded : true,
        }
    } catch {
        return base
    }
}

function ensureInit(): void {
    if (initialized) return
    state.value = loadFromStorage()
    initialized = true
}

export function useWidgetState() {
    ensureInit()

    /** 持久化到 localStorage（写入失败静默忽略）。 */
    function persist(): void {
        try {
            window.localStorage.setItem(WIDGET_KEY, JSON.stringify(state.value))
        } catch {
            /* 隐私模式等：仅内存 */
        }
    }

    /** 设置位置（拖动结束时调用）。 */
    function setPosition(x: number, y: number): void {
        state.value = { ...state.value, x, y }
        persist()
    }

    /** 切换展开 / 收起。 */
    function toggleExpanded(): void {
        state.value = { ...state.value, expanded: !state.value.expanded }
        persist()
    }

    /** 直接设置展开状态。 */
    function setExpanded(expanded: boolean): void {
        if (state.value.expanded === expanded) return
        state.value = { ...state.value, expanded }
        persist()
    }

    /**
     * 把胶囊位置拉回视口内（SystemWidget 挂载时 + window resize 时调用）。
     *
     * 触发：x 越界——大于 innerWidth-宽-4（含 9999 哨兵、旧大屏残留坐标、
     * 视口缩小）或小于 4；或 y 越界——大于 innerHeight-高-4 或小于 40。
     * 修正：x 贴右侧边缘（innerWidth - 宽 - 12，比拖拽吸附多留一点边距），
     * y 钳制到 [40, innerHeight - 高 - 4]（与拖拽约束一致）。
     * 尺寸按展开 200x320 / 收起 48x48 保守估计；仅在位置变化时持久化。
     */
    function ensureVisible(): void {
        const { expanded, x, y } = state.value
        const w = expanded ? 200 : 48
        const h = expanded ? 320 : 48
        const maxX = window.innerWidth - w - 4
        const maxY = window.innerHeight - h - 4
        if (x >= 4 && x <= maxX && y >= 40 && y <= maxY) return
        setPosition(Math.max(4, window.innerWidth - w - 12), Math.max(40, Math.min(maxY, y)))
    }

    return { state, setPosition, toggleExpanded, setExpanded, ensureVisible }
}
