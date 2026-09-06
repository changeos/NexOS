<script setup lang="ts">
/**
 * WindowFrame —— 桌面浮窗外壳。
 *
 * 职责：
 *   - 渲染标题栏（GNOME 风窗口按钮：✕橙=关闭 —灰=最小化 □灰=最大化/还原，置于右上角）
 *   - 标题栏可拖拽（mousedown -> document mousemove 更新位置 -> mouseup 结束）
 *   - 右下角 resize 手柄拖拽缩放（最小 320x240）
 *   - 点击窗口任意区域聚焦（z-index 置顶）
 *   - 最大化时占满桌面区域；最小化时 display:none
 *   - 内容区通过具名默认 slot 渲染应用组件
 *
 * 拖拽/缩放监听挂到 document，保证鼠标移出窗口仍能继续。
 */
import { computed, onBeforeUnmount, ref } from 'vue'
import AppIcon from '@/components/AppIcon.vue'
import { useWindowManager, type WindowState } from '@/composables/useWindowManager'
import { findApp, getAppName } from '@/appRegistry'

const props = defineProps<{ win: WindowState }>()

const wm = useWindowManager()
const { focusWindow, closeWindow, minimizeWindow, maximizeWindow, moveWindow, resizeWindow, commitSnap } =
    wm

/** 当前窗口是否聚焦（用于阴影深浅）。 */
const isActive = computed(() => wm.activeId.value === props.win.id)

/** 窗口显示标题：注册应用实时跟随自定义名（改名后已打开窗口的标题同步更新）。 */
const displayTitle = computed(() =>
    findApp(props.win.id) ? getAppName(props.win.id) : props.win.title,
)

/** Aero Snap 预览高亮区的样式（拖到边缘时显示半透明蓝色区）。仅在拖拽本窗口时生效。 */
const snapPreviewStyle = computed<Record<string, string> | null>(() => {
    if (!dragging.value) return null
    const dir = wm.snapPreview.value
    if (!dir) return null
    if (dir === 'left') return { left: '0', top: '0', width: '50vw', height: '100vh' }
    if (dir === 'right')
        return { left: '50vw', top: '0', width: '50vw', height: '100vh' }
    return { left: '0', top: '0', width: '100vw', height: '100vh' } // maximized
})

// =============================================================================
// 拖拽：标题栏 mousedown -> 记录起始偏移 -> document mousemove 更新 x/y
// =============================================================================
const dragging = ref(false)
const dragStart = { mx: 0, my: 0, ox: 0, oy: 0 }

function onDragStart(e: MouseEvent): void {
    // 左键才拖拽。允许拖动最大化/snap 状态的窗口（moveWindow 会自动还原）。
    if (e.button !== 0) return
    // 不在标题栏按钮上才开始拖（按钮 stopPropagation）。
    e.preventDefault()
    focusWindow(props.win.id)
    dragging.value = true
    dragStart.mx = e.clientX
    dragStart.my = e.clientY
    dragStart.ox = props.win.x
    dragStart.oy = props.win.y
    document.addEventListener('mousemove', onDragMove)
    document.addEventListener('mouseup', onDragEnd)
}

function onDragMove(e: MouseEvent): void {
    if (!dragging.value) return
    const dx = e.clientX - dragStart.mx
    const dy = e.clientY - dragStart.my
    moveWindow(props.win.id, dragStart.ox + dx, dragStart.oy + dy)
}

function onDragEnd(): void {
    if (!dragging.value) return
    dragging.value = false
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
    // 松手时若处于 snap 预览区 → 提交吸附（Aero Snap）
    commitSnap(props.win.id)
}

// =============================================================================
// 缩放：右下角手柄 mousedown -> document mousemove 更新 width/height
// =============================================================================
const resizing = ref(false)
const resizeStart = { mx: 0, my: 0, w: 0, h: 0 }

function onResizeStart(e: MouseEvent): void {
    if (e.button !== 0 || props.win.maximized) return
    e.preventDefault()
    e.stopPropagation()
    focusWindow(props.win.id)
    resizing.value = true
    resizeStart.mx = e.clientX
    resizeStart.my = e.clientY
    resizeStart.w = props.win.width
    resizeStart.h = props.win.height
    document.addEventListener('mousemove', onResizeMove)
    document.addEventListener('mouseup', onResizeEnd)
}

