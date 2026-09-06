<script setup lang="ts">
// =============================================================================
// HealthBadge —— 健康状态彩色徽章（pill 样式，复用 GNOME 设计 token）
//
// 后端 health 枚举为 snake_case：healthy / degraded / unhealthy / unknown
// （见 os-core::Health + #[serde(rename_all = "snake_case")]）。
// =============================================================================
import { computed } from 'vue';
import type { Health } from '@/api/types';

const props = defineProps<{
  /** 健康状态（snake_case）。 */
  health?: Health | null;
  /** 是否显示中文标签（默认 true）；为 false 时直接显示原值。 */
  localized?: boolean;
}>();

interface Style {
  label: string;
  cls: string;
}

const MAP: Record<Health, Style> = {
  healthy: { label: '健康', cls: 'is-healthy' },
  degraded: { label: '降级', cls: 'is-degraded' },
  unhealthy: { label: '故障', cls: 'is-unhealthy' },
  unknown: { label: '未知', cls: 'is-unknown' },
};

const current = computed<Style>(() => {
  const v = String((props.health ?? 'unknown') as string).toLowerCase() as Health;
  return MAP[v] ?? MAP.unknown;
});

const text = computed(() =>
  props.localized === false ? String(props.health ?? 'unknown') : current.value.label,
);
</script>

<template>
  <span class="health-badge" :class="current.cls">{{ text }}</span>
</template>

<style scoped>
.health-badge {
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

/* 健康：Yaru 绿 */
.is-healthy {
  color: #15803d;
  background: #dcfce7;
  border-color: rgba(21, 128, 61, 0.15);
}

/* 降级：Yaru 黄橙 */
.is-degraded {
  color: #b45309;
  background: #fef3c7;
  border-color: rgba(180, 83, 9, 0.15);
}

/* 故障：Yaru 红 */
.is-unhealthy {
  color: #b91c1c;
  background: #fee2e2;
  border-color: rgba(185, 28, 28, 0.15);
}

/* 未知：中灰 */
.is-unknown {
  color: var(--text-muted, #475569);
  background: #e2e8f0;
  border-color: rgba(71, 85, 105, 0.12);
}
</style>
