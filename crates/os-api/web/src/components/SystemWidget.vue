<script setup lang="ts">
/**
 * SystemWidget —— 系统运行信息磁贴卡片。
 *
 * 浮在桌面（position: fixed; z-index: 100），不影响窗口管理器，不占顶栏。
 * 两种显示模式：
 *   - expanded：完整指标卡片（CPU/MEM/NET/磁盘/负载/uptime/时间）
 *   - compact：缩成一个小圆，只显示精简 CPU% + 仪表盘图标
 *
 * 交互：
 *   - 标题栏 mousedown → document mousemove 拖动 → mouseup 存 localStorage
 *   - 右上角按钮切换展开/收起
 *
 * 数据：onMounted + setInterval(5000) 调 /api/v1/monitor/metrics（CPU/内存/磁盘）
 * 与 /api/v1/monitor/net-rate（实时网速——两次 /proc/net/dev 采样差值，首拍全 0
 * 记基线，下一拍起为真实 B/s；此前误把 metrics 的累计 net_rx/tx_bytes 当速率
 * 展示，已改接本端点）；时间每秒更新。
 * 失败降级：metrics 拉取失败只显示时间；net-rate 失败网速行显示 "--"。
 *
 * 位置 + 展开状态由 useWidgetState 持久化（localStorage `os-widget-state`）；
 * 挂载与 window resize 时 ensureVisible() 把越界坐标钳回视口内（修复默认
 * x=9999 哨兵在新环境直接渲染到屏幕外导致胶囊不可见）。
 */
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { endpoints } from '@/api/client'
import type { NetRateSnapshot } from '@/api/client'
import { useWidgetState } from '@/composables/useWidgetState'

interface SystemMetrics {
    hostname?: string
    uptime_secs?: number
    cpu_usage?: number
    cpu_cores?: number
    mem_total_bytes?: number
    mem_used_bytes?: number
    disk_total_bytes?: number
    disk_used_bytes?: number
    load1?: number
    load5?: number
    load15?: number
    process_count?: number
    [k: string]: unknown
}

const { state, setPosition, toggleExpanded, ensureVisible } = useWidgetState()

const metrics = ref<SystemMetrics | null>(null)
const netRate = ref<NetRateSnapshot | null>(null)
const clock = ref(formatClock(new Date()))
let metricsTimer: ReturnType<typeof setInterval> | null = null
let clockTimer: ReturnType<typeof setInterval> | null = null

async function refreshMetrics(): Promise<void> {
    try {
        const data = (await endpoints.monitorMetrics()) as SystemMetrics
        metrics.value = data ?? null
    } catch {
        // 监控端点未启用 / 失败：降级只显示时间
        metrics.value = null
    }
}

/** 实时网速（独立拉取：失败只影响网速行，显示 "--"）。 */
async function refreshNetRate(): Promise<void> {
    try {
        netRate.value = await endpoints.monitorNetRate()
    } catch {
        netRate.value = null
    }
}

// —— 工具：速率（字节/秒）-> "xB/s / xKB/s / xMB/s"（单位自适应）——
function formatRate(bytesPerSec?: number | null): string {
    if (bytesPerSec == null || bytesPerSec < 0) return '--'
    if (bytesPerSec < 1024) return `${Math.round(bytesPerSec)}B/s`
    const units = ['KB/s', 'MB/s', 'GB/s', 'TB/s']
    let val = bytesPerSec / 1024
    let i = 0
    while (val >= 1024 && i < units.length - 1) {
        val /= 1024
        i++
    }
    return `${val.toFixed(1)}${units[i]}`
}

// —— 工具：字节 -> "4.2G"（用于内存/磁盘大数字展示）——
function formatBytesShort(bytes?: number, dp = 1): string {
    if (!bytes || bytes <= 0) return '0'
    const units = ['B', 'K', 'M', 'G', 'T', 'P']
    const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)))
    const val = bytes / Math.pow(1024, i)
    return `${val.toFixed(i === 0 ? 0 : dp)}${units[i]}`
}

