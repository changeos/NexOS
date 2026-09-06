<script setup lang="ts">
// =============================================================================
// StoryPage.vue —— 剧情页（流程 ①，FilmHub v0.1.37 文档处理管线增强）。
//
// 左=原文区：导入（点击 + 拖拽；大文件提示）+ sources 列表（文件名 / 字节 /
// 状态徽章[原始|已清理|已分章]）+ 选中源内容 pre（超长截断提示）；
// 管线操作条（选中源后显示）：🧹清理 → 📖分章 → 👥人物梳理 三步
// （清理可选 rules 本地零成本 / LLM 深清；分章建议先清理，可跳过直接对原文）。
// 右=结构区三 Tab：章节（index 清单 + 点开 ch 文件正文，每章字数）/
// 人物档案（characters-profile 卡片：别名/外貌/性格/关系/成长线 + 「转定妆
// 对象」→ POST casting/characters，作者自动认领——衔接定妆页）/ 正稿
// （story.md 现有 AI 生成与展示功能保留）。
// 任务中心联动三阶段任务（202 → addTracked；终态 refreshTick 重载本页）。
// =============================================================================
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  filmContentText,
  filmCreateCasting,
  filmGetFile,
  filmListFiles,
  filmStoryChapterize,
  filmStoryClean,
  filmStoryGenerate,
  filmStoryImport,
  filmStoryProfile,
  readFileAsDataUrl,
  splitDataUrl,
  type FilmCharacterProfile,
  type FilmChapterEntry,
  type FilmChapterIndex,
  type FilmFileContent,
  type FilmFileEntry,
  type FilmStoryCleanMode,
  type FilmTask,
} from '../api'
import FlowPageHead from './FlowPageHead.vue'
import { useFlow } from './flowContext'
import {
  clampForDisplay,
  fileBasename,
  fmtBytes,
  parseFrontmatter,
  storySourceStatus,
  storySources,
  storyWordCount,
} from './flowFiles'

const { t } = useI18n()
const ctx = useFlow()

/** 客户端导入护栏（与服务端缺省一致：env NEXOS_FILM_SOURCE_MAX_MB=64MB）。 */
const IMPORT_LIMIT_MB = 64
/** 大文件慢路径提示阈值（b64 上传体感分界）。 */
const IMPORT_HINT_MB = 8

// —— 原文素材（左栏）——
const tree = ref<FilmFileEntry[]>([])
const sources = ref<FilmFileEntry[]>([])
const sourcesError = ref('')
const importing = ref(false)
const importHint = ref('')
const importInput = ref<HTMLInputElement | null>(null)
const dragOver = ref(false)
/** 选中查看的原文（path + 文本；超长截断展示）。 */
const selectedSource = ref('')
const sourceText = ref('')
const sourceTruncated = ref(false)
const sourceLoading = ref(false)

/** 章节清单（右栏·章 Tab 数据源 + 左栏「已分章」徽章派生输入）。 */
const chaptersIndex = ref<FilmChapterIndex | null>(null)
const chaptersError = ref('')
const selectedChapter = ref<FilmChapterEntry | null>(null)
const chapterText = ref('')
const chapterLoading = ref(false)

/** 人物档案（右栏·人物档案 Tab）。 */
const profiles = ref<FilmCharacterProfile[]>([])
const profilesError = ref('')
/** 转定妆状态（name → busy/done/error）。 */
const castingBusy = reactive(new Set<string>())
const castingDone = reactive(new Set<string>())
const castingError = reactive(new Map<string, string>())

// —— 管线操作条 ——
const cleanMode = ref<FilmStoryCleanMode>('rules')
const cleanBusy = ref(false)
const chapterizeBusy = ref(false)
const profileBusy = ref(false)
const pipelineError = ref('')

// —— 右栏 Tab ——
const rightTab = ref<'chapters' | 'profile' | 'draft'>('draft')

