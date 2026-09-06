<script setup lang="ts">
// =============================================================================
// CostBadge.vue —— 成本徽章 + 只读成本面板弹窗（FilmHub v0.1.35）。
//
// 顶栏徽章：调用数 + 估算费用（GET :id/cost 聚合；refreshTick 联动任务终态
// 重载）。点击弹只读面板：by=stage / by=channel 两组聚合表——表格展示抽为
// CostPanel.vue（v0.1.1，与 HubBrowse「成本」Tab 共用），本组件保留取数。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { filmGetCost, type FilmCostReport } from '../api'
import CostPanel from './CostPanel.vue'
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

  <!-- 只读成本面板（表格 = CostPanel 共用组件） -->
  <div v-if="panelOpen" class="fh-modal-backdrop" @click.self="panelOpen = false">
    <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-cost-title">
      <div class="fh-modal-head">
        <h3 id="film-cost-title">{{ t('film.costPanelTitle') }}</h3>
        <button class="fh-modal-close" type="button" @click="panelOpen = false">×</button>
      </div>
      <div class="fh-modal-body">
        <CostPanel
          :loading="panelLoading"
          :error="panelError"
          :by-stage="byStage"
          :by-channel="byChannel"
          :total="totalCost"
          :currency="currency"
        />
      </div>
    </div>
  </div>
</template>

<style scoped>
.fh-cost-badge { white-space: nowrap; }
</style>

