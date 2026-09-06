<script setup lang="ts">
/**
 * 主布局：飞牛 fnOS / OS 风虚拟桌面。
 *
 * 结构（OS 范式）：
 *   - 顶部状态栏（深色半透明，36px）：左侧系统名 + 活跃窗口标题；
 *     右侧健康灯 + 语言切换 + 通知中心 + 设置齿轮。运行信息（CPU/内存/网络/时间）
 *     已移至 SystemWidget 浮窗磁贴（见 desktop-area 内）。
 *   - 桌面区域：
 *       * 壁纸层 = useWallpaper 提供的 CSS 渐变（默认 Aubergine，可在设置切换）
 *         浅色壁纸时 desktop-area 加 data-theme="light"，前景文字转深色
 *       * 桌面图标（DashboardView，透明背景，图标自由排列在壁纸上）
 *       * SystemWidget 运行信息磁贴卡片（浮窗，可拖动 / 展开 / 收起）
 *       * 浮在上面 = 所有打开的 WindowFrame（v-for windows）
 *   - 底部 Dock 栏（毛玻璃胶囊，居中浮）：水平排列应用图标，
 *     点击 = openWindow / toggleFromDock；运行中显示小圆点
 *
 * 路由兼容：应用路径（/chat、/storage、/settings 等）直接访问时由
 * router 守卫重定向到 /?app=<id>，本布局监听该参数自动打开对应浮窗
 * （见下方 watch route.query.app）。全屏 fallback 仅作兜底保留。
 */
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { endpoints } from '@/api/client'
import { storeToRefs } from 'pinia'
import AppIcon from '@/components/AppIcon.vue'
import WindowFrame from '@/components/WindowFrame.vue'
import SystemWidget from '@/components/SystemWidget.vue'
import Launchpad from '@/components/Launchpad.vue'
import ToastContainer from '@/components/ToastContainer.vue'
import LanguageSwitcher from '@/components/LanguageSwitcher.vue'
import DashboardView from '@/views/DashboardView.vue'
import { useSystemStore } from '@/stores/system'
import { useWindowManager } from '@/composables/useWindowManager'
import { useDockLayout } from '@/composables/useDockLayout'
import { useWallpaper } from '@/composables/useWallpaper'
import { appRegistry, findApp, getAppName, type AppMeta } from '@/appRegistry'
import { bootstrapApps } from '@/appRuntime'


/** 系统版本（状态栏小徽章，/update/status 只读；点击开更新应用）。 */
const osVersion = ref('…');
onMounted(async () => {
    try {
        const st = await endpoints.updateStatus();
        osVersion.value = st.current_version ?? '?';
    } catch {
        osVersion.value = '?';
    }
});

const route = useRoute()
// router：清除 ?app= 参数 + fallback「返回桌面」按钮使用。
const router = useRouter()
const systemStore = useSystemStore()
const { healthLevel, loading } = storeToRefs(systemStore)

const wm = useWindowManager()
const { windows, toggleFromDock, isOpen } = wm

// Dock / 桌面互斥布局（单例）：决定哪些应用在 Dock，哪些在桌面。
const dockLayout = useDockLayout()
const { dockApps, moveToDock, moveToDesktop } = dockLayout

// 启动台（Launchpad）开关
const launchpadOpen = ref(false)

// 健康指示灯 class
const healthClass = computed(() => `health-dot ${healthLevel.value}`)
const healthText = computed(() => {
    switch (healthLevel.value) {
        case 'ok':
            return '正常'
        case 'warn':
            return '关注'
        case 'err':
            return '异常'
        default:
            return '—'
    }
})

// Dock 点击：调用 toggleFromDock，传入标题/图标（用于新建窗口）。
// 标题走 getAppName：应用改名后，Dock 打开的窗口直接使用新名字。
function onDockClick(app: AppMeta): void {
    toggleFromDock(app.id, { title: getAppName(app.id), icon: app.icon })
}

// ============================================================
// Dock / 桌面拖拽互斥（HTML5 drag API）
// ============================================================
/** 当前从 Dock 拖出的应用 id（dragstart 记录，drop 时消费）。 */
const draggedDockId = ref<string | null>(null)
/** Dock 是否正被悬停（用于高亮"可投放"态）。 */
const dockHover = ref(false)
/** 桌面区域是否正被悬停（用于高亮"可投放"态）。 */
const desktopHover = ref(false)

