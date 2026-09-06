<script setup lang="ts">
// =============================================================================
// CostPanel.vue —— 只读成本面板（by stage / by channel 两表；v0.1.1 从
// CostBadge 抽出的展示组件）。
//
// 纯展示：数据（FilmCostReport 宽容解析 {total, currency, groups}）由调用方
// 拉取传入。消费方：CostBadge 弹窗、HubBrowse「成本」Tab——同一张脸。
// =============================================================================
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import type { FilmCostReport } from '../api'

const props = defineProps<{
  /** 加载中。 */
  loading: boolean
  /** 加载错误文案（''=无）。 */
  error: string
  /** by=stage 聚合（null=未加载）。 */
  byStage: FilmCostReport | null
  /** by=channel 聚合（null=未加载）。 */
  byChannel: FilmCostReport | null
  /** 合计费用（顶部琥珀 pill 展示）。 */
  total: number
  /** 币种符号（缺省 ¥）。 */
  currency: string
}>()

const { t } = useI18n()

/** 组展示行（key/cost/events 宽容归一）。 */
function groupRows(r: FilmCostReport | null): { key: string; cost: number; events: number }[] {
  return (r?.groups ?? []).map((g) => ({
    key: String(g.key ?? g.stage ?? g.channel ?? '—'),
    cost: typeof g.cost === 'number' ? g.cost : typeof g.est_cost === 'number' ? g.est_cost : 0,
    events: typeof (g.events ?? g.calls) === 'number' ? Number(g.events ?? g.calls) : 0,
  }))
}

const stageRows = computed(() => groupRows(props.byStage))
const channelRows = computed(() => groupRows(props.byChannel))
</script>

<template>
  <div class="fh-pill fh-pill-amber" :title="t('film.costCurrencyNote')">
    {{ t('film.costEst') }} · {{ t('film.costTotal') }} {{ currency }}{{ total.toFixed(2) }}
  </div>
  <div v-if="loading" class="fh-empty">{{ t('film.loading') }}</div>
  <div v-else-if="error" class="fh-error-box">
    {{ t('film.costLoadFailed') }}{{ error }}
  </div>
  <template v-else>
    <div v-if="stageRows.length" class="fh-field">
      <span class="fh-field-label">{{ t('film.costByStage') }}</span>
      <table class="fh-cost-table">
        <thead>
          <tr><th>{{ t('film.costColKey') }}</th><th>{{ t('film.costColCost') }}</th><th>{{ t('film.costColEvents') }}</th></tr>
        </thead>
        <tbody>
          <tr v-for="(r, i) in stageRows" :key="`s${i}`">
            <td>{{ r.key }}</td>
            <td class="fh-mono">{{ currency }}{{ r.cost.toFixed(4) }}</td>
            <td class="fh-mono">{{ r.events }}</td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-if="channelRows.length" class="fh-field">
      <span class="fh-field-label">{{ t('film.costByChannel') }}</span>
      <table class="fh-cost-table">
        <thead>
          <tr><th>{{ t('film.costColKey') }}</th><th>{{ t('film.costColCost') }}</th><th>{{ t('film.costColEvents') }}</th></tr>
        </thead>
        <tbody>
          <tr v-for="(r, i) in channelRows" :key="`c${i}`">
            <td>{{ r.key }}</td>
            <td class="fh-mono">{{ currency }}{{ r.cost.toFixed(4) }}</td>
            <td class="fh-mono">{{ r.events }}</td>
          </tr>
        </tbody>
      </table>
    </div>
    <div v-if="!stageRows.length && !channelRows.length" class="fh-empty">
      {{ t('film.costEmpty') }}
    </div>
    <div class="fh-muted fh-small">{{ t('film.costCurrencyNote') }}</div>
  </template>
</template>

<style scoped>
.fh-cost-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 12.5px;
}
.fh-cost-table th,
.fh-cost-table td {
  text-align: left;
  padding: 5px 8px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.fh-cost-table th {
  font-weight: 600;
  color: var(--text-muted, #5E5C5F);
}
</style>
