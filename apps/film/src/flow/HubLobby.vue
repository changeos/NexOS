<script setup lang="ts">
// =============================================================================
// HubLobby.vue —— FilmHub 大厅（v0.1.7 UI 重设计第一批·全量重皮试点页一）。
//
// 形态不变（品牌顶栏 + 项目卡网格 + 丰富模式），视觉全量迁 design token 体系
// （nx-* 基座类 + 语义工具类；样式唯一事实源 src/design/theme.css 的
// 「试点页一」区）：
//   · 顶栏：品牌方标（渐变胶片标）+ 搜索（pill 输入 + focus 环）+ 深浅主题
//     切换 + 丰富模式开关 + 刷新/新建（NxButton）；
//   · 表头内嵌筛选（v0.1.7 新增）：类别（分辨率档）/ 阶段 收进列表头
//     NxPopover 下拉（数据面 filterHubProjects 纯函数，hubFilters.ts）；
//   · 项目卡：NxCard 三态（静默 0.5px 描边+shadow-xs / hover 浮起 shadow-md
//     +换底 / 选中 accent+shadow-lg）+ 五阶段进度点（完成绿描点/当前主色
//     实底）+ 成员 chips（首字头像）+ 最近活动行 + 成本小字；
//   · 空态（搜索空/列表空分口径）与加载 skeleton（shimmer 占位卡）。
// 丰富取数/降级逻辑（v0.1.1）原样保留；卡片操作仍只 emit（弹窗在 FilmStudio）。
// =============================================================================
import { computed, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  filmGetCost,
  filmGetFile,
  type FilmProject,
} from '../api'
import { fmtActivityTime, parseActivity, parseOwnership, type FilmActivityEntry } from './collab'
import { deriveStageFromProject } from './flowFiles'
import { filterHubProjects, hubRatioOptions, type HubFilterState } from './hubFilters'
import { FLOW_STAGES, ratioPresetOf, presetKeyCap, stageIndex, type FilmStage } from './flowTypes'
import NxButton from '../nx/NxButton.vue'
import NxCard from '../nx/NxCard.vue'
import NxPopover from '../nx/NxPopover.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'

const props = defineProps<{
  /** 项目列表（FilmStudio loadProjects 态）。 */
  projects: FilmProject[]
  /** 列表加载中。 */
  loading: boolean
  /** 列表加载错误（''=无）。 */
  error: string
}>()

const emit = defineEmits<{
  (e: 'refresh'): void
  (e: 'create'): void
  (e: 'open', p: FilmProject): void
  (e: 'browse', p: FilmProject): void
  (e: 'delete', p: FilmProject): void
}>()

const { t } = useI18n()

// —— 筛选态（搜索 × 阶段 × 类别；数据面纯函数 filterHubProjects） ——
const filter = reactive<HubFilterState>({ q: '', stage: '', ratio: '' })
const stageMenuOpen = ref(false)
const ratioMenuOpen = ref(false)

const filtered = computed<FilmProject[]>(() => filterHubProjects(props.projects, filter))
const ratioOptions = computed(() => hubRatioOptions(props.projects))

/** 阶段筛选当前标签（''=全部阶段）。 */
const stageFilterLabel = computed(() =>
  filter.stage ? stageLabel(filter.stage) : t('filmhub.filterAll'),
)

/** 类别筛选当前标签（预设名，未知档回退原始 ratio 串）。 */
const ratioFilterLabel = computed(() => {
  if (!filter.ratio) return t('filmhub.filterAll')
  const p = ratioPresetOf(filter.ratio)
  return p ? t(`film.preset${presetKeyCap(p.key)}`) : filter.ratio
})

function setStage(v: FilmStage | ''): void {
  filter.stage = v
  stageMenuOpen.value = false
}

function setRatio(v: string): void {
  filter.ratio = v
  ratioMenuOpen.value = false
}

// —— 丰富模式（缺省开；localStorage 记忆） ——
const RICH_KEY = 'nexos.film.hubRich'
const rich = ref(true)
try {
  rich.value = localStorage.getItem(RICH_KEY) !== '0'
} catch {
  /* 隐私模式等：忽略 */
}
function toggleRich(): void {
  rich.value = !rich.value
  try {
    localStorage.setItem(RICH_KEY, rich.value ? '1' : '0')
  } catch {
    /* 忽略 */
  }
}