function onResizeMove(e: MouseEvent): void {
    if (!resizing.value) return
    const dw = e.clientX - resizeStart.mx
    const dh = e.clientY - resizeStart.my
    resizeWindow(props.win.id, resizeStart.w + dw, resizeStart.h + dh)
}

function onResizeEnd(): void {
    if (!resizing.value) return
    resizing.value = false
    document.removeEventListener('mousemove', onResizeMove)
    document.removeEventListener('mouseup', onResizeEnd)
}

// 点击窗口任意区域聚焦。
function onFocus(): void {
    if (!isActive.value) focusWindow(props.win.id)
}

// 双击标题栏 = 最大化/还原。
function onTitleDblClick(): void {
    maximizeWindow(props.win.id)
}

onBeforeUnmount(() => {
    document.removeEventListener('mousemove', onDragMove)
    document.removeEventListener('mouseup', onDragEnd)
    document.removeEventListener('mousemove', onResizeMove)
    document.removeEventListener('mouseup', onResizeEnd)
})

// 关闭/最小化按钮需阻止冒泡，避免触发拖拽。
function doClose(e: MouseEvent): void {
    e.stopPropagation()
    closeWindow(props.win.id)
}
function doMinimize(e: MouseEvent): void {
    e.stopPropagation()
    minimizeWindow(props.win.id)
}
function doMaximize(e: MouseEvent): void {
    e.stopPropagation()
    maximizeWindow(props.win.id)
}
</script>

<template>
    <section
        class="window-frame"
        :class="{ active: isActive, maximized: win.maximized, minimized: win.minimized, dragging }"
        :style="{
            left: win.x + 'px',
            top: win.y + 'px',
            width: win.width + 'px',
            height: win.height + 'px',
            zIndex: win.zIndex,
        }"
        @mousedown="onFocus"
        :aria-hidden="win.minimized"
    >
        <!-- 标题栏：标题居左 + GNOME 风窗口按钮居右（可拖拽 + 双击最大化） -->
        <header class="title-bar" @mousedown="onDragStart" @dblclick="onTitleDblClick">
            <!-- 标题居左 -->
            <div class="title-text">
                <AppIcon :name="win.icon" :size="14" />
                <span>{{ displayTitle }}</span>
            </div>
            <!-- GNOME 风窗口按钮（右上角） -->
            <div class="traffic-lights">
                <button
                    class="tl tl-min"
                    type="button"
                    :aria-label="'最小化 ' + displayTitle"
                    title="最小化"
                    @click="doMinimize"
                    @mousedown.stop
                >
                    <svg viewBox="0 0 8 8" class="tl-icon"><path d="M1.5 4h5" /></svg>
                </button>
                <button
                    class="tl tl-max"
                    type="button"
                    :aria-label="win.maximized ? '还原 ' + displayTitle : '最大化 ' + displayTitle"
                    :title="win.maximized ? '还原' : '最大化'"
                    @click="doMaximize"
                    @mousedown.stop
                >
                    <svg v-if="!win.maximized" viewBox="0 0 8 8" class="tl-icon">
                        <path d="M2 2.5h4v4H2z" fill="currentColor" stroke="none" />
                    </svg>
                    <svg v-else viewBox="0 0 8 8" class="tl-icon">
                        <path d="M2.6 2H6v3.4M5.4 6H2V2.6" fill="none" />
                    </svg>
                </button>
                <button
                    class="tl tl-close"
                    type="button"
                    :aria-label="'关闭 ' + displayTitle"
                    title="关闭"
                    @click="doClose"
                    @mousedown.stop
                >
                    <svg viewBox="0 0 8 8" class="tl-icon"><path d="M1.5 1.5l5 5M6.5 1.5l-5 5" /></svg>
                </button>
            </div>
        </header>

        <!-- 内容区：slot 渲染应用组件 -->
        <div class="window-body">
            <slot />
        </div>

        <!-- 右下角缩放手柄 -->
        <span
            v-if="!win.maximized"
            class="resize-handle"
            @mousedown="onResizeStart"
            title="拖拽缩放"
        ></span>

        <!-- Aero Snap 预览高亮区（拖到屏幕边缘时显示半透明蓝色区，提示松手将吸附） -->
        <Teleport to="body">
            <div v-if="snapPreviewStyle" class="snap-preview" :style="snapPreviewStyle"></div>
        </Teleport>
    </section>
</template>

