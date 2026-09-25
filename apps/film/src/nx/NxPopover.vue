<script setup lang="ts">
// =============================================================================
// NxPopover.vue —— 浮层基座（无依赖手写；试点页需要时再评估 reka-ui）。
//
// 形态：trigger 插槽（点击/hover 展开）+ 默认插槽（面板内容，Teleport 到 body
// + fixed 定位——躲开卡片 hover transform 形成的 containing block）。
// 定位：按触发器 getBoundingClientRect 计算，四角放置（bottom-start 缺省）；
// 打开后二次测量校正；窗口滚动/缩放时重算，触发器滚出视口不强制关闭。
// 无障碍：trigger aria-haspopup/aria-expanded；面板 role=dialog + tabindex；
// Esc 关闭（click 模式焦点回触发器）；点面板外关闭；hover 模式带进出场延时。
// 开合对冒烟可见：面板根类 nx-pop__panel（open 时存在于 DOM）。
// =============================================================================
import { nextTick, onBeforeUnmount, ref, watch } from 'vue'

const props = withDefaults(
  defineProps<{
    /** v-model:open 受控开合（也支持非受控自管）。 */
    open?: boolean
    /** 放置角（相对触发器）。 */
    placement?: 'bottom-start' | 'bottom-end' | 'top-start' | 'top-end'
    /** 展开方式：click（点按，缺省）/ hover（悬停，源行任务明细用）。 */
    trigger?: 'click' | 'hover'
    /** 禁用展开（如无任务数据的源行）。 */
    disabled?: boolean
    /** 面板标题（可省；给 aria-label 与 nx-pop__title 行）。 */
    title?: string
  }>(),
  { open: undefined, placement: 'bottom-start', trigger: 'click', disabled: false, title: '' },
)

const emit = defineEmits<{ (e: 'update:open', v: boolean): void }>()

const isOpen = ref(props.open ?? false)
const rootEl = ref<HTMLElement | null>(null)
const panelEl = ref<HTMLElement | null>(null)
const panelStyle = ref<Record<string, string>>({})
/** 受控模式同步（外部 v-model 变化 → 内部态 + 重定位）。 */
watch(
  () => props.open,
  (v) => {
    if (v !== undefined && v !== isOpen.value) {
      isOpen.value = v
      if (v) void place()
    }
  },
)

function setOpen(v: boolean): void {
  if (props.disabled && v) return
  if (isOpen.value === v) return
  isOpen.value = v
  emit('update:open', v)
  if (v) void place()
}

/** 计算面板坐标（fixed；触发器矩形 + 面板实测尺寸两遍收敛）。 */
async function place(): Promise<void> {
  await nextTick()
  const root = rootEl.value
  const panel = panelEl.value
  if (!root || !panel) return
  const r = root.getBoundingClientRect()
  const pw = panel.offsetWidth
  const ph = panel.offsetHeight
  const GAP = 6
  let top = 0
  let left = 0
  const p = props.placement
  if (p === 'bottom-start' || p === 'bottom-end') top = r.bottom + GAP
  else top = r.top - ph - GAP
  if (p === 'bottom-start' || p === 'top-start') left = r.left
  else left = r.right - pw
  // 视口夹取（水平方向）
  left = Math.max(8, Math.min(left, window.innerWidth - pw - 8))
  panelStyle.value = { top: `${Math.round(top)}px`, left: `${Math.round(left)}px` }
}

function onDocPointerDown(e: PointerEvent): void {
  const t = e.target as Node
  if (rootEl.value?.contains(t) || panelEl.value?.contains(t)) return
  setOpen(false)
}
function onKeydown(e: KeyboardEvent): void {
  if (e.key !== 'Escape') return
  setOpen(false)
  if (props.trigger === 'click')
    (rootEl.value?.querySelector('button, [tabindex]') as HTMLElement | null)?.focus?.()
}
function onViewportChange(): void {
  if (isOpen.value) void place()
}

watch(isOpen, (v) => {
  if (typeof document === 'undefined') return
  if (v) {
    document.addEventListener('pointerdown', onDocPointerDown, true)
    document.addEventListener('keydown', onKeydown, true)
    window.addEventListener('scroll', onViewportChange, true)
    window.addEventListener('resize', onViewportChange)
  } else {
    document.removeEventListener('pointerdown', onDocPointerDown, true)
    document.removeEventListener('keydown', onKeydown, true)
    window.removeEventListener('scroll', onViewportChange, true)
    window.removeEventListener('resize', onViewportChange)
  }
})
onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', onDocPointerDown, true)
  document.removeEventListener('keydown', onKeydown, true)
  window.removeEventListener('scroll', onViewportChange, true)
  window.removeEventListener('resize', onViewportChange)
})

// —— hover 模式延时进出场（防掠过误开/移入面板途中误关） ——
let hoverTimer: ReturnType<typeof setTimeout> | null = null
function hoverEnter(): void {
  if (props.trigger !== 'hover' || props.disabled) return
  if (hoverTimer) clearTimeout(hoverTimer)
  hoverTimer = setTimeout(() => setOpen(true), 120)
}
function hoverLeave(): void {
  if (props.trigger !== 'hover') return
  if (hoverTimer) clearTimeout(hoverTimer)
  hoverTimer = setTimeout(() => setOpen(false), 180)
}

defineExpose({ close: () => setOpen(false) })
</script>

<template>
  <span
    ref="rootEl"
    class="nx-pop"
    @mouseenter="hoverEnter"
    @mouseleave="hoverLeave"
  >
    <span
      class="nx-pop__trigger"
      :aria-haspopup="'dialog'"
      :aria-expanded="isOpen"
      @click.stop="setOpen(!isOpen)"
    ><slot name="trigger" /></span>
    <Teleport to="body">
      <div
        v-if="isOpen"
        ref="panelEl"
        class="nx-pop__panel"
        :style="panelStyle"
        role="dialog"
        tabindex="-1"
        :aria-label="title || undefined"
      >
        <div v-if="title" class="nx-pop__title">{{ title }}</div>
        <slot />
      </div>
    </Teleport>
  </span>
</template>
