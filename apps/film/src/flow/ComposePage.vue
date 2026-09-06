<script setup lang="ts">
// =============================================================================
// ComposePage.vue —— 合成页（流程 ⑤，FilmHub v0.1.35）。
//
// BGM 下拉（音频页库联动；global 缺省）+「合成成片」（compose body 可含
// bgm_track → 任务轮询）。产物两区分离：
//   · dist 成品版本列表（files 树 dist/final-v*.mp4：版本号+时间戳+大小+
//     下载+「预览成片」——切工作台监视器 final 模式，previewEngine.setFinalName
//     指定版本文件）；
//   · cache 半成品区（cache/ 试生成产物：预览缩略+「确认采用」commit 转正
//     +丢弃——v1 后端无删除端点，丢弃仅本地隐藏）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  bgmEntryKey,
  bgmEntryMood,
  bgmEntryTrigger,
  filmCacheCommit,
  filmCompose,
  filmFileDataUrl,
  filmListBgm,
  filmListFiles,
  type FilmBgmEntry,
  type FilmFileEntry,
} from '../api'
import FlowPageHead from './FlowPageHead.vue'
import { usePreviewEngine } from '../previewEngine'
import { useFlow } from './flowContext'
import { ratioPresetOf } from './flowTypes'
import {
  cacheEntries,
  distVersionOf,
  distVersions,
  entryMtime,
  fileBasename,
  fmtBytes,
  isImagePath,
  isVideoPath,
} from './flowFiles'

const { t } = useI18n()
const ctx = useFlow()
const engine = usePreviewEngine()

// —— 输出分辨率小字（v0.1.37：项目 ratio → 六档预设合成分辨率）——
/** 输出分辨率文案（如 1920×1080；未知档空串不渲染）。 */
const outputRes = computed(() => {
  const p = ratioPresetOf(ctx?.project.value?.ratio ?? '')
  return p ? `${p.width}×${p.height}` : ''
})

// —— BGM 库（音频页同源）——
const bgmEntries = ref<FilmBgmEntry[]>([])
const bgmTrack = ref('')
const bgmError = ref('')

async function loadBgm(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    const raw = await filmListBgm(pid)
    bgmEntries.value = Array.isArray(raw) ? raw : []
    // 缺省：global 条目；否则第一项；空库=''
    if (bgmTrack.value && bgmEntries.value.some((e) => bgmEntryKey(e) === bgmTrack.value)) return
    const global = bgmEntries.value.find((e) => bgmEntryTrigger(e).toLowerCase() === 'global')
    bgmTrack.value = global
      ? bgmEntryKey(global)
      : bgmEntryKey(bgmEntries.value[0] ?? {}) || ''
    bgmError.value = ''
  } catch (e) {
    bgmEntries.value = []
    bgmError.value = ctx ? ctx.errMsg(e) : String(e)
  }
}

// —— files 树派生：dist 成品版本 + cache 半成品 ——
const tree = ref<FilmFileEntry[]>([])
const treeError = ref('')
const treeLoading = ref(false)

const versions = computed(() => distVersions(tree.value))
const caches = computed(() => cacheEntries(tree.value))
/** 本地已丢弃的 cache 路径（后端暂无删除端点——v1 仅列表隐藏）。 */
const dismissed = ref<Set<string>>(new Set())
const visibleCaches = computed(() => caches.value.filter((c) => !dismissed.value.has(c.path)))

async function loadTree(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  treeLoading.value = true
  try {
    const raw = await filmListFiles(pid)
    tree.value = Array.isArray(raw) ? raw : []
    dismissed.value = new Set()
    treeError.value = ''
  } catch (e) {
    tree.value = []
    treeError.value = t('film.cpLoadFailed') + (ctx ? ctx.errMsg(e) : String(e))
  } finally {
    treeLoading.value = false
  }
}

function fmtTime(iso?: string | null): string {
  if (!iso) return '—'
  const d = new Date(iso)
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString()
}

/** BGM 下拉选项尾注（非 global 显示 · mood）。 */
function bgmMoodSuffix(e: FilmBgmEntry): string {
  const m = bgmEntryMood(e)
  return m ? ` · ${m}` : ''
}

function versionLabel(e: FilmFileEntry): string {
  return `v${distVersionOf(e.path)}`
}

// —— 合成成片 ——
const composeBusy = ref(false)
const composeError = ref('')