// —— 工具：秒 -> "1d2h" 紧凑在线时长 ——
function formatUptime(sec?: number): string {
    if (!sec || sec <= 0) return ''
    const d = Math.floor(sec / 86400)
    const h = Math.floor((sec % 86400) / 3600)
    const m = Math.floor((sec % 3600) / 60)
    const parts: string[] = []
    if (d > 0) parts.push(`${d}d`)
    if (h > 0) parts.push(`${h}h`)
    if (d === 0 && m > 0) parts.push(`${m}m`)
    return parts.join('') || '0m'
}

// —— 工具：Date -> "HH:MM:SS" ——
function formatClock(d: Date): string {
    const hh = String(d.getHours()).padStart(2, '0')
    const mm = String(d.getMinutes()).padStart(2, '0')
    const ss = String(d.getSeconds()).padStart(2, '0')
    return `${hh}:${mm}:${ss}`
}

// —— 展示字段 computed ——
const cpuPct = computed(() =>
    metrics.value?.cpu_usage != null ? Math.round(metrics.value.cpu_usage) : null,
)
const memUsed = computed(() => formatBytesShort(metrics.value?.mem_used_bytes, 1))
const memTotal = computed(() => formatBytesShort(metrics.value?.mem_total_bytes, 0))
const memPct = computed(() => {
    const m = metrics.value
    if (!m || !m.mem_total_bytes) return 0
    return Math.min(100, Math.round(((m.mem_used_bytes ?? 0) / m.mem_total_bytes) * 100))
})
const netDown = computed(() => formatRate(netRate.value?.total.rx_bps))
const netUp = computed(() => formatRate(netRate.value?.total.tx_bps))
const diskUsed = computed(() => formatBytesShort(metrics.value?.disk_used_bytes, 1))
const diskTotal = computed(() => formatBytesShort(metrics.value?.disk_total_bytes, 0))
const diskPct = computed(() => {
    const m = metrics.value
    if (!m || !m.disk_total_bytes) return 0
    return Math.min(100, Math.round(((m.disk_used_bytes ?? 0) / m.disk_total_bytes) * 100))
})
const uptimeText = computed(() => formatUptime(metrics.value?.uptime_secs))
const loadText = computed(() => {
    const m = metrics.value
    if (m?.load1 == null) return null
    return `${m.load1?.toFixed(2)} ${m.load5?.toFixed(2)} ${m.load15?.toFixed(2)}`
})
const procCount = computed(() => {
    const m = metrics.value
    if (m?.process_count == null) return null
    return String(m.process_count)
})
const hostname = computed(() => (metrics.value?.hostname ?? null) as string | null)
const cpuCores = computed(() => {
    const c = metrics.value?.cpu_cores
    return c != null ? `${c}核` : null
})

// CPU 进度条颜色（>=80 红，>=50 黄，否则绿）
const cpuBarColor = computed(() => {
    const c = cpuPct.value
    if (c == null) return 'rgba(255,255,255,0.4)'
    if (c >= 80) return '#ff7a93'
    if (c >= 50) return '#ffc56b'
    return '#6ee08a'
})

// ============================================================
// 拖拽：标题栏 mousedown -> document mousemove 更新 x/y
// ============================================================
const dragging = ref(false)
const dragStart = { mx: 0, my: 0, ox: 0, oy: 0 }

function onDragStart(e: MouseEvent): void {
    if (e.button !== 0) return // 仅左键
    e.preventDefault()
    dragging.value = true
    dragStart.mx = e.clientX
    dragStart.my = e.clientY
    dragStart.ox = state.value.x
    dragStart.oy = state.value.y
    document.addEventListener('mousemove', onDragMove)
    document.addEventListener('mouseup', onDragEnd)
}