/** Dock item 开始被拖（拖向桌面）。记录 id，设置 drag 数据。 */
function onDockItemDragStart(e: DragEvent, app: AppMeta): void {
    draggedDockId.value = app.id
    if (e.dataTransfer) {
        // text/plain 兜底，确保 Firefox 等浏览器真正进入拖拽态。
        e.dataTransfer.setData('text/plain', app.id)
        e.dataTransfer.effectAllowed = 'move'
    }
}

/** Dock item 拖拽结束：清空记录。 */
function onDockItemDragEnd(): void {
    draggedDockId.value = null
}

/** Dock drop zone 半高（与 dock-zone CSS 的 height 一致，便于命中判定）。 */
const DOCK_ZONE_HEIGHT = 120

/** 判断坐标是否在底部 Dock drop zone 内（全宽底部 120px）。 */
function inDockZone(clientY: number): boolean {
    const vh = typeof window !== 'undefined' ? window.innerHeight : 800
    return clientY > vh - DOCK_ZONE_HEIGHT
}

/**
 * document 级 drag 监听（在 onMounted 注册）：
 * 平时 dock-zone 是 pointer-events:none（不挡窗口点击），所以拖拽事件
 * 通过 document 冒泡捕获。仅当不在"从 Dock 拖出"状态时才高亮/接收。
 */
function onDocDragOver(e: DragEvent): void {
    if (draggedDockId.value != null) return
    if (!inDockZone(e.clientY)) {
        if (dockHover.value) dockHover.value = false
        return
    }
    e.preventDefault()
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
    dockHover.value = true
}

function onDocDragLeave(e: DragEvent): void {
    // dragleave 在移出视口或进子元素时触发；用坐标判定决定是否清除高亮。
    if (!inDockZone(e.clientY)) dockHover.value = false
}

function onDocDrop(e: DragEvent): void {
    if (draggedDockId.value != null) return
    if (!inDockZone(e.clientY)) return
    e.preventDefault()
    dockHover.value = false
    // 来源是桌面图标（DashboardView 用原生 mousedown 拖拽，会在 mouseup 自行调
    // moveToDock，不经过这里）；这里处理 HTML5 drag 通道（dataTransfer）。
    const id = e.dataTransfer ? e.dataTransfer.getData('text/plain') : ''
    if (id) moveToDock(id)
}

/** 桌面区域允许放置（接收从 Dock 拖来的应用）。 */
function onDesktopDragOver(e: DragEvent): void {
    // 仅当正从 Dock 拖出时才允许（避免桌面图标自身拖拽触发 drop）。
    if (draggedDockId.value == null) return
    e.preventDefault()
    if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
    desktopHover.value = true
}

/** 桌面区域拖出：清除高亮。 */
function onDesktopDragLeave(): void {
    desktopHover.value = false
}

/** 桌面接收 drop：把应用从 Dock 移到桌面。 */
function onDesktopDrop(e: DragEvent): void {
    e.preventDefault()
    desktopHover.value = false
    const id = draggedDockId.value
    if (id) moveToDesktop(id)
}

// 设置入口：以浮窗形式打开（与其他桌面应用一致，自带标题栏关闭按钮）。
// 直接 URL 访问 /settings 也走同样路径：router 守卫重定向到 /?app=settings，
// 由上方 watch 自动弹出设置浮窗。
function goToSettings(): void {
    wm.openWindow({ id: 'settings', title: '设置', icon: 'settings' })
}

// 当前活跃窗口标题（状态栏左侧显示）。无活跃窗口显示系统名。
const activeTitle = computed<string | null>(() => {
    const id = wm.activeId.value
    if (!id) return null
    const win = windows.find((w) => w.id === id)
    if (!win) return null
    return win.title
})

// —— 广告位资讯栏：从广告 API 拉取滚动内容 —— //
interface NewsItem { text: string; level: string; icon?: string }
const newsItems = ref<NewsItem[]>([])
let newsTimer: ReturnType<typeof setInterval> | null = null

async function loadNews(): Promise<void> {
    try {
        const resp = await fetch('/api/v1/system/ads')
        if (resp.ok) {
            const ads = await resp.json() as Array<{ text: string; icon?: string; link?: string }>
            if (ads.length > 0) {
                newsItems.value = ads.map(a => ({ text: a.text, level: 'ad', icon: a.icon }))
            } else {
                newsItems.value = [{ text: '欢迎使用 NexOS — 连接每一个超级个体', level: 'ad' }]
            }
        } else {
            newsItems.value = [{ text: '欢迎使用 NexOS — 连接每一个超级个体', level: 'ad' }]
        }
    } catch {
        newsItems.value = [{ text: '欢迎使用 NexOS — 连接每一个超级个体', level: 'ad' }]
    }
    // 通知红点：仅当拉取到与上次已读签名不同的新资讯时才点亮
    refreshNotifUnread()
}

