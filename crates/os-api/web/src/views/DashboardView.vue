<script setup lang="ts">
/**
 * Desktop —— 飞牛 fnOS / OS 风桌面（纯壁纸 + 桌面图标）。
 *
 * 设计变化（vs 旧 DSM 风后台）：
 *   - 删除顶部系统概览栏（hostname / 版本 / 在线时间 / CPU 虚拟化徽章）
 *   - 删除右侧"系统信息"卡片区（存储 / VM / 共享概览）—— 移到顶部状态栏与浮窗
 *   - 删除 .dsm-dashboard 的滚动 flex-column Web 布局
 *
 * 现在是纯 OS 桌面：
 *   - 根容器 .desktop-root：position:absolute; inset:0; padding:24px，背景透明
 *     （让 MainLayout 的 Aubergine 壁纸透出）
 *   - 桌面图标绝对定位（position:absolute），可自由拖拽到任意位置，位置持久化到
 *     localStorage（key: STORAGE_KEY）；首次加载自动生成网格布局
 *   - 每个图标：圆角方块 tile（gradient 背景，80×80）+ 下方白色文字标签
 *   - 区分点击 vs 拖拽：< 5px 位移 = 点击开窗口；>= 5px = 拖拽不触发 click
 *
 * hostname / 系统信息 / 实时状态 由 MainLayout 的顶部状态栏统一承载。
 */
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { useRouter } from 'vue-router'
import { useWindowManager } from '@/composables/useWindowManager'
import { useDockLayout } from '@/composables/useDockLayout'
import {
    appRegistry,
    findApp,
    getAppName,
    hasCustomName,
    resetAppName,
    runtimeApps,
    setAppName,
} from '@/appRegistry'
import { appLoadFailures, dismissAppFailure, retryApp } from '@/appRuntime'
import { useI18n } from 'vue-i18n'
import AppIcon from '@/components/AppIcon.vue'

const { t } = useI18n()

// =============================================================================
// 桌面文件夹（类 iOS/Android：拖一个图标到另一个图标上合并成文件夹）
// =============================================================================
/** 单个文件夹。id 形如 'folder-<timestamp>'。 */
interface DesktopFolder {
    id: string
    name: string
    appIds: string[]
    x: number
    y: number
}

const FOLDER_STORAGE_KEY = 'os-folders'
/** 所有桌面文件夹：folderId -> DesktopFolder。reactive 让模板自动跟随更新。 */
const folders = reactive<Record<string, DesktopFolder>>({})
/** 当前展开（双击打开）的文件夹 id。 */
const openFolderId = ref<string | null>(null)

const wm = useWindowManager()
const router = useRouter()
// Dock / 桌面互斥布局：apps 列表只渲染"在桌面"的应用（不含 settings / 已进 Dock 的）。
const dockLayout = useDockLayout()
const { isOnDesktop, moveToDock } = dockLayout

// —— 桌面图标配置（路由 + 内联 SVG + 标签 + 配色）——
interface DesktopApp {
    label: string
    route: string
    gradient: string
    icon: string
}

/**
 * 桌面图标完整配置（全量，含 route + 内联 SVG 标签 + 配色）。
 * 实际渲染的 apps = 过滤掉"已移到 Dock"的应用（settings 不在此列表，永不显示在桌面）。
 */
const allApps: DesktopApp[] = [
    {
        label: '存储管理',
        route: '/storage',
        gradient: 'linear-gradient(135deg, #E95420 0%, #2C001E 100%)',
        icon: 'storage',
    },
    {
        label: '虚拟机',
        route: '/vms',
        gradient: 'linear-gradient(135deg, #5e5ce6 0%, #8e8ef0 100%)',
        icon: 'vm',
    },
    {
        label: '文件共享',
        route: '/shares',
        gradient: 'linear-gradient(135deg, #0E8420 0%, #6ee08a 100%)',
        icon: 'share',
    },
    {
        label: '用户管理',
        route: '/users',
        gradient: 'linear-gradient(135deg, #F99B11 0%, #ffc56b 100%)',
        icon: 'users',
    },
    {
        label: '联邦节点',
        route: '/nodes',
        gradient: 'linear-gradient(135deg, #bf5af2 0%, #d99bff 100%)',
        icon: 'nodes',
    },
    {
        label: 'IM',
        route: '/chat',
        gradient: 'linear-gradient(135deg, #36d1dc 0%, #5b86e5 100%)',
        icon: 'chat',
    },
    // 「模型对话」已并入「模型管理」(/llm) 的「对话」Tab，不再单独占桌面图标
    {
        label: '网络管理',
        route: '/network',
        gradient: 'linear-gradient(135deg, #30b0c7 0%, #66d6e7 100%)',
        icon: 'network',
    },
    {
        label: '系统自举',
        route: '/provisioning',
        gradient: 'linear-gradient(135deg, #64d2ff 0%, #a3e3ff 100%)',
        icon: 'provisioning',
    },
    {
        label: '更新',
        route: '/update',
        gradient: 'linear-gradient(135deg, #f7971e 0%, #ffd200 100%)',
        icon: 'update',
    },
    {
        label: '远程转发',
        route: '/forwarding',
        gradient: 'linear-gradient(135deg, #11998e 0%, #38ef7d 100%)',
        icon: 'forwarding',
    },
    // 「直播」已随流媒体中心剥离为独立应用包 apps/streaming 的「直播」Tab
    {
        label: '管理',
        route: '/terminal',
        gradient: 'linear-gradient(135deg, #1f2933 0%, #3b4252 100%)',
        icon: 'terminal',
    },
    {
        label: '备份管理',
        route: '/backup',
        gradient: 'linear-gradient(135deg, #5ac8fa 0%, #34aadc 100%)',
        icon: 'backup',
    },
    {
        label: '系统监控',
        route: '/monitor',
        gradient: 'linear-gradient(135deg, #ff375f 0%, #ff7a93 100%)',
        icon: 'monitor',
    },
    {
        label: '文件管理',
        route: '/files',
        gradient: 'linear-gradient(135deg, #fa709a 0%, #fee140 100%)',
        icon: 'files',
    },
    {
        label: '下载中心',
        route: '/downloads',
        gradient: 'linear-gradient(135deg, #30cfd0 0%, #330867 100%)',
        icon: 'downloads',
    },
    {
        label: '容器管理',
        route: '/containers',
        gradient: 'linear-gradient(135deg, #43e97b 0%, #38f9d7 100%)',
        icon: 'containers',
    },
    {
        label: '监控摄像头',
        route: '/surveillance',
        gradient: 'linear-gradient(135deg, #5ee7df 0%, #b490ca 100%)',
        icon: 'surveillance',
    },
    {
        label: '云同步',
        route: '/cloudsync',
        gradient: 'linear-gradient(135deg, #84fab0 0%, #8fd3f4 100%)',
        icon: 'cloudsync',
    },
    {
        label: '笔记',
        route: '/notes',
        gradient: 'linear-gradient(135deg, #ffecd2 0%, #fcb69f 100%)',
        icon: 'notes',
    },
    // 流媒体中心已剥离为独立应用包（apps/streaming，含「直播」Tab 的 LivePanel）：
    // 安装后经 runtimeApps 动态出现在桌面（见下方 apps computed）。
    {
        label: '模型管理',
        route: '/llm',
        gradient: 'linear-gradient(135deg, #a8edea 0%, #fed6e3 100%)',
        icon: 'llm',
    },
    {
        label: 'API 网关',
        route: '/gateway',
        gradient: 'linear-gradient(135deg, #772953, #E95420)',
        icon: 'gateway',
    },
    {
        label: '区块链管理',
        route: '/blockchain',
        gradient: 'linear-gradient(135deg, #2C001E, #772953)',
        icon: 'blockchain',
    },
    // 「模型仓库」已并入「模型管理」(/llm) 的一级分组「仓库」（本地大厅/联邦大厅
    // 同款二级 Tab），不再单独占桌面图标
    {
        label: '应用中心',
        route: '/appstore',
        gradient: 'linear-gradient(135deg, #E95420, #F99B11)',
        icon: 'appstore',
    },
    {
        label: 'Agent 集合',
        route: '/agenthub',
        gradient: 'linear-gradient(135deg, #4f46e5 0%, #06b6d4 100%)',
        icon: 'agenthub',
    },
    // 二维码传输已剥离为独立应用包（apps/qrtransfer）：安装后经 runtimeApps
    // 动态出现在桌面（见下方 apps computed）。
    {
        label: 'NexHub',
        route: '/codehub',
        gradient: 'linear-gradient(135deg, #24292e, #586069)',
        icon: 'codehub',
    },
    {
        label: '开发者中心',
        route: '/devdocs',
        gradient: 'linear-gradient(135deg, #141e30, #586069)',
        icon: 'devdocs',
    },
    {
        label: '影院',
        route: '/video',
        gradient: 'linear-gradient(135deg, #667eea 0%, #764ba2 100%)',
        icon: 'video',
    },
    {
        label: '音乐',
        route: '/music',
        gradient: 'linear-gradient(135deg, #f093fb 0%, #f5576c 100%)',
        icon: 'music',
    },
    {
        label: '相册',
        route: '/photo',
        gradient: 'linear-gradient(135deg, #4facfe 0%, #00f2fe 100%)',
        icon: 'photo',
    },
    // 影片制作已剥离为独立应用包：安装后经 runtimeApps 动态出现在桌面（见下方 apps computed）。
]

