<script setup lang="ts">
// =============================================================================
// CiBadge —— 内置 CI 状态徽章（v0.1.33）。
//
// 数据源：壳共享 ctx.ciLatest（GET /api/v1/coderepo/ci/latest 聚合，Explore
// 页一次拉全）。四态：
//   无记录   → 灰「CI –」（title 提示暂无运行）
//   queued   → 黄「排队中」
//   running  → 黄 spinner「运行中」
//   passed   → 绿「✓ 通过 · 耗时」（title 流水线命令）
//   failed   → 红「✗ 失败」（title 流水线命令）
//   skipped  → 灰「无流水线」
// 纯展示组件（点击行为归调用方——详情页传入 clickable 跳 CI Tab）。
// =============================================================================

import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { useNexhub } from '@/views/nexhub/context';
import type { CiRun } from '@/api/client';

const props = defineProps<{
  /** 仓库名（查 ctx.ciLatest 映射）。 */
  repo: string;
  /** 可点击（详情页头 → 切到 CI Tab；卡片上不传）。 */
  clickable?: boolean;
}>();

const emit = defineEmits<{ (e: 'open'): void }>();

const { t } = useI18n();
const ctx = useNexhub();

const run = computed<CiRun | undefined>(() => ctx.ciLatest.value[props.repo]);

/** 语义键：none | queued | running | passed | failed | skipped。 */
const kind = computed<string>(() => {
  const s = run.value?.status;
  if (!s) return 'none';
  return s;
});

/** 徽章文案。 */
const label = computed<string>(() => {
  switch (kind.value) {
    case 'queued':
      return t('nexhub.ci.statusQueued');
    case 'running':
      return t('nexhub.ci.statusRunning');
    case 'passed':
      return `${t('nexhub.ci.statusPassed')}${formatDuration(run.value)}`;
    case 'failed':
      return t('nexhub.ci.statusFailed');
    case 'skipped':
      return t('nexhub.ci.statusSkipped');
    default:
      return t('nexhub.ci.statusNone');
  }
});

/** 图标（spinner 由 CSS 动画驱动）。 */
const icon = computed<string>(() => {
  switch (kind.value) {
    case 'passed':
      return '✓';
    case 'failed':
      return '✗';
    case 'running':
      return '↻';
    case 'queued':
      return '◔';
    default:
      return '–';
  }
});

/** 悬浮提示（流水线命令 / 耗时）。 */
const title = computed<string>(() => {
  const r = run.value;
  if (!r) return t('nexhub.ci.noRunsHint');
  const parts: string[] = [];
  if (r.pipeline) parts.push(r.pipeline);
  if (r.trigger) parts.push(t(r.trigger === 'push' ? 'nexhub.ci.triggerPush' : 'nexhub.ci.triggerManual'));
  if (r.finished_at) parts.push(r.finished_at);
  return parts.join(' · ');
});

/** 毫秒 → 人读短时长（"1.2s" / "3m04s"）。 */
function formatDuration(run?: CiRun): string {
  const ms = run?.duration_ms;
  if (!ms || ms < 0) return '';
  const s = ms / 1000;
  if (s < 60) return ` · ${s < 10 ? s.toFixed(1) : Math.round(s)}s`;
  const m = Math.floor(s / 60);
  return ` · ${m}m${String(Math.round(s % 60)).padStart(2, '0')}s`;
}

function onClick(): void {
  if (props.clickable) emit('open');
}
</script>

<template>
  <span
    class="ci-badge"
    :class="[`is-${kind}`, { clickable }]"
    :title="title"
    role="status"
    :aria-label="`CI ${label}`"
    @click="onClick"
  >
    <span class="ci-icon" :class="{ spinning: kind === 'running' }" aria-hidden="true">{{ icon }}</span>
    <span class="ci-label">{{ label }}</span>
  </span>
</template>

<style scoped>
.ci-badge {
  display: inline-flex; align-items: center; gap: 4px;
  padding: 1px 9px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; font-weight: 600; white-space: nowrap;
  border: 1px solid transparent; user-select: none;
}
.ci-badge.clickable { cursor: pointer; }
.ci-badge.clickable:hover { filter: brightness(0.95); }
.ci-icon { display: inline-block; line-height: 1; }
.ci-icon.spinning { animation: ci-spin 1s linear infinite; }
@keyframes ci-spin { to { transform: rotate(360deg); } }

/* 无记录 / skipped：灰 */
.is-none, .is-skipped {
  background: var(--border-soft, #F3F4F6); color: var(--text-muted, #5E5C5F);
  border-color: var(--border, #d1d5db);
}
/* 排队 / 运行中：黄 */
.is-queued, .is-running {
  background: #fef9c3; color: #854d0e; border-color: rgba(161, 98, 7, 0.3);
}
/* 通过：绿 */
.is-passed { background: #dcfce7; color: #166534; border-color: rgba(22, 101, 52, 0.3); }
/* 失败：红 */
.is-failed { background: #fee2e2; color: #b91c1c; border-color: rgba(185, 28, 28, 0.3); }
</style>
