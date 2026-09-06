<script setup lang="ts">
// =============================================================================
// StoryPage.vue —— 剧情页（流程 ①，FilmHub v0.1.35）。
//
// 左右分栏：
//   左=原文区：导入按钮（文件选择 → b64 + 文件名上传 story/import）+ 已导入
//   source 列表（files 树 sources/ 派生；点击看内容 pre）；
//   右=剧情正稿 story.md：AI 生成（chat model_ref 复用 + prompt + 可选参考
//   原文 → story/generate 任务轮询）；生成后 pre 等宽展示（v1 不引 marked）；
//   字数 / 来源 front-matter 信息条。任务终态（refreshTick）自动重载。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  b64ToText,
  filmGetFile,
  filmListFiles,
  filmStoryGenerate,
  filmStoryImport,
  readFileAsDataUrl,
  splitDataUrl,
  type FilmFileContent,
  type FilmFileEntry,
} from '../api'
import FlowPageHead from './FlowPageHead.vue'
import { useFlow } from './flowContext'
import { fileBasename, parseFrontmatter, storySources, storyWordCount } from './flowFiles'

const { t } = useI18n()
const ctx = useFlow()

// —— 原文素材（左栏）——
const sources = ref<FilmFileEntry[]>([])
const sourcesError = ref('')
const importing = ref(false)
const importInput = ref<HTMLInputElement | null>(null)
/** 选中查看的原文（path + 文本）。 */
const selectedSource = ref('')
const sourceText = ref('')
const sourceLoading = ref(false)

async function loadSources(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    const tree = await filmListFiles(pid)
    sources.value = storySources(tree)
    sourcesError.value = ''
  } catch (e) {
    sources.value = []
    sourcesError.value = ctx ? ctx.errMsg(e) : String(e)
  }
}

async function viewSource(path: string): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  selectedSource.value = path
  sourceText.value = ''
  sourceLoading.value = true
  try {
    const env: FilmFileContent = await filmGetFile(pid, path)
    sourceText.value = b64ToText(env.content_b64 ?? '')
  } catch (e) {
    sourceText.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    sourceLoading.value = false
  }
}

async function onImportFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  const pid = ctx?.project.value?.id
  if (!file || !pid || !ctx) return
  if (file.size > 10 * 1024 * 1024) {
    sourcesError.value = t('film.storyImportTooLarge')
    return
  }
  importing.value = true
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
  }
}

// —— 剧情正稿 story.md（右栏）——
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

async function loadStory(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  storyLoading.value = true
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story.md')
    storyMd.value = b64ToText(env.content_b64 ?? '')
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

onMounted(() => {
  void loadSources()
  void loadStory()
})

// 任务终态 / 项目刷新 → 重载本页数据
watch(
  () => ctx?.refreshTick.value,
  () => {
    void loadSources()
    void loadStory()
  },
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
      <!-- 左：原文素材区 -->
      <section class="fh-card fh-col">
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
          <div v-if="sourcesError" class="fh-error-box">{{ sourcesError }}</div>
          <div v-if="!sources.length && !sourcesError" class="fh-empty">
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
            <span class="fh-muted fh-small fh-mono">{{ s.bytes ?? 0 }}B</span>
          </div>
          <!-- 选中原文内容（pre 等宽） -->
          <div v-if="selectedSource" class="fh-pre-box">
            <div class="fh-muted fh-small fh-ellipsis" style="margin-bottom: 6px">
              {{ selectedSource }}
            </div>
            <pre v-if="sourceLoading" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
            <pre v-else class="fh-pre">{{ sourceText || '—' }}</pre>
          </div>
        </div>
      </section>

      <!-- 右：剧情正稿 story.md -->
      <section class="fh-card fh-col">
        <div class="fh-card-head">
          <span>{{ t('film.storyDraft') }}</span>
          <!-- 信息条：字数 / 来源 front-matter -->
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
        <div class="fh-card-body">
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
        </div>
      </section>
    </div>
  </div>
</template>