// —— 剧情正稿 story.md（正稿 Tab，v0.1.35 功能保留）——
const storyMd = ref('')
const storyLoading = ref(false)
/** story.md 缺失（404 等）→ 空态提示而非错误。 */
const storyMissing = ref(false)
const storyError = ref('')
const genPrompt = ref('')
const genSourceFile = ref('')
const genBusy = ref(false)
const genError = ref('')

/** front-matter 信息条（来源/字数）。 */
const storyFm = computed(() => parseFrontmatter(storyMd.value))
const storyWords = computed(() => storyWordCount(storyMd.value))

/** 源文件管线状态徽章（树 + chaptersIndex.source 派生）。 */
function sourceStatus(path: string): { cleaned: boolean; chapterized: boolean } {
  return storySourceStatus(tree.value, path, chaptersIndex.value?.source ?? null)
}

async function loadSources(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    const files = await filmListFiles(pid)
    tree.value = files
    sources.value = storySources(files)
    sourcesError.value = ''
  } catch (e) {
    tree.value = []
    sources.value = []
    sourcesError.value = ctx ? ctx.errMsg(e) : String(e)
  }
}

/** 读 hub 树内文本文件（content / b64 双契约归一）。 */
async function fetchText(path: string): Promise<string> {
  const pid = ctx?.project.value?.id
  if (!pid) return ''
  const env: FilmFileContent = await filmGetFile(pid, path)
  return filmContentText(env)
}

async function loadChapters(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx) return
  selectedChapter.value = null
  chapterText.value = ''
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/chapters/index.json')
    const idx = JSON.parse(filmContentText(env)) as FilmChapterIndex
    chaptersIndex.value = Array.isArray(idx.chapters) ? idx : { chapters: [] }
    chaptersError.value = ''
  } catch (e) {
    // 无 index = 未分章（空态），其余如实展示
    chaptersIndex.value = null
    const m = ctx.errMsg(e)
    chaptersError.value = /404|not found/i.test(m) ? '' : t('film.storyChapterLoadFailed') + m
  }
}

async function loadProfiles(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx) return
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/characters-profile.json')
    const arr = JSON.parse(filmContentText(env)) as FilmCharacterProfile[]
    profiles.value = Array.isArray(arr) ? arr : []
    profilesError.value = ''
  } catch (e) {
    profiles.value = []
    const m = ctx.errMsg(e)
    profilesError.value = /404|not found/i.test(m) ? '' : m
  }
}

async function viewSource(path: string): Promise<void> {
  selectedSource.value = path
  sourceText.value = ''
  sourceTruncated.value = false
  sourceLoading.value = true
  try {
    const text = await fetchText(path)
    const clamped = clampForDisplay(text, 50_000)
    sourceText.value = clamped.text
    sourceTruncated.value = clamped.truncated
  } catch (e) {
    sourceText.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    sourceLoading.value = false
  }
}

async function viewChapter(ch: FilmChapterEntry): Promise<void> {
  selectedChapter.value = ch
  chapterText.value = ''
  chapterLoading.value = true
  try {
    chapterText.value = await fetchText(ch.file || `story/chapters/ch-${String(ch.no).padStart(2, '0')}.md`)
  } catch (e) {
    chapterText.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    chapterLoading.value = false
  }
}

async function importFile(file: File | undefined | null): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!file || !pid || !ctx) return
  if (file.size > IMPORT_LIMIT_MB * 1024 * 1024) {
    sourcesError.value = t('film.storyImportMax', { mb: IMPORT_LIMIT_MB })
    return
  }
  importing.value = true
  importHint.value =
    file.size > IMPORT_HINT_MB * 1024 * 1024
      ? t('film.storyImportBig', { mb: (file.size / 1024 / 1024).toFixed(0) })
      : ''
  sourcesError.value = ''
  try {
    const { b64 } = splitDataUrl(await readFileAsDataUrl(file))
    await filmStoryImport(pid, {
      filename: file.name,
      content_b64: b64,
      author: ctx.author.value,
    })
    await loadSources()
    await ctx.refreshCollab()
  } catch (err) {
    sourcesError.value = t('film.storyImportFailed') + ctx.errMsg(err)
  } finally {
    importing.value = false
    importHint.value = ''
  }
}