function onDragMove(e: MouseEvent): void {
    if (!dragging.value) return
    const dx = e.clientX - dragStart.mx
    const dy = e.clientY - dragStart.my
    let nx = dragStart.ox + dx
    let ny = dragStart.oy + dy
    // 约束在视口内（展开宽 200 / 高 ~300，收起 48）
    const w = state.value.expanded ? 200 : 48
    const h = state.value.expanded ? 320 : 48
    nx = Math.max(4, Math.min(window.innerWidth - w - 4, nx))
    ny = Math.max(40, Math.min(window.innerHeight - h - 4, ny))
    state.value = { ...state.value, x: nx, y: ny }
}

function onDragEnd(): void {
    if (!dragging.value) return
    dragging.value = false
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
    // 边缘吸附：松手时贴最近一侧边缘（中线左侧贴左、右侧贴右），减少遮挡
    const w = state.value.expanded ? 200 : 48
    const snappedX =
        state.value.x < window.innerWidth / 2 ? 4 : Math.max(4, window.innerWidth - w - 4)
    setPosition(snappedX, state.value.y)
}

// 切换按钮需阻止冒泡，避免触发拖拽
function onToggleClick(e: MouseEvent): void {
    e.stopPropagation()
    toggleExpanded()
}

onMounted(() => {
    // 挂载即钳制：默认 9999 哨兵 / 其他节点残留的越界坐标先拉回视口内，
    // 否则胶囊渲染在屏幕外永远不可见（106 因有拖动过的有效坐标而正常）
    ensureVisible()
    refreshMetrics()
    refreshNetRate()
    metricsTimer = setInterval(() => {
        refreshMetrics()
        refreshNetRate()
    }, 5000)
    clockTimer = setInterval(() => {
        clock.value = formatClock(new Date())
    }, 1000)
    // 视口变化（改窗口大小 / 旋转屏）后胶囊不会留在新视口之外
    window.addEventListener('resize', ensureVisible)
})

onUnmounted(() => {
    if (metricsTimer) clearInterval(metricsTimer)
    if (clockTimer) clearInterval(clockTimer)
    window.removeEventListener('resize', ensureVisible)
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
})
</script>

