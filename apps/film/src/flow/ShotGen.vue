<script setup lang="ts">
// =============================================================================
// ShotGen.vue —— 短片生成首屏（v0.1.45 应用侧，film 0.1.13）。
//
// 对标海螺 H3 创作页的直连生成器形态（不建项目不走分镜/生图/合成管线）：
//   · 顶栏：🎬 短片生成 + 创作｜作品双 Tab + 当前模型标识（video 位渠道名，
//     未配置=琥珀「未配置」+ 黄条「生视频需配置 video 渠道模型」+ 去配置）
//     + 深浅主题切换 + 「高级模式 →」（进现有 FilmHub 大厅/项目全流程——
//     全流程降为高级模式，本页为应用默认首屏）；
//   · 创作页：文生视频｜图生视频双模式（图生=上传参考图 ≤10MB png/jpeg/webp）
//     + 提示词多行（0/3000 计数，占位含示例）+ 灵感 chips（点击回填预置示例，
//     含时间轴分镜式结构化长提示词）+ 画幅五档 16:9/9:16/1:1/4:3/21:9 +
//     时长 5/8/10/15s + 画质固定档显示（渠道原生）+ 种子（0/空=随机）+
//     大按钮「✦ 生成视频 · <模型名>」（in-flight 防重复：提交中或已有作品
//     生成中→禁用 + 文案「任务已入队，请勿重复提交」）；
//   · 作品页：瀑布流卡片（提示词可展开 + 画幅/时长/画质标签 + 视频播放器
//     [点击惰性装载] + 下载 + 状态标签 生成中/已完成/失败）+ 全部/生成中筛选；
//     每卡操作：复制提示词（承接上段直接复制生成）/ 基于此再生成（回填创作页）。
//
// 引擎（createShotgenEngine）经 SHOTGEN_ENGINE_KEY 注入（FilmStudio 创建，
// 跨模式切换存活）；toast 静默：反馈全在页内（状态标签/红条），不落全局 toast。
// 视觉 = nx token 体系（scoped 样式全用 --nx-* 变量，随 <html data-theme> 换值）。
// =============================================================================
import { computed, onMounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import NxButton from '../nx/NxButton.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
import type { FilmShotgenWork } from '../api'
import { ratioValueOf } from './flowTypes'
import {
  SHOTGEN_DURATIONS,
  SHOTGEN_INSPIRATIONS,
  SHOTGEN_PROMPT_MAX,
  SHOTGEN_RATIOS,
  filterShotgenWorks,
  normalizeShotgenStatus,
  shotgenPromptLong,
  useShotgenEngine,
} from './shotgen'

const props = defineProps<{
  /** video 位是否可用（有启用渠道且非离线——黄条与按钮置灰口径）。 */
  videoReady: boolean
  /** 当前模型标识（video 位渠道名；''=未配置）。 */
  modelName: string
  /** 离线态（能力探测失败——全停用）。 */
  isOffline: boolean
}>()

const emit = defineEmits<{
  /** 「高级模式 →」：进现有 FilmHub 大厅（全流程入口）。 */
  (e: 'advanced'): void
  /** 「去配置」：跳模型设置（项目级 video 能力位）。 */
  (e: 'go-models'): void
}>()

const { t } = useI18n()
const engine = useShotgenEngine()

onMounted(() => engine?.start())

// —— 顶栏 Tab ——
const tab = computed(() => engine?.form.tab ?? 'create')
function setTab(v: 'create' | 'works'): void {
  if (engine) engine.form.tab = v
}

/** 进行中作品数（引擎 computed 的本地投影——模板自动解包）。 */
const generatingCount = computed(() => engine?.generatingCount.value ?? 0)

// —— 创作页面（本地 UI 态；提交态在引擎） ——
const fileInput = ref<HTMLInputElement | null>(null)

function setMode(m: 'text' | 'image'): void {
  if (engine) engine.form.mode = m
}

function applyInspiration(key: string): void {
  if (!engine) return
  engine.form.prompt = t(`shotgen.insp${key}Prompt`)
}

async function onPickImage(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0] ?? null
  input.value = ''
  await engine?.setImage(file)
}

function pickImage(): void {
  fileInput.value?.click()
}

function clearImage(): void {
  void engine?.setImage(null)
}

/** 画幅小图标比例条（44×32 盒内按数值比例；与新建项目预设卡同口径的迷你版）。 */
function ratioBarStyle(r: { width: number; height: number }): Record<string, string> {
  const v = r.width / r.height
  const w = v >= 1 ? 40 : Math.max(5, Math.round(40 * v))
  const h = v >= 1 ? Math.max(5, Math.round(40 / v)) : 28
  return { width: `${w}px`, height: `${h}px` }
}