async function onImportFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  await importFile(file)
}

async function onDrop(e: DragEvent): Promise<void> {
  dragOver.value = false
  const file = e.dataTransfer?.files?.[0]
  if (file) await importFile(file)
}

// —— 管线三步（202 任务进任务中心；终态经 refreshTick 重载）——

function needModel(): boolean {
  if (!ctx) return false
  if (ctx.modelRefFor('chat')) return true
  pipelineError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.storyNoModel')
  return false
}

async function trackTask(task: FilmTask | Promise<FilmTask>): Promise<boolean> {
  try {
    ctx?.addTracked(await task)
    return true
  } catch (e) {
    pipelineError.value = ctx ? ctx.errMsg(e) : String(e)
    return false
  }
}

async function runClean(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!ctx || !pid || cleanBusy.value) return
  if (!selectedSource.value) {
    pipelineError.value = t('film.storyPipelineNeedsSource')
    return
  }
  const ref = cleanMode.value === 'llm' ? ctx.modelRefFor('chat') : null
  if (cleanMode.value === 'llm' && !ref) {
    pipelineError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.storyNoModel')
    return
  }
  cleanBusy.value = true
  pipelineError.value = ''
  try {
    await trackTask(
      filmStoryClean(pid, {
        source_file: selectedSource.value,
        mode: cleanMode.value,
        ...(ref ? { model_ref: ref } : {}),
        author: ctx.author.value,
      }),
    )
  } finally {
    cleanBusy.value = false
  }
}

async function runChapterize(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!ctx || !pid || chapterizeBusy.value) return
  if (!selectedSource.value) {
    pipelineError.value = t('film.storyPipelineNeedsSource')
    return
  }
  if (!needModel()) return
  chapterizeBusy.value = true
  pipelineError.value = ''
  try {
    await trackTask(
      filmStoryChapterize(pid, {
        model_ref: ctx.modelRefFor('chat')!,
        source_file: selectedSource.value,
        author: ctx.author.value,
      }),
    )
  } finally {
    chapterizeBusy.value = false
  }
}

async function runProfile(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!ctx || !pid || profileBusy.value) return
  if (!selectedSource.value) {
    pipelineError.value = t('film.storyPipelineNeedsSource')
    return
  }
  if (!needModel()) return
  profileBusy.value = true
  pipelineError.value = ''
  try {
    await trackTask(
      filmStoryProfile(pid, {
        model_ref: ctx.modelRefFor('chat')!,
        source_file: selectedSource.value,
        author: ctx.author.value,
      }),
    )
  } finally {
    profileBusy.value = false
  }
}

/** 人物档案 → 定妆对象（desc = 外貌+性格拼接；作者自动认领）。 */
async function castToCasting(p: FilmCharacterProfile): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx || castingBusy.has(p.name) || castingDone.has(p.name)) return
  castingBusy.add(p.name)
  castingError.delete(p.name)
  try {
    const desc = [p.appearance, p.personality].filter((s) => (s ?? '').trim()).join('；')
    await filmCreateCasting(pid, 'characters', {
      name: p.name,
      desc: desc || t('film.storyToCasting'),
      author: ctx.author.value,
    })
    castingDone.add(p.name)
    await ctx.refreshCollab()
  } catch (e) {
    castingError.set(p.name, ctx.errMsg(e))
  } finally {
    castingBusy.delete(p.name)
  }
}

// —— 剧情正稿（正稿 Tab；AI 生成走 chat model_ref）——