<style scoped>
.window-frame {
    position: absolute;
    display: flex;
    flex-direction: column;
    background: var(--bg-card, #fff);
    border: 1px solid var(--border, rgba(0, 0, 0, 0.1));
    border-radius: var(--radius-md, 12px);
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.18), 0 2px 8px rgba(0, 0, 0, 0.1);
    overflow: hidden;
    /* 拖拽/缩放时不触发文本选中 */
    user-select: none;
}
.window-frame.active {
    box-shadow: 0 18px 48px rgba(0, 0, 0, 0.28), 0 6px 16px rgba(0, 0, 0, 0.16);
}
.window-frame.dragging {
    transition: none;
}
.window-frame.minimized {
    display: none;
}
/* 最大化时占满桌面区域：位置归零，宽高撑满（由父容器定位上下文控制） */
.window-frame.maximized {
    left: 0 !important;
    top: 0 !important;
    width: 100% !important;
    height: 100% !important;
    border-radius: 0;
    border: none;
}

/* —— 标题栏 —— */
.title-bar {
    height: 38px;
    flex-shrink: 0;
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 0 6px 0 12px;
    background: var(--bg-elev, #F7F7F7);
    border-bottom: 1px solid var(--hairline, rgba(0, 0, 0, 0.08));
    cursor: default;
}
.window-frame.active .title-bar {
    background: var(--bg-elev, #F7F7F7);
}

/* —— GNOME 风窗口按钮（右上角符号按钮，默认低饱和，hover 才显色） —— */
.traffic-lights {
    display: flex;
    align-items: center;
    gap: 2px;
    flex-shrink: 0;
}
.tl {
    width: 22px;
    height: 22px;
    border-radius: var(--radius-sm, 6px);
    border: none;
    padding: 0;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    position: relative;
    background: transparent;
    transition: background 0.12s ease, color 0.12s ease;
}
.tl-icon {
    width: 9px;
    height: 9px;
    fill: none;
    stroke: var(--text-muted, #5E5C5F);
    stroke-width: 1.4;
    stroke-linecap: round;
    opacity: 1; /* GNOME 风：符号常驻显示 */
    transition: stroke 0.12s ease;
}
/* 默认低饱和，仅 hover 该按钮时才显色（GNOME 交互特征） */
.tl-min:hover {
    background: var(--accent-soft, rgba(233, 84, 32, 0.12));
}
.tl-min:hover .tl-icon {
    stroke: var(--win-min, #5E5C5F);
}
.tl-max:hover {
    background: var(--accent-soft, rgba(233, 84, 32, 0.12));
}
.tl-max:hover .tl-icon {
    stroke: var(--win-max, #5E5C5F);
}
.tl-close:hover {
    background: var(--win-close, #E95420);
}
.tl-close:hover .tl-icon {
    stroke: #fff;
}

/* —— 标题文字（图标 + 名称，居左 flex:1） —— */
.title-text {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: flex-start;
    gap: 6px;
    font-size: 13px;
    font-weight: 600;
    color: var(--text, #2B2B2B);
    letter-spacing: -0.01em;
    pointer-events: none;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
}

/* —— 内容区 —— */
.window-body {
    flex: 1;
    min-height: 0;
    overflow: auto;
    position: relative;
    background: var(--bg, #fafafa);
    /* 应用内容允许选择复制（接入说明/文档/日志等）；标题栏仍保持
       user-select:none 防拖拽误选 */
    user-select: text;
}

/* —— 右下角缩放手柄 —— */
.resize-handle {
    position: absolute;
    right: 0;
    bottom: 0;
    width: 16px;
    height: 16px;
    cursor: nwse-resize;
    z-index: 2;
}
.resize-handle::after {
    content: '';
    position: absolute;
    right: 3px;
    bottom: 3px;
    width: 0;
    height: 0;
    border-style: solid;
    border-width: 0 0 10px 10px;
    border-color: transparent transparent rgba(0, 0, 0, 0.25) transparent;
}
.resize-handle:hover::after {
    border-bottom-color: var(--accent, #E95420);
}

/* —— Aero Snap 预览高亮区（半透明蓝色，覆盖桌面屏幕区；与 Windows/GNOME snap 反馈一致）—— */
.snap-preview {
    position: fixed;
    z-index: 9999;
    pointer-events: none;
    background: rgba(48, 119, 255, 0.22); /* 半透明蓝色高亮 */
    border: 2px solid rgba(48, 119, 255, 0.6);
    border-radius: 8px;
    box-shadow: 0 0 0 4px rgba(48, 119, 255, 0.1);
    transition: all 0.08s ease-out;
}
</style>
