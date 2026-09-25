<script setup lang="ts">
// =============================================================================
// AudioPage.vue —— 音频页（BGM 库，流程 ④，FilmHub v0.1.35）。
//
// BGM 库列表：trigger 徽章（global=全局蓝 / 场景名）、mood、时长、有无 track；
// 导入（mp3 文件 + trigger/mood 表单 → POST audio/bgm {info, track_b64?}）；
// AI 生成（music model_ref + prompt → :track/generate 任务；v0.1.38 页头选择器
// 首项「项目默认」= 不传 model_ref，走 models.json music 缺省链）；删除。
// 表单校验纯函数 validateBgmForm（trigger 必填；导入口径文件必选）。
// 合成页 BGM 下拉与本病同源（filmListBgm）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  bgmEntryKey,
  bgmEntryMood,
  bgmEntryTrigger,
  filmCreateBgm,
  filmDeleteBgm,
  filmGenBgm,
  filmListBgm,
  readFileAsDataUrl,
  splitDataUrl,
  type FilmBgmEntry,
} from '../api'
import FlowPageHead from './FlowPageHead.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
import { PROJECT_DEFAULT_KEY, useFlow } from './flowContext'
import { validateBgmForm, type BgmFormError } from './flowFiles'
// v0.1.11 全局操作反馈：保存/删除/提交类成败 toast（BGM 生成任务走通知闭环）
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

// —— BGM 库列表 ——
const entries = ref<FilmBgmEntry[]>([])
const listLoading = ref(false)
const listError = ref('')

async function loadBgm(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  listLoading.value = true
  try {
    const raw = await filmListBgm(pid)
    entries.value = Array.isArray(raw) ? raw : []
    listError.value = ''
  } catch (e) {
    entries.value = []
    listError.value = t('film.bgmLoadFailed') + (ctx ? ctx.errMsg(e) : String(e))
  } finally {
    listLoading.value = false
  }
}

function isGlobal(e: FilmBgmEntry): boolean {
  return bgmEntryTrigger(e).toLowerCase() === 'global'
}

// —— 导入 / 新建表单 ——
const formTrigger = ref('')
const formMood = ref('')
const formFile = ref<File | null>(null)
const formFileName = ref('')
const formError = ref('')
const formBusy = ref(false)
const fileInput = ref<HTMLInputElement | null>(null)

function onPickFile(e: Event): void {
  const input = e.target as HTMLInputElement
  formFile.value = input.files?.[0] ?? null
  formFileName.value = formFile.value?.name ?? ''
}

/** 表单校验（纯函数同款口径——happy-dom 冒烟共用）。 */
function validateForm(requireFile: boolean): BgmFormError {
  return validateBgmForm(formTrigger.value, requireFile, !!formFile.value)
}

async function submitForm(requireFile: boolean): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx || formBusy.value) return
  const err = validateForm(requireFile)
  if (err === 'trigger') {
    formError.value = t('film.bgmErrTrigger')
    return
  }
  if (err === 'file') {
    formError.value = t('film.bgmErrFile')
    return
  }
  formBusy.value = true
  formError.value = ''
  try {
    let trackB64: string | undefined
    if (formFile.value) {
      const { b64 } = splitDataUrl(await readFileAsDataUrl(formFile.value))
      trackB64 = b64
    }
    const created = await filmCreateBgm(pid, {
      info: { trigger: formTrigger.value.trim(), ...(formMood.value.trim() ? { mood: formMood.value.trim() } : {}) },
      ...(trackB64 ? { track_b64: trackB64 } : {}),
      author: ctx.author.value,
    })
    // 建条目后无音频 → 直接链一步 AI 生成（requireFile=false 的「AI 生成」口径；
    // 「项目默认」= 不传 model_ref 走 models.json music 缺省链）
    if (!requireFile) {
      const useDefault = ctx.isProjectDefaultSel('music')
      const ref = ctx.modelRefFor('music')
      const key = bgmEntryKey(created)
      if (!ref && !useDefault) {
        formError.value = ctx.hasOptionsFor('music') ? t('film.pickModel') : t('film.noSource')
      } else if (key) {
        const task = await filmGenBgm(pid, key, {
          model_ref: ref ?? undefined,
          prompt: formMood.value.trim() || undefined,
          author: ctx.author.value,
        })
        ctx.addTracked(task)
      }
    }
    formTrigger.value = ''
    formMood.value = ''
    formFile.value = null
    formFileName.value = ''
    if (fileInput.value) fileInput.value.value = ''
    await loadBgm()
    await ctx.refreshCollab()
    // v0.1.11 保存类：BGM 条目落库全局反馈（AI 生成链路另有任务终态通知）
    toast.success(t('toast.saved'))
  } catch (e) {
    formError.value = t('film.bgmCreateFailed') + ctx.errMsg(e)
    toast.error(t('film.bgmCreateFailed') + ctx.errMsg(e))
  } finally {
    formBusy.value = false
  }
}