/**
 * 实际渲染的桌面图标：内置应用 + 运行时应用包（应用中心安装后动态出现，
 * 卸载即消失），再过滤掉"在桌面"的应用（isOnDesktop = 不在 Dock）。
 * isOnDesktop(id) = 不在 Dock；settings 不在 allApps，故桌面永不显示设置。
 * computed 自动跟随 dockAppIds / runtimeApps 变化（拖到 Dock 即从桌面消失）。
 */
const apps = computed<DesktopApp[]>(() =>
    [
        ...allApps,
        // 运行时应用包：icon 字段用 id（窗口 id / 位置持久化 / Dock 键的稳定键）；
        // 图标渲染走 AppIcon 的 runtimeIcons（注册名 = id，见 appRuntime）。
        ...runtimeApps.map((a) => ({
            label: a.label,
            route: a.route,
            gradient: a.gradient,
            icon: a.id,
        })),
    ].filter((a) => isOnDesktop(a.icon)),
)

/**
 * 桌面可见图标：排除已进入文件夹的应用（它们渲染在文件夹内部，不占桌面位）。
 * 模板 v-for 用这个；避免文件夹内的应用同时出现在桌面和文件夹里。
 */
const visibleApps = computed<DesktopApp[]>(() => {
    const foldered = new Set<string>()
    for (const f of Object.values(folders)) {
        for (const aid of f.appIds) foldered.add(aid)
    }
    return apps.value.filter((a) => !foldered.has(a.icon))
})

function openApp(app: DesktopApp) {
    // 桌面图标点击 -> 打开浮窗（已开则聚焦/还原）。
    // app.icon 同时是窗口 id 与 appRegistry 的 key。窗口管理逻辑不动。
    // 标题走 getAppName：改名后打开的窗口直接使用新名。
    // 未注册进 appRegistry 的应用（如 远程转发）→ 路由跳转全屏页兜底，
    // 避免打开空窗口（WindowFrame 对未注册 id 只会渲染"未注册应用"占位）。
    if (!appRegistry[app.icon]) {
        void router.push(app.route)
        return
    }
    wm.openWindow({
        id: app.icon,
        title: getAppName(app.icon),
        icon: app.icon,
    })
}

// =============================================================================
// 自由拖拽定位 —— 绝对定位 + localStorage 持久化
// =============================================================================
const STORAGE_KEY = 'os-icon-positions'

/** 单个图标坐标。 */
interface IconPos {
    x: number
    y: number
}

/** 图标定位常量（与 CSS 的 .app-icon 尺寸一致：宽 92，含标签总高约 120）。 */
const ICON_WIDTH = 62
const ICON_HEIGHT = 80
const ICON_GAP_X = 12
const ICON_GAP_Y = 10

/** 图标 id(app.icon) -> {x,y}。reactive 让 :style 自动跟随更新。 */
const iconPositions = reactive<Record<string, IconPos>>({})

/** 记录已手动定位过的图标 id（持久化在 localStorage，避免 resize 时被默认布局覆盖）。 */
const manuallyPlaced = reactive<Record<string, boolean>>({})

/** 当前正在拖拽的图标 id（用于绑定 .dragging 类）。 */
const draggingId = ref<string | null>(null)

/** 容器尺寸（用于默认布局计算 + 边界约束）。 */
const containerSize = reactive({ width: 0, height: 0 })

/** 桌面图标网格容器引用。 */
const gridRef = ref<HTMLElement | null>(null)
let resizeObserver: ResizeObserver | null = null

/**
 * 纯函数：按容器宽度生成从左上角开始的网格布局坐标。
 * - 每列宽 ICON_WIDTH + ICON_GAP_X，每行高 ICON_HEIGHT + ICON_GAP_Y
 * - 返回顺序坐标数组（调用方按 app.icon 顺序映射）
 */
function computeDefaultLayout(appCount: number, containerWidth: number): IconPos[] {
    const stepX = ICON_WIDTH + ICON_GAP_X
    const stepY = ICON_HEIGHT + ICON_GAP_Y
    // 至少 1 列，防止容器宽度为 0 时除零。
    const perRow = Math.max(1, Math.floor((containerWidth + ICON_GAP_X) / stepX))
    const positions: IconPos[] = []
    for (let i = 0; i < appCount; i++) {
        const col = i % perRow
        const row = Math.floor(i / perRow)
        positions.push({ x: col * stepX, y: row * stepY })
    }
    return positions
}

/** 读 localStorage（解析失败返回 null，调用方回退默认布局）。 */
function loadStoredPositions(): { positions: Record<string, IconPos>; placed: Record<string, boolean> } | null {
    try {
        const raw = localStorage.getItem(STORAGE_KEY)
        if (!raw) return null
        const parsed = JSON.parse(raw) as unknown
        if (typeof parsed !== 'object' || parsed === null) return null
        const positions: Record<string, IconPos> = {}
        const placed: Record<string, boolean> = {}
        for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
            if (
                v &&
                typeof v === 'object' &&
                typeof (v as IconPos).x === 'number' &&
                typeof (v as IconPos).y === 'number' &&
                Number.isFinite((v as IconPos).x) &&
                Number.isFinite((v as IconPos).y)
            ) {
                positions[k] = { x: (v as IconPos).x, y: (v as IconPos).y }
                placed[k] = true
            }
        }
        return { positions, placed }
    } catch {
        return null
    }
}

/** 写 localStorage（仅写已手动定位的图标，节省空间且语义清晰）。 */
function savePositions(): void {
    const data: Record<string, IconPos> = {}
    for (const id of Object.keys(manuallyPlaced)) {
        if (manuallyPlaced[id] && iconPositions[id]) {
            data[id] = { ...iconPositions[id] }
        }
    }
    try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(data))
    } catch {
        // 配额满 / 隐私模式：静默失败，不影响拖拽功能。
    }
}

// —— 文件夹持久化 ——
/** 读 localStorage 中的文件夹数据。解析失败返回空对象。 */
function loadFolders(): Record<string, DesktopFolder> {
    try {
        const raw = localStorage.getItem(FOLDER_STORAGE_KEY)
        if (!raw) return {}
        const parsed = JSON.parse(raw) as unknown
        if (typeof parsed !== 'object' || parsed === null) return {}
        const result: Record<string, DesktopFolder> = {}
        for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
            if (
                v &&
                typeof v === 'object' &&
                typeof (v as DesktopFolder).id === 'string' &&
                typeof (v as DesktopFolder).name === 'string' &&
                Array.isArray((v as DesktopFolder).appIds) &&
                typeof (v as DesktopFolder).x === 'number' &&
                typeof (v as DesktopFolder).y === 'number'
            ) {
                const f = v as DesktopFolder
                result[k] = {
                    id: f.id,
                    name: f.name,
                    appIds: [...f.appIds].filter((a) => typeof a === 'string'),
                    x: f.x,
                    y: f.y,
                }
            }
        }
        return result
    } catch {
        return {}
    }
}

/** 写 localStorage：序列化当前所有文件夹。 */
function saveFolders(): void {
    try {
        const data: Record<string, DesktopFolder> = {}
        for (const [id, f] of Object.entries(folders)) {
            data[id] = {
                id: f.id,
                name: f.name,
                appIds: [...f.appIds],
                x: f.x,
                y: f.y,
            }
        }
        localStorage.setItem(FOLDER_STORAGE_KEY, JSON.stringify(data))
    } catch {
        // 静默失败
    }
}

/**
 * 把一个应用从文件夹移除并放回桌面（重新分配位置）。
 * 如果文件夹变空则删除文件夹。
 */
function removeFromFolder(folderId: string, appId: string): void {
    const folder = folders[folderId]
    if (!folder) return
    folder.appIds = folder.appIds.filter((a) => a !== appId)
    if (folder.appIds.length === 0) {
        // 空文件夹自动删除
        delete folders[folderId]
        if (openFolderId.value === folderId) openFolderId.value = null
        saveFolders()
        return
    }
    saveFolders()
}

/** 双击文件夹 -> 展开面板。 */
function openFolder(folderId: string): void {
    if (!folders[folderId]) return
    openFolderId.value = folderId
}

// —— 右键菜单 —— //
// 三种目标共用一个菜单：桌面空白（创建文件夹）/ 应用图标（重命名）/ 文件夹图标（重命名/删除）。
type MenuKind = 'desktop' | 'app' | 'folder'
const contextMenu = reactive({
    visible: false,
    x: 0,
    y: 0,
    kind: 'desktop' as MenuKind,
    /** 目标 id（app.icon 或 folder id），desktop 时为空。 */
    targetId: '',
})