/** 生成按钮禁用 tooltip（优先级：离线 > 渠道未配 > 表单校验 > 防重复）。 */
const genDisabledTip = computed(() => {
  if (props.isOffline) return t('film.capsOfflineTip')
  if (!props.videoReady) return t('shotgen.videoMissing')
  if (engine?.generatingCount.value) return t('shotgen.inflight')
  if (engine?.formError.value === 'prompt') return t('shotgen.errPromptEmpty')
  if (engine?.formError.value === 'seed') return t('shotgen.errSeed')
  if (engine?.formError.value === 'image') return t('shotgen.errImage')
  return undefined
})

const canGenerate = computed(
  () => props.videoReady && !props.isOffline && !!engine?.canSubmit.value,
)

async function generate(): Promise<void> {
  if (!canGenerate.value) return
  await engine?.submit()
}

// —— 作品页 ——
const worksFilter = computed(() => engine?.worksFilter.value ?? 'all')
function setWorksFilter(v: 'all' | 'generating'): void {
  if (engine) engine.worksFilter.value = v
}

const filteredWorks = computed<FilmShotgenWork[]>(() =>
  filterShotgenWorks(engine?.works.value ?? [], worksFilter.value),
)

/** 卡片装载中集合（点击播放 → ensureVideoUrl 在途防双击）。 */
const loadingVideoIds = ref(new Set<string>())
/** 已复制提示词反馈（卡 id → 短时 ✓）。 */
const copiedId = ref('')
let copiedTimer: ReturnType<typeof setTimeout> | null = null

async function playWork(w: FilmShotgenWork): Promise<void> {
  if (!engine || normalizeShotgenStatus(w.status) !== 'completed') return
  if (engine.videoUrls[w.id] || loadingVideoIds.value.has(w.id)) return
  loadingVideoIds.value.add(w.id)
  try {
    await engine.ensureVideoUrl(w)
  } catch {
    /* 装载失败：占位区保持「点击播放」，重试即可 */
  } finally {
    loadingVideoIds.value.delete(w.id)
  }
}

function workVideoUrl(w: FilmShotgenWork): string {
  return engine?.videoUrls[w.id] ?? ''
}

async function copyPrompt(w: FilmShotgenWork): Promise<void> {
  if (!engine) return
  const ok = await engine.copyPrompt(w)
  if (ok) {
    copiedId.value = w.id
    if (copiedTimer) clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => {
      copiedId.value = ''
    }, 1500)
  }
}

function refill(w: FilmShotgenWork): void {
  engine?.refill(w)
}

async function downloadWork(w: FilmShotgenWork): Promise<void> {
  await engine?.download(w)
}

/** 卡片画面比例（ratio → CSS aspect-ratio；非法回退 16/9）。 */
function workAspect(w: FilmShotgenWork): string {
  const v = ratioValueOf(w.ratio ?? '')
  return `${v ?? 16 / 9}`
}

/** 状态标签刻面。 */
function statusKind(s: 'queued' | 'running' | 'completed' | 'failed'): string {
  return s === 'completed' ? 'ok' : s === 'failed' ? 'err' : 'run'
}

function statusLabel(s: 'queued' | 'running' | 'completed' | 'failed'): string {
  return s === 'completed'
    ? t('shotgen.stDone')
    : s === 'failed'
      ? t('shotgen.stFailed')
      : t('shotgen.stGenerating')
}

/** 失败原因首行（完整 error 在 title）。 */
function firstLine(s: string | null | undefined): string {
  return (s ?? '').split('\n').find((l) => l.trim()) ?? ''
}

function isLong(w: FilmShotgenWork): boolean {
  return shotgenPromptLong(w.prompt ?? '')
}

function expanded(w: FilmShotgenWork): boolean {
  return engine?.expanded.has(w.id) ?? false
}

function toggleExpand(w: FilmShotgenWork): void {
  engine?.expanded.toggle(w.id)
}

function fmtTime(w: FilmShotgenWork): string {
  const v = w.created_at
  const d = typeof v === 'number' ? new Date(v) : typeof v === 'string' ? new Date(v) : null
  if (!d || Number.isNaN(d.getTime())) return ''
  return d.toLocaleString()
}
</script>