<template>
    <!-- 浮层根：position:fixed，不影响窗口管理器 -->
    <div
        class="sys-widget"
        :class="{ expanded: state.expanded, compact: !state.expanded, dragging }"
        :style="state.x >= 5000
            ? { right: '20px', top: state.y + 'px' }
            : { left: state.x + 'px', top: state.y + 'px' }"
    >
        <!-- ============ 收起态：48x48 小圆 ============ -->
        <div v-if="!state.expanded" class="sw-compact" @click="onToggleClick" title="展开运行信息">
            <svg class="sw-compact-ring" viewBox="0 0 36 36">
                <circle class="ring-bg" cx="18" cy="18" r="15.5" />
                <circle
                    class="ring-fg"
                    cx="18"
                    cy="18"
                    r="15.5"
                    :stroke-dasharray="`${((cpuPct ?? 0) / 100) * 97.4} 97.4`"
                    :style="{ stroke: cpuBarColor }"
                />
            </svg>
            <span class="sw-compact-text">{{ cpuPct ?? '--' }}<small>%</small></span>
            <span class="sw-compact-time">{{ clock.slice(0, 5) }}</span>
        </div>

        <!-- ============ 展开态：完整卡片 ============ -->
        <template v-else>
            <!-- 标题栏（可拖动） -->
            <header class="sw-header" @mousedown="onDragStart">
                <span class="sw-title">
                    <span class="sw-dot" :style="{ background: cpuBarColor }"></span>
                    {{ hostname ?? 'OS' }}
                </span>
                <button
                    class="sw-toggle"
                    type="button"
                    title="收起"
                    aria-label="收起"
                    @click="onToggleClick"
                    @mousedown.stop
                >
                    <svg viewBox="0 0 8 8" class="sw-toggle-icon">
                        <path d="M2 3l2-2 2 2M2 5l2 2 2-2" />
                    </svg>
                </button>
            </header>

            <!-- 时钟（始终显示，每秒更新） -->
            <div class="sw-clock">{{ clock }}</div>

            <!-- 指标列表（metrics 失败时不渲染，只留时钟） -->
            <div v-if="metrics" class="sw-metrics">
                <!-- CPU -->
                <div class="sw-row">
                    <div class="sw-row-head">
                        <span class="sw-label">CPU</span>
                        <span class="sw-value">{{ cpuPct ?? '--' }}%<small v-if="cpuCores"> · {{ cpuCores }}</small></span>
                    </div>
                    <div class="sw-bar">
                        <div class="sw-bar-fill" :style="{ width: (cpuPct ?? 0) + '%', background: cpuBarColor }"></div>
                    </div>
                </div>

                <!-- 内存 -->
                <div class="sw-row">
                    <div class="sw-row-head">
                        <span class="sw-label">内存</span>
                        <span class="sw-value">{{ memUsed }}/{{ memTotal }}</span>
                    </div>
                    <div class="sw-bar">
                        <div class="sw-bar-fill sw-bar-mem" :style="{ width: memPct + '%' }"></div>
                    </div>
                </div>

                <!-- 磁盘 -->
                <div class="sw-row">
                    <div class="sw-row-head">
                        <span class="sw-label">磁盘</span>
                        <span class="sw-value">{{ diskUsed }}/{{ diskTotal }}</span>
                    </div>
                    <div class="sw-bar">
                        <div class="sw-bar-fill sw-bar-disk" :style="{ width: diskPct + '%' }"></div>
                    </div>
                </div>

                <!-- 网络（实时速率：/api/v1/monitor/net-rate 差值采样） -->
                <div class="sw-net">
                    <span class="sw-label">网络</span>
                    <span class="sw-net-val">
                        <span class="sw-net-down">↓{{ netDown }}</span>
                        <span class="sw-net-up">↑{{ netUp }}</span>
                    </span>
                </div>

                <!-- 负载 / 进程 -->
                <div v-if="loadText" class="sw-line">
                    <span class="sw-label">负载</span>
                    <span class="sw-value-sm">{{ loadText }}</span>
                </div>
                <div v-if="procCount" class="sw-line">
                    <span class="sw-label">进程</span>
                    <span class="sw-value-sm">{{ procCount }}</span>
                </div>

                <!-- uptime -->
                <div v-if="uptimeText" class="sw-line">
                    <span class="sw-label">在线</span>
                    <span class="sw-value-sm">{{ uptimeText }}</span>
                </div>
            </div>

            <!-- 失败降级提示 -->
            <div v-else class="sw-fallback">
                <span class="sw-label">运行数据暂不可用</span>
            </div>
        </template>
    </div>
</template>

<style scoped>
/* ============================================================
   根浮层：position fixed，深色半透明 Yaru 风卡片
   ============================================================ */
.sys-widget {
    position: fixed;
    z-index: 100;
    font-family: var(--font);
    user-select: none;
}

/* ============================================================
   收起态：48x48 圆形仪表盘
   ============================================================ */
.sw-compact {
    /* 胶囊形态（非圆圈）——收起时仍可见关键数据 */
    min-width: 80px;
    height: 36px;
    padding: 0 14px;
    border-radius: 18px;
    gap: 8px;
    background: rgba(0, 0, 0, 0.6);
    backdrop-filter: blur(14px);
    -webkit-backdrop-filter: blur(14px);
    border: 1px solid rgba(255, 255, 255, 0.12);
    box-shadow: 0 4px 14px rgba(0, 0, 0, 0.4);
    cursor: pointer;
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    transition: transform 0.12s ease, box-shadow 0.12s ease;
}
.sw-compact:hover {
    transform: scale(1.06);
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.5);
}
.sw-compact-ring {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    transform: rotate(-90deg);
}
.ring-bg {
    fill: none;
    stroke: rgba(255, 255, 255, 0.15);
    stroke-width: 2.5;
}
.ring-fg {
    fill: none;
    stroke: #6ee08a;
    stroke-width: 2.5;
    stroke-linecap: round;
    transition: stroke-dasharray 0.4s ease, stroke 0.3s ease;
}
.sw-compact-text {
    color: #fff;
    font-size: 12px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    line-height: 1;
}
.sw-compact-text small {
    font-size: 8px;
    opacity: 0.7;
}