// 资讯栏可关闭（持久化），并新增通知中心入口。
const NEWS_DISMISS_KEY = 'os-news-dismissed'
const newsDismissed = ref(false)
try {
    newsDismissed.value = localStorage.getItem(NEWS_DISMISS_KEY) === '1'
} catch {
    /* ignore */
}
function dismissNews(): void {
    newsDismissed.value = true
    try {
        localStorage.setItem(NEWS_DISMISS_KEY, '1')
    } catch {
        /* ignore */
    }
}
function showNewsAgain(): void {
    newsDismissed.value = false
    try {
        localStorage.removeItem(NEWS_DISMISS_KEY)
    } catch {
        /* ignore */
    }
}

// 通知中心面板开关 + 红点已读状态（独立于资讯栏关闭，解耦修复）
const showNotif = ref(false)
// 通知"已读"签名：点开通知面板即标记已读（红点消失），
// 仅当拉取到与上次已读内容不同的新资讯时才重新点亮。
const NEWS_READ_SIG_KEY = 'os-news-read-sig'
const notifUnread = ref(false)
function newsSignature(): string {
    return newsItems.value.map((n) => n.text).join('\u0001')
}
function markNotifRead(): void {
    notifUnread.value = false
    try {
        localStorage.setItem(NEWS_READ_SIG_KEY, newsSignature())
    } catch {
        /* ignore */
    }
}
function refreshNotifUnread(): void {
    let lastRead = ''
    try {
        lastRead = localStorage.getItem(NEWS_READ_SIG_KEY) ?? ''
    } catch {
        lastRead = ''
    }
    notifUnread.value = newsSignature() !== lastRead
}
function toggleNotif(): void {
    showNotif.value = !showNotif.value
    if (showNotif.value) markNotifRead()
}

// 是否处于 fallback 全屏路由。
// 桌面根 / 走 OS 桌面浮窗布局；已注册应用路径已被 router 守卫重定向
// 回 /，正常不会落到这里 —— fallback 仅为未注册路径/异常场景兜底
// （带"返回桌面"按钮，不会被困）。
const showFallback = computed(() => route.path !== '/')

// ============================================================
// 壁纸（CSS 渐变）：从 localStorage 读，默认 Aubergine
// ============================================================
const { cssValue: wallpaperCss, isLight: wallpaperIsLight } = useWallpaper()
/** 壁纸前景色：浅色壁纸用深字，深色壁纸用白字。 */
const desktopDataTheme = computed(() => (wallpaperIsLight.value ? 'light' : 'dark'))

// ============================================================
// 直接 URL 打开应用（/?app=<id>）
//
// router 守卫把 /chat、/storage、/settings 等应用路径重定向到
// /?app=<id>；这里监听该参数，桌面就绪后自动打开对应浮窗，并立即
// 清除参数（防止刷新 / 历史记录重复弹窗）。
//
// 用 watch（immediate）而非仅 onMounted：SPA 内部 router.push('/storage')
// （如 Vms / Shares 页跳转、CodeHub 的 RouterLink）时 MainLayout 已挂载，
// 不会重新触发 onMounted，仍需响应参数变化。
// ============================================================
watch(
    () => route.query.app,
    (appParam) => {
        if (typeof appParam !== 'string' || appParam === '') return
        if (appRegistry[appParam]) {
            const meta = findApp(appParam)
            // nextTick：等桌面区域（desktop-area）渲染完成后再开窗，
            // 保证级联定位能拿到正确的可视区尺寸。
            nextTick(() => {
                wm.openWindow({
                    id: appParam,
                    title: getAppName(appParam),
                    icon: meta?.icon ?? appParam,
                })
            })
        }
        // appRegistry 未注册的应用：只清参数留在桌面（全屏 fallback
        // 已由 router 守卫层兜底，不会带 ?app= 到这里）。
        // 只清 app 参数（保留 node/name 等业务参数，Chat.vue 需要读取）
        // 之前 router.replace({ query: {} }) 会把 ?node= 一并清掉，
        // 导致远程大厅会话无法创建（用户 2026-08-23 反馈 106 不生效）
        const nextQuery = { ...route.query }
        delete nextQuery.app
        router.replace({ query: nextQuery }).catch(() => {
            /* 导航中断可忽略 */
        })
    },
    { immediate: true },
)

