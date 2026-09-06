<script setup lang="ts">
// =============================================================================
// HubBrowse.vue —— 项目 Hub 浏览页（v0.1.1；v0.1.36 树卡抽公共组件）。
//
// 「项目像仓库一样浏览」：左侧项目文件树（v0.1.36 起消费公共 HubTreeCard
// ——加载/折叠/图标与 SideNav 底部常开树卡同一实现）+ 右侧内容区：
//   · 文本（md/json/txt 等，mime 优先 + 扩展名兜底）等宽 pre；
//   · 图片 data URL 直显；音频 <audio controls>；视频 <video controls>（加菜）；
//   · 其余二进制：不支持预览提示（文件名/大小如实展示）。
// 内容区顶 = 当前路径面包屑 + 「在工作台打开」（hubTargetView 映射：
// story.md→剧情 / storyboard.json→分镜 / casting/*→定妆（pendingCastSelect
// 选中对象）/ audio/*→音频 / dist|cache|final*→合成 / 其余→工作台）。
// 页内四 Tab：📁 文件 / 📜 活动流（activity.json 时间线，FlowContext 共享）
// / 💰 成本（by stage/channel 两表，CostPanel 与成本徽章弹窗共用）
// / 🤖 AI 接入指南（curl 三段 + 应用说明，agent 像写代码一样改影片项目）。
// =============================================================================
import { computed, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  b64ToText,
  filmFileDataUrl,
  filmGetCost,
  filmGetFile,
  type FilmCostReport,
} from '../api'
import CostPanel from './CostPanel.vue'
import HubTreeCard from './HubTreeCard.vue'
import { fmtActivityTime } from './collab'
import { useFlow } from './flowContext'
import {
  fileBasename,
  fmtBytes,
  hubCastSelect,
  hubPreviewKind,
  hubTargetView,
  isAudioPath,
  isImagePath,
  isVideoPath,
} from './flowFiles'

const { t } = useI18n()
const ctx = useFlow()

const projectId = computed(() => ctx?.project.value?.id ?? '')

// —— Tab ——
const tab = ref<'files' | 'activity' | 'cost' | 'guide'>('files')

// —— 文件树（HubTreeCard 承载；页头刷新钮经 expose 联动） ——
const treeCard = ref<InstanceType<typeof HubTreeCard> | null>(null)
const treeLoading = computed(() => !!treeCard.value?.isLoading)
function loadTree(): void {
  void treeCard.value?.reload()
}

// —— 内容区 ——
const selected = ref('')
const content = reactive<{
  status: 'idle' | 'loading' | 'error'
  kind: 'text' | 'image' | 'audio' | 'video' | 'binary' | ''
  text: string
  dataUrl: string
  bytes?: number
  error: string
}>({ status: 'idle', kind: '', text: '', dataUrl: '', error: '' })

async function openFile(path: string): Promise<void> {
  if (!projectId.value) return
  selected.value = path
  content.status = 'loading'
  content.kind = ''
  content.text = ''
  content.dataUrl = ''
  content.error = ''
  try {
    if (isImagePath(path) || isAudioPath(path) || isVideoPath(path)) {
      content.dataUrl = await filmFileDataUrl(projectId.value, path)
      content.kind = isImagePath(path) ? 'image' : isAudioPath(path) ? 'audio' : 'video'
    } else {
      const env = await filmGetFile(projectId.value, path)
      const mime = env.mime || env.mime_type || ''
      const kind = hubPreviewKind(path, mime)
      content.bytes = typeof env.bytes === 'number' ? env.bytes : undefined
      if (kind === 'text' && env.content_b64 !== undefined) {
        content.kind = 'text'
        content.text = b64ToText(env.content_b64)
      } else if (kind === 'image') {
        content.dataUrl = await filmFileDataUrl(projectId.value, path)
        content.kind = 'image'
      } else {
        content.kind = 'binary'
      }
    }
    content.status = 'idle'
  } catch (e) {
    content.status = 'error'
    content.error = ctx ? ctx.errMsg(e) : String(e)
  }
}

