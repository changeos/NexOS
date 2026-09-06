<script setup lang="ts">
// =============================================================================
// CapacityBar —— 容量进度条
//
// 接受 Capacity 对象（{used_bytes, total_bytes}），渲染彩色进度条 + 文本。
// 颜色阈值（与 dashboard.js / storage.js 一致）：
//   >= 90% 红（err）、>= 75% 橙（warn）、其余 绿（ok）。
// =============================================================================
import { computed } from 'vue';
import type { Capacity } from '@/api/types';
import { formatBytes, ratioPct } from '@/utils/format';

const props = withDefaults(
  defineProps<{
    /** 容量对象。 */
    capacity?: Capacity | null;
    /** 是否显示右侧文本（百分比 + 已用/总量）；默认 true。 */
    showText?: boolean;
    /** 是否在容量未配置（total=0/null）时显示「未配置」字样；默认 true。 */
    showEmpty?: boolean;
  }>(),
  { showText: true, showEmpty: true },
);

const used = computed(() => Number(props.capacity?.used_bytes ?? 0));
const total = computed(() => Number(props.capacity?.total_bytes ?? 0));
const ratio = computed(() => (total.value > 0 ? used.value / total.value : 0));
const pct = computed(() => ratioPct(ratio.value));

const cls = computed(() => {
  if (ratio.value >= 0.9) return 'is-err';
  if (ratio.value >= 0.75) return 'is-warn';
  return 'is-ok';
});

const empty = computed(() => !props.capacity || !total.value);
</script>

<template>
  <div v-if="empty && showEmpty" class="capacity-empty muted">未配置</div>
  <div v-else class="capacity">
    <div class="progress" role="progressbar" :aria-valuenow="pct" aria-valuemin="0" aria-valuemax="100">
      <div class="progress-fill" :class="cls" :style="{ width: `${pct}%` }"></div>
    </div>
    <span v-if="showText" class="capacity-text">
      {{ pct }}% · {{ formatBytes(used) }} / {{ formatBytes(total) }}
    </span>
  </div>
</template>

<style scoped>
.capacity {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 200px;
}

.progress {
  flex: 1;
  height: 8px;
  background: var(--border-soft, #EDEDED);
  border-radius: 4px;
  overflow: hidden;
}

.progress-fill {
  height: 100%;
  border-radius: 4px;
  transition: width 0.25s ease;
}

.is-ok {
  background: var(--ok, #0E8420);
}
.is-warn {
  background: var(--warn, #F99B11);
}
.is-err {
  background: var(--err, #C7162B);
}

.capacity-text {
  font-size: 12px;
  color: var(--text, #2B2B2B);
  min-width: 180px;
  text-align: right;
  font-variant-numeric: tabular-nums;
}

.capacity-empty {
  font-size: 13px;
  color: var(--text-muted, #5E5C5F);
}

.muted {
  color: var(--text-muted, #5E5C5F);
}
</style>