// —— 库内条目：AI 生成 / 删除 ——
const genPrompt = ref('')
const generatingKey = ref('')
const rowError = ref('')

async function genTrack(e: FilmBgmEntry): Promise<void> {
  const pid = ctx?.project.value?.id
  const key = bgmEntryKey(e)
  if (!pid || !ctx || !key || generatingKey.value) return
  const useDefault = ctx.isProjectDefaultSel('music')
  const ref = ctx.modelRefFor('music')
  if (!ref && !useDefault) {
    rowError.value = ctx.hasOptionsFor('music') ? t('film.pickModel') : t('film.noSource')
    return
  }
  generatingKey.value = key
  rowError.value = ''
  try {
    const task = await filmGenBgm(pid, key, {
      model_ref: ref ?? undefined,
      ...(genPrompt.value.trim() ? { prompt: genPrompt.value.trim() } : {}),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (err) {
    rowError.value = t('film.actFailed') + ctx.errMsg(err)
    toast.error(t('film.actFailed') + ctx.errMsg(err))
  } finally {
    generatingKey.value = ''
  }
}

async function removeEntry(e: FilmBgmEntry): Promise<void> {
  const pid = ctx?.project.value?.id
  const key = bgmEntryKey(e)
  if (!pid || !ctx || !key) return
  if (!window.confirm(t('film.bgmDelConfirm', { track: key }))) return
  try {
    await filmDeleteBgm(pid, key, ctx.author.value)
    await loadBgm()
    await ctx.refreshCollab()
    // v0.1.11 危险确认类：删除成功 toast
    toast.success(t('toast.deleted'))
  } catch (err) {
    rowError.value = t('film.bgmDelFailed') + ctx.errMsg(err)
    toast.error(t('film.bgmDelFailed') + ctx.errMsg(err))
  }
}

/** 有音频文件 = file/path 非空。 */
function hasTrack(e: FilmBgmEntry): boolean {
  return !!(e.file || e.path)
}

/** music 面就绪（有渠道可选项或选中「项目默认」）。 */
const musicModelReady = computed(() => ctx?.modelSelReady('music') ?? false)

onMounted(() => void loadBgm())
watch(
  () => ctx?.refreshTick.value,
  () => void loadBgm(),
)
</script>

<template>
  <div class="fh-page nx-page">
    <FlowPageHead stage="audio" :title="t('film.flowAudio')">
      <template #actions>
        <NxThemeToggle />
        <span class="fh-pill fh-pill-muted">{{ t('film.bgmCount', { n: entries.length }) }}</span>
        <!-- 🏷 项目默认（v0.1.38）：不传 model_ref，走 models.json music 缺省链 -->
        <select
          v-if="ctx"
          v-model="ctx.modelSel.music"
          class="fh-select"
          :title="t('film.model')"
        >
          <option :value="PROJECT_DEFAULT_KEY">
            🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('music') ? ' · ' + ctx.defaultModelSummary('music') : '' }}
          </option>
          <option v-if="!ctx.hasOptionsFor('music')" value="" disabled>
            {{ t('film.channelOnlyHint') }}
          </option>
          <optgroup v-for="g in ctx.optionsFor('music')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">
              {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
            </option>
          </optgroup>
        </select>
      </template>
    </FlowPageHead>

    <div class="fh-two-col fh-page-scroll">
      <!-- 左：BGM 库列表 -->
      <section class="fh-card fh-col">
        <div class="fh-card-head"><span>{{ t('film.bgmTitle') }}</span></div>
        <div class="fh-card-body">
          <div v-if="listError" class="fh-error-box">{{ listError }}</div>
          <div v-if="rowError" class="fh-error-box">{{ rowError }}</div>
          <div v-if="listLoading" class="fh-empty">{{ t('film.loading') }}</div>
          <div v-else-if="!entries.length && !listError" class="fh-empty">
            {{ t('film.bgmListEmpty') }}
          </div>
          <div v-for="e in entries" :key="bgmEntryKey(e)" class="fh-row">
            <span class="fh-pill fh-pill-mini" :class="isGlobal(e) ? 'fh-pill-blue' : 'fh-pill-violet'">
              {{ isGlobal(e) ? `🌐 ${t('film.bgmGlobal')}` : `🎬 ${bgmEntryTrigger(e) || '—'}` }}
            </span>
            <span v-if="bgmEntryMood(e)" class="fh-muted fh-small">{{ bgmEntryMood(e) }}</span>
            <span
              class="fh-pill fh-pill-mini"
              :class="hasTrack(e) ? 'fh-pill-ok' : 'fh-pill-muted'"
              style="border-style: dashed"
            >{{ hasTrack(e) ? `✅ ${t('film.bgmHasTrack')}` : `⚠ ${t('film.bgmNoTrack')}` }}</span>
            <span
              v-if="typeof e.duration_secs === 'number' && e.duration_secs > 0"
              class="fh-muted fh-small fh-mono"
            >{{ t('film.bgmDuration', { n: Math.round(e.duration_secs) }) }}</span>
            <div style="margin-left: auto; display: flex; gap: 6px">
              <button
                class="fh-btn fh-btn-mini"
                type="button"
                :disabled="generatingKey === bgmEntryKey(e) || !musicModelReady || !ctx?.channelAvailable.value || ctx?.isOffline.value"
                :title="t('film.charGenTip')"
                @click="genTrack(e)"
              >{{ generatingKey === bgmEntryKey(e) ? '…' : `✨ ${t('film.bgmGen')}` }}</button>
              <button
                class="fh-btn fh-btn-mini fh-btn-danger"
                type="button"
                @click="removeEntry(e)"
              >{{ t('film.del') }}</button>
            </div>
          </div>
          <!-- AI 生成共享提示词 -->
          <div v-if="entries.length" class="fh-field">
            <span class="fh-field-label">{{ t('film.bgmGenPrompt') }}</span>
            <input v-model="genPrompt" type="text" class="fh-input" :placeholder="t('film.bgmPromptPh')">
          </div>
        </div>
      </section>

      <!-- 右：导入 / 新建表单 -->
      <section class="fh-card fh-col fh-col-side">
        <div class="fh-card-head"><span>{{ t('film.bgmImportTitle') }}</span></div>
        <div class="fh-card-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('film.bgmTrigger') }}</span>
            <input v-model="formTrigger" type="text" class="fh-input" :placeholder="t('film.bgmTriggerPh')" :disabled="formBusy">
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('film.bgmMood') }}</span>
            <input v-model="formMood" type="text" class="fh-input" :placeholder="t('film.bgmMoodPh')" :disabled="formBusy">
          </label>
          <div class="fh-field">
            <span class="fh-field-label">{{ t('film.bgmFile') }}</span>
            <div class="fh-field-row">
              <button class="fh-btn fh-btn-small" type="button" :disabled="formBusy" @click="fileInput?.click()">
                📁 {{ formFileName || t('film.bgmPickFile') }}
              </button>
              <span v-if="formFileName" class="fh-muted fh-small fh-ellipsis" style="flex: 1">{{ formFileName }}</span>
              <input ref="fileInput" type="file" accept="audio/mpeg,.mp3" class="fh-hidden-input" @change="onPickFile">
            </div>
          </div>
          <div v-if="formError" class="fh-error-box">{{ formError }}</div>
          <div class="fh-field-row">
            <button class="fh-btn fh-btn-primary fh-btn-small" type="button" :disabled="formBusy" @click="submitForm(true)">
              {{ formBusy ? '…' : t('film.bgmSubmit') }}
            </button>
            <button
              class="fh-btn fh-btn-small"
              type="button"
              :disabled="formBusy || !musicModelReady || !ctx?.channelAvailable.value || ctx?.isOffline.value"
              :title="t('film.bgmGenEntryTip')"
              @click="submitForm(false)"
            >✨ {{ t('film.bgmGenEntry') }}</button>
          </div>
          <div class="fh-muted fh-small">{{ t('film.bgmFormHint') }}</div>
        </div>
      </section>
    </div>
  </div>
</template>
