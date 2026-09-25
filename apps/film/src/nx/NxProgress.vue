<script setup lang="ts">
// =============================================================================
// NxProgress.vue —— 进度基座（离散 X/Y 与不确定态）。
//
// 离散：传 done/total（或 value 0-100）→ 定宽条（width 过渡）；不确定：
// indeterminate → 滑动条（无假进度口径：拿不到分块数据时用运行态徽章而非
// 本组件乱跳）。state 变色：accent（缺省）/ success / danger。
// 无障碍：role=progressbar + aria-valuenow/min/max（不确定态省 now）。
// =============================================================================
import { computed } from 'vue'

const props = withDefaults(
  defineProps<{
    /** 0-100 直给（与 done/total 二选一）。 */
    value?: number | null
    done?: number
    total?: number
    indeterminate?: boolean
    size?: 'sm' | 'md'
    state?: 'accent' | 'success' | 'danger'
  }>(),
  {
    value: null,
    done: 0,
    total: 0,
    indeterminate: false,
    size: 'md',
    state: 'accent',
  },
)

const pct = computed(() => {
  if (props.indeterminate) return 0
  if (typeof props.value === 'number') return Math.max(0, Math.min(100, props.value))
  if (props.total > 0) return Math.max(0, Math.min(100, Math.round((props.done / props.total) * 100)))
  return 0
})
</script>

<template>
  <div
    class="nx-progress"
    :class="[size === 'sm' ? 'nx-progress--sm' : '', state !== 'accent' ? `nx-progress--${state}` : '']"
    role="progressbar"
    :aria-valuenow="indeterminate ? undefined : pct"
    aria-valuemin="0"
    aria-valuemax="100"
  >
    <div
      class="nx-progress__bar"
      :class="{ 'is-indeterminate': indeterminate }"
      :style="indeterminate ? undefined : { width: `${pct}%` }"
    />
  </div>
</template>