onMounted(() => {
    // 进入布局即拉取系统概览（健康）。
    systemStore.fetchOverview().catch(() => {
        /* 错误已写入 store.error，不阻塞渲染 */
    })
    // 应用包运行时装载：GET /api/v1/apps → 逐个 import /apps-assets/:id/entry.js
    // → register(ctx)（失败进桌面占位卡，不崩桌面；后端未就绪时静默）。
    void bootstrapApps()
    // 注册 document 级 drag 监听（dock-zone pointer-events:none，靠冒泡接收）。
    document.addEventListener('dragover', onDocDragOver)
    document.addEventListener('dragleave', onDocDragLeave)
    document.addEventListener('drop', onDocDrop)

    // 资讯栏轮询
    loadNews()
    newsTimer = setInterval(loadNews, 15000)
})

onUnmounted(() => {
    if (newsTimer) clearInterval(newsTimer)
    document.removeEventListener('dragover', onDocDragOver)
    document.removeEventListener('dragleave', onDocDragLeave)
    document.removeEventListener('drop', onDocDrop)
})
</script>

<template>
    <div class="app-layout">
        <div class="main-wrap">
            <!-- 顶部 OS 状态栏（深色半透明）：仅左侧系统名 + 右侧健康灯 + 齿轮 -->
            <header class="statusbar">
                <!-- 左侧：系统名 + 活跃窗口标题 -->
                <div class="sb-left">
                    <button class="sb-launch" type="button" title="启动台" aria-label="启动台" @click="launchpadOpen = true">
                        <svg viewBox="0 0 20 20" width="16" height="16" fill="currentColor"><rect x="2" y="2" width="6" height="6" rx="1.5"/><rect x="12" y="2" width="6" height="6" rx="1.5"/><rect x="2" y="12" width="6" height="6" rx="1.5"/><rect x="12" y="12" width="6" height="6" rx="1.5"/></svg>
                    </button>
                    <span class="sb-brand">NexOS</span>
                    <span class="sb-version" title="系统版本（点击打开更新应用）" @click="wm.openWindow({ id: 'update', title: '更新', icon: 'update' })">v{{ osVersion }}</span>
                    <span v-if="activeTitle" class="sb-sep">›</span>
                    <span v-if="activeTitle" class="sb-active">{{ activeTitle }}</span>
                </div>

                <!-- 右侧：健康灯 + 通知中心 + 语言切换 + 设置（运行信息已移至 SystemWidget 浮窗） -->
                <div class="sb-right">
                    <span class="sb-health" :title="healthText">
                        <span :class="healthClass"></span>
                    </span>

                    <button
                        class="sb-bell"
                        type="button"
                        title="通知中心"
                        aria-label="通知中心"
                        @click="toggleNotif"
                    >
                        <span class="sb-bell-icon">🔔</span>
                        <span v-if="newsItems.length && notifUnread" class="sb-bell-dot"></span>
                    </button>

                    <!-- 语言切换（🌐 下拉）：立即生效并持久化（os.locale），与设置页联动 -->
                    <LanguageSwitcher variant="bar" />

                    <button
                        class="sb-gear"
                        type="button"
                        title="设置"
                        aria-label="设置"
                        @click="goToSettings"
                    >
                        <AppIcon name="settings" :size="16" />
                    </button>
                </div>

                <!-- 通知中心面板 -->
                <div v-if="showNotif" class="notif-panel" @click.self="showNotif = false">
                    <div class="notif-head">
                        <span>通知中心</span>
                        <button class="notif-close" type="button" @click="showNotif = false">×</button>
                    </div>
                    <div class="notif-body">
                        <div v-if="newsItems.length === 0" class="notif-empty">暂无通知</div>
                        <div
                            v-for="(item, i) in newsItems"
                            :key="i"
                            class="notif-item"
                            :class="'news-' + item.level"
                        >
                            <span class="notif-dot"></span>{{ item.text }}
                        </div>
                        <button v-if="newsDismissed" class="notif-restore" type="button" @click="showNewsAgain">
                            恢复资讯栏
                        </button>
                    </div>
                </div>
            </header>

            <!-- 资讯栏：滚动播放系统告警/通知（可关闭） -->
            <div v-if="newsItems.length && !newsDismissed" class="news-ticker">
                <span class="news-label">📢 资讯</span>
                <div class="news-track">
                    <span v-for="(item, i) in newsItems" :key="i" class="news-item" :class="'news-' + item.level">
                        {{ item.text }}
                    </span>
                </div>
                <button class="news-close" type="button" title="关闭资讯栏" aria-label="关闭资讯栏" @click="dismissNews">×</button>
            </div>

            <!-- 桌面 + 浮窗主区域（仅桌面根路由下显示） -->
            <main v-if="!showFallback" class="content">
                <div v-if="loading" class="loading">加载中…</div>
                <!-- 桌面区域：相对定位，作为浮窗定位上下文 -->
                <div
                    class="desktop-area"
                    :class="{ 'drop-target': desktopHover }"
                    :data-theme="desktopDataTheme"
                    @dragover="onDesktopDragOver"
                    @dragleave="onDesktopDragLeave"
                    @drop="onDesktopDrop"
                >
                    <!-- 壁纸层：从 useWallpaper 读 CSS 背景（默认 Aubergine）；桌面图标透明叠在上面 -->
                    <div class="desktop-wallpaper" :style="{ background: wallpaperCss }">
                        <DashboardView />
                    </div>

                    <!-- 浮窗层：所有打开的窗口 -->
                    <WindowFrame
                        v-for="win in windows"
                        :key="win.id"
                        :win="win"
                    >
                        <component :is="appRegistry[win.id]" v-if="appRegistry[win.id]" />
                        <div v-else class="missing-app">未注册应用：{{ win.id }}</div>
                    </WindowFrame>
                </div>

                <!-- 底部 Dock drop zone：全宽透明大区域（120px 高），扩大拖拽接收范围。
                     pointer-events:none 平时不挡窗口点击；dock 子元素单独 auto。
                     拖拽命中由 document 级监听（onDocDragOver/OnDocDrop）处理。 -->
                <div class="dock-zone" :class="{ active: dockHover }">
                    <!-- 拖拽高亮提示条（仅 dragover 时可见） -->
                    <div v-if="dockHover" class="dock-zone-hint">拖到此处固定到 Dock</div>

                    <!-- Dock 栏（毛玻璃胶囊，居中浮） -->
                    <nav class="dock" :class="{ 'dock-active': dockHover }" aria-label="应用 Dock">
                        <button
                            v-for="app in dockApps"
                            :key="app.id"
                            class="dock-item"
                            :class="{ running: isOpen(app.id), active: wm.activeId.value === app.id }"
                            type="button"
                            :title="getAppName(app.id)"
                            draggable="true"
                            @click="onDockClick(app)"
                            @dragstart="onDockItemDragStart($event, app)"
                            @dragend="onDockItemDragEnd"
                        >
                            <span class="dock-tile" :style="{ background: app.gradient }">
                                <AppIcon :name="app.icon" :size="42" />
                            </span>
                            <span v-if="isOpen(app.id)" class="dock-dot"></span>
                        </button>
                    </nav>
                </div>
            </main>

            <!-- fallback：直接 URL 访问具体应用 / 设置时全屏展示（兼容） -->
            <main v-else class="content fallback-content">
                <button class="fallback-back" type="button" @click="$router.push('/')">
                    <span aria-hidden="true">‹</span> 返回桌面
                </button>
                <div v-if="loading" class="loading">加载中…</div>
                <RouterView />
            </main>
        </div>

        <!-- 运行信息磁贴卡片（app-layout 直接子级，脱离 desktop-area overflow:hidden） -->
        <SystemWidget />

        <!-- 全局 Toast 浮层（右上角，浮于浮窗/Dock 之上） -->
        <ToastContainer />

        <!-- 启动台（Launchpad）：全屏应用分组启动台 -->
        <Launchpad :open="launchpadOpen" @close="launchpadOpen = false" />
    </div>
