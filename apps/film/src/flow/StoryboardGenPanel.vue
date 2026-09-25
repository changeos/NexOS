<script setup lang="ts">
// =============================================================================
// StoryboardGenPanel.vue —— 生成分镜配置面板（v0.1.39，共用组件）。
//
// 工作台顶部「生成分镜」按钮与分镜页「从剧情生成分镜」共用：折叠面板
// （默认收起，点「⚙ 生成配置」展开），可选项——
//   · 分镜章节（v0.1.44 按章分镜）：下拉「全部剧情」（缺省）/「第 N 章 …」
//     （读 story/chapters/index.json）；选中即随请求 chapter_range 字段
//     （"3"/"3-5"/"3,7,9" 宽松解析），后端语料改读章节文件不再喂全文。
//     无章节时只显示「全部剧情」+ 提示「未分章，建议先分章以支持按章生成」。
//     剧情页章节卡「从此章生成分镜」经 FlowContext.pendingStoryboardChapter
//     预填（挂载/更新时消费——下拉选中该章 + 展开面板）。
//   · 出场人物（多选 chips）：casting/characters 对象列表勾选；勾选集合随
//     生成请求 characters 字段（语义=提示词注入【出场角色】约束）。
//   · 声线选择：每勾选人物一行 OpenAI 11 枚举 voice 下拉；改动即 PUT
//     casting/characters/:name card 的 voice（既有端点，立即生效——生成后
//     分镜 tts 按角色 voice 透传）。**此项无论后端生成端点通否都可落地。**
//   · 镜头数 shot_count：可选 5-12，空=缺省自动。
//   · 总时长提示 duration_hint：可选文本（如「约 60 秒」），语义=注入提示词。
//
// 宿主经 ref 调 config() 取拼装结果，随生成请求发出（FilmStudio 工作台 /
// StoryboardPage 两处接线）。FlowContext 注入项目 id / 操作人 / refreshTick。
// =============================================================================
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  OPENAI_VOICES,
  filmContentText,
  filmGetFile,
  filmListCasting,
  filmUpdateCasting,
  type FilmCastingObject,
  type FilmChapterIndex,
} from '../api'
import { useFlow } from './flowContext'
// v0.1.11 全局操作反馈：声线即时保存成败 toast
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

// —— 折叠态（默认收起——不挡流程；记忆到 localStorage）——
const open = ref(false)
try {
  open.value = localStorage.getItem('nexos.film.sbGenPanel') === '1'
} catch {
  /* 隐私模式等：忽略 */
}
watch(open, (v) => {
  try {
    localStorage.setItem('nexos.film.sbGenPanel', v ? '1' : '0')
  } catch {
    /* 忽略 */
  }
})

// —— 分镜章节（v0.1.44 按章分镜：读 chapters index；''=全部剧情缺省）——
const chaptersIndex = ref<FilmChapterIndex | null>(null)
/** 章节清单是否已加载完成（一次 fetch 落定——未分章 404 也算落定；预填消费
 *  依据：未落定时保留 pending 待 watch 重试，不悬留不误清）。 */
const chaptersSettled = ref(false)
const chapterSel = ref('')
/** 下拉选项：[{value, label}]——全部剧情 + 每章一条（第 N 章 · 标题截 18 字）。 */
const chapterOptions = computed(() => {
  const out: { value: string; label: string }[] = [
    { value: '', label: t('film.sbgChapterAll') },
  ]
  for (const ch of chaptersIndex.value?.chapters ?? []) {
    if (typeof ch.no !== 'number') continue
    out.push({ value: String(ch.no), label: t('film.sbgChapterOption', { n: ch.no, title: (ch.title ?? '').slice(0, 18) }) })
  }
  return out
})
/** 无章节（未分章）——只显示「全部剧情」+ 建议提示行。 */
const noChapters = computed(
  () => !chaptersIndex.value || !(chaptersIndex.value.chapters ?? []).length,
)

async function loadChapters(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    const env = await filmGetFile(pid, 'story/chapters/index.json')
    const idx = JSON.parse(filmContentText(env)) as FilmChapterIndex
    chaptersIndex.value = Array.isArray(idx.chapters) ? idx : { chapters: [] }
  } catch {
    // 无 index = 未分章（缺省态，提示行引导）；其余静默降级不崩
    chaptersIndex.value = null
  } finally {
    chaptersSettled.value = true
  }
}