<template>
  <div class="sg-root">
    <!-- ==================== 顶栏：品牌 + 双 Tab + 模型标识 + 高级模式 ==================== -->
    <header class="sg-top">
      <div class="sg-brand">
        <span class="sg-brand-mark" aria-hidden="true">🎬</span>
        <span class="sg-brand-title">{{ t('shotgen.title') }}</span>
      </div>
      <nav class="sg-tabs" role="tablist" :aria-label="t('shotgen.title')">
        <button
          type="button"
          class="sg-tab"
          :class="{ active: tab === 'create' }"
          role="tab"
          :aria-selected="tab === 'create'"
          @click="setTab('create')"
        >{{ t('shotgen.tabCreate') }}</button>
        <button
          type="button"
          class="sg-tab"
          :class="{ active: tab === 'works' }"
          role="tab"
          :aria-selected="tab === 'works'"
          @click="setTab('works')"
        >
          {{ t('shotgen.tabWorks') }}
          <span v-if="generatingCount" class="sg-tab-badge">{{ generatingCount }}</span>
        </button>
      </nav>
      <div class="sg-top-actions">
        <!-- 当前模型标识（video 位渠道名；未配置=琥珀警示刻面） -->
        <span
          class="sg-model-chip"
          :class="{ warn: !videoReady }"
          :title="videoReady ? t('shotgen.modelChipTip', { name: modelName }) : t('shotgen.videoMissing')"
        >
          <span class="sg-model-dot" aria-hidden="true" />
          {{ videoReady ? modelName : t('shotgen.modelUnset') }}
        </span>
        <NxThemeToggle />
        <NxButton size="sm" :title="t('shotgen.advancedTip')" @click="emit('advanced')">
          {{ t('shotgen.advanced') }} →
        </NxButton>
      </div>
    </header>

    <!-- video 位未配黄条（降级不失败：如实提示 + 去配置跳模型设置） -->
    <div v-if="!videoReady" class="sg-warn" role="alert">
      <span>⚠ {{ t('shotgen.videoMissing') }}</span>
      <NxButton size="sm" @click="emit('go-models')">{{ t('shotgen.goModels') }}</NxButton>
    </div>

    <template v-if="engine">
      <!-- ==================== 创作页 ==================== -->
      <section v-if="tab === 'create'" class="sg-create">
        <!-- 文生 / 图生双模式 -->
        <div class="sg-modes" role="tablist" :aria-label="t('shotgen.modeLabel')">
          <button
            type="button"
            class="sg-mode"
            :class="{ active: engine.form.mode === 'text' }"
            role="tab"
            :aria-selected="engine.form.mode === 'text'"
            @click="setMode('text')"
          >✍ {{ t('shotgen.modeText') }}</button>
          <button
            type="button"
            class="sg-mode"
            :class="{ active: engine.form.mode === 'image' }"
            role="tab"
            :aria-selected="engine.form.mode === 'image'"
            @click="setMode('image')"
          >🖼 {{ t('shotgen.modeImage') }}</button>
        </div>

        <div class="sg-composer">
          <!-- 参考图（图生视频档） -->
          <div v-if="engine.form.mode === 'image'" class="sg-ref">
            <div class="sg-ref-thumb">
              <img v-if="engine.imagePreview.value" :src="engine.imagePreview.value" :alt="t('shotgen.imagePick')" />
              <span v-else class="sg-ref-ph" aria-hidden="true">🖼</span>
            </div>
            <div class="sg-ref-ops">
              <NxButton size="sm" @click="pickImage">{{ t('shotgen.imagePick') }}</NxButton>
              <NxButton v-if="engine.imageB64.value" size="sm" @click="clearImage">
                {{ t('shotgen.imageClear') }}
              </NxButton>
              <span class="sg-hint">{{ t('shotgen.imageHint') }}</span>
            </div>
            <input
              ref="fileInput"
              type="file"
              accept="image/png,image/jpeg,image/webp"
              class="sg-hidden"
              @change="onPickImage"
            />
          </div>
          <div v-if="engine.imageError.value" class="sg-field-err">
            {{ engine.imageError.value === 'size'
              ? t('shotgen.imageTooLarge') : engine.imageError.value === 'type'
                ? t('shotgen.imageBadType') : engine.imageError.value }}
          </div>

          <!-- 提示词（多行 + 0/3000 计数） -->
          <textarea
            v-model="engine.form.prompt"
            class="sg-prompt"
            rows="6"
            :maxlength="SHOTGEN_PROMPT_MAX"
            :placeholder="t('shotgen.promptPh')"
            :aria-label="t('shotgen.promptLabel')"
          />
          <div class="sg-prompt-meta">
            <span class="sg-hint">{{ t('shotgen.promptHint') }}</span>
            <span class="sg-count mono">{{ engine.form.prompt.length }}/{{ SHOTGEN_PROMPT_MAX }}</span>
          </div>

          <!-- 灵感 chips（点击回填预置示例提示词；结构化长提示词含时间轴分镜式） -->
          <div class="sg-insp">
            <span class="sg-hint">{{ t('shotgen.inspLabel') }}</span>
            <button
              v-for="insp in SHOTGEN_INSPIRATIONS"
              :key="insp.key"
              type="button"
              class="sg-chip"
              :title="t(`shotgen.insp${insp.key}Prompt`)"
              @click="applyInspiration(insp.key)"
            >
              ✦ {{ t(`shotgen.insp${insp.key}`) }}<span v-if="insp.structured" class="sg-chip-tag">⌗</span>
            </button>
          </div>
        </div>

        <!-- 参数组：画幅五档 / 时长四档 / 画质固定档 / 种子 -->
        <div class="sg-params">
          <div class="sg-param">
            <span class="sg-param-label">{{ t('shotgen.ratioLabel') }}</span>
            <div class="sg-ratios" role="radiogroup" :aria-label="t('shotgen.ratioLabel')">
              <button
                v-for="r in SHOTGEN_RATIOS"
                :key="r.ratio"
                type="button"
                class="sg-ratio"
                :class="{ active: engine.form.ratio === r.ratio }"
                role="radio"
                :aria-checked="engine.form.ratio === r.ratio"
                :title="`${r.ratio} · ${r.width}×${r.height}`"
                @click="engine.form.ratio = r.ratio"
              >
                <span class="sg-ratio-box"><span class="sg-ratio-bar" :style="ratioBarStyle(r)" /></span>
                <span class="sg-ratio-label mono">{{ r.ratio }}</span>
              </button>
            </div>
          </div>
          <div class="sg-param">
            <span class="sg-param-label">{{ t('shotgen.durLabel') }}</span>
            <div class="sg-durs" role="radiogroup" :aria-label="t('shotgen.durLabel')">
              <button
                v-for="d in SHOTGEN_DURATIONS"
                :key="d"
                type="button"
                class="sg-dur"
                :class="{ active: engine.form.duration === d }"
                role="radio"
                :aria-checked="engine.form.duration === d"
                @click="engine.form.duration = d"
              >{{ d }}s</button>
            </div>
          </div>
          <div class="sg-param sg-param-row">
            <span class="sg-param-label">{{ t('shotgen.qualityLabel') }}</span>
            <span class="sg-static-pill" :title="t('shotgen.qualityNativeTip')">
              {{ t('shotgen.qualityNative') }}
            </span>
            <span class="sg-param-label sg-seed">{{ t('shotgen.seedLabel') }}</span>
            <input
              v-model="engine.form.seedRaw"
              type="text"
              inputmode="numeric"
              class="sg-seed-input mono"
              :placeholder="t('shotgen.seedPh')"
              :aria-label="t('shotgen.seedLabel')"
            />
          </div>
        </div>

        <!-- 提交行：大按钮 + 防重复/校验文案 -->
        <div class="sg-submit">
          <NxButton
            variant="primary"
            class="sg-gen"
            :loading="!!engine.submitting.value"
            :disabled="!canGenerate"
            :title="genDisabledTip"
            @click="generate"
          >
            <span v-if="!engine.submitting.value" aria-hidden="true">✦</span>
            {{ engine.submitting.value ? t('shotgen.submitting') : t('shotgen.generate')
            }}<template v-if="videoReady && !engine.submitting.value"> · {{ modelName }}</template>
          </NxButton>
          <div class="sg-submit-note">
            <span v-if="generatingCount" class="sg-inflight">
              <span class="spin spinning" aria-hidden="true">↻</span>
              {{ t('shotgen.inflight') }}
            </span>
            <span v-else-if="engine.formError.value === 'prompt'" class="sg-field-err">
              {{ t('shotgen.errPromptEmpty') }}
            </span>
            <span v-else-if="engine.formError.value === 'seed'" class="sg-field-err">
              {{ t('shotgen.errSeed') }}
            </span>
            <span v-else-if="engine.formError.value === 'image'" class="sg-field-err">
              {{ t('shotgen.errImage') }}
            </span>
          </div>
          <div v-if="engine.submitError.value" class="sg-error" role="alert">
            {{ t('shotgen.submitFailed') }}{{ engine.submitError.value }}
            <NxButton size="sm" @click="generate">{{ t('film.retry') }}</NxButton>
          </div>
        </div>
      </section>

      <!-- ==================== 作品页 ==================== -->
      <section v-else class="sg-works">
        <div class="sg-works-head">
          <div class="sg-works-filter" role="tablist" :aria-label="t('shotgen.worksTitle')">
            <button
              type="button"
              class="sg-ftab"
              :class="{ active: worksFilter === 'all' }"
              @click="setWorksFilter('all')"
            >{{ t('shotgen.worksAll') }}</button>
            <button
              type="button"
              class="sg-ftab"
              :class="{ active: worksFilter === 'generating' }"
              @click="setWorksFilter('generating')"
            >
              {{ t('shotgen.worksGenerating') }}<span v-if="generatingCount" class="sg-tab-badge">{{ generatingCount }}</span>
            </button>
          </div>
          <NxButton size="sm" :loading="!!engine.worksLoading.value" @click="engine.refreshWorks()">
            {{ t('film.refresh') }}
          </NxButton>
        </div>

        <div v-if="engine.worksError.value" class="sg-error" role="alert">
          {{ t('shotgen.worksLoadFailed') }}{{ engine.worksError.value }}
          <NxButton size="sm" @click="engine.refreshWorks()">{{ t('film.retry') }}</NxButton>
        </div>

        <div v-else-if="!filteredWorks.length" class="sg-empty">
          <span class="sg-empty-art" aria-hidden="true">🎬</span>
          <p class="sg-empty-title">{{ t('shotgen.worksEmpty') }}</p>
          <p class="sg-empty-hint">{{ t('shotgen.worksEmptyHint') }}</p>
        </div>

        <div v-else class="sg-grid">
          <article
            v-for="w in filteredWorks"
            :key="w.id"
            class="sg-card"
            :class="{ 'is-failed': normalizeShotgenStatus(w.status) === 'failed' }"
          >
            <!-- 画面区：完成=点击装载播放器；生成中=spinner；失败=警示占位 -->
            <div
              class="sg-media"
              :style="{ aspectRatio: workAspect(w) }"
              @click="playWork(w)"
            >
              <video
                v-if="workVideoUrl(w)"
                :src="workVideoUrl(w)"
                controls
                playsinline
                preload="metadata"
                @click.stop
              />
              <template v-else-if="normalizeShotgenStatus(w.status) === 'completed'">
                <span v-if="loadingVideoIds.has(w.id)" class="spin spinning sg-media-spin" aria-hidden="true">↻</span>
                <template v-else>
                  <span class="sg-media-play" aria-hidden="true">▶</span>
                  <span class="sg-media-tip">{{ t('shotgen.playToLoad') }}</span>
                </template>
              </template>
              <span v-else-if="normalizeShotgenStatus(w.status) === 'failed'" class="sg-media-fail" aria-hidden="true">⚠</span>
              <span v-else class="spin spinning sg-media-spin" aria-hidden="true">↻</span>
              <span v-if="w.duration_secs" class="sg-media-dur mono">{{ w.duration_secs }}s</span>
              <span class="sg-media-status" :class="`is-${statusKind(normalizeShotgenStatus(w.status))}`">
                <span v-if="normalizeShotgenStatus(w.status) === 'queued' || normalizeShotgenStatus(w.status) === 'running'" class="spin spinning" aria-hidden="true">↻</span>
                {{ statusLabel(normalizeShotgenStatus(w.status)) }}
              </span>
            </div>

            <div class="sg-card-body">
              <p
                class="sg-prompt-text"
                :class="{ clamp: isLong(w) && !expanded(w) }"
                :title="isLong(w) && !expanded(w) ? w.prompt ?? '' : undefined"
              >{{ w.prompt }}</p>
              <button v-if="isLong(w)" type="button" class="sg-expand" @click="toggleExpand(w)">
                {{ expanded(w) ? t('shotgen.collapse') : t('shotgen.expand') }}
              </button>
              <div class="sg-tags">
                <span v-if="w.ratio" class="sg-tag mono">{{ w.ratio }}</span>
                <span v-if="w.duration_secs" class="sg-tag mono">{{ w.duration_secs }}s</span>
                <span class="sg-tag">{{ t('shotgen.qualityNative') }}</span>
                <span v-if="typeof w.seed === 'number' && w.seed > 0" class="sg-tag mono" :title="t('shotgen.seedLabel')">seed {{ w.seed }}</span>
                <span v-if="w.model" class="sg-tag" :title="w.model">{{ w.model }}</span>
              </div>
              <div v-if="normalizeShotgenStatus(w.status) === 'failed'" class="sg-card-err" :title="w.error ?? ''">
                {{ firstLine(w.error) || t('shotgen.stFailed') }}
              </div>
              <div class="sg-card-ops">
                <button type="button" class="sg-op" :title="t('shotgen.copyTip')" @click="copyPrompt(w)">
                  {{ copiedId === w.id ? '✓ ' + t('shotgen.copied') : '⧉ ' + t('shotgen.copy') }}
                </button>
                <button type="button" class="sg-op" :title="t('shotgen.regenerateTip')" @click="refill(w)">
                  ↻ {{ t('shotgen.regenerate') }}
                </button>
                <button
                  v-if="normalizeShotgenStatus(w.status) === 'completed'"
                  type="button"
                  class="sg-op"
                  @click="downloadWork(w)"
                >⬇ {{ t('shotgen.download') }}</button>
              </div>
              <span v-if="fmtTime(w)" class="sg-card-time">{{ fmtTime(w) }}</span>
            </div>
          </article>
        </div>
      </section>
    </template>

    <!-- 无引擎上下文（异常宿主形态）——空态不崩 -->
    <div v-else class="sg-empty">
      <span class="sg-empty-art" aria-hidden="true">🎬</span>
      <p class="sg-empty-title">{{ t('shotgen.title') }}</p>
    </div>
  </div>
