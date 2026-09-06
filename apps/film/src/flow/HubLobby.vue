<script setup lang="ts">
// =============================================================================
// HubLobby.vue —— FilmHub 大厅（v0.1.1 显性大厅；应用首页=项目列表页重造）。
//
// NexHub（/s/codehub）Explore 仓库大厅的对等形态：项目像「仓库」一样浏览。
//   · 顶栏：🎬 FilmHub 品牌（副标题「影片项目中心 · AI 像写代码一样创作」）
//     + 搜索框（标题/idea 过滤）+ 丰富模式开关 + 刷新 + 「+ 新建项目」；
//     head-extra slot 承接宿主侧杂项（能力徽章 / 独立模式外链按钮）。
//   · 项目卡网格（NexHub 仓库卡风格）：标题 + ratio pill + 五阶段进度点
//     （story→storyboard→casting→audio→compose，已过 ✓ / 当前橙实心——
//     列表无 README stage，deriveStageFromProject 按产物启发式推导，详情以
//     README 为准）+ idea 两行截断 + 更新时间。
//   · 丰富模式（缺省开，localStorage 记忆，可关）：并发（≤12 项目，按更新
//     时间取新）GET files/ownership.json + activity.json + cost 聚合 → 卡片
//     增补成员 chips / 最近活动一句 / 成本小字；任一失败静默降级素卡。
//   · 卡片操作：打开（进项目工作流，缺省页）/ Hub 浏览（进文件树浏览）/
//     删除（确认弹窗在 FilmStudio——本组件只 emit）。
// 数据与弹窗归 FilmStudio（projects props / 新建项目对话框 / 删除确认），
// 本组件只管大厅形态与丰富模式取数。
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
import { FLOW_STAGES, stageBadge, stageIndex, type FilmStage } from './flowTypes'

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

// —— 搜索（标题 / idea，大小写不敏感） ——
const q = ref('')

const filtered = computed<FilmProject[]>(() => {
  const kw = q.value.trim().toLowerCase()
  if (!kw) return props.projects
  return props.projects.filter(
    (p) =>
      (p.title ?? '').toLowerCase().includes(kw) ||
      (p.idea ?? '').toLowerCase().includes(kw),
  )
})

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

function stageTextOf(p: FilmProject, s: FilmStage): string {
  return stageStateOf(p, s) === 'done' ? '✓' : stageBadge(stageIndex(s))
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

function fmtTime(iso?: string | null): string {
  if (!iso) return '—'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}
</script>

<template>
  <div class="hub-lobby">
    <!-- 品牌顶栏 -->
    <div class="hub-head">
      <div class="hub-brand">
        <span class="hub-brand-icon" aria-hidden="true">🎬</span>
        <div>
          <div class="hub-brand-name">FilmHub</div>
          <div class="hub-brand-sub">{{ t('filmhub.tagline') }}</div>
        </div>
      </div>
      <div class="hub-head-actions">
        <input
          v-model="q"
          type="search"
          class="hub-search"
          :placeholder="t('filmhub.searchPh')"
          :aria-label="t('filmhub.searchPh')"
        >
        <button
          class="fh-btn fh-btn-small hub-rich-toggle"
          :class="{ 'is-on': rich }"
          type="button"
          :title="t('filmhub.richModeTip')"
          :aria-pressed="rich"
          @click="toggleRich"
        >🌿 {{ t('filmhub.richMode') }}</button>
        <button class="fh-btn fh-btn-small" type="button" :disabled="loading" @click="emit('refresh')">
          <span class="fh-spin" :class="{ 'is-spinning': loading }" aria-hidden="true">↻</span>
          {{ t('film.refresh') }}
        </button>
        <button class="fh-btn fh-btn-primary fh-btn-small" type="button" @click="emit('create')">
          + {{ t('film.newProject') }}
        </button>
        <slot name="head-extra" />
      </div>
    </div>

    <!-- 错误条 -->
    <div v-if="error" class="fh-error-box">
      {{ t('film.listFailed') }}{{ error }}
      <button class="fh-btn fh-btn-mini" type="button" @click="emit('refresh')">
        {{ t('film.retry') }}
      </button>
    </div>

    <!-- 卡网格 -->
    <div class="hub-scroll">
      <div v-if="!loading && filtered.length === 0" class="fh-card" style="padding: 48px 20px; align-items: center; gap: 8px">
        <div style="font-size: 44px">🎬</div>
        <div style="font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B)">
          {{ q ? t('filmhub.searchEmpty') : t('film.emptyTitle') }}
        </div>
        <div class="fh-muted fh-small">
          {{ q ? t('filmhub.searchEmptyHint') : t('film.emptyHint') }}
        </div>
      </div>
      <template v-else>
        <span class="hub-count">{{ t('filmhub.count', { n: filtered.length }) }}</span>
        <div class="hub-grid">
          <div
            v-for="p in filtered"
            :key="p.id"
            class="hub-card"
            @click="emit('open', p)"
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
              >{{ stageTextOf(p, s) }} {{ stageLabel(s) }}</span>
            </div>
            <!-- 丰富模式：成员 chips -->
            <div v-if="(richInfo(p)?.members ?? []).length" class="hub-members">
              <span v-for="m in richInfo(p)!.members" :key="m" class="hub-member-chip">👤 {{ m }}</span>
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
            >▸ {{ latestLine(richInfo(p)!.latest!) }}</div>
            <div class="hub-actions" @click.stop>
              <button class="fh-btn fh-btn-small fh-btn-primary" type="button" @click="emit('open', p)">
                {{ t('film.open') }}
              </button>
              <button class="fh-btn fh-btn-small" type="button" @click="emit('browse', p)">
                🗂 {{ t('filmhub.browse') }}
              </button>
              <button class="fh-btn fh-btn-small fh-btn-danger" type="button" @click="emit('delete', p)">
                {{ t('film.del') }}
              </button>
            </div>
          </div>
        </div>
      </template>
    </div>
  </div>
</template>