async function composeFinal(): Promise<void> {
  if (!ctx || composeBusy.value) return
  composeBusy.value = true
  composeError.value = ''
  try {
    const task = await filmCompose(ctx.project.value!.id, bgmTrack.value || undefined, ctx.author.value)
    ctx.addTracked(task)
  } catch (e) {
    composeError.value = t('film.actFailed') + ctx.errMsg(e)
  } finally {
    composeBusy.value = false
  }
}

// —— 成品版本操作 ——

/** 下载版本（files b64 信封 → data URL → 浏览器另存）。 */
async function downloadVersion(e: FilmFileEntry): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    const url = await filmFileDataUrl(pid, e.path)
    const a = document.createElement('a')
    a.href = url
    a.download = fileBasename(e.path)
    a.click()
  } catch (err) {
    composeError.value = t('film.actFailed') + (ctx ? ctx.errMsg(err) : String(err))
  }
}

/** 预览成片：切工作台监视器 final 模式（引擎装载该版本文件）。 */
function previewVersion(e: FilmFileEntry): void {
  if (engine) engine.setFinalName(e.path)
  ctx?.setView('workbench')
  if (engine) engine.setMode('final')
}

// —— cache 半成品 ——

const committing = ref('')
const commitError = ref('')
/** 缩略（cache path → data URL；图片直接显，视频显图标）。 */
const cacheThumbs = ref<Record<string, string>>({})

function ensureThumb(e: FilmFileEntry): void {
  const pid = ctx?.project.value?.id
  if (!pid || cacheThumbs.value[e.path] || !isImagePath(e.path)) return
  void filmFileDataUrl(pid, e.path)
    .then((u) => {
      cacheThumbs.value[e.path] = u
    })
    .catch(() => undefined)
}
watch(visibleCaches, (list) => list.forEach(ensureThumb), { immediate: true })

/** 确认采用（半成品转正 → commit 端点；成功刷新树+项目）。 */
async function commitCache(e: FilmFileEntry): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx || committing.value) return
  committing.value = e.path
  commitError.value = ''
  try {
    await filmCacheCommit(pid, fileBasename(e.path), ctx.author.value)
    await loadTree()
    await ctx.reloadProject()
    await ctx.refreshCollab()
  } catch (err) {
    commitError.value = t('film.cpCommitFailed') + ctx.errMsg(err)
  } finally {
    committing.value = ''
  }
}

/** 丢弃（v1 后端无 cache 删除端点——仅本地隐藏，刷新后复原）。 */
function dismissCache(e: FilmFileEntry): void {
  dismissed.value = new Set([...dismissed.value, e.path])
}

onMounted(() => {
  void loadBgm()
  void loadTree()
})

// 任务终态 → 重载树/库
watch(
  () => ctx?.refreshTick.value,
  () => {
    void loadBgm()
    void loadTree()
  },
)
</script>

