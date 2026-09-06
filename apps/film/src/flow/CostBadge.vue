<script setup lang="ts">
// =============================================================================
// CostBadge.vue —— 成本徽章 + 只读成本面板（FilmHub v0.1.35）。
//
// 顶栏徽章：调用数 + 估算费用（GET :id/cost 聚合；refreshTick 联动任务终态
// 重载）。点击弹只读面板：by=stage / by=channel 两组聚合表（{total, currency,
// groups:[{key,cost,events}]} 宽容解析）——面板标注「估算」口径。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { filmGetCost, type FilmCostReport } from '../api'
import { useFlow } from './flowContext'

const { t } = useI18n()
const ctx = useFlow()

const summary = ref<FilmCostReport | null>(null)
const summaryError = ref('')
const loading = ref(false)

/** 面板态。 */
const panelOpen = ref(false)
const byStage = ref<FilmCostReport | null>(null)
const byChannel = ref<FilmCostReport | null>(null)
const panelError = ref('')
const panelLoading = ref(false)

const projectId = computed(() => ctx?.project.value?.id ?? '')

/** 费用合计（宽容：total > 0 才格式化；无记录显示 0）。 */
const totalCost = computed(() => {
  const v = summary.value?.total
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
})
const currency = computed(() => summary.value?.currency || '¥')

/** 调用数（calls > events 两字段宽容）。 */
const callCount = computed(() => {
  const s = summary.value
  const v = s?.calls ?? s?.events
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
})

async function loadSummary(): Promise<void> {
  if (!projectId.value) return
  loading.value = true
  try {
    summary.value = await filmGetCost(projectId.value)
    summaryError.value = ''
  } catch (e) {
    summary.value = null
    summaryError.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    loading.value = false
  }
}

async function openPanel(): Promise<void> {
  if (!projectId.value) return
  panelOpen.value = true
  panelLoading.value = true
  panelError.value = ''
  try {
    const [s, c] = await Promise.all([
      filmGetCost(projectId.value, 'stage'),
      filmGetCost(projectId.value, 'channel'),
    ])
    byStage.value = s
    byChannel.value = c
  } catch (e) {
    panelError.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    panelLoading.value = false
  }
}

/** 组展示行（key/cost/events 宽容归一）。 */
function groupRows(r: FilmCostReport | null): { key: string; cost: number; events: number }[] {
  return (r?.groups ?? []).map((g) => ({
    key: String(g.key ?? g.stage ?? g.channel ?? '—'),
    cost: typeof g.cost === 'number' ? g.cost : typeof g.est_cost === 'number' ? g.est_cost : 0,
    events: typeof (g.events ?? g.calls) === 'number' ? Number(g.events ?? g.calls) : 0,
  }))
}

onMounted(() => void loadSummary())
// 任务终态/项目刷新 → 重载徽章
watch(
  () => [ctx?.refreshTick.value, projectId.value] as const,
  () => void loadSummary(),
)
</script>

<template>
  <button
    class="fh-btn fh-btn-small fh-cost-badge"
    type="button"
    :title="summaryError ? t('film.costLoadFailed') + summaryError : t('film.costBadgeTip')"
    @click="openPanel"
  >
    <span class="fh-spin" :class="{ 'is-spinning': loading }" aria-hidden="true">↻</span>
    <span v-if="summaryError" class="fh-muted">💰 —</span>
    <template v-else>
      💰 {{ t('film.costCalls', { n: callCount }) }} ·
      {{ currency }}{{ totalCost.toFixed(2) }}
    </template>
  </button>

  <!-- 只读成本面板 -->
  <div v-if="panelOpen" class="fh-modal-backdrop" @click.self="panelOpen = false">
    <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-cost-title">
      <div class="fh-modal-head">
        <h3 id="film-cost-title">{{ t('film.costPanelTitle') }}</h3>
        <button class="fh-modal-close" type="button" @click="panelOpen = false">×</button>
      </div>
      <div class="fh-modal-body">
        <div class="fh-pill fh-pill-amber" :title="t('film.costCurrencyNote')">
          {{ t('film.costEst') }} · {{ t('film.costTotal') }} {{ currency }}{{ totalCost.toFixed(2) }}
        </div>
        <div v-if="panelLoading" class="fh-empty">{{ t('film.loading') }}</div>
        <div v-else-if="panelError" class="fh-error-box">
          {{ t('film.costLoadFailed') }}{{ panelError }}
        </div>
        <template v-else>
          <div v-if="groupRows(byStage).length" class="fh-field">
            <span class="fh-field-label">{{ t('film.costByStage') }}</span>
            <table class="fh-cost-table">
              <thead>
                <tr><th>{{ t('film.costColKey') }}</th><th>{{ t('film.costColCost') }}</th><th>{{ t('film.costColEvents') }}</th></tr>
              </thead>
              <tbody>
                <tr v-for="(r, i) in groupRows(byStage)" :key="`s${i}`">
                  <td>{{ r.key }}</td>
                  <td class="fh-mono">{{ currency }}{{ r.cost.toFixed(4) }}</td>
                  <td class="fh-mono">{{ r.events }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <div v-if="groupRows(byChannel).length" class="fh-field">
            <span class="fh-field-label">{{ t('film.costByChannel') }}</span>
            <table class="fh-cost-table">
              <thead>
                <tr><th>{{ t('film.costColKey') }}</th><th>{{ t('film.costColCost') }}</th><th>{{ t('film.costColEvents') }}</th></tr>
              </thead>
              <tbody>
                <tr v-for="(r, i) in groupRows(byChannel)" :key="`c${i}`">
                  <td>{{ r.key }}</td>
                  <td class="fh-mono">{{ currency }}{{ r.cost.toFixed(4) }}</td>
                  <td class="fh-mono">{{ r.events }}</td>
                </tr>
              </tbody>
            </table>
          </div>
          <div v-if="!groupRows(byStage).length && !groupRows(byChannel).length" class="fh-empty">
            {{ t('film.costEmpty') }}
          </div>
          <div class="fh-muted fh-small">{{ t('film.costCurrencyNote') }}</div>
        </template>
      </div>
    </div>
  </div>
</template>

<style scoped>
.fh-cost-badge { white-space: nowrap; }
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