</template>

<style scoped>
/* ============================================================
   应用整体布局
   ============================================================ */
.app-layout {
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
}

.main-wrap {
    display: flex;
    flex-direction: column;
    flex: 1;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
}

/* ============================================================
   顶部 OS 状态栏（深色半透明，36px，Ubuntu Yaru 顶栏风）
   ============================================================ */
.statusbar {
    height: 36px;
    background: rgba(0, 0, 0, 0.55);
    backdrop-filter: blur(14px);
    -webkit-backdrop-filter: blur(14px);
    color: #fff;
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 0 14px;
    position: sticky;
    top: 0;
    z-index: 60;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    font-size: 12.5px;
    font-family: var(--font);
    user-select: none;
}

/* —— 资讯栏（滚动） —— */
.news-ticker {
    height: 28px;
    display: flex;
    align-items: center;
    background: rgba(0, 0, 0, 0.35);
    backdrop-filter: blur(8px);
    overflow: hidden;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
    font-size: 12px;
}
.news-label {
    flex-shrink: 0;
    padding: 0 10px;
    color: var(--accent, #E95420);
    font-weight: 600;
    font-size: 11px;
    border-right: 1px solid rgba(255, 255, 255, 0.1);
    margin-right: 6px;
}
.news-track {
    display: flex;
    align-items: center;
    gap: 40px;
    animation: news-scroll 40s linear infinite;
    white-space: nowrap;
    will-change: transform;
}
.news-item {
    color: rgba(255, 255, 255, 0.8);
}
.news-item.news-warning { color: var(--warn, #F99B11); }
.news-item.news-critical { color: var(--err, #C7162B); font-weight: 600; }
.news-item.news-info { color: rgba(255, 255, 255, 0.7); }
@keyframes news-scroll {
    0% { transform: translateX(0); }
    100% { transform: translateX(-50%); }
}
.news-ticker:hover .news-track {
    animation-play-state: paused;
}

.sb-version {
    font-size: 11px;
    opacity: 0.65;
    padding: 1px 8px;
    border-radius: 8px;
    background: rgba(255, 255, 255, 0.1);
    cursor: pointer;
    user-select: none;
}
.sb-version:hover {
    opacity: 0.9;
}
.sb-left {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
    flex: 1;
    overflow: hidden;
}
.sb-brand {
    font-weight: 600;
    letter-spacing: -0.01em;
    white-space: nowrap;
}
.sb-sep {
    opacity: 0.45;
}
.sb-active {
    opacity: 0.82;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}
.sb-right {
    display: flex;
    align-items: center;
    gap: 14px;
    flex-shrink: 0;
}

.sb-health {
    display: inline-flex;
    align-items: center;
}
.health-dot {
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.4);
    transition: background 0.2s;
}
.health-dot.ok {
    background: var(--ok);
    box-shadow: 0 0 6px rgba(110, 224, 138, 0.7);
}
.health-dot.warn {
    background: var(--warn);
    box-shadow: 0 0 6px rgba(255, 197, 107, 0.7);
}
.health-dot.err {
    background: var(--err);
    box-shadow: 0 0 6px rgba(255, 122, 147, 0.7);
}
.health-dot.unknown {
    background: rgba(255, 255, 255, 0.4);
}

.sb-gear {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    background: transparent;
    border: none;
    border-radius: 50%;
    color: #fff;
    cursor: pointer;
    opacity: 0.8;
    transition: background 0.14s ease, opacity 0.14s ease;
}
.sb-gear:hover {
    background: rgba(255, 255, 255, 0.16);
    opacity: 1;
}

/* 启动台入口按钮 */
.sb-launch {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    background: transparent;
    border: none;
    border-radius: 50%;
    color: #fff;
    cursor: pointer;
    opacity: 0.8;
    transition: background 0.14s ease, opacity 0.14s ease;
}
.sb-launch:hover {
    background: rgba(255, 255, 255, 0.16);
    opacity: 1;
}

/* 资讯栏关闭按钮 */
.news-close {
    flex-shrink: 0;
    width: 22px;
    height: 22px;
    margin-left: 8px;
    border: none;
    border-radius: 50%;
    background: transparent;
    color: rgba(255, 255, 255, 0.7);
    font-size: 16px;
    line-height: 1;
    cursor: pointer;
    transition: background 0.12s ease, color 0.12s ease;
}
.news-close:hover {
    background: rgba(255, 255, 255, 0.16);
    color: #fff;
}

/* 通知中心铃铛 */
.sb-bell {
    position: relative;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    background: transparent;
    border: none;
    border-radius: 50%;
    color: #fff;
    cursor: pointer;
    opacity: 0.8;
    transition: background 0.14s ease, opacity 0.14s ease;
}
.sb-bell:hover {
    background: rgba(255, 255, 255, 0.16);
    opacity: 1;
}
.sb-bell-dot {
    position: absolute;
    top: 2px;
    right: 2px;
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: var(--accent, #E95420);
    box-shadow: 0 0 4px rgba(233, 84, 32, 0.8);
}

/* 通知中心面板 */
.notif-panel {
    position: absolute;
    top: 42px;
    right: 12px;
    z-index: 70;
    width: min(320px, 90vw);
    background: rgba(28, 28, 34, 0.97);
    backdrop-filter: blur(14px);
    -webkit-backdrop-filter: blur(14px);
    border: 1px solid rgba(255, 255, 255, 0.14);
    border-radius: 12px;
    box-shadow: 0 12px 36px rgba(0, 0, 0, 0.5);
    overflow: hidden;
    font-family: var(--font);
}
.notif-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 14px;
    color: #fff;
    font-size: 13px;
    font-weight: 600;
    border-bottom: 1px solid rgba(255, 255, 255, 0.08);
}
.notif-close {
    width: 24px;
    height: 24px;
    border: none;
    border-radius: 50%;
    background: transparent;
    color: #fff;
    font-size: 18px;
    line-height: 1;
    cursor: pointer;
}
.notif-close:hover { background: rgba(255, 255, 255, 0.16); }
.notif-body {
    max-height: 320px;
    overflow: auto;
    padding: 8px;
}
.notif-empty {
    color: rgba(255, 255, 255, 0.5);
    font-size: 12px;
    text-align: center;
    padding: 18px 0;
}
.notif-item {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 9px 10px;
    border-radius: 8px;
    color: rgba(255, 255, 255, 0.85);
    font-size: 12.5px;
    line-height: 1.4;
}
.notif-item:hover { background: rgba(255, 255, 255, 0.06); }
.notif-dot {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: var(--accent, #E95420);
    flex-shrink: 0;
}
.notif-item.news-warning .notif-dot { background: var(--warn, #F99B11); }
.notif-item.news-critical .notif-dot { background: var(--err, #C7162B); }
.notif-restore {
    width: 100%;
    margin-top: 6px;
    padding: 8px;
    border: 1px solid rgba(255, 255, 255, 0.16);
    border-radius: 8px;
    background: transparent;
    color: #fff;
    font-size: 12px;
    cursor: pointer;
}
.notif-restore:hover { background: rgba(255, 255, 255, 0.1); }

/* ============================================================
   桌面 + 浮窗主区域
   ============================================================ */
.content {
    flex: 1;
    min-height: 0;
    position: relative;
    overflow: hidden;
    /* 为底部 dock 留出空间 */
    padding-bottom: 92px;
}

/* 桌面区域：相对定位，作为 WindowFrame 的定位上下文（取整屏高度） */
.desktop-area {
    position: absolute;
    inset: 0;
    overflow: hidden;
}

/* 壁纸层：Ubuntu Yaru Aubergine 对角渐变 + 细微纹理 */
.desktop-wallpaper {
    position: absolute;
    inset: 0;
    overflow: hidden;
    z-index: 1;
    /* Aubergine 主底（Ubuntu 经典深紫 #2C001E -> #4A0E3C），对角渐变 */
    background:
        /* 细微暖橙光晕（右上） */
        radial-gradient(ellipse 70% 60% at 85% 12%, rgba(233, 84, 32, 0.18), transparent 60%),
        /* 深紫光晕（左下） */
        radial-gradient(ellipse 60% 50% at 8% 92%, rgba(119, 41, 83, 0.35), transparent 65%),
        /* 主对角渐变 */
        linear-gradient(135deg, #2C001E 0%, #3A0A2C 45%, #4A0E3C 100%);
}

.fallback-back {
    position: sticky; top: 0; z-index: 5;
    margin: 0 0 12px; padding: 6px 14px;
    border: 1px solid var(--border, rgba(0,0,0,0.12));
    border-radius: 8px; background: var(--bg-card, #fff);
    color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer;
}
.fallback-back:hover { background: var(--accent-soft, rgba(233,84,32,0.1)); }

.fallback-content {
    overflow: auto;
    padding: 20px;
    background: var(--bg-app);
}

.missing-app {
    padding: 24px;
    color: var(--err);
    font-size: 14px;
}

.loading {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--text-muted);
    z-index: 5;
    pointer-events: none;
}

/* ============================================================
   底部 Dock 栏（毛玻璃胶囊，居中浮）
   ============================================================ */
/* 底部 Dock drop zone：全宽透明区域，扩大拖拽接收范围。
   平时 pointer-events:none（不挡窗口点击）；drag 事件靠 document 冒泡监听。
   Dock 子元素单独 pointer-events:auto 保留点击/拖出交互。 */
.dock-zone {
    position: fixed;
    left: 0;
    right: 0;
    bottom: 0;
    height: 120px;
    z-index: 40;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    padding-bottom: 14px;
    pointer-events: none;
}
/* Dock 本身可点击 / 可拖出 */
.dock-zone .dock {
    pointer-events: auto;
}
/* 拖拽悬停高亮提示条（仅 dragover 时可见，浮在 Dock 上方） */
.dock-zone-hint {
    position: absolute;
    bottom: 84px;
    left: 50%;
    transform: translateX(-50%);
    padding: 5px 14px;
    background: rgba(110, 224, 138, 0.22);
    border: 1px solid rgba(110, 224, 138, 0.6);
    border-radius: 14px;
    color: #fff;
    font-size: 12px;
    font-weight: 600;
    letter-spacing: 0.3px;
    backdrop-filter: blur(10px);
    -webkit-backdrop-filter: blur(10px);
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.3);
    white-space: nowrap;
    pointer-events: none;
    animation: dock-hint-fade 0.15s ease;
}
@keyframes dock-hint-fade {
    from {
        opacity: 0;
        transform: translate(-50%, 6px);
    }
    to {
        opacity: 1;
        transform: translate(-50%, 0);
    }
}
/* dragover 整个 zone 高亮（半透明绿色覆盖，给出"可投放"反馈） */
.dock-zone.active::before {
    content: '';
    position: absolute;
    inset: 0;
    background: linear-gradient(to top, rgba(110, 224, 138, 0.16), transparent 70%);
    border-top: 2px dashed rgba(110, 224, 138, 0.6);
    pointer-events: none;
}

/* Dock 栏（毛玻璃胶囊，居中）：相对定位在 dock-zone 内居中 */
.dock {
    position: relative;
    left: auto;
    bottom: auto;
    transform: none;
    display: flex;
    align-items: flex-end;
    gap: 6px;
    padding: 8px 12px;
    background: rgba(255, 255, 255, 0.16);
    backdrop-filter: blur(18px) saturate(1.4);
    -webkit-backdrop-filter: blur(18px) saturate(1.4);
    border: 1px solid rgba(255, 255, 255, 0.22);
    border-radius: 22px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.38);
    max-width: calc(100% - 24px);
    overflow-x: auto;
    scrollbar-width: none;
}
.dock::-webkit-scrollbar {
    display: none;
}
.dock-item {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 2px;
    padding: 4px 4px 6px;
    background: none;
    border: none;
    cursor: pointer;
    border-radius: 14px;
    transition: transform 0.12s ease, background 0.12s ease;
    flex-shrink: 0;
}
/* 可拖出：抓取光标 */
.dock-item[draggable='true'] {
    cursor: grab;
}
.dock-item[draggable='true']:active {
    cursor: grabbing;
}
.dock-item:hover {
    transform: translateY(-6px) scale(1.1);
    background: rgba(255, 255, 255, 0.18);
}
/* 拖入 Dock 区域时所有 Dock 图标放大（视觉反馈） */
.dock.dock-active .dock-tile {
    transform: scale(1.25);
}
.dock-item.active {
    background: rgba(255, 255, 255, 0.26);
}
.dock-tile {
    width: 68px;
    height: 68px;
    border-radius: 16px;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #fff;
    box-shadow: 0 3px 8px rgba(0, 0, 0, 0.32);
    transition: transform 0.2s ease;
}
/* dock 运行指示点：浮于图标下方，白色 */
.dock-dot {
    position: absolute;
    bottom: 1px;
    width: 4px;
    height: 4px;
    border-radius: 50%;
    background: #fff;
    opacity: 0.9;
    box-shadow: 0 0 4px rgba(255, 255, 255, 0.6);
}

/* 拖放高亮：桌面作为投放目标时加亮边框，给出"可投放"反馈
   （Dock 的投放反馈由 .dock-zone.active::before 处理） */
.desktop-area.drop-target {
    box-shadow: inset 0 0 0 2px rgba(110, 224, 138, 0.45);
}

/* ============================================================
   响应式：平板 / 手机
   ============================================================ */
@media (max-width: 900px) {
    .content {
        padding-bottom: 86px;
    }
    .dock-tile {
        width: 40px;
        height: 40px;
    }
    /* 窄屏缩小 drop zone 高度 */
    .dock-zone {
        height: 100px;
    }
}

@media (max-width: 560px) {
    .statusbar {
        height: 32px;
        padding: 0 10px;
        gap: 8px;
        font-size: 12px;
    }
    .sb-brand {
        font-size: 12px;
    }
}
</style>