/* ============================================================
   展开态：200px 宽深色半透明卡片
   ============================================================ */
.sys-widget.expanded {
    width: 200px;
    background: rgba(0, 0, 0, 0.6);
    backdrop-filter: blur(14px);
    -webkit-backdrop-filter: blur(14px);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 10px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
    color: #fff;
    padding: 0;
    overflow: hidden;
}

/* —— 标题栏（拖动手柄）—— */
.sw-header {
    height: 28px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 4px 0 10px;
    cursor: grab;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
}
.sw-header:active {
    cursor: grabbing;
}
.sys-widget.dragging {
    transition: none;
}
.sw-title {
    font-size: 11.5px;
    font-weight: 600;
    letter-spacing: -0.01em;
    display: inline-flex;
    align-items: center;
    gap: 5px;
    opacity: 0.92;
    max-width: 140px;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
}
.sw-dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    flex-shrink: 0;
}
.sw-toggle {
    width: 22px;
    height: 22px;
    border: none;
    background: transparent;
    color: #fff;
    border-radius: 5px;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    opacity: 0.7;
    transition: background 0.12s ease, opacity 0.12s ease;
}
.sw-toggle:hover {
    background: rgba(255, 255, 255, 0.16);
    opacity: 1;
}
.sw-toggle-icon {
    width: 10px;
    height: 10px;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.3;
    stroke-linecap: round;
    stroke-linejoin: round;
}

/* —— 时钟 —— */
.sw-clock {
    font-family: var(--mono);
    font-size: 24px;
    font-weight: 700;
    text-align: center;
    letter-spacing: 1px;
    padding: 10px 0 8px;
    font-variant-numeric: tabular-nums;
    border-bottom: 1px solid rgba(255, 255, 255, 0.06);
}

/* —— 指标列表 —— */
.sw-metrics {
    padding: 8px 12px 12px;
    display: flex;
    flex-direction: column;
    gap: 9px;
}

/* —— 进度条行（CPU/MEM/磁盘）—— */
.sw-row {
    display: flex;
    flex-direction: column;
    gap: 4px;
}
.sw-row-head {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
}
.sw-label {
    opacity: 0.55;
    font-weight: 600;
    letter-spacing: 0.3px;
    font-size: 10px;
    text-transform: uppercase;
}
.sw-value {
    font-weight: 600;
}
.sw-value small {
    opacity: 0.55;
    font-weight: 500;
}
.sw-bar {
    height: 4px;
    background: rgba(255, 255, 255, 0.12);
    border-radius: 2px;
    overflow: hidden;
}
.sw-bar-fill {
    height: 100%;
    border-radius: 2px;
    transition: width 0.4s ease, background 0.3s ease;
}
.sw-bar-mem {
    background: #6ee08a;
}
.sw-bar-disk {
    background: #6cb8ee;
}

/* —— 网络行 —— */
.sw-net {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
}
.sw-net-val {
    display: inline-flex;
    gap: 8px;
}
.sw-net-down {
    color: #6ee08a;
}
.sw-net-up {
    color: #ffc56b;
}

/* —— 单行信息（负载/进程/在线）—— */
.sw-line {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
}
.sw-value-sm {
    opacity: 0.9;
}

/* —— 失败降级 —— */
.sw-fallback {
    padding: 12px;
    text-align: center;
}
</style>

.sw-compact-time {
    font-size: 12px;
    opacity: 0.8;
    font-variant-numeric: tabular-nums;
}