/** 在事件坐标处打开指定目标类型的菜单。 */
function openContextMenuAt(e: MouseEvent, kind: MenuKind, targetId = ''): void {
    contextMenu.visible = true
    contextMenu.kind = kind
    contextMenu.targetId = targetId
    contextMenu.x = e.clientX
    contextMenu.y = e.clientY
}

function onContextMenu(e: MouseEvent): void {
    // 窗口内部右键不弹桌面菜单。
    const target = e.target as HTMLElement
    if (target.closest('.window-frame')) return
    // 命中图标（含文件夹图标，folder-icon 同时带 app-icon class）：按 data 属性分流。
    const iconEl = target.closest('.app-icon') as HTMLElement | null
    if (iconEl?.dataset.folderId) {
        openContextMenuAt(e, 'folder', iconEl.dataset.folderId)
        return
    }
    if (iconEl?.dataset.appId) {
        openContextMenuAt(e, 'app', iconEl.dataset.appId)
        return
    }
    // 桌面空白区。
    openContextMenuAt(e, 'desktop')
}

function closeContextMenu(): void {
    contextMenu.visible = false
}

// —— 改名弹窗（应用 / 文件夹共用，Ubuntu Yaru 对话框风格）—— //
const renameDialog = reactive({
    visible: false,
    kind: 'app' as 'app' | 'folder',
    id: '',
    value: '',
})
/** 弹窗输入框引用（打开后自动聚焦）。 */
const renameInputRef = ref<HTMLInputElement | null>(null)

function openRenameDialog(kind: 'app' | 'folder', id: string): void {
    renameDialog.kind = kind
    renameDialog.id = id
    renameDialog.value = kind === 'app' ? getAppName(id) : (folders[id]?.name ?? '')
    renameDialog.visible = true
    closeContextMenu()
    void nextTick(() => renameInputRef.value?.focus())
}

function confirmRename(): void {
    if (renameDialog.kind === 'app') {
        setAppName(renameDialog.id, renameDialog.value)
    } else {
        renameFolder(renameDialog.id, renameDialog.value)
    }
    renameDialog.visible = false
}

function cancelRename(): void {
    renameDialog.visible = false
}

/** 右键菜单"创建文件夹"：在右键位置创建空文件夹。 */
function createEmptyFolder(): void {
    const folderId = 'folder-' + Date.now()
    // 转换屏幕坐标到桌面坐标（减去 desktop-root 的 offset）
    const root = document.querySelector('.desktop-root') as HTMLElement
    const rect = root?.getBoundingClientRect()
    const localX = contextMenu.x - (rect?.left ?? 0)
    const localY = contextMenu.y - (rect?.top ?? 0)
    folders[folderId] = {
        id: folderId,
        name: '新建文件夹',
        appIds: [],
        x: Math.max(0, localX),
        y: Math.max(0, localY),
    }
    saveFolders()
    closeContextMenu()
    // 自动打开文件夹面板准备放应用
    openFolderId.value = folderId
}

/** 关闭文件夹面板。 */
function closeFolder(): void {
    openFolderId.value = null
}

/** 在文件夹展开面板里点击某个应用 -> 打开该应用窗口（标题跟随自定义名）。 */
function openAppFromFolder(appId: string): void {
    const meta = findApp(appId)
    if (!meta) return
    wm.openWindow({
        id: appId,
        title: getAppName(appId),
        icon: appId,
    })
}

/** 在文件夹展开面板里点击 × 移除应用 -> 放回桌面（重新分配默认位置）。 */
function removeAppFromFolderPanel(folderId: string, appId: string): void {
    removeFromFolder(folderId, appId)
    // 应用回到桌面：清除 manuallyPlaced，让 applyDefaultLayoutForUnplaced 重新分配位置
    delete manuallyPlaced[appId]
    delete iconPositions[appId]
    // 用 nextTick 风格的延迟确保容器已测量
    applyDefaultLayoutForUnplaced()
    savePositions()
}

/** 文件夹改名：新名为空 = 删除文件夹（应用放回桌面），否则更新并持久化。 */
function renameFolder(folderId: string, newName: string): void {
    const folder = folders[folderId]
    if (!folder) return
    const val = newName.trim()
    if (val === '') {
        deleteFolder(folderId)
        return
    }
    folder.name = val
    saveFolders()
}

/** 删除文件夹：内部应用放回桌面（重新分配默认位置），并持久化。 */
function deleteFolder(folderId: string): void {
    const folder = folders[folderId]
    if (!folder) return
    for (const aid of folder.appIds) {
        delete manuallyPlaced[aid]
        delete iconPositions[aid]
    }
    delete folders[folderId]
    if (openFolderId.value === folderId) openFolderId.value = null
    saveFolders()
    applyDefaultLayoutForUnplaced()
    savePositions()
    closeContextMenu()
}

/** 文件夹改名（input 实时同步）。名为空时自动删除文件夹（应用放回桌面）。 */
function onFolderNameInput(e: Event, folderId: string): void {
    if (!folders[folderId]) return
    renameFolder(folderId, (e.target as HTMLInputElement).value)
}

/**
 * 用默认网格坐标填充尚未定位的图标（保留已定位的图标不动）。
 * 调用时机：首次加载、窗口 resize、应用数量变化（应用移入/移出 Dock）。
 */
function applyDefaultLayoutForUnplaced(): void {
    const list = visibleApps.value
    const defaults = computeDefaultLayout(list.length, containerSize.width)
    list.forEach((app, idx) => {
        if (manuallyPlaced[app.icon] && iconPositions[app.icon]) return
        const pos = defaults[idx]
        if (pos) {
            // 直接赋值以触发 reactive；用新对象避免引用同一份 defaults。
            iconPositions[app.icon] = { x: pos.x, y: pos.y }
        }
    })
}

/** 边界约束：把坐标限制在桌面区内，图标不可拖出可视范围。 */
function clampPos(pos: IconPos): IconPos {
    const maxX = Math.max(0, containerSize.width - ICON_WIDTH)
    const maxY = Math.max(0, containerSize.height - ICON_HEIGHT)
    return {
        x: Math.min(Math.max(0, pos.x), maxX),
        y: Math.min(Math.max(0, pos.y), maxY),
    }
}

/**
 * 碰撞检测：检查 pos 与其它图标是否重叠（矩形相交）。
 * 如果重叠，沿原方向偏移直到不重叠或到达边界。
 */
function avoidOverlap(pos: IconPos, appId: string): IconPos {
    const margin = 4 // 图标间最小间距
    const minDistX = ICON_WIDTH + margin
    const minDistY = ICON_HEIGHT + margin
    let result = { ...pos }
    for (let attempt = 0; attempt < 50; attempt++) {
        let collided = false
        // 检查与其它桌面图标重叠
        for (const [id, other] of Object.entries(iconPositions)) {
            if (id === appId) continue
            // 检查矩形相交
            const overlapX = Math.abs(result.x - other.x) < minDistX
            const overlapY = Math.abs(result.y - other.y) < minDistY
            if (overlapX && overlapY) {
                collided = true
                // 沿拖拽方向偏移（向右下推）
                const dx = result.x >= other.x ? 1 : -1
                const dy = result.y >= other.y ? 1 : -1
                result.x += dx * 8
                result.y += dy * 8
                result = clampPos(result)
                break
            }
        }
        // 检查与文件夹重叠（文件夹也占位）
        if (!collided) {
            for (const [, f] of Object.entries(folders)) {
                const overlapX = Math.abs(result.x - f.x) < minDistX
                const overlapY = Math.abs(result.y - f.y) < minDistY
                if (overlapX && overlapY) {
                    collided = true
                    const dx = result.x >= f.x ? 1 : -1
                    const dy = result.y >= f.y ? 1 : -1
                    result.x += dx * 8
                    result.y += dy * 8
                    result = clampPos(result)
                    break
                }
            }
        }
        if (!collided) break
    }
    return result
}

// —— 拖拽：mousedown -> document mousemove 更新坐标 -> mouseup 收尾 ——
// 当前拖拽会话状态（用普通对象，非响应式，避免拖拽过程多余渲染）。
const dragSession = {
    active: false,
    appId: '',
    startMouseX: 0,
    startMouseY: 0,
    startIconX: 0,
    startIconY: 0,
    moved: false, // 是否达到拖拽阈值（>= 5px）
}

/** 拖拽阈值：移动距离 < 此值视为点击（触发 openApp），>= 此值视为拖拽。 */
const DRAG_THRESHOLD = 5

/**
 * 查找图标落点是否命中另一个桌面图标或文件夹。
 * 命中判定：中心距 < ICON_WIDTH * 0.6（约重叠超过 50% 面积）。
 * 返回命中的目标 id（普通 app id 或 'folder-xxx'），未命中返回 null。
 */