async function loadStory(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  storyLoading.value = true
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/story.md')
    storyMd.value = filmContentText(env)
    storyMissing.value = false
    storyError.value = ''
  } catch (e) {
    storyMd.value = ''
    const m = ctx ? ctx.errMsg(e) : String(e)
    // 无 story.md = 尚未生成（空态），其余如实展示
    if (/404|not found/i.test(m)) storyMissing.value = true
    else storyError.value = t('film.storyLoadFailed') + m
  } finally {
    storyLoading.value = false
  }
}

/** AI 写剧情（model_ref 复用 chat 面 + prompt + 可选参考原文）。 */
async function generateStory(): Promise<void> {
  if (!ctx || genBusy.value) return
  const ref = ctx.modelRefFor('chat')
  if (!ref) {
    genError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  genBusy.value = true
  genError.value = ''
  try {
    const task = await filmStoryGenerate(ctx.project.value!.id, {
      model_ref: ref,
      ...(genPrompt.value.trim() ? { prompt: genPrompt.value.trim() } : {}),
      ...(genSourceFile.value ? { source_file: genSourceFile.value } : {}),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (e) {
    genError.value = t('film.storyGenFailed') + ctx.errMsg(e)
  } finally {
    genBusy.value = false
  }
}

function loadAll(): void {
  void loadSources()
  void loadChapters()
  void loadProfiles()
  void loadStory()
}

onMounted(loadAll)

// 任务终态 / 项目刷新 → 重载本页数据
watch(
  () => ctx?.refreshTick.value,
  () => loadAll(),
)
</script>

<template>
  <div class="fh-page">
    <FlowPageHead stage="story" :title="t('film.storyTitle')">
      <template #actions>
        <select v-model="genSourceFile" class="fh-select" :title="t('film.storySourceFile')">
          <option value="">{{ t('film.storySourceNone') }}</option>
          <option v-for="s in sources" :key="s.path" :value="s.path">
            {{ fileBasename(s.path) }}
          </option>
        </select>
        <select
          v-if="ctx"
          v-model="ctx.modelSel.chat"
          class="fh-select"
          :title="t('film.model')"
        >
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>
            {{ t('film.noRunningLlm') }}
          </option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">
              {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
            </option>
          </optgroup>
        </select>
        <button
          class="fh-btn fh-btn-primary fh-btn-small"
          type="button"
          :disabled="genBusy || !ctx?.project.value || !ctx?.hasOptionsFor('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="generateStory"
        >{{ genBusy ? t('film.taskRunning') + '…' : t('film.storyGenerate') }}</button>
      </template>
    </FlowPageHead>

    <div class="fh-two-col fh-page-scroll">
      <!-- 左：原文素材 + 管线操作条 -->
      <div class="fh-col">
        <section
          class="fh-card"
          :class="{ 'is-dragover': dragOver }"
          style="flex: 1 1 50%; min-height: 0"
          @dragover.prevent="dragOver = true"
          @dragleave="dragOver = false"
          @drop.prevent="onDrop"
        >
          <div class="fh-card-head">
            <span>{{ t('film.storySources') }}</span>
            <span class="fh-muted fh-small">{{ sources.length }}</span>
            <div class="fh-head-actions">
              <button
                class="fh-btn fh-btn-small"
                type="button"
                :disabled="importing || !ctx?.project.value"
                @click="importInput?.click()"
              >{{ importing ? '…' : t('film.storyImport') }}</button>
              <input
                ref="importInput"
                type="file"
                accept=".txt,.md,text/plain,text/markdown"
                class="fh-hidden-input"
                @change="onImportFile"
              />
            </div>
          </div>
          <div class="fh-card-body">
            <div v-if="importHint" class="fh-muted fh-small">{{ importHint }}</div>
            <div v-if="sourcesError" class="fh-error-box">{{ sourcesError }}</div>
            <div v-if="!sources.length && !sourcesError && !importHint" class="fh-empty">
              {{ t('film.storySourceEmpty') }}
            </div>
            <div
              v-for="s in sources"
              :key="s.path"
              class="fh-row"
              :class="{ 'is-active': selectedSource === s.path }"
              style="cursor: pointer"
              @click="viewSource(s.path)"
            >
              <span class="fh-pill fh-pill-blue fh-pill-mini">📄</span>
              <span class="fh-ellipsis" style="flex: 1; min-width: 0" :title="s.path">
                {{ fileBasename(s.path) }}
              </span>
              <span
                v-if="sourceStatus(s.path).cleaned"
                class="fh-pill fh-pill-ok fh-pill-mini"
              >{{ t('film.storyStatusCleaned') }}</span>
              <span
                v-if="sourceStatus(s.path).chapterized"
                class="fh-pill fh-pill-violet fh-pill-mini"
              >{{ t('film.storyStatusChapterized') }}</span>
              <span class="fh-muted fh-small fh-mono">{{ fmtBytes(s.bytes) }}</span>
            </div>
            <!-- 选中原文内容（pre 等宽；超长截断） -->
            <div v-if="selectedSource" class="fh-pre-box" style="flex: 1; min-height: 120px">
              <div class="fh-muted fh-small fh-ellipsis" style="margin-bottom: 6px">
                {{ selectedSource }}
                <span v-if="sourceTruncated" class="fh-pill fh-pill-amber fh-pill-mini" style="margin-left: 6px">
                  {{ t('film.storyTruncated', { n: 50000 }) }}
                </span>
              </div>
              <pre v-if="sourceLoading" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
              <pre v-else class="fh-pre">{{ sourceText || '—' }}</pre>
            </div>
          </div>
        </section>

        <!-- 管线操作条（选中源后显示） -->
        <section v-if="selectedSource" class="fh-card">
          <div class="fh-card-head">
            <span>{{ t('film.storyPipeline') }}</span>
            <span class="fh-muted fh-small fh-ellipsis" :title="t('film.storyPipelineHint')">
              {{ t('film.storyPipelineHint') }}
            </span>
          </div>
          <div class="fh-card-body" style="flex-direction: row; flex-wrap: wrap; gap: 8px; align-items: center">
            <select v-model="cleanMode" class="fh-select" style="width: auto" :title="t('film.storyCleanMode')">
              <option value="rules">{{ t('film.storyCleanModeRules') }}</option>
              <option value="llm">{{ t('film.storyCleanModeLlm') }}</option>
            </select>
            <button
              class="fh-btn fh-btn-small"
              type="button"
              :disabled="cleanBusy || !ctx?.project.value"
              @click="runClean"
            >{{ cleanBusy ? t('film.taskRunning') + '…' : t('film.storyClean') }}</button>
            <button
              class="fh-btn fh-btn-small"
              type="button"
              :disabled="chapterizeBusy || !ctx?.project.value"
              @click="runChapterize"
            >{{ chapterizeBusy ? t('film.taskRunning') + '…' : t('film.storyChapterize') }}</button>
            <button
              class="fh-btn fh-btn-small"
              type="button"
              :disabled="profileBusy || !ctx?.project.value"
              @click="runProfile"
            >{{ profileBusy ? t('film.taskRunning') + '…' : t('film.storyProfile') }}</button>
            <span v-if="pipelineError" class="fh-error-box" style="flex-basis: 100%">{{ pipelineError }}</span>
          </div>
        </section>
      </div>

      <!-- 右：结构区三 Tab（章节 / 人物档案 / 正稿） -->
      <section class="fh-card fh-col">
        <div class="fh-tabs" style="padding: 0 12px">
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'chapters' }"
            type="button"
            @click="rightTab = 'chapters'"
          >📖 {{ t('film.storyTabChapters') }}
            <span v-if="chaptersIndex?.chapters?.length" class="fh-pill fh-pill-muted fh-pill-mini">
              {{ t('film.storyChaptersCount', { n: chaptersIndex.chapters.length }) }}
            </span>
          </button>
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'profile' }"
            type="button"
            @click="rightTab = 'profile'"
          >👥 {{ t('film.storyTabProfile') }}
            <span v-if="profiles.length" class="fh-pill fh-pill-muted fh-pill-mini">
              {{ t('film.storyProfileCount', { n: profiles.length }) }}
            </span>
          </button>
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'draft' }"
            type="button"
            @click="rightTab = 'draft'"
          >✍ {{ t('film.storyTabDraft') }}</button>
        </div>

        <div class="fh-card-body" style="flex: 1; min-height: 0">
          <!-- Tab 1：章节 -->
          <template v-if="rightTab === 'chapters'">
            <div class="fh-card-head" style="padding: 0 0 8px">
              <span v-if="chaptersIndex?.auto" class="fh-pill fh-pill-amber fh-pill-mini">
                {{ t('film.storyAutoSeg') }}
              </span>
              <div class="fh-head-actions">
                <button class="fh-btn fh-btn-mini" type="button" :disabled="chapterLoading" @click="loadChapters">↻</button>
              </div>
            </div>
            <div v-if="chaptersError" class="fh-error-box">{{ chaptersError }}</div>
            <div v-if="!chaptersIndex?.chapters?.length && !chaptersError" class="fh-empty">
              {{ t('film.storyChaptersEmpty') }}
            </div>
            <div
              v-for="ch in chaptersIndex?.chapters ?? []"
              :key="ch.no"
              class="fh-row"
              :class="{ 'is-active': selectedChapter?.no === ch.no }"
              style="cursor: pointer"
              @click="viewChapter(ch)"
            >
              <span class="fh-muted fh-mono fh-small" style="width: 34px">{{ ch.no }}</span>
              <span class="fh-ellipsis" style="flex: 1; min-width: 0" :title="ch.title">{{ ch.title }}</span>
              <span v-if="ch.auto" class="fh-pill fh-pill-amber fh-pill-mini">{{ t('film.storyAutoSeg') }}</span>
              <span class="fh-muted fh-small">{{ t('film.storyChapterWords', { n: ch.words ?? 0 }) }}</span>
            </div>
            <div v-if="selectedChapter" class="fh-pre-box" style="flex: 1; min-height: 120px">
              <div class="fh-muted fh-small fh-ellipsis" style="margin-bottom: 6px">
                {{ selectedChapter.file || `story/chapters/ch-${String(selectedChapter.no).padStart(2, '0')}.md` }}
              </div>
              <pre v-if="chapterLoading" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
              <pre v-else class="fh-pre">{{ chapterText || '—' }}</pre>
            </div>
          </template>

          <!-- Tab 2：人物档案 -->
          <template v-else-if="rightTab === 'profile'">
            <div class="fh-card-head" style="padding: 0 0 8px">
              <div class="fh-head-actions">
                <button class="fh-btn fh-btn-mini" type="button" @click="loadProfiles">↻</button>
              </div>
            </div>
            <div v-if="profilesError" class="fh-error-box">{{ profilesError }}</div>
            <div v-if="!profiles.length && !profilesError" class="fh-empty">
              {{ t('film.storyProfileEmpty') }}
            </div>
            <div v-for="p in profiles" :key="p.name" class="story-profile-card">
              <div class="story-profile-head">
                <span class="story-profile-name">{{ p.name }}</span>
                <span v-if="p.gender || p.age" class="fh-muted fh-small">{{ [p.gender, p.age].filter(Boolean).join(' · ') }}</span>
                <span
                  v-if="p.first_chapter"
                  class="fh-pill fh-pill-blue fh-pill-mini"
                >{{ t('film.storyProfileFirst', { n: p.first_chapter }) }}</span>
                <button
                  class="fh-btn fh-btn-mini"
                  :class="{ 'fh-btn-primary': !castingDone.has(p.name) }"
                  type="button"
                  style="margin-left: auto"
                  :disabled="castingBusy.has(p.name) || castingDone.has(p.name)"
                  @click="castToCasting(p)"
                >{{ castingDone.has(p.name) ? '✓ ' + t('film.storyToCastingDone') : t('film.storyToCasting') }}</button>
              </div>
              <div v-if="castingError.get(p.name)" class="fh-error-box">
                {{ t('film.storyToCastingFailed') }}{{ castingError.get(p.name) }}
              </div>
              <div v-if="p.aliases?.length" class="story-profile-line">
                <span class="fh-muted fh-small">{{ t('film.storyProfileAliases') }}：</span>
                <span
                  v-for="a in p.aliases"
                  :key="a"
                  class="fh-pill fh-pill-muted fh-pill-mini"
                >{{ a }}</span>
              </div>
              <div v-if="p.appearance" class="story-profile-line fh-small">🎭 {{ p.appearance }}</div>
              <div v-if="p.personality" class="story-profile-line fh-small">🧠 {{ p.personality }}</div>
              <div v-if="p.relations?.length" class="story-profile-line fh-small">
                <span class="fh-muted">{{ t('film.storyProfileRelations') }}：</span>
                <span v-for="(r, i) in p.relations" :key="i" class="fh-pill fh-pill-blue fh-pill-mini" style="margin-right: 4px">
                  {{ r.name }}{{ r.relation ? `·${r.relation}` : '' }}
                </span>
              </div>
              <div v-if="p.arc" class="story-profile-line fh-small">
                <span class="fh-muted">{{ t('film.storyProfileArc') }}：</span>{{ p.arc }}
              </div>
            </div>
          </template>

          <!-- Tab 3：正稿 story.md（v0.1.35 功能保留） -->
          <template v-else>
            <div class="fh-card-head" style="padding: 0 0 8px">
              <span v-if="storyMd" class="fh-pill fh-pill-muted">
                {{ t('film.storyWords', { n: storyWords }) }}
              </span>
              <span
                v-if="storyFm.source"
                class="fh-pill fh-pill-blue fh-ellipsis"
                style="max-width: 200px"
                :title="storyFm.source"
              >{{ t('film.storySourceLabel', { source: storyFm.source }) }}</span>
              <div class="fh-head-actions">
                <button
                  v-if="storyMd || storyMissing"
                  class="fh-btn fh-btn-small"
                  type="button"
                  :disabled="storyLoading"
                  @click="loadStory"
                >↻</button>
              </div>
            </div>
            <label class="fh-field">
              <span class="fh-field-label">{{ t('film.storyPrompt') }}</span>
              <textarea v-model="genPrompt" rows="2" :placeholder="t('film.storyPromptPh')" />
            </label>
            <div v-if="genError" class="fh-error-box">{{ genError }}</div>
            <div v-if="storyError" class="fh-error-box">{{ storyError }}</div>
            <div v-if="storyLoading" class="fh-empty">{{ t('film.loading') }}</div>
            <div v-else-if="!storyMd" class="fh-empty">
              {{ storyMissing || !storyError ? t('film.storyDraftEmpty') : '—' }}
            </div>
            <div v-else class="fh-pre-box" style="flex: 1">
              <pre class="fh-pre">{{ storyMd }}</pre>
            </div>
          </template>
        </div>
      </section>
    </div>
  </div>
</template>

<style scoped>
/* 拖拽导入高亮（左栏原文卡） */
.fh-card.is-dragover {
  outline: 2px dashed var(--accent, #e95420);
  outline-offset: -4px;
}

/* 人物档案卡（右栏·人物档案 Tab） */
.story-profile-card {
  border: 1px solid var(--border, rgba(0, 0, 0, 0.1));
  border-radius: 8px;
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  background: rgba(0, 0, 0, 0.015);
}
.story-profile-head {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.story-profile-name {
  font-weight: 600;
  font-size: 14px;
}
.story-profile-line {
  display: flex;
  align-items: baseline;
  gap: 4px;
  flex-wrap: wrap;
  min-width: 0;
}
</style>