/** 单项目丰富信息（失败的字段缺省——素卡降级）。 */
interface HubRichInfo {
  members: string[]
  latest: FilmActivityEntry | null
  calls: number | null
}
const richMap = reactive<Record<string, HubRichInfo>>({})

function richInfo(p: FilmProject): HubRichInfo | null {
  return rich.value && richMap[p.id] ? richMap[p.id] : null
}

/** 丰富取数代际号（防过期响应覆写新列表）。 */
let richGen = 0

/**
 * 并发拉丰富数据：最近 ≤12 个项目（更新时间取新），每项目三个轻读
 * （ownership/activity 走 files 面 + cost 聚合）；Promise.allSettled 单项
 * 失败静默——卡片按字段降级，不打扰列表。
 */
async function loadRich(): Promise<void> {
  const gen = ++richGen
  const targets = [...props.projects]
    .sort((a, b) => (b.updated_at ?? '').localeCompare(a.updated_at ?? ''))
    .slice(0, 12)
  if (!targets.length) return
  await Promise.allSettled(
    targets.map(async (p) => {
      const [own, act, cost] = await Promise.allSettled([
        filmGetFile(p.id, 'ownership.json'),
        filmGetFile(p.id, 'activity.json'),
        filmGetCost(p.id),
      ])
      if (gen !== richGen) return
      const calls =
        cost.status === 'fulfilled' &&
        typeof (cost.value.calls ?? cost.value.events) === 'number'
          ? Number(cost.value.calls ?? cost.value.events)
          : null
      richMap[p.id] = {
        members: own.status === 'fulfilled' ? (parseOwnership(own.value)?.members ?? []) : [],
        latest: act.status === 'fulfilled' ? (parseActivity(act.value)[0] ?? null) : null,
        calls,
      }
    }),
  )
}

watch(
  () => [props.projects, rich.value] as const,
  () => {
    if (rich.value) void loadRich()
  },
  { immediate: true, deep: false },
)

// —— 卡片阶段进度（启发式推导；详见 deriveStageFromMarkdown 注释） ——
function stageStateOf(p: FilmProject, s: FilmStage): 'done' | 'current' | 'todo' {
  const cur = stageIndex(deriveStageFromProject(p))
  const idx = stageIndex(s)
  return idx < cur ? 'done' : idx === cur ? 'current' : 'todo'
}

/** 阶段项 i18n 标签（显式映射——vue-i18n 键不做运行时拼接）。 */
function stageLabel(s: FilmStage): string {
  switch (s) {
    case 'story':
      return t('film.flowStory')
    case 'storyboard':
      return t('film.flowStoryboard')
    case 'casting':
      return t('film.flowCasting')
    case 'audio':
      return t('film.flowAudio')
    case 'compose':
      return t('film.flowCompose')
  }
}

/** 最近活动一句（谁 + 动作 + 对象；时间入 title）。 */
function latestLine(a: FilmActivityEntry): string {
  return `${a.author || 'anonymous'} ${a.action || '—'} ${a.target ?? ''}`.trim()
}

function latestTitle(a: FilmActivityEntry): string {
  return `${t('filmhub.latestTip')} · ${fmtActivityTime(a.ts)}`
}

/** 成员 chip 首字头像（CJK 首字 / 拉丁首字母大写）。 */
function memberInitial(m: string): string {
  const c = (m || '?').trim().charAt(0)
  return c || '?'
}