onMounted(() => void loadChapters())
watch(
  () => [ctx?.project.value?.id, ctx?.refreshTick.value] as const,
  () => {
    chaptersSettled.value = false
    void loadChapters()
  },
)

// 章节清单重载后下拉选中项失效（如重新分章少了该章）→ 回落「全部剧情」
watch(chapterOptions, (opts) => {
  if (chapterSel.value && !opts.some((o) => o.value === chapterSel.value)) {
    chapterSel.value = ''
  }
})

// —— 剧情页章节卡「从此章生成分镜」预填（pendingStoryboardChapter 注册表式：
//    消费即清空——下拉选中该章 + 展开面板；章节清单未加载完成时保留 pending
//    待 watch 重试，落定后无该章则丢弃不悬留）——
function consumePendingChapter(): void {
  const pending = ctx?.pendingStoryboardChapter?.value
  if (!pending) return
  if (chapterOptions.value.some((o) => o.value === pending)) {
    ctx!.pendingStoryboardChapter.value = null
    chapterSel.value = pending
    open.value = true
    toast.success(t('film.sbgChapterPrefilled', { range: pending }))
  } else if (chaptersSettled.value) {
    // 清单已落定且无该章（陈旧章号/未分章）——丢弃 pending 不悬留
    ctx!.pendingStoryboardChapter.value = null
  }
}

onMounted(consumePendingChapter)
watch(
  () => [ctx?.pendingStoryboardChapter?.value, chaptersIndex.value] as const,
  () => consumePendingChapter(),
)

// —— casting 人物列表（chips 数据源；加载/错误态友好降级）——
const castChars = ref<FilmCastingObject[]>([])
const castLoading = ref(false)
const castError = ref('')

async function loadCastChars(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  castLoading.value = true
  try {
    const list = await filmListCasting(pid, 'characters')
    castChars.value = Array.isArray(list) ? list : []
    // 已勾选但被删的人物清出勾选集
    const names = new Set(castChars.value.map((c) => c.name))
    for (const n of [...selected.value]) if (!names.has(n)) toggleChar(n, false)
    castError.value = ''
  } catch (e) {
    castChars.value = []
    castError.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    castLoading.value = false
  }
}

onMounted(() => void loadCastChars())
watch(
  () => [ctx?.project.value?.id, ctx?.refreshTick.value] as const,
  () => void loadCastChars(),
)

// —— 出场人物勾选（有序数组——请求 characters 与 voice 行顺序稳定）——
const selected = ref<string[]>([])

function toggleChar(name: string, on: boolean): void {
  if (on && !selected.value.includes(name)) {
    selected.value = [...selected.value, name]
    // voice 初值 = card 既有 voice（无则缺省 alloy）
    const card = castChars.value.find((c) => c.name === name)
    voiceSel[name] = card?.voice || 'alloy'
  } else if (!on) {
    selected.value = selected.value.filter((n) => n !== name)
    delete voiceSel[name]
  }
}

function isPicked(name: string): boolean {
  return selected.value.includes(name)
}

// —— 声线选择（每勾选人物一行下拉；改动即 PUT casting card voice）——
const voiceSel = reactive<Record<string, string>>({})
const voiceBusy = ref('')
const voiceError = ref('')

/** 下拉选项：11 枚举 +（card 既有自定义 voice 时）额外保留一项可读回显。 */
function voiceOptions(name: string): string[] {
  const cur = castChars.value.find((c) => c.name === name)?.voice ?? ''
  if (cur && !(OPENAI_VOICES as readonly string[]).includes(cur)) {
    return [...OPENAI_VOICES, cur]
  }
  return [...OPENAI_VOICES]
}

async function onVoiceChange(name: string, voice: string): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || voiceBusy.value) return
  voiceBusy.value = name
  voiceError.value = ''
  // 乐观更新本地选中值（失败回退 card 既有 voice——下拉显示与请求拼装同源）
  const prev = castChars.value.find((c) => c.name === name)?.voice || 'alloy'
  voiceSel[name] = voice
  try {
    await filmUpdateCasting(pid, 'characters', name, {
      voice,
      ...(ctx?.author.value ? { author: ctx.author.value } : {}),
    })
    const card = castChars.value.find((c) => c.name === name)
    if (card) card.voice = voice
    // v0.1.11 保存类：声线即时写入成功全局反馈（失败下方红条 + toast）
    toast.success(t('toast.saved'))
  } catch (e) {
    // 回退下拉显示 + 错误直显（card 未写成功）
    voiceSel[name] = prev
    voiceError.value = t('film.sbgVoiceSaveFailed') + (ctx ? ctx.errMsg(e) : String(e))
    toast.error(voiceError.value)
  } finally {
    voiceBusy.value = ''
  }
}

