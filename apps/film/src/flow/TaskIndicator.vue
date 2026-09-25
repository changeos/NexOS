<script setup lang="ts">
// =============================================================================
// TaskIndicator.vue —— 顶栏全局任务指示器（v0.1.11）。
//
// 痛点：任务中心在工作台左栏底部——切到剧情/分镜/音频等流程页后「正在跑什么」
// 不可见。本组件以 NxBadge 风格挂工作室顶栏（及大厅 head-extra），**全部页面
// 可见**：
//   · 指示钮：`↻ N`（spinner 旋转 + 进行中任务数；N=0 整体不渲染）；
//   · 点击展开 NxPopover：进行中任务列表（标签 + 已耗时 mm:ss + 「X/Y」
//     分块进度——日志可解析时）+「查看全部任务」跳工作台并展开任务中心。
//
// 数据面（props.tasks）由 FilmStudio 从 trackedTasks 投影（非终态任务 →
// {id,label,createdAt,xy}）；耗时用组件内 1s tick 自刷（挂载即起、有任务才走）。
// =============================================================================
import { onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import NxPopover from '../nx/NxPopover.vue'

/** 指示器行（FilmStudio 投影）。 */
export interface IndicatorTask {
  id: string
  /** 本地化标签（stage 标签 + 关联镜头）。 */
  label: string
  /** 创建时刻（ms epoch；null=未知）。 */
  createdAt: number | null
  /** 分块进度「X/Y」（日志可解析时；''=不显示）。 */
  xy: string
}

const props = defineProps<{ tasks: IndicatorTask[] }>()
const emit = defineEmits<{ (e: 'view-all'): void }>()

const { t } = useI18n()

// —— 耗时自刷（1s tick；无任务时挂空转不发条）——
const nowMs = ref(Date.now())
let tick: ReturnType<typeof setInterval> | null = null
onMounted(() => {
  tick = setInterval(() => {
    if (props.tasks.length) nowMs.value = Date.now()
  }, 1000)
})
onUnmounted(() => {
  if (tick) clearInterval(tick)
})

/** 已耗时（mm:ss / h:mm:ss；无 createdAt → ''）。 */
function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  const pad = (n: number): string => String(n).padStart(2, '0')
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`
}

/** 行右侧 meta：X/Y > 已耗时 > —。 */
function rowMeta(task: IndicatorTask): string {
  if (task.xy) return task.xy
  if (typeof task.createdAt === 'number') {
    return t('toast.elapsed', { time: fmtElapsed(nowMs.value - task.createdAt) })
  }
  return '—'
}

// —— popover 受控开合（「查看全部」后收起）——
const open = ref(false)
function viewAll(): void {
  open.value = false
  emit('view-all')
}
</script>

<template>
  <NxPopover
    v-if="tasks.length"
    v-model:open="open"
    placement="bottom-end"
    :title="t('toast.tasksRunning', { n: tasks.length })"
  >
    <template #trigger>
      <button
        class="task-ind"
        type="button"
        :title="t('toast.tasksRunning', { n: tasks.length })"
        :aria-label="t('toast.tasksRunning', { n: tasks.length })"
      >
        <span class="task-ind__spin spin spinning" aria-hidden="true">↻</span>
        <span class="task-ind__count mono">{{ tasks.length }}</span>
      </button>
    </template>
    <div v-for="task in tasks" :key="task.id" class="task-ind__row">
      <span class="task-ind__label" :title="task.label">{{ task.label }}</span>
      <span class="task-ind__meta mono">{{ rowMeta(task) }}</span>
    </div>
    <button class="task-ind__all" type="button" @click="viewAll">
      {{ t('toast.viewAll') }} →
    </button>
  </NxPopover>
</template>

<style scoped>
/* 指示钮：info soft 徽章刻面（nx-badge--info 同族；独立类避免与 NxBadge 深耦合） */
.task-ind {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 2px 10px;
  border: 1px solid var(--nx-info-border);
  border-radius: var(--nx-radius-badge);
  background: var(--nx-info-bg);
  color: var(--nx-info);
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  font-weight: 500;
  line-height: 1.5;
  cursor: pointer;
  flex-shrink: 0;
  white-space: nowrap;
  transition: filter 0.15s ease;
}
.task-ind:hover { filter: brightness(1.04); }
.task-ind:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; }
.task-ind__spin {
  display: inline-block;
  font-size: 12px;
  line-height: 1;
  animation: task-ind-rot 0.9s linear infinite;
}
@keyframes task-ind-rot {
  to { transform: rotate(360deg); }
}
.task-ind__count { font-size: 11px; }

/* popover 行：标签（ellipsis）+ meta（mono 小字） */
.task-ind__row {
  display: flex;
  align-items: center;
  gap: 10px;
  min-width: 0;
}
.task-ind__label {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--nx-text-primary);
}
.task-ind__meta {
  flex-shrink: 0;
  font-size: 11px;
  color: var(--nx-text-tertiary);
}
/* 「查看全部任务」：accent 链接钮 */
.task-ind__all {
  border: none;
  background: transparent;
  color: var(--nx-accent);
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  padding: 2px 0 0;
  cursor: pointer;
  align-self: flex-start;
  white-space: nowrap;
}
.task-ind__all:hover { text-decoration: underline; }
.task-ind__all:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; border-radius: 4px; }
</style>