function findDropTarget(appId: string, x: number, y: number): string | null {
    const cx = x + ICON_WIDTH / 2
    const cy = y + ICON_HEIGHT / 2
    const threshold = ICON_WIDTH * 0.6
    // 检查与其它桌面图标
    for (const [id, pos] of Object.entries(iconPositions)) {
        if (id === appId) continue
        const ox = pos.x + ICON_WIDTH / 2
        const oy = pos.y + ICON_HEIGHT / 2
        if (Math.hypot(cx - ox, cy - oy) < threshold) return id
    }
    // 检查与文件夹
    for (const [fid, folder] of Object.entries(folders)) {
        const ox = folder.x + ICON_WIDTH / 2
        const oy = folder.y + ICON_HEIGHT / 2
        if (Math.hypot(cx - ox, cy - oy) < threshold) return fid
    }
    return null
}

function onIconDragStart(e: MouseEvent, appId: string): void {
    // 仅左键开始拖拽。
    if (e.button !== 0) return
    const current = iconPositions[appId]
    if (!current) return
    e.preventDefault()

    dragSession.active = true
    dragSession.appId = appId
    dragSession.startMouseX = e.clientX
    dragSession.startMouseY = e.clientY
    dragSession.startIconX = current.x
    dragSession.startIconY = current.y
    dragSession.moved = false

    document.addEventListener('mousemove', onDragMove)
    document.addEventListener('mouseup', onDragEnd)
}

/** 拖拽时指针是否在 Dock 区域（用于高亮提示）。 */
const dockDraggingHover = ref(false)

function onDragMove(e: MouseEvent): void {
    if (!dragSession.active) return
    const dx = e.clientX - dragSession.startMouseX
    const dy = e.clientY - dragSession.startMouseY

    // 达到拖拽阈值才算"真拖拽"：此时才置 draggingId + 标记已移动。
    if (!dragSession.moved && Math.hypot(dx, dy) >= DRAG_THRESHOLD) {
        dragSession.moved = true
        draggingId.value = dragSession.appId
    }
    if (!dragSession.moved) return

    const raw = {
        x: dragSession.startIconX + dx,
        y: dragSession.startIconY + dy,
    }
    // Dock 区域高亮联动
    dockDraggingHover.value = pointerInDock(e.clientX, e.clientY)

    // 不在 Dock 区域时才做碰撞检测（Dock 区域允许重叠，因为要移走）
    if (!dockDraggingHover.value) {
        const clamped = clampPos(raw)
        const safe = avoidOverlap(clamped, dragSession.appId)
        iconPositions[dragSession.appId] = safe
    } else {
        iconPositions[dragSession.appId] = clampPos(raw)
    }
}

function onDragEnd(e: MouseEvent): void {
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
    if (!dragSession.active) return

    const wasMoved = dragSession.moved
    const appId = dragSession.appId

    if (wasMoved) {
        // 检测松手位置是否落在 Dock 区域内（拖到 Dock）。
        // 命中判定：指针在 .dock 元素的矩形里（elementFromPoint 兜底）。
        const onDock = pointerInDock(e.clientX, e.clientY)
        if (onDock) {
            // 拖到 Dock：从桌面移除（moveToDock 互斥），清理其桌面位置记录，
            // 这样日后从 Dock 拖回桌面时会重新分配默认位置。
            delete manuallyPlaced[appId]
            delete iconPositions[appId]
            savePositions()
            moveToDock(appId)
        } else {
            // 真拖拽（仍在桌面）：检测是否落在另一个图标/文件夹上 -> 合并
            const cur = iconPositions[appId]
            const targetId = cur ? findDropTarget(appId, cur.x, cur.y) : null
            if (targetId && targetId.startsWith('folder-')) {
                // 落在已有文件夹上：加入该文件夹
                const folder = folders[targetId]
                if (folder && !folder.appIds.includes(appId)) {
                    folder.appIds.push(appId)
                    delete iconPositions[appId]
                    delete manuallyPlaced[appId]
                    saveFolders()
                    savePositions()
                }
            } else if (targetId) {
                // 落在另一个普通图标上：创建新文件夹
                const targetPos = iconPositions[targetId]
                if (targetPos) {
                    const folderId = 'folder-' + Date.now()
                    folders[folderId] = {
                        id: folderId,
                        name: '文件夹',
                        appIds: [targetId, appId],
                        x: targetPos.x,
                        y: targetPos.y,
                    }
                    delete iconPositions[appId]
                    delete iconPositions[targetId]
                    delete manuallyPlaced[appId]
                    delete manuallyPlaced[targetId]
                    saveFolders()
                    savePositions()
                }
            } else {
                // 未命中任何目标：标记手动定位 + 持久化
                manuallyPlaced[appId] = true
                savePositions()
            }
        }
    }
    draggingId.value = null
    dockDraggingHover.value = false
    dragSession.active = false
    dragSession.appId = ''
    dragSession.moved = false
    // 注意：不在此处 openApp。
    // 点击（未达阈值）由 @click 处理；拖拽（已达阈值）在 click 守卫中被拦截。
}

/**
 * 判断指针(clientX, clientY)是否落在底部 Dock drop zone 内。
 *
 * 与 MainLayout 的透明 drop zone（底部 120px 全宽）保持一致：
 * 只要指针进入屏幕底部 120px 区域即视为命中 Dock，配合大透明
 * drop zone 让用户轻松把桌面图标拖进 Dock。
 */
function pointerInDock(clientX: number, clientY: number): boolean {
    const vh = typeof window !== 'undefined' ? window.innerHeight : 800
    const vw = typeof window !== 'undefined' ? window.innerWidth : 1280
    return clientY > vh - 120 && clientX >= 0 && clientX <= vw
}

/**
 * @dblclick 处理：双击桌面图标打开应用浮窗。
 * 单击只选中（高亮），拖拽用于移动位置——不会打开应用。
 */
function onIconDblClick(app: DesktopApp): void {
    openApp(app)
}

// —— 文件夹拖拽 ——
// 文件夹复用与图标相同的 document mousemove/mouseup 机制，但通过单独的
// dragSession 标记 folderId 来区分（拖文件夹时不参与合并检测）。
const folderDragSession = {
    active: false,
    folderId: '',
    startMouseX: 0,
    startMouseY: 0,
    startFolderX: 0,
    startFolderY: 0,
    moved: false,
}

/** 文件夹图标 mousedown：开始拖拽文件夹。 */
function onFolderDragStart(e: MouseEvent, folderId: string): void {
    if (e.button !== 0) return
    const folder = folders[folderId]
    if (!folder) return
    // 双击事件会在 mousedown 后触发；这里不阻止默认，但要让 dblclick 仍能冒泡。
    // 不过为避免与拖拽冲突，仅标记会话，达到阈值才真正开始拖拽。
    folderDragSession.active = true
    folderDragSession.folderId = folderId
    folderDragSession.startMouseX = e.clientX
    folderDragSession.startMouseY = e.clientY
    folderDragSession.startFolderX = folder.x
    folderDragSession.startFolderY = folder.y
    folderDragSession.moved = false

    document.addEventListener('mousemove', onFolderDragMove)
    document.addEventListener('mouseup', onFolderDragEnd)
}

function onFolderDragMove(e: MouseEvent): void {
    if (!folderDragSession.active) return
    const dx = e.clientX - folderDragSession.startMouseX
    const dy = e.clientY - folderDragSession.startMouseY
    if (!folderDragSession.moved && Math.hypot(dx, dy) >= DRAG_THRESHOLD) {
        folderDragSession.moved = true
    }
    if (!folderDragSession.moved) return
    const folder = folders[folderDragSession.folderId]
    if (!folder) return
    const raw = {
        x: folderDragSession.startFolderX + dx,
        y: folderDragSession.startFolderY + dy,
    }
    const clamped = clampPos(raw)
    folder.x = clamped.x
    folder.y = clamped.y
}

function onFolderDragEnd(): void {
    document.removeEventListener('mousemove', onFolderDragMove)
    document.removeEventListener('mouseup', onFolderDragEnd)
    if (!folderDragSession.active) return
    if (folderDragSession.moved) {
        // 持久化文件夹位置
        saveFolders()
    }
    folderDragSession.active = false
    folderDragSession.folderId = ''
    folderDragSession.moved = false
}

onMounted(() => {
    // 0. 读持久化文件夹。
    const storedFolders = loadFolders()
    Object.assign(folders, storedFolders)

    // 1. 读持久化位置。
    const stored = loadStoredPositions()
    if (stored) {
        Object.assign(iconPositions, stored.positions)
        Object.assign(manuallyPlaced, stored.placed)
    }

    // 2. 测量容器尺寸 + 首次填充默认布局。
    const measureAndLayout = (): void => {
        const el = gridRef.value
        if (!el) return
        const rect = el.getBoundingClientRect()
        containerSize.width = rect.width
        containerSize.height = rect.height
        applyDefaultLayoutForUnplaced()
    }
    measureAndLayout()

    // 3. 监听容器尺寸变化：仅重新计算未手动定位的图标（保留用户拖过的位置）。
    if (gridRef.value && typeof ResizeObserver !== 'undefined') {
        resizeObserver = new ResizeObserver(() => {
            measureAndLayout()
        })
        resizeObserver.observe(gridRef.value)
    }

    // 4. 监听桌面应用集合变化（应用移入/移出 Dock / 文件夹）：
    //    新进桌面的应用无 manuallyPlaced -> 由 applyDefaultLayoutForUnplaced 分配默认位置。
    watch(
        () => visibleApps.value.length,
        () => {
            measureAndLayout()
        },
    )

    // 5. 点击空白处关闭右键菜单
    document.addEventListener('click', closeContextMenu)
})

