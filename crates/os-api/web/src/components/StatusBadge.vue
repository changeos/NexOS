<script setup lang="ts">
// =============================================================================
// StatusBadge —— VM 运行状态徽章（pill 样式，复用 GNOME 设计 token）
//
// 后端 VmState 枚举（snake_case）：running / paused / stopped / failed / migrating
// （见 os-compute::VmState + #[serde(rename_all = "snake_case")]）。
// =============================================================================
import { computed } from 'vue';
import type { VmState } from '@/api/types';

const props = defineProps<{
  /** VM 状态（snake_case）。 */
  state?: VmState | null;
}>();

interface Style {
  label: string;
  cls: string;
}

const MAP: Record<VmState, Style> = {
  running: { label: '运行中', cls: 'is-running' },
  stopped: { label: '已停止', cls: 'is-stopped' },
  paused: { label: '已暂停', cls: 'is-paused' },
  failed: { label: '故障', cls: 'is-failed' },
  migrating: { label: '迁移中', cls: 'is-migrating' },
};

const current = computed<Style>(() => {
  const v = String(props.state ?? 'stopped').toLowerCase() as VmState;
  return MAP[v] ?? { label: v || '未知', cls: 'is-stopped' };
});
</script>

<template>
  <span class="status-badge" :class="current.cls">{{ current.label }}</span>
</template>

<style scoped>
.status-badge {
  display: inline-block;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  font-weight: 600;
  line-height: 1.5;
  letter-spacing: -0.01em;
  white-space: nowrap;
  border: 1px solid transparent;
}

/* 运行中：Yaru 绿（带脉冲点） */
.is-running {
  color: #15803d;
  background: #dcfce7;
  border-color: rgba(21, 128, 61, 0.15);
}
.is-running::before {
  content: '';
  display: inline-block;
  width: 6px;
  height: 6px;
  margin-right: 5px;
  border-radius: 50%;
  background: #16a34a;
  vertical-align: 1px;
  animation: pulse 1.8s ease-in-out infinite;
}

/* 已停止：中灰 */
.is-stopped {
  color: var(--text-muted, #6b7280);
  background: #f3f4f6;
  border-color: rgba(107, 114, 128, 0.15);
}

/* 已暂停：Yaru 黄橙 */
.is-paused {
  color: #b45309;
  background: #fef3c7;
  border-color: rgba(180, 83, 9, 0.15);
}

/* 故障：Yaru 红 */
.is-failed {
  color: #b91c1c;
  background: #fee2e2;
  border-color: rgba(185, 28, 28, 0.15);
}

/* 迁移中：Ubuntu 橙 */
.is-migrating {
  color: #C7421A;
  background: #dbeafe;
  border-color: rgba(29, 78, 216, 0.15);
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.35;
  }
}

@media (prefers-reduced-motion: reduce) {
  .is-running::before {
    animation: none;
  }
}
</style>