<template>
  <div class="fh-page">
    <FlowPageHead stage="compose" :title="t('film.flowCompose')">
      <template #actions>
        <!-- 输出分辨率小字（compose 统一尺寸 = 预设档合成分辨率） -->
        <span
          v-if="outputRes"
          class="fh-muted fh-small fh-mono cp-output-res"
          :title="t('film.cpOutputTip')"
        >{{ t('film.cpOutput', { res: outputRes }) }}</span>
        <!-- BGM 下拉（音频页库联动；global 缺省） -->
        <select v-model="bgmTrack" class="fh-select" :title="t('film.cpBgm')" style="min-width: 170px">
          <option value="">{{ t('film.cpBgmDefault') }}</option>
          <option v-for="e in bgmEntries" :key="bgmEntryKey(e)" :value="bgmEntryKey(e)">
            {{ bgmEntryTrigger(e) || bgmEntryKey(e) }}{{ bgmMoodSuffix(e) }}
          </option>
        </select>
        <button
          class="fh-btn fh-btn-primary fh-btn-small"
          type="button"
          :disabled="composeBusy || !ctx?.project.value || !ctx?.composeAvailable.value"
          :title="ctx?.isOffline.value ? t('film.capsOfflineTip') : t('film.finalEmpty')"
          @click="composeFinal"
        >{{ composeBusy ? t('film.taskRunning') + '…' : t('film.cpCompose') }}</button>
      </template>
    </FlowPageHead>

    <div class="fh-page-scroll">
      <div v-if="composeError" class="fh-error-box">{{ composeError }}</div>
      <div v-if="commitError" class="fh-error-box">{{ commitError }}</div>
      <div v-if="bgmError" class="fh-warn-box">{{ t('film.bgmLoadFailed') }}{{ bgmError }}</div>
      <div v-if="treeError" class="fh-error-box">{{ treeError }}</div>

      <!-- ① dist 成品版本列表 -->
      <section class="fh-card">
        <div class="fh-card-head">
          <span>🎬 {{ t('film.cpDistTitle') }}</span>
          <span class="fh-muted fh-small">{{ versions.length }}</span>
          <div class="fh-head-actions">
            <button class="fh-btn fh-btn-small" type="button" :disabled="treeLoading" @click="loadTree">↻</button>
          </div>
        </div>
        <div class="fh-card-body">
          <div v-if="treeLoading && !tree.length" class="fh-empty">{{ t('film.loading') }}</div>
          <div v-else-if="!versions.length" class="fh-empty">{{ t('film.cpDistEmpty') }}</div>
          <div v-for="e in versions" :key="e.path" class="fh-row">
            <span class="fh-pill fh-pill-blue">{{ versionLabel(e) }}</span>
            <span class="fh-mono fh-small fh-ellipsis" style="flex: 1; min-width: 0" :title="e.path">
              {{ fileBasename(e.path) }}
            </span>
            <span class="fh-muted fh-small">{{ fmtTime(entryMtime(e)) }}</span>
            <span class="fh-muted fh-small fh-mono">{{ fmtBytes(e.bytes) }}</span>
            <div style="display: flex; gap: 6px">
              <button class="fh-btn fh-btn-mini" type="button" @click="previewVersion(e)">
                ▶ {{ t('film.cpPreview') }}
              </button>
              <button class="fh-btn fh-btn-mini" type="button" @click="downloadVersion(e)">
                ⬇ {{ t('film.cpDownload') }}
              </button>
            </div>
          </div>
        </div>
      </section>

      <!-- ② cache 半成品区（试生成产物；确认采用转正 / 丢弃） -->
      <section class="fh-card">
        <div class="fh-card-head">
          <span>🧪 {{ t('film.cpCacheTitle') }}</span>
          <span class="fh-muted fh-small">{{ visibleCaches.length }}</span>
        </div>
        <div class="fh-card-body">
          <div v-if="!visibleCaches.length" class="fh-empty">{{ t('film.cpCacheEmpty') }}</div>
          <div class="fh-grid">
            <div v-for="e in visibleCaches" :key="e.path" class="cp-cache-card">
              <!-- 预览缩略（图片直显 / 视频图标） -->
              <div class="cp-cache-thumb">
                <img v-if="cacheThumbs[e.path]" :src="cacheThumbs[e.path]" :alt="fileBasename(e.path)">
                <span v-else>{{ isVideoPath(e.path) ? '🎞' : '📄' }}</span>
              </div>
              <div class="cp-cache-meta">
                <div class="fh-mono fh-small fh-ellipsis" :title="e.path">{{ fileBasename(e.path) }}</div>
                <div class="fh-muted fh-small">{{ fmtBytes(e.bytes) }} · {{ fmtTime(entryMtime(e)) }}</div>
              </div>
              <div style="display: flex; gap: 6px; flex-wrap: wrap">
                <button
                  class="fh-btn fh-btn-mini fh-btn-primary"
                  type="button"
                  :disabled="committing === e.path"
                  @click="commitCache(e)"
                >{{ committing === e.path ? '…' : `✓ ${t('film.cpCacheCommit')}` }}</button>
                <button
                  class="fh-btn fh-btn-mini fh-btn-danger"
                  type="button"
                  :title="t('film.cpCacheDiscardTip')"
                  @click="dismissCache(e)"
                >{{ t('film.cpCacheDiscard') }}</button>
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
.cp-output-res { align-self: center; white-space: nowrap; }
.cp-cache-card {
  border: 1px dashed var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 10px);
  padding: 8px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}
.cp-cache-thumb {
  width: 100%;
  aspect-ratio: 16 / 10;
  border-radius: var(--radius-sm, 8px);
  background: var(--border-soft, #FAFAFA);
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 28px;
  overflow: hidden;
}
.cp-cache-thumb img { width: 100%; height: 100%; object-fit: cover; }
.cp-cache-meta { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
</style>