// —— 镜头数 / 总时长提示 ——
const shotCount = ref<number | null>(null)
const durationHint = ref('')

const shotCountInvalid = computed(
  () => shotCount.value !== null && (shotCount.value < 5 || shotCount.value > 12),
)

/** 已配置项数（折叠头徽章：章节/人物/镜头数/时长任一设置即计数）。 */
const tunedCount = computed(() => {
  let n = selected.value.length ? 1 : 0
  if (chapterSel.value) n++
  if (shotCount.value !== null) n++
  if (durationHint.value.trim()) n++
  return n
})

/**
 * 拼装生成配置（宿主生成时读取；全部可空——未配置 = 不带字段，行为与
 * 旧请求完全一致）。chapter_range 选中章节时以章号字符串发出（"3"）。
 */
function config(): {
  characters?: string[]
  voices?: Record<string, string>
  shot_count?: number
  duration_hint?: string
  chapter_range?: string
} {
  const out: {
    characters?: string[]
    voices?: Record<string, string>
    shot_count?: number
    duration_hint?: string
    chapter_range?: string
  } = {}
  if (selected.value.length) out.characters = [...selected.value]
  if (selected.value.length) {
    const voices: Record<string, string> = {}
    for (const n of selected.value) voices[n] = voiceSel[n] ?? 'alloy'
    out.voices = voices
  }
  if (shotCount.value !== null && !shotCountInvalid.value) out.shot_count = shotCount.value
  const hint = durationHint.value.trim()
  if (hint) out.duration_hint = hint
  if (chapterSel.value) out.chapter_range = chapterSel.value
  return out
}

defineExpose({ config })
</script>