</template>

<style scoped>
/* ===================== 短片生成（nx token 体系；零 vh：flex + 滚动） ===================== */
.sg-root {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.sg-hint { color: var(--nx-text-tertiary); font-size: var(--nx-font-size-xs); }
.mono { font-family: var(--nx-font-mono); }

/* —— 顶栏 —— */
.sg-top {
  display: flex;
  align-items: center;
  gap: 16px;
  flex-wrap: wrap;
  flex-shrink: 0;
}
.sg-brand { display: flex; align-items: center; gap: 8px; min-width: 0; }
.sg-brand-mark { font-size: 20px; line-height: 1; }
.sg-brand-title {
  font-size: var(--nx-font-size-md);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  letter-spacing: -0.01em;
  white-space: nowrap;
}
.sg-tabs {
  display: inline-flex;
  gap: 2px;
  padding: 2px;
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  border: 0.5px solid var(--nx-line-regular);
}
.sg-tab {
  border: 0;
  background: transparent;
  color: var(--nx-text-secondary);
  font-size: var(--nx-font-size-sm);
  padding: 5px 16px;
  border-radius: calc(var(--nx-radius-input) - 2px);
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 6px;
}
.sg-tab:hover { color: var(--nx-text-primary); }
.sg-tab.active {
  background: var(--nx-bg-card);
  color: var(--nx-text-primary);
  box-shadow: var(--nx-shadow-xs);
  font-weight: var(--nx-font-weight-semibold);
}
.sg-tab-badge {
  min-width: 16px;
  height: 16px;
  padding: 0 4px;
  border-radius: 999px;
  background: var(--nx-accent);
  color: var(--nx-text-inverse);
  font-size: 10px;
  line-height: 16px;
  text-align: center;
  font-family: var(--nx-font-mono);
}
.sg-top-actions { display: flex; align-items: center; gap: 8px; margin-left: auto; flex-wrap: wrap; }
.sg-model-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  border-radius: var(--nx-radius-badge);
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  background: var(--nx-bg-hover);
  border: 1px solid var(--nx-line-regular);
  white-space: nowrap;
  max-width: 220px;
  overflow: hidden;
  text-overflow: ellipsis;
}
.sg-model-dot {
  width: 6px;
  height: 6px;
  border-radius: 50%;
  background: var(--nx-success);
  flex-shrink: 0;
}
.sg-model-chip.warn { color: var(--nx-warning); background: var(--nx-warning-bg); border-color: var(--nx-warning-border); }
.sg-model-chip.warn .sg-model-dot { background: var(--nx-warning); }