/** 「在工作台打开」目标视图（hubTargetView 映射；casting 路径带对象选中）。 */
const flowTarget = computed(() => (selected.value ? hubTargetView(selected.value) : 'workbench'))

const flowTargetLabel = computed(() => {
  switch (flowTarget.value) {
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
    default:
      return t('film.flowWorkbench')
  }
})

function openInFlow(): void {
  if (!ctx || !selected.value) return
  const view = hubTargetView(selected.value)
  const cast = view === 'casting' ? hubCastSelect(selected.value) : null
  if (cast?.name) ctx.pendingCastSelect.value = cast
  ctx.setView(view)
}

// —— 活动流 Tab（FlowContext 共享 activity + refreshCollab） ——
const activity = computed(() => ctx?.activity.value ?? [])
const activityLoading = ref(false)
async function refreshActivity(): Promise<void> {
  if (!ctx || activityLoading.value) return
  activityLoading.value = true
  try {
    await ctx.refreshCollab()
  } finally {
    activityLoading.value = false
  }
}

// —— 成本 Tab（by stage/channel；CostPanel 展示） ——
const costLoaded = ref(false)
const costLoading = ref(false)
const costError = ref('')
const costSummary = ref<FilmCostReport | null>(null)
const byStage = ref<FilmCostReport | null>(null)
const byChannel = ref<FilmCostReport | null>(null)

const costTotal = computed(() => {
  const v = costSummary.value?.total
  return typeof v === 'number' && Number.isFinite(v) ? v : 0
})
const costCurrency = computed(() => costSummary.value?.currency || '¥')

async function loadCost(): Promise<void> {
  if (!projectId.value || costLoading.value) return
  costLoading.value = true
  try {
    const [sum, s, c] = await Promise.all([
      filmGetCost(projectId.value),
      filmGetCost(projectId.value, 'stage'),
      filmGetCost(projectId.value, 'channel'),
    ])
    costSummary.value = sum
    byStage.value = s
    byChannel.value = c
    costError.value = ''
    costLoaded.value = true
  } catch (e) {
    costError.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    costLoading.value = false
  }
}

watch(tab, (v) => {
  if (v === 'cost' && !costLoaded.value) void loadCost()
})

// —— AI 接入指南（curl 三段；$BASE/$TOKEN 占位 + 真实项目 id） ——
const pid = computed(() => projectId.value || '<id>')
const curlTree = computed(
  () =>
    `# ${t('filmhub.guideTree')}\ncurl -H "Authorization: Bearer $TOKEN" \\\n  "$BASE/api/v1/film/projects/${pid.value}/files"`,
)
const curlPut = computed(
  () =>
    `# ${t('filmhub.guidePut')}\ncurl -X PUT -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \\\n  -d '{"content_b64":"<base64>","author":"agent"}' \\\n  "$BASE/api/v1/film/projects/${pid.value}/files/storyboard.json"`,
)
const curlGen = computed(
  () =>
    `# ${t('filmhub.guideGen')}\ncurl -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \\\n  -d '{"model_ref":{"source":"local","capability":"chat"},"author":"agent"}' \\\n  "$BASE/api/v1/film/projects/${pid.value}/storyboard/generate"`,
)

const copiedKey = ref('')
async function copyCurl(key: string, text: string): Promise<void> {
  try {
    await navigator.clipboard?.writeText(text)
    copiedKey.value = key
    setTimeout(() => {
      if (copiedKey.value === key) copiedKey.value = ''
    }, 1500)
  } catch {
    /* 剪贴板不可用（非安全上下文等）——静默 */
  }
}

// 树卡挂载即自加载；任务终态/项目重载（refreshTick）时按需重拉
watch(
  () => ctx?.refreshTick.value,
  () => {
    if (tab.value === 'files') loadTree()
  },
)

/** 二进制未选中文件名/大小展示（树卡化后从选中路径与信封派生）。 */
const selName = computed(() => (selected.value ? fileBasename(selected.value) : ''))
const bytesOfSelected = computed(() => content.bytes)
const crumbs = computed<string[]>(() =>
  selected.value ? selected.value.split('/').filter(Boolean) : [],
)
</script>