function fmtTime(iso?: string | null): string {
  if (!iso) return '—'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

// —— 卡片选中态（键盘 Tab/点击聚焦的行——视觉第三态） ——
const selectedCard = ref('')
function onCardFocusIn(p: FilmProject): void {
  selectedCard.value = p.id
}
function onCardFocusOut(e: FocusEvent): void {
  const to = e.relatedTarget as Node | null
  if (to && (e.currentTarget as HTMLElement).contains(to)) return
  selectedCard.value = ''
}

/** 键盘开卡（Enter/Space——卡片可达性）。 */
function onCardKey(e: KeyboardEvent, p: FilmProject): void {
  if (e.key !== 'Enter' && e.key !== ' ') return
  e.preventDefault()
  emit('open', p)
}
</script>

<template>
  <div class="hub-lobby nx-page">
    <!-- 品牌顶栏 -->
    <div class="hub-head">
      <div class="hub-brand">
        <span class="hub-brand-icon" aria-hidden="true">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
            <rect x="2" y="4" width="20" height="16" rx="2.5" />
            <path d="M7 4v16M17 4v16M2 9h5M2 15h5M17 9h5M17 15h5" />
          </svg>
        </span>
        <div>
          <div class="hub-brand-name">FilmHub</div>
          <div class="hub-brand-sub">{{ t('filmhub.tagline') }}</div>
        </div>
      </div>
      <div class="hub-head-actions">
        <NxThemeToggle />
        <input
          v-model="filter.q"
          type="search"
          class="hub-search"
          :placeholder="t('filmhub.searchPh')"
          :aria-label="t('filmhub.searchPh')"
        >
        <button
          class="hub-rich-toggle nx-focus"
          :class="{ 'is-on': rich }"
          type="button"
          :title="t('filmhub.richModeTip')"
          :aria-pressed="rich"
          @click="toggleRich"
        >🌿 {{ t('filmhub.richMode') }}</button>
        <NxButton size="sm" :disabled="loading" @click="emit('refresh')">
          {{ t('film.refresh') }}
        </NxButton>
        <NxButton variant="primary" size="sm" @click="emit('create')">
          + {{ t('film.newProject') }}
        </NxButton>
        <slot name="head-extra" />
      </div>
    </div>

    <!-- 错误条 -->
    <div v-if="error" class="nx-alert nx-alert--danger" role="alert">
      <span>{{ t('film.listFailed') }}{{ error }}</span>
      <NxButton size="sm" @click="emit('refresh')">{{ t('film.retry') }}</NxButton>
    </div>

    <!-- 列表（滚动区） -->
    <div class="hub-scroll">
      <!-- 表头：计数 + 内嵌筛选（类别/阶段收进下拉） -->
      <div class="hub-listhead">
        <span class="hub-count">{{ t('filmhub.count', { n: filtered.length }) }}</span>
        <div class="hub-filters">
          <NxPopover v-model:open="ratioMenuOpen" placement="bottom-end">
            <template #trigger>
              <NxButton
                size="sm"
                class="hub-filter-btn"
                :class="{ 'is-filtered': !!filter.ratio }"
                :aria-label="t('filmhub.filterRatio')"
              >{{ t('filmhub.filterRatio') }}：{{ ratioFilterLabel }}
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>
              </NxButton>
            </template>
            <div class="hub-filter-menu">
              <button
                class="hub-filter-opt"
                :class="{ 'is-on': !filter.ratio }"
                type="button"
                @click="setRatio('')"
              ><span class="hub-filter-opt__mark">{{ !filter.ratio ? '✓' : '' }}</span>{{ t('filmhub.filterAll') }}</button>
              <button
                v-for="r in ratioOptions"
                :key="r"
                class="hub-filter-opt"
                :class="{ 'is-on': filter.ratio === r }"
                type="button"
                @click="setRatio(r)"
              ><span class="hub-filter-opt__mark">{{ filter.ratio === r ? '✓' : '' }}</span>{{ ratioPresetOf(r) ? t(`film.preset${presetKeyCap(ratioPresetOf(r)!.key)}`) : r }}</button>
            </div>
          </NxPopover>
          <NxPopover v-model:open="stageMenuOpen" placement="bottom-end">
            <template #trigger>
              <NxButton
                size="sm"
                class="hub-filter-btn"
                :class="{ 'is-filtered': !!filter.stage }"
                :aria-label="t('filmhub.filterStage')"
              >{{ t('filmhub.filterStage') }}：{{ stageFilterLabel }}
                <svg viewBox="0 0 24 24" width="12" height="12" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>
              </NxButton>
            </template>
            <div class="hub-filter-menu">
              <button
                class="hub-filter-opt"
                :class="{ 'is-on': !filter.stage }"
                type="button"
                @click="setStage('')"
              ><span class="hub-filter-opt__mark">{{ !filter.stage ? '✓' : '' }}</span>{{ t('filmhub.filterAll') }}</button>
              <button
                v-for="s in FLOW_STAGES"
                :key="s"
                class="hub-filter-opt"
                :class="{ 'is-on': filter.stage === s }"
                type="button"
                @click="setStage(s)"
              ><span class="hub-filter-opt__mark">{{ filter.stage === s ? '✓' : '' }}</span>{{ stageLabel(s) }}</button>
            </div>
          </NxPopover>
        </div>
      </div>

      <!-- 加载 skeleton（首载占位，不与真实卡混类名） -->
      <div v-if="loading && !projects.length" class="hub-grid" aria-hidden="true">
        <div v-for="i in 8" :key="i" class="nx-skeleton hub-skeleton">
          <div class="hub-skeleton-line hub-skeleton-line--w60" />
          <div class="hub-skeleton-line hub-skeleton-line--w40" />
          <div class="hub-skeleton-line hub-skeleton-line--w80" />
        </div>
      </div>

      <!-- 空态（搜索无命中 / 列表本空两口径） -->
      <div v-else-if="filtered.length === 0" class="nx-empty hub-empty">
        <svg viewBox="0 0 24 24" width="36" height="36" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <rect x="2" y="4" width="20" height="16" rx="2.5" />
          <path d="M7 4v16M17 4v16M2 9h5M2 15h5M17 9h5M17 15h5" />
        </svg>
        <div class="hub-empty-title">
          {{ filter.q.trim() ? t('filmhub.searchEmpty') : t('film.emptyTitle') }}
        </div>
        <div>{{ filter.q.trim() ? t('filmhub.searchEmptyHint') : t('film.emptyHint') }}</div>
      </div>

      <!-- 卡网格 -->
      <div v-else class="hub-grid">
        <NxCard
          v-for="p in filtered"
          :key="p.id"
          interactive
          :selected="selectedCard === p.id"
          class="hub-card fh-card"
          tabindex="0"
          role="button"
          :aria-label="p.title"
          @click="emit('open', p)"
          @keydown="onCardKey($event, p)"
          @focusin="onCardFocusIn(p)"
          @focusout="onCardFocusOut"
        >
          <div class="hub-card-head">
            <span class="hub-card-title" :title="p.title">{{ p.title }}</span>
            <span class="hub-pill-ratio">{{ p.ratio }}</span>
          </div>
          <p class="hub-idea">{{ p.idea }}</p>
          <!-- 五阶段进度点（启发式；进入项目后以 README stage 为准） -->
          <div class="hub-stages" :title="t('filmhub.cardStageTip')">
            <span
              v-for="s in FLOW_STAGES"
              :key="s"
              class="hub-stage-dot"
              :class="{
                'is-done': stageStateOf(p, s) === 'done',
                'is-current': stageStateOf(p, s) === 'current',
              }"
            ><span class="hub-stage-dot__mark" aria-hidden="true" />{{ stageLabel(s) }}</span>
          </div>
          <!-- 丰富模式：成员 chips -->
          <div v-if="(richInfo(p)?.members ?? []).length" class="hub-members">
            <span
              v-for="m in richInfo(p)!.members"
              :key="m"
              class="hub-member-chip"
            ><span class="hub-member-chip__ava" aria-hidden="true">{{ memberInitial(m) }}</span>{{ m }}</span>
          </div>
          <div class="hub-card-foot">
            <span>{{ t('film.updatedAt', { time: fmtTime(p.updated_at) }) }}</span>
            <span v-if="richInfo(p)?.calls != null">
              💰 {{ t('film.costCalls', { n: richInfo(p)!.calls ?? 0 }) }}
            </span>
          </div>
          <!-- 丰富模式：最近活动一句 -->
          <div
            v-if="richInfo(p)?.latest"
            class="hub-latest"
            :title="latestTitle(richInfo(p)!.latest!)"
          ><span class="hub-latest__arrow" aria-hidden="true">▸</span> {{ latestLine(richInfo(p)!.latest!) }}</div>
          <div class="hub-actions" @click.stop @keydown.stop>
            <NxButton variant="primary" size="sm" @click="emit('open', p)">
              {{ t('film.open') }}
            </NxButton>
            <NxButton size="sm" @click="emit('browse', p)">
              🗂 {{ t('filmhub.browse') }}
            </NxButton>
            <NxButton variant="destructive" size="sm" @click="emit('delete', p)">
              {{ t('film.del') }}
            </NxButton>
          </div>
        </NxCard>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 空态标题（排印刻度 text-md + semibold；组件局部排版） */
.hub-empty-title {
  font-size: var(--nx-font-size-md);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-secondary);
}
.hub-empty {
  gap: 10px;
}
</style>