onBeforeUnmount(() => {
    document.removeEventListener('click', closeContextMenu)
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
    document.removeEventListener('mousemove', onFolderDragMove)
    document.removeEventListener('mouseup', onFolderDragEnd)
    if (resizeObserver) {
        resizeObserver.disconnect()
        resizeObserver = null
    }
})
</script>

<template>
    <div class="desktop-root" @contextmenu.prevent="onContextMenu">
        <!-- 右键菜单（桌面空白 / 应用图标 / 文件夹图标） -->
        <div
            v-if="contextMenu.visible"
            class="context-menu"
            :style="{ left: contextMenu.x + 'px', top: contextMenu.y + 'px' }"
            @click.stop
        >
            <template v-if="contextMenu.kind === 'desktop'">
                <button class="ctx-item" @click="createEmptyFolder">📁 创建文件夹</button>
            </template>
            <template v-else-if="contextMenu.kind === 'app'">
                <button class="ctx-item" @click="openRenameDialog('app', contextMenu.targetId)">
                    ✏️ 重命名
                </button>
                <button
                    v-if="hasCustomName(contextMenu.targetId)"
                    class="ctx-item"
                    @click="resetAppName(contextMenu.targetId); closeContextMenu()"
                >
                    ↩️ 恢复默认名称
                </button>
            </template>
            <template v-else>
                <button class="ctx-item" @click="openRenameDialog('folder', contextMenu.targetId)">
                    ✏️ 重命名
                </button>
                <button class="ctx-item danger" @click="deleteFolder(contextMenu.targetId)">
                    🗑️ 删除文件夹
                </button>
            </template>
        </div>

        <!-- 改名弹窗（应用 / 文件夹共用，Ubuntu Yaru 对话框风格） -->
        <div v-if="renameDialog.visible" class="rename-overlay" @click.self="cancelRename">
            <div class="rename-dialog" role="dialog" aria-modal="true">
                <div class="rename-title">
                    {{ renameDialog.kind === 'app' ? '重命名应用' : '重命名文件夹' }}
                </div>
                <input
                    ref="renameInputRef"
                    v-model="renameDialog.value"
                    class="rename-input"
                    type="text"
                    maxlength="24"
                    @keyup.enter="confirmRename"
                    @keyup.esc="cancelRename"
                />
                <div class="rename-hint">
                    {{
                        renameDialog.kind === 'app'
                            ? '改名后桌面 / Dock / 窗口标题同步更新；留空确定 = 恢复默认名称'
                            : '留空确定 = 删除文件夹（应用放回桌面）'
                    }}
                </div>
                <div class="rename-actions">
                    <button class="rename-btn cancel" type="button" @click="cancelRename">取消</button>
                    <button class="rename-btn ok" type="button" @click="confirmRename">确定</button>
                </div>
            </div>
        </div>
        <!-- Dock 拖拽高亮提示（拖动图标到底部时显示） -->
        <div v-if="dockDraggingHover" class="dock-drop-hint">
            拖到此处固定到 Dock 栏
        </div>

        <!-- 应用包加载失败占位卡（含重试；不阻塞桌面其余部分） -->
        <div v-if="appLoadFailures.length" class="app-fail-panel">
            <div class="app-fail-head">{{ t('apps.loadErrorTitle') }}</div>
            <div v-for="f in appLoadFailures" :key="f.id" class="app-fail-item">
                <div class="app-fail-text">
                    <span class="app-fail-name">{{ t('apps.loadFailed', { name: f.name }) }}</span>
                    <span class="app-fail-err">{{ f.error }}</span>
                </div>
                <button class="app-fail-btn retry" type="button" @click="retryApp(f.id)">
                    {{ t('apps.retry') }}
                </button>
                <button class="app-fail-btn dismiss" type="button" :title="t('apps.dismiss')" @click="dismissAppFailure(f.id)">
                    ×
                </button>
            </div>
        </div>

        <!-- 桌面图标：绝对定位，可自由拖拽，位置持久化（壁纸透出） -->
        <div ref="gridRef" class="icon-grid">
            <button
                v-for="app in visibleApps"
                :key="app.route"
                class="app-icon"
                :class="{ dragging: draggingId === app.icon }"
                type="button"
                :data-app-id="app.icon"
                :title="getAppName(app.icon)"
                :style="
                    iconPositions[app.icon]
                        ? { left: iconPositions[app.icon].x + 'px', top: iconPositions[app.icon].y + 'px' }
                        : undefined
                "
                @mousedown="onIconDragStart($event, app.icon)"
                @dblclick="onIconDblClick(app)"
                @contextmenu.prevent="openContextMenuAt($event, 'app', app.icon)"
            >
                <span class="app-icon-tile" :style="{ background: app.gradient }">
                    <!-- storage.svg -->
                    <svg
                        v-if="app.icon === 'storage'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <ellipse cx="12" cy="5" rx="8" ry="2.5" />
                        <path d="M4 5v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5V5" />
                        <path d="M4 11v6c0 1.38 3.58 2.5 8 2.5s8-1.12 8-2.5v-6" />
                    </svg>
                    <!-- vm.svg -->
                    <svg
                        v-else-if="app.icon === 'vm'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="2.5" y="4" width="19" height="13" rx="2" />
                        <path d="M8 21h8" />
                        <path d="M12 17v4" />
                        <path d="M7 9l-2 2 2 2" />
                        <path d="M17 9l2 2-2 2" />
                        <path d="M13.5 8l-3 6" />
                    </svg>
                    <!-- share.svg -->
                    <svg
                        v-else-if="app.icon === 'share'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path
                            d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"
                        />
                        <circle cx="16.5" cy="11.5" r="1.8" />
                        <path d="M18 11.5V8.5a2 2 0 0 0-2-2h-2" />
                    </svg>
                    <!-- users.svg -->
                    <svg
                        v-else-if="app.icon === 'users'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <circle cx="9" cy="8" r="3.2" />
                        <path d="M3.5 20a5.5 5.5 0 0 1 11 0" />
                        <path d="M16 5.2a3 3 0 0 1 0 5.6" />
                        <path d="M17.5 14.2a5.5 5.5 0 0 1 3 5.8" />
                    </svg>
                    <!-- nodes.svg -->
                    <svg
                        v-else-if="app.icon === 'nodes'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <circle cx="6" cy="6" r="2.2" />
                        <circle cx="18" cy="6" r="2.2" />
                        <circle cx="12" cy="18" r="2.2" />
                        <path d="M7.6 7.6l3.2 8.4" />
                        <path d="M16.4 7.6l-3.2 8.4" />
                        <path d="M8.2 6h7.6" />
                    </svg>
                    <!-- chat.svg (聊天气泡) -->
                    <svg
                        v-else-if="app.icon === 'chat'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path
                            d="M4 5a2 2 0 0 1 2-2h12a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H9l-4 4v-4H6a2 2 0 0 1-2-2z"
                        />
                        <circle cx="8.5" cy="9.5" r="1.2" fill="currentColor" stroke="none" />
                        <circle cx="12" cy="9.5" r="1.2" fill="currentColor" stroke="none" />
                        <circle cx="15.5" cy="9.5" r="1.2" fill="currentColor" stroke="none" />
                    </svg>
                    <!-- network.svg -->
                    <svg
                        v-else-if="app.icon === 'network'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="2.5" y="3" width="7" height="5.5" rx="1" />
                        <rect x="14.5" y="3" width="7" height="5.5" rx="1" />
                        <rect x="8.5" y="15.5" width="7" height="5.5" rx="1" />
                        <path d="M6 8.5v3.5a1 1 0 0 0 1 1h4.5" />
                        <path d="M18 8.5v3.5a1 1 0 0 1-1 1h-4.5" />
                        <path d="M12 13v2.5" />
                    </svg>
                    <!-- provisioning.svg (火箭发射 / 系统自举) -->
                    <svg
                        v-else-if="app.icon === 'provisioning'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M12 3c2.5 1.8 4 4.5 4 8v3h-8v-3c0-3.5 1.5-6.2 4-8z" />
                        <circle cx="12" cy="9" r="1.6" />
                        <path d="M8 14H5.5a1 1 0 0 0-1 1.2l1 4.8h13l1-4.8a1 1 0 0 0-1-1.2H16" />
                        <path d="M9 20v1.5a1 1 0 0 0 1 1h4a1 1 0 0 0 1-1V20" />
                        <path d="M12 14v2.5" />
                    </svg>
                    <!-- update.svg (环形双箭头循环升级 + A/B 槽位点) -->
                    <svg
                        v-else-if="app.icon === 'update'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M4.5 12a7.5 7.5 0 0 1 13-5.1" />
                        <path d="M17.5 4v3.2h-3.2" />
                        <path d="M19.5 12a7.5 7.5 0 0 1-13 5.1" />
                        <path d="M6.5 20v-3.2h3.2" />
                        <circle cx="12" cy="12" r="2.2" />
                        <path d="M12 9.8V7.5" opacity="0.7" />
                        <path d="M12 16.5v-2.3" opacity="0.7" />
                    </svg>
                    <!-- forwarding.svg (双箭头穿透隧道：双向转发) -->
                    <svg
                        v-else-if="app.icon === 'forwarding'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 左右两端节点 -->
                        <rect x="2.5" y="9" width="5" height="6" rx="1" />
                        <rect x="16.5" y="9" width="5" height="6" rx="1" />
                        <!-- 上行：左 → 右 -->
                        <path d="M8.5 10.6h6.4" />
                        <path d="M13.2 8.8l1.8 1.8-1.8 1.8" />
                        <!-- 下行：右 → 左 -->
                        <path d="M15.5 13.4H9.1" />
                        <path d="M10.8 11.6l-1.8 1.8 1.8 1.8" />
                    </svg>
                    <!-- terminal.svg (命令行窗口 + `>_` 提示符：管理 / Web 终端) -->
                    <svg
                        v-else-if="app.icon === 'terminal'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 终端窗口 -->
                        <rect x="2.5" y="4" width="19" height="16" rx="2" />
                        <path d="M2.5 8h19" />
                        <!-- 标题栏圆点 -->
                        <circle cx="5.2" cy="6" r="0.55" fill="currentColor" stroke="none" />
                        <circle cx="7.6" cy="6" r="0.55" fill="currentColor" stroke="none" />
                        <!-- 提示符 `>` 与光标 `_` -->
                        <path d="M6 12.2l2.6 2.3L6 16.8" />
                        <path d="M10.6 17h4" />
                    </svg>
                    <!-- backup.svg (盾牌+时钟回溯) -->
                    <svg
                        v-else-if="app.icon === 'backup'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M12 2.5l7.5 2.8v5.2c0 4.6-3.2 8.4-7.5 9.5-4.3-1.1-7.5-4.9-7.5-9.5V5.3L12 2.5z" />
                        <circle cx="12" cy="10.5" r="3.4" />
                        <path d="M12 8.8v1.9l1.3 1.3" />
                    </svg>
                    <!-- monitor.svg (仪表/图表) -->
                    <svg
                        v-else-if="app.icon === 'monitor'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M3 20h18" />
                        <path d="M5 20a7 7 0 0 1 14 0" />
                        <path d="M12 13v7" />
                        <path d="M12 13l-2.5-3" />
                        <path d="M12 13l2.5-3" />
                        <path d="M6.5 11.5a6 6 0 0 1 11 0" />
                    </svg>
                    <!-- video.svg (播放三角) -->
                    <svg
                        v-else-if="app.icon === 'video'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="2.5" y="5" width="14" height="14" rx="2" />
                        <path d="M16.5 9.5l5-3v11l-5-3z" />
                    </svg>
                    <!-- music.svg (音符) -->
                    <svg
                        v-else-if="app.icon === 'music'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M9 18V6l10-2v12" />
                        <circle cx="6" cy="18" r="3" />
                        <circle cx="16" cy="16" r="3" />
                    </svg>
                    <!-- photo.svg (山+太阳) -->
                    <svg
                        v-else-if="app.icon === 'photo'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="2.5" y="4" width="19" height="16" rx="2" />
                        <circle cx="8" cy="9.5" r="1.8" />
                        <path d="M5 18l5-6 4 4 3-3 4 5" />
                    </svg>
                    <!-- files.svg (文件夹) -->
                    <svg
                        v-else-if="app.icon === 'files'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path
                            d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"
                        />
                        <path d="M3 11h18" />
                    </svg>
                    <!-- downloads.svg (向下箭头入盘) -->
                    <svg
                        v-else-if="app.icon === 'downloads'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M12 3v10" />
                        <path d="M8 10l4 4 4-4" />
                        <path d="M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2" />
                    </svg>
                    <!-- containers.svg (层叠方块) -->
                    <svg
                        v-else-if="app.icon === 'containers'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="8.5" y="2.5" width="7" height="5" rx="1" />
                        <rect x="3" y="9.5" width="7" height="5" rx="1" />
                        <rect x="14" y="9.5" width="7" height="5" rx="1" />
                        <path d="M12 14.5V18" />
                        <path d="M7 21h10" />
                    </svg>
                    <!-- surveillance.svg (摄像头) -->
                    <svg
                        v-else-if="app.icon === 'surveillance'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <rect x="2.5" y="5.5" width="13" height="9" rx="2" />
                        <path d="M15.5 8.5l5-2.5v10l-5-2.5" />
                        <path d="M6 17.5v1.5a1 1 0 0 0 1 1h4" />
                        <circle cx="6.5" cy="10" r="1.8" />
                    </svg>
                    <!-- cloudsync.svg (云+箭头) -->
                    <svg
                        v-else-if="app.icon === 'cloudsync'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path
                            d="M6.5 18h10a4 4 0 0 0 .5-7.97 5.5 5.5 0 0 0-10.7-.8A3.75 3.75 0 0 0 6.5 18z"
                        />
                        <path d="M12 11v6" />
                        <path d="M10 13l2-2 2 2" />
                    </svg>
                    <!-- notes.svg (文档+铅笔) -->
                    <svg
                        v-else-if="app.icon === 'notes'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path d="M5 3h9l5 5v12a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z" />
                        <path d="M14 3v5h5" />
                        <path d="M8 13l7-2.5" />
                        <path d="M14.5 8.5l2 .7-4.5 9-2-.7z" />
                    </svg>
                    <!-- 流媒体中心 SVG 已随应用剥离（apps/streaming，runtimeIcons 注册） -->
                    <!-- llm.svg (AI 神经网络节点 / 大脑) -->
                    <svg
                        v-else-if="app.icon === 'llm'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <circle cx="5" cy="6" r="1.6" fill="currentColor" stroke="none" />
                        <circle cx="5" cy="18" r="1.6" fill="currentColor" stroke="none" />
                        <circle cx="12" cy="3" r="1.6" fill="currentColor" stroke="none" />
                        <circle cx="12" cy="12" r="2" fill="currentColor" stroke="none" />
                        <circle cx="12" cy="21" r="1.6" fill="currentColor" stroke="none" />
                        <circle cx="19" cy="6" r="1.6" fill="currentColor" stroke="none" />
                        <circle cx="19" cy="18" r="1.6" fill="currentColor" stroke="none" />
                        <path d="M6.3 6.5L10.4 11" />
                        <path d="M6.3 17.5L10.4 13" />
                        <path d="M12 5v5" />
                        <path d="M12 14v5" />
                        <path d="M17.7 6.5L13.6 11" />
                        <path d="M17.7 17.5L13.6 13" />
                        <path d="M10 12h4" />
                    </svg>
                    <svg
                        v-else-if="app.icon === 'gateway'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 网络节点/路由/分发图标：中心网关 + 多上游入口 + 多下游出口 -->
                        <circle cx="12" cy="12" r="2.4" fill="currentColor" stroke="none" />
                        <path d="M12 9.6V6" />
                        <path d="M12 14.4V18" />
                        <path d="M9.6 12H6" />
                        <path d="M14.4 12H18" />
                        <circle cx="12" cy="4" r="1.4" fill="currentColor" stroke="none" />
                        <circle cx="12" cy="20" r="1.4" fill="currentColor" stroke="none" />
                        <circle cx="4" cy="12" r="1.4" fill="currentColor" stroke="none" />
                        <circle cx="20" cy="12" r="1.4" fill="currentColor" stroke="none" />
                        <path d="M5.2 7L9.5 9.5" opacity="0.6" />
                        <path d="M18.8 7L14.5 9.5" opacity="0.6" />
                        <path d="M5.2 17L9.5 14.5" opacity="0.6" />
                        <path d="M18.8 17L14.5 14.5" opacity="0.6" />
                    </svg>
                    <svg
                        v-else-if="app.icon === 'blockchain'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 区块链方块链接图标：三个连接的区块 -->
                        <rect x="3" y="9" width="6" height="6" rx="1" />
                        <rect x="15" y="9" width="6" height="6" rx="1" />
                        <rect x="9" y="3" width="6" height="6" rx="1" />
                        <rect x="9" y="15" width="6" height="6" rx="1" />
                        <path d="M9 6H7.5" opacity="0.6" />
                        <path d="M15 6h1.5" opacity="0.6" />
                        <path d="M9 18H7.5" opacity="0.6" />
                        <path d="M15 18h1.5" opacity="0.6" />
                    </svg>
                    <svg
                        v-else-if="app.icon === 'appstore'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 应用商店/购物袋图标 -->
                        <path d="M5 8h14l-1 11a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 8z" />
                        <path d="M9 8V6a3 3 0 0 1 6 0v2" />
                        <path d="M9 12v5" opacity="0.6" />
                        <path d="M15 12v5" opacity="0.6" />
                    </svg>
                    <!-- agenthub.svg (机器人头像 + 下载箭头：AI agent 集合) -->
                    <svg
                        v-else-if="app.icon === 'agenthub'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 天线 -->
                        <path d="M12 8V5" />
                        <circle cx="12" cy="4" r="1.2" fill="currentColor" stroke="none" />
                        <!-- 头 -->
                        <rect x="4.5" y="8" width="15" height="11" rx="2.5" />
                        <!-- 双眼 -->
                        <circle cx="9" cy="13" r="1.2" fill="currentColor" stroke="none" />
                        <circle cx="15" cy="13" r="1.2" fill="currentColor" stroke="none" />
                        <!-- 嘴（下载箭头：一键安装）-->
                        <path d="M12 15v2.2" />
                        <path d="M10.6 16.4l1.4 1.4 1.4-1.4" />
                        <!-- 双耳 -->
                        <path d="M4.5 12H2.8" />
                        <path d="M19.5 12h1.7" />
                    </svg>
                    <!-- 二维码传输 SVG 已随应用剥离（apps/qrtransfer，runtimeIcons 注册） -->
                    <!-- codehub.svg (Git 分支图标) -->
                    <svg
                        v-else-if="app.icon === 'codehub'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 左侧主干竖线 -->
                        <line x1="6" y1="3" x2="6" y2="21" />
                        <!-- 顶部提交点 -->
                        <circle cx="6" cy="6" r="2" fill="currentColor" stroke="none" />
                        <!-- 底部分支汇合点 -->
                        <circle cx="6" cy="18" r="2" />
                        <!-- 右侧分支节点 -->
                        <circle cx="18" cy="6" r="2" />
                        <!-- 分支曲线（右上 → 右下汇入主干）-->
                        <path d="M18 8 a6 6 0 0 1 -6 6 H6" />
                        <!-- 小提交圆点（分支中部）-->
                        <circle cx="18" cy="6" r="0.6" fill="currentColor" stroke="none" />
                    </svg>
                    <!-- devdocs.svg (翻开的书 + 左右尖括号：开发者文档) -->
                    <svg
                        v-else-if="app.icon === 'devdocs'"
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <!-- 书体：左右两页 + 中缝 -->
                        <path
                            d="M3 5.5a1.5 1.5 0 0 1 1.5-1.5H10a2.5 2.5 0 0 1 2 .8 2.5 2.5 0 0 1 2-.8h5.5A1.5 1.5 0 0 1 21 5.5v12a1.5 1.5 0 0 1-1.5 1.5H14a2.5 2.5 0 0 0-2 .8 2.5 2.5 0 0 0-2-.8H4.5A1.5 1.5 0 0 1 3 17.5z"
                        />
                        <path d="M12 4.8v15" />
                        <!-- 左页代码括号 < -->
                        <path d="M7 10l-1.5 1.5L7 13" />
                        <!-- 右页代码括号 > -->
                        <path d="M17 10l1.5 1.5L17 13" />
                    </svg>
                    <!-- 运行时应用包图标（AppIcon 查内置 ICONS → runtimeIcons） -->
                    <AppIcon v-else :name="app.icon" :size="38" class="app-svg" />
                </span>
                <span class="app-label">{{ getAppName(app.icon) }}</span>
            </button>

            <!-- 文件夹图标：绝对定位，可拖拽，双击展开 -->
            <button
                v-for="folder in folders"
                :key="folder.id"
                class="app-icon folder-icon"
                type="button"
                :data-folder-id="folder.id"
                :title="folder.name"
                :style="{ left: folder.x + 'px', top: folder.y + 'px' }"
                @mousedown="onFolderDragStart($event, folder.id)"
                @dblclick="openFolder(folder.id)"
                @contextmenu.prevent="openContextMenuAt($event, 'folder', folder.id)"
            >
                <span class="folder-tile">
                    <svg
                        class="app-svg"
                        viewBox="0 0 24 24"
                        fill="none"
                        stroke="currentColor"
                        stroke-width="1.6"
                        stroke-linecap="round"
                        stroke-linejoin="round"
                    >
                        <path
                            d="M3 7a1 1 0 0 1 1-1h5l2 2h8a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1z"
                        />
                        <path d="M3 11h18" />
                    </svg>
                    <span class="folder-count">{{ folder.appIds.length }}</span>
                </span>
                <span class="app-label">{{ folder.name }}</span>
            </button>
        </div>

        <!-- 文件夹展开面板（浮层）：双击文件夹后弹出 -->
        <div v-if="openFolderId && folders[openFolderId]" class="folder-panel" @click.self="closeFolder">
            <div class="folder-panel-inner" @click.self="closeFolder">
                <div class="folder-panel-header">
                    <input
                        class="folder-name-input"
                        :value="folders[openFolderId].name"
                        @input="onFolderNameInput($event, openFolderId!)"

                    />
                    <button class="folder-close-btn" type="button" @click="closeFolder">×</button>
                </div>
                <div class="folder-apps-grid">
                    <div
                        v-for="appId in folders[openFolderId].appIds"
                        :key="appId"
                        class="folder-app-item"
                    >
                        <button
                            class="folder-app-tile-btn"
                            type="button"
                            :title="getAppName(appId)"
                            @click="openAppFromFolder(appId)"
                        >
                            <span class="app-icon-tile" :style="{ background: findApp(appId)?.gradient ?? '#777' }">
                                <AppIcon :name="appId" :size="48" />
                            </span>
                            <span class="folder-app-label">{{ getAppName(appId) }}</span>
                        </button>
                        <button
                            class="remove-btn"
                            type="button"
                            title="移出文件夹"
                            @click="removeAppFromFolderPanel(openFolderId!, appId)"
                        >
                            ×
                        </button>
                    </div>
                </div>
            </div>
        </div>
    </div>