/* —— 黄条 / 错误条 —— */
.sg-warn {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  padding: 8px 12px;
  border-radius: var(--nx-radius-card);
  background: var(--nx-warning-bg);
  border: 1px solid var(--nx-warning-border);
  color: var(--nx-warning);
  font-size: var(--nx-font-size-sm);
  flex-shrink: 0;
}
.sg-error {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  padding: 8px 12px;
  border-radius: var(--nx-radius-card);
  background: var(--nx-danger-bg);
  border: 1px solid var(--nx-danger-border);
  color: var(--nx-danger);
  font-size: var(--nx-font-size-sm);
}
.sg-field-err { color: var(--nx-danger); font-size: var(--nx-font-size-xs); }

/* —— 创作页 —— */
.sg-create {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.sg-modes {
  display: inline-flex;
  gap: 2px;
  padding: 2px;
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  border: 0.5px solid var(--nx-line-regular);
  align-self: flex-start;
}
.sg-mode {
  border: 0;
  background: transparent;
  color: var(--nx-text-secondary);
  font-size: var(--nx-font-size-sm);
  padding: 5px 14px;
  border-radius: calc(var(--nx-radius-input) - 2px);
  cursor: pointer;
}
.sg-mode.active {
  background: var(--nx-bg-card);
  color: var(--nx-accent);
  box-shadow: var(--nx-shadow-xs);
  font-weight: var(--nx-font-weight-semibold);
}
.sg-composer {
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-xs);
  padding: 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.sg-ref { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.sg-ref-thumb {
  width: 72px;
  height: 72px;
  border-radius: var(--nx-radius-input);
  border: 1px dashed var(--nx-line-strong);
  background: var(--nx-bg-subtle);
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
  flex-shrink: 0;
}
.sg-ref-thumb img { width: 100%; height: 100%; object-fit: cover; }
.sg-ref-ph { font-size: 22px; opacity: 0.6; }
.sg-ref-ops { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.sg-hidden { display: none; }
.sg-prompt {
  width: 100%;
  border: 1px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-body);
  color: var(--nx-text-primary);
  font-size: var(--nx-font-size-base);
  font-family: inherit;
  line-height: 1.6;
  padding: 10px 12px;
  resize: vertical;
  min-height: 120px;
  box-sizing: border-box;
}
.sg-prompt:focus-visible { outline: 2px solid var(--nx-ring-color, var(--nx-accent)); outline-offset: -1px; }
.sg-prompt::placeholder { color: var(--nx-text-tertiary); }
.sg-prompt-meta {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}
.sg-count { color: var(--nx-text-tertiary); font-size: var(--nx-font-size-xs); flex-shrink: 0; }
.sg-insp { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.sg-chip {
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-hover);
  color: var(--nx-text-secondary);
  border-radius: 999px;
  padding: 3px 12px;
  font-size: var(--nx-font-size-xs);
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
.sg-chip:hover { color: var(--nx-accent); border-color: var(--nx-accent-border); background: var(--nx-bg-card); }
.sg-chip-tag { color: var(--nx-text-tertiary); font-size: 10px; }

/* —— 参数组 —— */
.sg-params {
  display: flex;
  flex-direction: column;
  gap: 10px;
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-xs);
  padding: 12px 14px;
}
.sg-param { display: flex; flex-direction: column; gap: 6px; }
.sg-param-row { flex-direction: row; align-items: center; gap: 10px; flex-wrap: wrap; }
.sg-param-label {
  color: var(--nx-text-tertiary);
  font-size: var(--nx-font-size-xs);
  flex-shrink: 0;
}
.sg-seed { margin-left: 12px; }
.sg-ratios, .sg-durs { display: flex; gap: 6px; flex-wrap: wrap; }
.sg-ratio {
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-body);
  border-radius: var(--nx-radius-input);
  padding: 6px 10px;
  cursor: pointer;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 4px;
  min-width: 56px;
}
.sg-ratio:hover { border-color: var(--nx-accent-border); }
.sg-ratio.active {
  border-color: var(--nx-accent);
  background: var(--nx-accent-soft);
  box-shadow: var(--nx-shadow-xs);
}
.sg-ratio-box { width: 44px; height: 30px; display: flex; align-items: center; justify-content: center; }
.sg-ratio-bar {
  background: var(--nx-accent);
  border-radius: 2px;
  opacity: 0.75;
}
.sg-ratio.active .sg-ratio-bar { opacity: 1; }
.sg-ratio-label { font-size: 10px; color: var(--nx-text-secondary); }
.sg-ratio.active .sg-ratio-label { color: var(--nx-accent); font-weight: var(--nx-font-weight-semibold); }
.sg-dur {
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-body);
  color: var(--nx-text-secondary);
  border-radius: 999px;
  padding: 4px 14px;
  font-size: var(--nx-font-size-xs);
  font-family: var(--nx-font-mono);
  cursor: pointer;
}
.sg-dur:hover { border-color: var(--nx-accent-border); }
.sg-dur.active {
  border-color: var(--nx-accent);
  background: var(--nx-accent-soft);
  color: var(--nx-accent);
  font-weight: var(--nx-font-weight-semibold);
}
.sg-static-pill {
  display: inline-flex;
  padding: 2px 10px;
  border-radius: var(--nx-radius-badge);
  font-size: var(--nx-font-size-xs);
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-hover);
  color: var(--nx-text-tertiary);
  cursor: help;
}
.sg-seed-input {
  width: 110px;
  border: 1px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-body);
  color: var(--nx-text-primary);
  font-size: var(--nx-font-size-sm);
  padding: 4px 10px;
}
.sg-seed-input:focus-visible { outline: 2px solid var(--nx-ring-color, var(--nx-accent)); outline-offset: -1px; }