<template>
  <div class="sbg-panel">
    <button
      class="sbg-toggle"
      type="button"
      :aria-expanded="open"
      @click="open = !open"
    >
      <span class="sbg-caret" aria-hidden="true">{{ open ? '▾' : '▸' }}</span>
      <span>⚙ {{ t('film.sbgToggle') }}</span>
      <span v-if="tunedCount" class="fh-pill fh-pill-blue fh-pill-mini">{{ tunedCount }}</span>
      <span class="sbg-toggle-hint fh-muted fh-small">{{ t('film.sbgToggleHint') }}</span>
    </button>

    <div v-if="open" class="sbg-body">
      <!-- 分镜章节（v0.1.44 按章分镜：全部剧情[缺省]/第 N 章…；无章节提示建议先分章） -->
      <div class="fh-field">
        <span class="fh-field-label">📖 {{ t('film.sbgChapter') }}</span>
        <select v-model="chapterSel" class="fh-select" :title="t('film.sbgChapterTip')">
          <option v-for="o in chapterOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
        </select>
        <span v-if="noChapters" class="fh-muted fh-small">{{ t('film.sbgChapterNoChapters') }}</span>
        <span v-else-if="chapterSel" class="fh-muted fh-small">{{ t('film.sbgChapterSelectedHint') }}</span>
        <span v-else class="fh-muted fh-small">{{ t('film.sbgChapterAllHint') }}</span>
      </div>

      <!-- 出场人物 chips（casting/characters 对象列表） -->
      <div class="fh-field">
        <span class="fh-field-label">👤 {{ t('film.sbgCharacters') }}</span>
        <div v-if="castLoading" class="fh-muted fh-small">{{ t('film.loading') }}</div>
        <div v-else-if="castError" class="fh-error-box">
          {{ t('film.sbgCastLoadFailed') }}{{ castError }}
          <button class="fh-btn fh-btn-mini" type="button" @click="loadCastChars">
            {{ t('film.retry') }}
          </button>
        </div>
        <template v-else>
          <div v-if="!castChars.length" class="fh-muted fh-small">
            {{ t('film.sbgCastEmpty') }}
            <button
              class="fh-btn fh-btn-mini"
              type="button"
              @click="ctx?.setView('casting')"
            >{{ t('film.sbgGoCasting') }} →</button>
          </div>
          <div v-else class="sbg-chip-row">
            <button
              v-for="c in castChars"
              :key="c.name"
              class="sbg-chip"
              :class="{ 'is-active': isPicked(c.name) }"
              type="button"
              :title="c.desc || c.name"
              @click="toggleChar(c.name, !isPicked(c.name))"
            >{{ isPicked(c.name) ? '✓ ' : '' }}{{ c.name }}</button>
          </div>
        </template>
      </div>

      <!-- 声线选择：每勾选人物一行（改动即写 card voice——已通链路） -->
      <div v-if="selected.length" class="fh-field">
        <span class="fh-field-label">🎙 {{ t('film.sbgVoices') }}</span>
        <div
          v-for="name in selected"
          :key="name"
          class="sbg-voice-row"
        >
          <span class="sbg-voice-name fh-ellipsis" :title="name">👤 {{ name }}</span>
          <select
            class="fh-select sbg-voice-select"
            :value="voiceSel[name] ?? 'alloy'"
            :disabled="voiceBusy === name"
            :title="t('film.sbgVoiceTip')"
            @change="onVoiceChange(name, ($event.target as HTMLSelectElement).value)"
          >
            <option v-for="v in voiceOptions(name)" :key="v" :value="v">{{ v }}</option>
          </select>
        </div>
        <span class="fh-muted fh-small">{{ t('film.sbgVoiceHint') }}</span>
        <div v-if="voiceError" class="fh-error-box">{{ voiceError }}</div>
      </div>

      <!-- 镜头数 + 总时长提示 -->
      <div class="fh-field-row">
        <label class="fh-field fh-field-narrow">
          <span class="fh-field-label">🎞 {{ t('film.sbgShotCount') }}</span>
          <input
            v-model.number="shotCount"
            type="number"
            min="5"
            max="12"
            step="1"
            :placeholder="t('film.sbgShotCountPh')"
          />
          <span v-if="shotCountInvalid" class="fh-muted fh-small">{{ t('film.sbgShotCountRange') }}</span>
          <span v-else class="fh-muted fh-small">{{ t('film.sbgShotCountHint') }}</span>
        </label>
        <label class="fh-field" style="flex: 1 1 220px">
          <span class="fh-field-label">⏱ {{ t('film.sbgDuration') }}</span>
          <input
            v-model="durationHint"
            type="text"
            :placeholder="t('film.sbgDurationPh')"
          />
        </label>
      </div>

      <!-- v1 实况标注（如实提示未通的面） -->
      <div class="fh-warn-box sbg-note">{{ t('film.sbgPendingNote') }}</div>
    </div>
  </div>
</template>

<style scoped>
/* v0.1.8 nx 化：折叠面板 = nx-card 静默刻面；chips = info 选中档 */
.sbg-panel {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  background: var(--nx-bg-card);
  box-shadow: var(--nx-shadow-xs);
  flex-shrink: 0;
}
.sbg-toggle {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 8px 12px;
  border: none;
  background: none;
  font-family: inherit;
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-secondary);
  cursor: pointer;
  text-align: left;
  border-radius: inherit;
  transition: background 0.12s ease, color 0.12s ease;
}
.sbg-toggle:hover { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.sbg-toggle:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: -2px;
}
.sbg-caret { width: 14px; color: var(--nx-text-tertiary); }
.sbg-toggle-hint { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sbg-body {
  padding: 4px 12px 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  border-top: 0.5px solid var(--nx-line-divider);
}
.sbg-chip-row { display: flex; gap: 6px; flex-wrap: wrap; }
.sbg-chip {
  padding: 3px 12px;
  border-radius: var(--nx-radius-badge);
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-card);
  color: var(--nx-text-secondary);
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    background 0.15s ease,
    color 0.15s ease;
}
.sbg-chip:hover {
  border-color: var(--nx-line-strong);
  background: var(--nx-bg-hover);
  color: var(--nx-text-primary);
}
.sbg-chip.is-active {
  color: var(--nx-info);
  background: var(--nx-info-bg);
  border-color: var(--nx-info-border);
  font-weight: var(--nx-font-weight-medium);
}
.sbg-chip:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.sbg-voice-row {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.sbg-voice-name { flex: 0 0 40%; min-width: 0; font-size: var(--nx-font-size-xs); }
.sbg-voice-select { flex: 0 0 160px; }
.sbg-note { font-size: var(--nx-font-size-xs); }
</style>