</template>

<style scoped>
/* ============================================================
   纯 OS 桌面：透明背景，让 MainLayout 的 Aubergine 壁纸透出。
   ============================================================ */
.desktop-root {
    position: absolute;
    inset: 0;
    /* 底部预留 140px 安全区，避免桌面图标被底部 Dock（120px）遮挡 */
    padding: 24px 24px 140px;
    overflow: auto;
    background: transparent;
}

/* Dock 拖拽高亮提示（弧形，贴合 Dock 胶囊上沿） */
/* 右键菜单 */
.context-menu {
    position: fixed;
    z-index: 500;
    background: rgba(30, 30, 32, 0.95);
    backdrop-filter: blur(12px);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 8px;
    padding: 6px;
    min-width: 160px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.4);
}
.ctx-item {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    color: #fff;
    padding: 8px 14px;
    border-radius: 6px;
    font-size: 13px;
    cursor: pointer;
    transition: background 0.12s ease;
}
.ctx-item:hover {
    background: var(--accent, #E95420);
}
/* 危险项（删除文件夹）：Yaru 红 */
.ctx-item.danger:hover {
    background: #c7162b;
}

/* —— 改名弹窗（Ubuntu Yaru 对话框风格） —— */
.rename-overlay {
    position: fixed;
    inset: 0;
    z-index: 400;
    background: rgba(0, 0, 0, 0.45);
    backdrop-filter: blur(4px);
    display: flex;
    align-items: center;
    justify-content: center;
}
.rename-dialog {
    width: min(92vw, 380px);
    background: var(--bg-0, #2C001E);
    color: var(--fg-0, #ffffff);
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 14px;
    padding: 20px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.5);
    font-family: var(--font);
}
.rename-title {
    font-size: 16px;
    font-weight: 700;
    margin-bottom: 12px;
    color: #ffffff;
}
.rename-input {
    width: 100%;
    box-sizing: border-box;
    background: rgba(255, 255, 255, 0.08);
    border: 1px solid rgba(255, 255, 255, 0.24);
    border-radius: var(--radius-sm, 8px);
    color: #ffffff;
    font-size: 14px;
    padding: 10px 12px;
    outline: none;
    font-family: var(--font);
    transition: border-color 0.14s ease;
}
.rename-input:focus {
    border-color: var(--accent, #E95420);
}
.rename-hint {
    font-size: 12px;
    color: rgba(255, 255, 255, 0.62);
    margin: 8px 0 14px;
    line-height: 1.5;
}
.rename-actions {
    display: flex;
    justify-content: flex-end;
    gap: 10px;
}
.rename-btn {
    min-width: 84px;
    padding: 8px 16px;
    border-radius: 20px;
    border: none;
    font-size: 13px;
    font-weight: 600;
    cursor: pointer;
    font-family: var(--font);
    transition: filter 0.12s ease;
}
.rename-btn:hover {
    filter: brightness(1.12);
}
.rename-btn.cancel {
    background: rgba(255, 255, 255, 0.14);
    color: #ffffff;
}
.rename-btn.ok {
    background: var(--accent, #E95420);
    color: #ffffff;
}

/* —— 应用包加载失败占位卡（右上角浮层，含重试/忽略） —— */
.app-fail-panel {
    position: absolute;
    top: 16px;
    right: 18px;
    z-index: 120;
    width: min(360px, 86vw);
    background: rgba(28, 28, 34, 0.95);
    backdrop-filter: blur(12px);
    border: 1px solid rgba(255, 120, 120, 0.35);
    border-radius: 12px;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
    padding: 10px 12px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    font-family: var(--font);
}
.app-fail-head {
    color: #ffb3b3;
    font-size: 12.5px;
    font-weight: 700;
    letter-spacing: 0.3px;
}
.app-fail-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.05);
}
.app-fail-text {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
}
.app-fail-name {
    color: #fff;
    font-size: 12.5px;
    font-weight: 600;
}
.app-fail-err {
    color: rgba(255, 255, 255, 0.55);
    font-size: 11px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}
.app-fail-btn {
    flex-shrink: 0;
    border: none;
    border-radius: 6px;
    cursor: pointer;
    font-family: var(--font);
    font-size: 11.5px;
    padding: 4px 10px;
}
.app-fail-btn.retry {
    background: var(--accent, #E95420);
    color: #fff;
    font-weight: 600;
}
.app-fail-btn.retry:hover {
    filter: brightness(1.15);
}
.app-fail-btn.dismiss {
    background: rgba(255, 255, 255, 0.12);
    color: #fff;
    width: 24px;
    height: 24px;
    padding: 0;
    font-size: 14px;
    line-height: 1;
}
.app-fail-btn.dismiss:hover {
    background: rgba(255, 255, 255, 0.22);
}

.dock-drop-hint {
    position: fixed;
    bottom: 0;
    left: 50%;
    transform: translateX(-50%);
    width: min(70vw, 640px);
    height: 120px;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    padding-bottom: 16px;
    background: radial-gradient(ellipse 80% 100% at 50% 100%,
        rgba(233, 84, 32, 0.22) 0%,
        rgba(233, 84, 32, 0.08) 50%,
        transparent 70%);
    border-radius: 50% 50% 0 0 / 100% 100% 0 0;
    border-top: 2px solid rgba(233, 84, 32, 0.5);
    border-left: 1px solid rgba(233, 84, 32, 0.2);
    border-right: 1px solid rgba(233, 84, 32, 0.2);
    color: #fff;
    font-size: 14px;
    font-weight: 600;
    z-index: 200;
    pointer-events: none;
    backdrop-filter: blur(4px);
    text-shadow: 0 1px 4px rgba(0, 0, 0, 0.4);
}

/* —— 桌面图标定位上下文：相对定位，撑满桌面区 —— */
.icon-grid {
    position: relative;
    width: 100%;
    height: 100%;
    min-width: 0;
}

.app-icon {
    /* 绝对定位：x/y 由 :style 绑定（iconPositions）。 */
    position: absolute;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 8px;
    width: 92px;
    padding: 10px 6px 8px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    cursor: grab;
    user-select: none;
    transition: transform 0.14s ease, background 0.14s ease, border-color 0.14s ease;
    font-family: var(--font);
}
.app-icon:hover {
    transform: scale(1.08);
    background: rgba(255, 255, 255, 0.14);
    border-color: rgba(255, 255, 255, 0.22);
    z-index: 10;
}
.app-icon:active {
    transform: scale(1.02);
}
/* 拖拽中：半透明 + 抓取光标 + 轻微放大，置顶避免被其它图标遮挡。 */
.app-icon.dragging {
    opacity: 0.8;
    cursor: grabbing;
    transform: scale(1.05);
    z-index: 10;
    transition: none;
}

.app-icon-tile {
    width: 52px;
    height: 52px;
    border-radius: 18px;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #ffffff;
    box-shadow: 0 6px 16px rgba(0, 0, 0, 0.32);
    overflow: hidden;
}
.app-svg {
    /* 38px，留出 tile 四周各 7px 留白；overflow:visible 防贴边描边被裁剪 */
    width: 38px;
    height: 38px;
    overflow: visible;
    flex-shrink: 0;
}

/* 白色文字标签 + 阴影，保证在深色壁纸上可读 */
.app-label {
    font-size: 10px;
    font-weight: 500;
    color: #ffffff;
    text-align: center;
    line-height: 1.3;
    text-shadow: 0 1px 3px rgba(0, 0, 0, 0.72), 0 0 1px rgba(0, 0, 0, 0.9);
    max-width: 88px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
}

/* ============================================================
   文件夹图标
   ============================================================ */
.folder-icon {
    /* 复用 .app-icon 的定位/外观；额外强调文件夹视觉 */
}
.folder-tile {
    position: relative;
    width: 80px;
    height: 80px;
    border-radius: 18px;
    display: flex;
    align-items: center;
    justify-content: center;
    /* Ubuntu Yaru 橙紫渐变，区分于普通应用图标 */
    background: linear-gradient(135deg, #772953 0%, #E95420 100%);
    color: #ffffff;
    box-shadow: 0 6px 16px rgba(0, 0, 0, 0.32);
    overflow: hidden;
}
.folder-tile .app-svg {
    width: 38px;
    height: 38px;
}
/* 文件夹内含应用数角标 */
.folder-count {
    position: absolute;
    right: 4px;
    bottom: 4px;
    min-width: 20px;
    height: 20px;
    padding: 0 5px;
    border-radius: 10px;
    background: #ffffff;
    color: #2C001E;
    font-size: 12px;
    font-weight: 700;
    display: flex;
    align-items: center;
    justify-content: center;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4);
}

/* ============================================================
   文件夹展开面板（浮层）
   ============================================================ */
.folder-panel {
    position: fixed;
    inset: 0;
    z-index: 300;
    background: rgba(0, 0, 0, 0.45);
    backdrop-filter: blur(4px);
    display: flex;
    align-items: center;
    justify-content: center;
}
.folder-panel-inner {
    width: min(90vw, 480px);
    max-height: 70vh;
    overflow: auto;
    background: var(--bg-0, #2C001E);
    color: var(--fg-0, #ffffff);
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 16px;
    padding: 18px 18px 22px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.5);
    font-family: var(--font);
}
.folder-panel-header {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-bottom: 14px;
}
.folder-name-input {
    flex: 1;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-sm, 8px);
    color: #ffffff;
    font-size: 18px;
    font-weight: 600;
    padding: 6px 8px;
    outline: none;
    font-family: var(--font);
}
.folder-name-input:hover,
.folder-name-input:focus {
    background: rgba(255, 255, 255, 0.1);
    border-color: rgba(255, 255, 255, 0.24);
}
.folder-close-btn {
    background: transparent;
    border: none;
    color: #ffffff;
    font-size: 24px;
    line-height: 1;
    cursor: pointer;
    width: 32px;
    height: 32px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: background 0.14s ease;
}
.folder-close-btn:hover {
    background: rgba(255, 255, 255, 0.16);
}
.folder-apps-grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(92px, 1fr));
    gap: 14px 10px;
}
.folder-app-item {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
}
.folder-app-tile-btn {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: none;
    cursor: pointer;
    padding: 4px;
    border-radius: var(--radius-sm, 8px);
    transition: background 0.14s ease, transform 0.14s ease;
}
.folder-app-tile-btn:hover {
    background: rgba(255, 255, 255, 0.12);
    transform: scale(1.05);
}
.folder-app-tile-btn .app-icon-tile {
    width: 60px;
    height: 60px;
    border-radius: 14px;
}
.folder-app-label {
    font-size: 12px;
    color: #ffffff;
    text-align: center;
    max-width: 88px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.6);
}
/* 移出按钮（应用右上角 ×） */
.remove-btn {
    position: absolute;
    top: -4px;
    right: -4px;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    background: #E95420;
    color: #ffffff;
    border: none;
    font-size: 14px;
    line-height: 1;
    cursor: pointer;
    display: flex;
    align-items: center;
    justify-content: center;
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4);
    z-index: 2;
    padding: 0;
}
.remove-btn:hover {
    background: #ff6a33;
}

/* —— 响应式：小屏图标略小 —— */
@media (max-width: 560px) {
    .desktop-root {
        padding: 14px;
    }
    .app-icon {
        width: 78px;
        gap: 6px;
    }
    .app-icon-tile {
        width: 64px;
        height: 64px;
        border-radius: 14px;
    }
    .app-svg {
        width: 44px;
        height: 44px;
    }
    .app-label {
        font-size: 12px;
        max-width: 74px;
    }
}
</style>