/* —— 提交行 —— */
.sg-submit { display: flex; flex-direction: column; gap: 8px; align-items: flex-start; }
.sg-gen { padding: 10px 28px; font-size: var(--nx-font-size-md); border-radius: 999px; }
.sg-submit-note { min-height: 18px; }
.sg-inflight {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  color: var(--nx-text-secondary);
  font-size: var(--nx-font-size-xs);
}
.spin { display: inline-block; }
.spinning { animation: sg-rot 1s linear infinite; }
@keyframes sg-rot { to { transform: rotate(360deg); } }

/* —— 作品页 —— */
.sg-works { flex: 1; min-height: 0; overflow-y: auto; display: flex; flex-direction: column; gap: 12px; }
.sg-works-head { display: flex; align-items: center; gap: 10px; flex-shrink: 0; }
.sg-works-filter {
  display: inline-flex;
  gap: 2px;
  padding: 2px;
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  border: 0.5px solid var(--nx-line-regular);
}
.sg-ftab {
  border: 0;
  background: transparent;
  color: var(--nx-text-secondary);
  font-size: var(--nx-font-size-xs);
  padding: 4px 12px;
  border-radius: calc(var(--nx-radius-input) - 2px);
  cursor: pointer;
  display: inline-flex;
  align-items: center;
  gap: 5px;
}
.sg-ftab.active { background: var(--nx-bg-card); color: var(--nx-text-primary); box-shadow: var(--nx-shadow-xs); }
.sg-works-head :deep(.nx-btn) { margin-left: auto; }
.sg-empty {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 6px;
  padding: 40px 0;
  color: var(--nx-text-tertiary);
}
.sg-empty-art { font-size: 40px; opacity: 0.5; }
.sg-empty-title { color: var(--nx-text-secondary); font-size: var(--nx-font-size-sm); margin: 0; }
.sg-empty-hint { font-size: var(--nx-font-size-xs); margin: 0; }