<template>
  <div class="fh-page">
    <!-- 面包屑页头：FilmHub 大厅 → 项目 → Hub 浏览 -->
    <div class="fh-head">
      <span class="fh-muted">🎬 {{ t('filmhub.lobby') }}</span>
      <span class="fh-muted">/</span>
      <span class="fh-head-title fh-ellipsis" style="max-width: 260px" :title="ctx?.project.value?.title">
        {{ ctx?.project.value?.title || '—' }}
      </span>
      <span class="fh-muted">/</span>
      <span class="fh-pill fh-pill-muted">🗂 {{ t('filmhub.hubView') }}</span>
      <div class="fh-head-actions">
        <button
          class="fh-btn fh-btn-small"
          type="button"
          :disabled="treeLoading"
          :title="t('filmhub.refreshTree')"
          @click="loadTree"
        >
          <span class="fh-spin" :class="{ 'is-spinning': treeLoading }" aria-hidden="true">↻</span>
        </button>
      </div>
    </div>

    <!-- 页内 Tab：文件 / 活动 / 成本 / 接入指南 -->
    <div class="fh-tabs">
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'files' }"
        type="button"
        @click="tab = 'files'"
      >📁 {{ t('filmhub.tabFiles') }}</button>
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'activity' }"
        type="button"
        @click="tab = 'activity'"
      >📜 {{ t('filmhub.tabActivity') }}</button>
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'cost' }"
        type="button"
        @click="tab = 'cost'"
      >💰 {{ t('filmhub.tabCost') }}</button>
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'guide' }"
        type="button"
        @click="tab = 'guide'"
      >🤖 {{ t('filmhub.tabGuide') }}</button>
    </div>

    <!-- ==================== 文件 Tab：左树（公共树卡）+ 右内容 ==================== -->
    <div v-show="tab === 'files'" class="hub-main">
      <aside class="hub-tree">
        <HubTreeCard
          ref="treeCard"
          :project-id="projectId"
          :selected="selected"
          @file-click="openFile"
        />
      </aside>

      <section class="hub-view">
        <div class="hub-view-head">
          <div class="hub-crumbs">
            <span class="fh-muted">📁 /</span>
            <template v-for="(c, i) in crumbs" :key="i">
              <span class="fh-muted">/</span>
              <span class="hub-crumb-seg">{{ c }}</span>
            </template>
            <span v-if="!crumbs.length" class="fh-muted">{{ t('filmhub.noFileSelected') }}</span>
          </div>
          <button
            v-if="selected"
            class="fh-btn fh-btn-small fh-btn-primary hub-open-flow"
            type="button"
            :title="t('filmhub.openInFlowTip')"
            @click="openInFlow"
          >🛠 {{ t('filmhub.openInFlow') }} · {{ flowTargetLabel }}</button>
        </div>
        <div class="hub-view-body">
          <div v-if="content.status === 'loading'" class="fh-empty">{{ t('film.loading') }}</div>
          <div v-else-if="content.status === 'error'" class="fh-error-box">
            {{ t('filmhub.fileLoadFailed') }}{{ content.error }}
            <button class="fh-btn fh-btn-mini" type="button" @click="openFile(selected)">
              {{ t('film.retry') }}
            </button>
          </div>
          <div v-else-if="!selected" class="fh-empty">{{ t('filmhub.noFileSelected') }}</div>
          <pre v-else-if="content.kind === 'text'" class="fh-pre">{{ content.text }}</pre>
          <img
            v-else-if="content.kind === 'image'"
            class="hub-media-img"
            :src="content.dataUrl"
            :alt="selected"
          >
          <audio v-else-if="content.kind === 'audio'" class="hub-media-audio" :src="content.dataUrl" controls />
          <video v-else-if="content.kind === 'video'" class="hub-media-video" :src="content.dataUrl" controls />
          <div v-else-if="content.kind === 'binary'" class="fh-empty">
            {{ t('filmhub.binaryUnsupported') }}
            <div class="fh-muted fh-small" style="margin-top: 6px">
              {{ selName }} · {{ fmtBytes(bytesOfSelected) }}
            </div>
          </div>
        </div>
      </section>
    </div>

    <!-- ==================== 活动 Tab：activity.json 时间线 ==================== -->
    <div v-if="tab === 'activity'" class="fh-card" style="flex: 1; min-height: 0">
      <div class="fh-card-head">
        <span>📜 {{ t('filmhub.tabActivity') }}</span>
        <span class="fh-muted fh-small">{{ activity.length }}</span>
        <div class="fh-head-actions">
          <button
            class="fh-btn fh-btn-small"
            type="button"
            :disabled="activityLoading"
            @click="refreshActivity"
          >
            <span class="fh-spin" :class="{ 'is-spinning': activityLoading }" aria-hidden="true">↻</span>
          </button>
        </div>
      </div>
      <div class="fh-card-body">
        <div v-if="!activity.length" class="fh-empty">{{ t('film.actEmpty') }}</div>
        <div v-for="(a, i) in activity" :key="i" class="fh-row hub-act-row" style="flex-wrap: nowrap">
          <span class="fh-muted fh-small fh-mono" style="flex-shrink: 0">{{ fmtActivityTime(a.ts) }}</span>
          <span class="fh-pill fh-pill-mini fh-pill-blue" style="flex-shrink: 0">
            {{ a.author || 'anonymous' }}
          </span>
          <span class="fh-small" style="flex-shrink: 0">{{ a.action || '—' }}</span>
          <span class="fh-mono fh-small fh-ellipsis" style="flex: 1; min-width: 0" :title="a.target ?? ''">
            {{ a.target || '' }}
          </span>
        </div>
        <div class="fh-muted fh-small">{{ t('film.actHint') }}</div>
      </div>
    </div>

    <!-- ==================== 成本 Tab：by stage/channel 两表 ==================== -->
    <div v-if="tab === 'cost'" class="fh-card" style="flex: 1; min-height: 0">
      <div class="fh-card-head">
        <span>💰 {{ t('filmhub.tabCost') }}</span>
        <div class="fh-head-actions">
          <button class="fh-btn fh-btn-small" type="button" :disabled="costLoading" @click="loadCost">
            <span class="fh-spin" :class="{ 'is-spinning': costLoading }" aria-hidden="true">↻</span>
          </button>
        </div>
      </div>
      <div class="fh-card-body">
        <CostPanel
          :loading="costLoading"
          :error="costError"
          :by-stage="byStage"
          :by-channel="byChannel"
          :total="costTotal"
          :currency="costCurrency"
        />
      </div>
    </div>

    <!-- ==================== 接入指南 Tab：agent / curl ==================== -->
    <div v-if="tab === 'guide'" class="hub-guide">
      <div class="hub-guide-block">
        <span class="hub-guide-title">🤖 {{ t('filmhub.guideTitle') }}</span>
        <span class="fh-muted fh-small">{{ t('filmhub.guideIntro') }}</span>
      </div>
      <div v-for="blk in [
            { key: 'tree', title: t('filmhub.guideTree'), curl: curlTree },
            { key: 'put', title: t('filmhub.guidePut'), curl: curlPut },
            { key: 'gen', title: t('filmhub.guideGen'), curl: curlGen },
          ]" :key="blk.key" class="hub-guide-block">
        <span class="hub-guide-title">
          {{ blk.title }}
          <button
            class="fh-btn fh-btn-mini"
            style="margin-left: auto"
            type="button"
            @click="copyCurl(blk.key, blk.curl)"
          >{{ copiedKey === blk.key ? t('filmhub.copied') : t('filmhub.copy') }}</button>
        </span>
        <pre class="hub-curl">{{ blk.curl }}</pre>
      </div>
      <div class="hub-guide-block">
        <span class="fh-muted fh-small">{{ t('filmhub.guideApply') }}</span>
      </div>
    </div>
  </div>
</template>