/* —— 作品卡（瀑布流自适应列） —— */
.sg-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
  gap: 12px;
  align-content: start;
}
.sg-card {
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-xs);
  overflow: hidden;
  display: flex;
  flex-direction: column;
}
.sg-card:hover { box-shadow: var(--nx-shadow-md); }
.sg-card.is-failed { border-color: var(--nx-danger-border); }
.sg-media {
  position: relative;
  background: var(--nx-bg-burn);
  display: flex;
  align-items: center;
  justify-content: center;
  cursor: pointer;
  overflow: hidden;
  min-height: 120px;
}
.sg-media video { width: 100%; height: 100%; object-fit: contain; display: block; background: #000; }
.sg-media-play { font-size: 30px; color: var(--nx-text-inverse); filter: drop-shadow(0 1px 4px rgba(0,0,0,0.5)); }
.sg-media-tip { position: absolute; bottom: 34px; color: var(--nx-text-inverse); font-size: var(--nx-font-size-xs); opacity: 0.85; }
.sg-media-fail { font-size: 26px; color: var(--nx-danger); }
.sg-media-spin { font-size: 22px; color: var(--nx-text-tertiary); }
.sg-media-dur {
  position: absolute;
  right: 6px;
  bottom: 6px;
  background: rgba(0, 0, 0, 0.6);
  color: #fff;
  border-radius: 4px;
  padding: 1px 6px;
  font-size: 10px;
}
.sg-media-status {
  position: absolute;
  left: 6px;
  top: 6px;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  border-radius: var(--nx-radius-badge);
  padding: 2px 8px;
  font-size: 10px;
  backdrop-filter: blur(4px);
}
.sg-media-status.is-run { background: rgba(0, 0, 0, 0.55); color: #fff; }
.sg-media-status.is-ok { background: var(--nx-success-bg); color: var(--nx-success); border: 1px solid var(--nx-success-border); }
.sg-media-status.is-err { background: var(--nx-danger-bg); color: var(--nx-danger); border: 1px solid var(--nx-danger-border); }
.sg-card-body { padding: 10px 12px; display: flex; flex-direction: column; gap: 8px; }
.sg-prompt-text {
  margin: 0;
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-word;
}
.sg-prompt-text.clamp {
  display: -webkit-box;
  -webkit-line-clamp: 3;
  -webkit-box-orient: vertical;
  overflow: hidden;
}
.sg-expand {
  border: 0;
  background: none;
  padding: 0;
  color: var(--nx-accent);
  font-size: var(--nx-font-size-xs);
  cursor: pointer;
  align-self: flex-start;
}
.sg-tags { display: flex; gap: 5px; flex-wrap: wrap; }
.sg-tag {
  padding: 1px 8px;
  border-radius: var(--nx-radius-badge);
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-hover);
  color: var(--nx-text-tertiary);
  font-size: 10px;
  max-width: 160px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.sg-card-err {
  color: var(--nx-danger);
  font-size: var(--nx-font-size-xs);
  background: var(--nx-danger-bg);
  border-radius: var(--nx-radius-input);
  padding: 4px 8px;
}
.sg-card-ops { display: flex; gap: 6px; flex-wrap: wrap; }
.sg-op {
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-body);
  color: var(--nx-text-secondary);
  border-radius: var(--nx-radius-input);
  padding: 3px 10px;
  font-size: var(--nx-font-size-xs);
  cursor: pointer;
}
.sg-op:hover { color: var(--nx-accent); border-color: var(--nx-accent-border); }
.sg-card-time { color: var(--nx-text-tertiary); font-size: 10px; }
</style>
