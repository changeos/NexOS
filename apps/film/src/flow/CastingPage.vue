<script setup lang="ts">
// =============================================================================
// CastingPage.vue —— 定妆页（流程 ③，FilmHub v0.1.35 骨架——后续细化）。
//
// 顶部「AI 提取定妆对象」（chat model_ref → casting/extract；202 任务或 200
// 直出报告）→ extraction.json 报告展示（六类分组 name/desc/frequency，可折叠）。
// 六类 Tab（人物/武器/宠物/排列/动作/场景）——每类对象卡列表（信息 + 视图缩略
// 横排 + 负责人徽章：未认领=「待认领」+认领按钮，多人按对象分工）。
// 对象详情：多视图网格（front/side/back/action/custom 五槽位；空槽虚线 +
// 「AI 生成」（image model_ref + 槽位 view）/「导入」双按钮）+ card.md 描述
// 编辑 + voice（人物类）。新建对象表单（创建人自动成为对象 owner）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  FILM_CAST_TYPES,
  OPENAI_VOICES,
  b64ToText,
  filmCreateCasting,
  filmCastingExtract,
  filmDeleteCasting,
  filmFileDataUrl,
  filmGenCastView,
  filmGetFile,
  filmImportCastView,
  filmListCasting,
  filmUpdateCasting,
  isFilmTask,
  readFileAsDataUrl,
  splitDataUrl,
  type FilmCastType,
  type FilmCastingExtraction,
  type FilmCastingObject,
  type FilmExtractionItem,
} from '../api'
import { claimCastingObject, objectOwner } from './collab'
import FlowPageHead from './FlowPageHead.vue'
import { useFlow } from './flowContext'
import { CAST_VIEW_SLOTS, matchCastView, readyViewCount, type CastViewSlot } from './flowFiles'

const { t } = useI18n()
const ctx = useFlow()

// —— 六类元数据（图标 + i18n 标签）——
const TYPE_ICONS: Record<FilmCastType, string> = {
  characters: '👤',
  props: '🗡',
  pets: '🐾',
  formations: '🧭',
  actions: '🏃',
  scenes: '🏞',
}
function typeLabel(ty: FilmCastType): string {
  switch (ty) {
    case 'characters': return t('film.castTabCharacters')
    case 'props': return t('film.castTabProps')
    case 'pets': return t('film.castTabPets')
    case 'formations': return t('film.castTabFormations')
    case 'actions': return t('film.castTabActions')
    case 'scenes': return t('film.castTabScenes')
  }
}

// —— Tab 态 + 对象列表 ——
const activeType = ref<FilmCastType>('characters')
const objects = ref<FilmCastingObject[]>([])
const objectsLoading = ref(false)
const objectsError = ref('')
/** 当前详情对象名。 */
const selectedName = ref('')

const selected = computed<FilmCastingObject | null>(
  () => objects.value.find((o) => o.name === selectedName.value) ?? null,
)

async function loadObjects(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  objectsLoading.value = true
  try {
    const list = await filmListCasting(pid, activeType.value)
    objects.value = Array.isArray(list) ? list : []
    objectsError.value = ''
    // 选中对象被删 → 回退首个
    if (!objects.value.some((o) => o.name === selectedName.value)) {
      selectedName.value = objects.value[0]?.name ?? ''
      resetDraft()
    }
  } catch (e) {
    objects.value = []
    objectsError.value = t('film.castLoadFailed') + (ctx ? ctx.errMsg(e) : String(e))
  } finally {
    objectsLoading.value = false
  }
}

function switchType(ty: FilmCastType): void {
  if (activeType.value === ty) return
  activeType.value = ty
  selectedName.value = ''
  resetDraft()
  void loadObjects()
}

// —— 视图缩略（path → data URL 懒加载缓存）——
const viewThumbs = ref<Record<string, string>>({})
async function loadThumb(path: string): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || viewThumbs.value[path]) return
  try {
    viewThumbs.value[path] = await filmFileDataUrl(pid, path)
  } catch {
    /* 缩略失败保持无图 */
  }
}
function ensureThumbs(): void {
  for (const o of objects.value) {
    for (const v of o.views ?? []) {
      if (v.path) void loadThumb(v.path)
    }
  }
}
watch(objects, () => ensureThumbs(), { immediate: true })

// —— AI 提取定妆对象（extraction 报告）——
const extracting = ref(false)
const extractError = ref('')
const extraction = ref<FilmCastingExtraction | null>(null)
const reportOpen = ref(false)

/** extraction 报告条目（六类分组宽容归一）。 */
function reportItems(ty: FilmCastType): FilmExtractionItem[] {
  const raw = extraction.value?.[ty]
  return Array.isArray(raw) ? raw : []
}

async function loadExtraction(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  for (const p of ['extraction.json', 'casting/extraction.json']) {
    try {
      const env = await filmGetFile(pid, p)
      if (!env.content_b64) continue
      extraction.value = JSON.parse(b64ToText(env.content_b64)) as FilmCastingExtraction
      return
    } catch {
      /* 逐路径尝试；均缺失=尚无报告 */
    }
  }
  extraction.value = null
}

async function runExtract(): Promise<void> {
  if (!ctx || extracting.value) return
  const ref = ctx.modelRefFor('chat')
  if (!ref) {
    extractError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  extracting.value = true
  extractError.value = ''
  try {
    const res = await filmCastingExtract(ctx.project.value!.id, ref, ctx.author.value)
    if (isFilmTask(res)) {
      ctx.addTracked(res) // 任务终态 → refreshTick → loadExtraction
    } else {
      extraction.value = res
      reportOpen.value = true
      await loadObjects()
    }
  } catch (e) {
    extractError.value = t('film.castExtractFailed') + ctx.errMsg(e)
  } finally {
    extracting.value = false
  }
}

// —— 对象详情（描述编辑 / voice / 多视图槽位）——
const descDraft = ref('')
const descDirty = ref(false)
const descSaving = ref(false)
const descMsg = ref('')
/** voice 表单（人物类）。 */
const voiceKind = ref<'enum' | 'custom' | 'none'>('none')
const voiceEnum = ref('alloy')
const voiceCustom = ref('')
/** 视图生成提示词（详情级共享）。 */
const viewPrompt = ref('')
/** custom 槽自定义视图名。 */
const customViewName = ref('custom')
/** 导入中槽位（busy 标记）。 */
const importingSlot = ref<CastViewSlot | null>(null)
const viewInput = ref<HTMLInputElement | null>(null)
let pendingImportSlot: CastViewSlot = 'front'
const detailError = ref('')

function resetDraft(): void {
  descDraft.value = ''
  descDirty.value = false
  descMsg.value = ''
  viewPrompt.value = ''
  customViewName.value = 'custom'
  detailError.value = ''
}

function selectObject(name: string): void {
  selectedName.value = name
  resetDraft()
}

watch(selected, (o) => {
  if (!o) return
  if (!descDirty.value) descDraft.value = o.desc ?? ''
  const v = o.voice ?? ''
  if (!v) voiceKind.value = 'none'
  else if ((OPENAI_VOICES as readonly string[]).includes(v)) {
    voiceKind.value = 'enum'
    voiceEnum.value = v
  } else {
    voiceKind.value = 'custom'
    voiceCustom.value = v
  }
}, { immediate: true })

function markDescDirty(): void {
  descDirty.value = true
  descMsg.value = ''
}

/** 保存 card.md 描述（+voice 人物类）——PUT casting/:type/:name。 */
async function saveCard(): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || descSaving.value) return
  descSaving.value = true
  descMsg.value = ''
  try {
    const voice =
      activeType.value === 'characters'
        ? voiceKind.value === 'custom'
          ? voiceCustom.value.trim() || ''
          : voiceKind.value === 'enum'
            ? voiceEnum.value
            : ''
        : undefined
    await filmUpdateCasting(
      pid,
      activeType.value,
      o.name,
      { desc: descDraft.value, ...(voice !== undefined ? { voice } : {}), author: ctx!.author.value },
    )
    descDirty.value = false
    descMsg.value = t('film.castSaved')
    await loadObjects()
    await ctx!.refreshCollab()
  } catch (e) {
    descMsg.value = t('film.castSaveFailed') + ctx!.errMsg(e)
  } finally {
    descSaving.value = false
  }
}

/** 槽位视图名（custom 槽用输入值）。 */
function slotViewName(slot: CastViewSlot): string {
  return slot === 'custom' ? customViewName.value.trim() || 'custom' : slot
}

function matchedView(slot: CastViewSlot) {
  return matchCastView(selected.value, slot)
}

/** AI 生成槽位视图（image model_ref + 槽位 view + 可选提示词）。 */
async function genView(slot: CastViewSlot): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || !ctx) return
  const ref = ctx.modelRefFor('image')
  if (!ref) {
    detailError.value = ctx.hasOptionsFor('image') ? t('film.pickModel') : t('film.noSource')
    return
  }
  detailError.value = ''
  try {
    const task = await filmGenCastView(pid, activeType.value, o.name, {
      model_ref: ref,
      view: slotViewName(slot),
      ...(viewPrompt.value.trim() ? { prompt: viewPrompt.value.trim() } : {}),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (e) {
    detailError.value = t('film.actFailed') + ctx.errMsg(e)
  }
}

function pickImport(slot: CastViewSlot): void {
  pendingImportSlot = slot
  viewInput.value?.click()
}

async function onViewFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  const pid = ctx?.project.value?.id
  const o = selected.value
  const slot = pendingImportSlot
  if (!file || !pid || !o || !ctx) return
  if (file.size > 10 * 1024 * 1024) {
    detailError.value = t('film.castViewTooLarge')
    return
  }
  importingSlot.value = slot
  detailError.value = ''
  try {
    const { b64 } = splitDataUrl(await readFileAsDataUrl(file))
    await filmImportCastView(pid, activeType.value, o.name, {
      image_b64: b64,
      view: slotViewName(slot),
      author: ctx.author.value,
    })
    await loadObjects()
    await ctx.refreshCollab()
  } catch (err) {
    detailError.value = t('film.castViewImportFailed') + ctx.errMsg(err)
  } finally {
    importingSlot.value = null
  }
}

function viewLabel(slot: CastViewSlot): string {
  switch (slot) {
    case 'front': return t('film.castViewFront')
    case 'side': return t('film.castViewSide')
    case 'back': return t('film.castViewBack')
    case 'action': return t('film.castViewAction')
    case 'custom': return t('film.castViewCustom')
  }
}

// —— 对象级认领（多人按对象分工）——
function objOwner(name: string): string {
  return ctx ? objectOwner(ctx.ownership.value, activeType.value, name) : ''
}
async function claimObject(name: string, owner: string): Promise<void> {
  if (!ctx) return
  await ctx.saveOwnership(
    claimCastingObject(ctx.ownership.value, activeType.value, name, owner),
  )
}

// —— 新建对象（弹窗；创建人自动成为 owner）——
const showCreate = ref(false)
const creating = ref(false)
const createError = ref('')
const createForm = ref({ name: '', desc: '', voiceKind: 'none' as 'enum' | 'custom' | 'none', voiceEnum: 'alloy', voiceCustom: '' })

function openCreate(): void {
  createForm.value = { name: '', desc: '', voiceKind: 'none', voiceEnum: 'alloy', voiceCustom: '' }
  createError.value = ''
  showCreate.value = true
}

async function submitCreate(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx || creating.value) return
  const f = createForm.value
  if (!f.name.trim()) {
    createError.value = t('film.castErrName')
    return
  }
  if (!f.desc.trim()) {
    createError.value = t('film.castErrDesc')
    return
  }
  creating.value = true
  createError.value = ''
  try {
    const voice =
      activeType.value === 'characters'
        ? f.voiceKind === 'custom'
          ? f.voiceCustom.trim() || undefined
          : f.voiceKind === 'enum'
            ? f.voiceEnum
            : undefined
        : undefined
    await filmCreateCasting(pid, activeType.value, {
      name: f.name.trim(),
      desc: f.desc.trim(),
      ...(voice ? { voice } : {}),
      author: ctx.author.value,
    })
    // 创建人自动成为对象 owner（认领粒度=对象级）
    await claimObject(f.name.trim(), ctx.author.value)
    showCreate.value = false
    selectedName.value = f.name.trim()
    await loadObjects()
  } catch (e) {
    createError.value = t('film.castSaveFailed') + ctx.errMsg(e)
  } finally {
    creating.value = false
  }
}

// —— 删除对象 ——
async function removeObject(o: FilmCastingObject): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx) return
  if (!window.confirm(t('film.castDelConfirm', { name: o.name }))) return
  try {
    await filmDeleteCasting(pid, activeType.value, o.name, ctx.author.value)
    if (selectedName.value === o.name) selectedName.value = ''
    await loadObjects()
    await ctx.refreshCollab()
  } catch (e) {
    objectsError.value = t('film.castDelFailed') + ctx.errMsg(e)
  }
}

onMounted(() => {
  void loadObjects()
  void loadExtraction()
})

// 任务终态 → 重载对象/报告
watch(
  () => ctx?.refreshTick.value,
  () => {
    void loadObjects()
    void loadExtraction()
  },
)
</script>

<template>
  <div class="fh-page">
    <FlowPageHead stage="casting" :title="t('film.flowCasting')">
      <template #actions>
        <select v-if="ctx" v-model="ctx.modelSel.chat" class="fh-select" :title="t('film.model')">
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>{{ t('film.noRunningLlm') }}</option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">{{ o.label }}{{ o.relay ? ' 🌐' : '' }}</option>
          </optgroup>
        </select>
        <button
          class="fh-btn fh-btn-primary fh-btn-small"
          type="button"
          :disabled="extracting || !ctx?.project.value || !ctx?.hasOptionsFor('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="runExtract"
        >{{ extracting ? t('film.taskRunning') + '…' : t('film.castExtract') }}</button>
        <button class="fh-btn fh-btn-small" type="button" @click="openCreate">+ {{ t('film.castNew') }}</button>
      </template>
    </FlowPageHead>

    <div class="fh-page-scroll">
      <!-- 提取报告（六类分组 name/desc/frequency；可折叠） -->
      <div v-if="extraction" class="fh-card">
        <button
          class="fh-card-head"
          type="button"
          style="cursor: pointer; background: transparent; border: none; border-bottom: 1px solid var(--border-soft, #EDEDED)"
          @click="reportOpen = !reportOpen"
        >
          <span>📋 {{ t('film.castExtractReport') }}</span>
          <span class="fh-muted" style="margin-left: auto">{{ reportOpen ? '▾' : '▴' }}</span>
        </button>
        <div v-if="reportOpen" class="fh-card-body">
          <div v-for="ty in FILM_CAST_TYPES" :key="ty" class="cast-report-group">
            <div class="fh-field-label">{{ TYPE_ICONS[ty] }} {{ typeLabel(ty) }}（{{ reportItems(ty).length }}）</div>
            <div v-if="reportItems(ty).length" class="cast-report-list">
              <div v-for="(it, i) in reportItems(ty)" :key="i" class="cast-report-item">
                <span class="cast-report-name">{{ it.name || '—' }}</span>
                <span v-if="typeof it.frequency === 'number'" class="fh-pill fh-pill-muted fh-pill-mini">
                  {{ t('film.castFreq', { n: it.frequency }) }}
                </span>
                <span class="fh-muted fh-small cast-report-desc">{{ it.desc || '' }}</span>
              </div>
            </div>
            <div v-else class="fh-muted fh-small" style="padding-left: 2px">—</div>
          </div>
        </div>
      </div>
      <div v-else-if="!extracting" class="fh-empty">{{ t('film.castExtractEmpty') }}</div>
      <div v-if="extractError" class="fh-error-box">{{ extractError }}</div>

      <!-- 六类 Tab -->
      <div class="fh-tabs">
        <button
          v-for="ty in FILM_CAST_TYPES"
          :key="ty"
          class="fh-tab"
          :class="{ 'is-active': activeType === ty }"
          type="button"
          @click="switchType(ty)"
        >{{ TYPE_ICONS[ty] }} {{ typeLabel(ty) }}</button>
      </div>

      <!-- 两栏：左对象卡列表 | 右对象详情 -->
      <div class="fh-two-col">
        <section class="fh-card fh-col fh-col-side">
          <div class="fh-card-head">
            <span>{{ typeLabel(activeType) }}</span>
            <span class="fh-muted fh-small">{{ objects.length }}</span>
          </div>
          <div class="fh-card-body">
            <div v-if="objectsError" class="fh-error-box">{{ objectsError }}</div>
            <div v-if="objectsLoading" class="fh-empty">{{ t('film.loading') }}</div>
            <div v-else-if="!objects.length && !objectsError" class="fh-empty">
              {{ t('film.castObjectsEmpty') }}
            </div>
            <!-- 对象卡（信息 + 视图缩略横排 + 负责人徽章） -->
            <div
              v-for="o in objects"
              :key="o.name"
              class="cast-obj-card"
              :class="{ 'is-active': selectedName === o.name }"
              @click="selectObject(o.name)"
            >
              <div class="cast-obj-row">
                <span class="cast-obj-name" :title="o.name">{{ TYPE_ICONS[activeType] }} {{ o.name }}</span>
                <span v-if="readyViewCount(o)" class="fh-pill fh-pill-ok fh-pill-mini">
                  🖼 {{ readyViewCount(o) }}
                </span>
                <span v-if="activeType === 'characters' && o.voice" class="fh-pill fh-pill-violet fh-pill-mini">
                  🎙 {{ o.voice }}
                </span>
              </div>
              <div v-if="o.desc" class="fh-muted fh-small cast-obj-desc" :title="o.desc">{{ o.desc }}</div>
              <!-- 视图缩略横排 -->
              <div v-if="(o.views ?? []).length" class="cast-obj-thumbs">
                <div
                  v-for="(v, i) in (o.views ?? []).filter((x) => x.path || x.url)"
                  :key="i"
                  class="fh-thumb"
                  style="width: 36px; height: 36px; font-size: 15px"
                  :title="v.view"
                >
                  <img v-if="v.path && viewThumbs[v.path]" :src="viewThumbs[v.path]" :alt="v.view ?? ''">
                  <span v-else>🖼</span>
                </div>
              </div>
              <!-- 对象级认领徽章（多人按对象分工） -->
              <div class="cast-obj-row" style="margin-top: 2px">
                <span v-if="objOwner(o.name)" class="fh-owner">
                  👤 {{ t('film.ownOwner', { name: objOwner(o.name) }) }}
                </span>
                <template v-else>
                  <span class="fh-owner is-unclaimed">{{ t('film.ownUnclaimed') }}</span>
                  <button
                    v-if="ctx"
                    class="fh-btn fh-btn-mini"
                    type="button"
                    :title="t('film.ownClaimTip', { name: ctx.author.value })"
                    @click.stop="claimObject(o.name, ctx.author.value)"
                  >{{ t('film.ownClaim') }}</button>
                </template>
                <button
                  class="fh-btn fh-btn-mini fh-btn-danger"
                  style="margin-left: auto"
                  type="button"
                  @click.stop="removeObject(o)"
                >{{ t('film.del') }}</button>
              </div>
            </div>
          </div>
        </section>

        <!-- 对象详情：多视图槽位网格 + card.md 描述编辑 + voice -->
        <section class="fh-card fh-col">
          <div class="fh-card-head">
            <span v-if="selected">{{ selected.name }}</span>
            <span v-else class="fh-muted">{{ t('film.castDetailHint') }}</span>
            <span v-if="selected && activeType === 'characters'" class="fh-muted fh-small">{{ t('film.castCardVoiceNote') }}</span>
          </div>
          <div v-if="!selected" class="fh-card-body">
            <div class="fh-empty">{{ t('film.castDetailHint') }}</div>
          </div>
          <div v-else class="fh-card-body">
            <div v-if="detailError" class="fh-error-box">{{ detailError }}</div>

            <!-- 多视图网格（五槽位；空槽虚线 + AI 生成/导入双按钮） -->
            <div class="fh-view-grid">
              <div
                v-for="slot in CAST_VIEW_SLOTS"
                :key="slot"
                class="fh-view-slot"
                :class="{ 'is-filled': !!matchedView(slot) }"
              >
                <div class="fh-view-label">
                  <span>{{ viewLabel(slot) }}</span>
                  <span v-if="matchedView(slot)" class="fh-pill fh-pill-ok fh-pill-mini">✓</span>
                  <span v-else class="fh-pill fh-pill-muted fh-pill-mini">{{ t('film.castViewEmpty') }}</span>
                </div>
                <img
                  v-if="matchedView(slot)?.path && viewThumbs[matchedView(slot)!.path!]"
                  class="fh-view-img"
                  :src="viewThumbs[matchedView(slot)!.path!]"
                  :alt="viewLabel(slot)"
                >
                <div v-else class="fh-view-ph">{{ matchedView(slot) ? '🖼' : '＋' }}</div>
                <input
                  v-if="slot === 'custom'"
                  v-model="customViewName"
                  type="text"
                  class="fh-input"
                  :placeholder="t('film.castViewCustomPh')"
                >
                <div class="fh-view-actions">
                  <button
                    class="fh-btn fh-btn-mini"
                    type="button"
                    :disabled="!ctx?.hasOptionsFor('image') || ctx?.isOffline.value"
                    :title="t('film.castViewGenTip')"
                    @click="genView(slot)"
                  >✨ {{ t('film.castViewGen') }}</button>
                  <button
                    class="fh-btn fh-btn-mini"
                    type="button"
                    :disabled="importingSlot === slot"
                    @click="pickImport(slot)"
                  >{{ importingSlot === slot ? '…' : `⬆ ${t('film.castViewImport')}` }}</button>
                </div>
              </div>
            </div>

            <!-- 视图生成：image model_ref + 共享提示词 -->
            <div class="fh-field-row">
              <select
                v-if="ctx"
                v-model="ctx.modelSel.image"
                class="fh-select"
                :title="t('film.castViewGenModel')"
                style="flex: 1"
              >
                <option v-if="!ctx.hasOptionsFor('image')" value="" disabled>{{ t('film.noSource') }}</option>
                <optgroup v-for="g in ctx.optionsFor('image')" :key="g.label" :label="g.label">
                  <option v-for="o in g.options" :key="o.key" :value="o.key">{{ o.label }}{{ o.relay ? ' 🌐' : '' }}</option>
                </optgroup>
              </select>
              <input
                v-model="viewPrompt"
                type="text"
                class="fh-input"
                style="flex: 2"
                :placeholder="t('film.castViewPrompt')"
              >
            </div>

            <!-- card.md 描述编辑 -->
            <label class="fh-field">
              <span class="fh-field-label">{{ t('film.castCardDesc') }}</span>
              <textarea v-model="descDraft" rows="3" :disabled="descSaving" @input="markDescDirty" />
            </label>

            <!-- voice（人物类） -->
            <div v-if="activeType === 'characters'" class="fh-field">
              <span class="fh-field-label">{{ t('film.castVoice') }}</span>
              <div class="fh-field-row">
                <select v-model="voiceKind" class="fh-select" :disabled="descSaving">
                  <option value="enum">{{ t('film.charVoiceEnum') }}</option>
                  <option value="custom">{{ t('film.charVoiceCustom') }}</option>
                  <option value="none">{{ t('film.charVoiceNone') }}</option>
                </select>
                <select v-if="voiceKind === 'enum'" v-model="voiceEnum" class="fh-select" :disabled="descSaving">
                  <option v-for="v in OPENAI_VOICES" :key="v" :value="v">{{ v }}</option>
                </select>
                <input
                  v-if="voiceKind === 'custom'"
                  v-model="voiceCustom"
                  type="text"
                  class="fh-input"
                  :placeholder="t('film.charVoiceCustomPh')"
                  :disabled="descSaving"
                >
              </div>
            </div>

            <div class="cast-save-row">
              <button
                class="fh-btn fh-btn-primary fh-btn-small"
                type="button"
                :disabled="descSaving || !descDirty"
                @click="saveCard"
              >{{ descSaving ? t('film.saving') : t('film.castSaveCard') }}</button>
              <span v-if="descMsg" class="fh-small" :class="{ 'cast-msg-err': descMsg.startsWith(t('film.castSaveFailed')) }">{{ descMsg }}</span>
            </div>
          </div>
        </section>
      </div>
    </div>

    <input ref="viewInput" type="file" accept="image/png,image/jpeg,image/webp" class="fh-hidden-input" @change="onViewFile" />

    <!-- 新建对象弹窗 -->
    <div v-if="showCreate" class="fh-modal-backdrop" @click.self="showCreate = false">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-cast-new-title">
        <div class="fh-modal-head">
          <h3 id="film-cast-new-title">{{ t('film.castNewTitle', { type: typeLabel(activeType) }) }}</h3>
          <button class="fh-modal-close" type="button" @click="showCreate = false">×</button>
        </div>
        <div class="fh-modal-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('film.castName') }}</span>
            <input v-model="createForm.name" type="text" :placeholder="t('film.castNamePh')" :disabled="creating">
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('film.castDesc2') }}</span>
            <textarea v-model="createForm.desc" rows="3" :placeholder="t('film.castDescPh')" :disabled="creating" />
            <span class="fh-muted fh-small">{{ t('film.charDescHint') }}</span>
          </label>
          <div v-if="activeType === 'characters'" class="fh-field">
            <span class="fh-field-label">{{ t('film.castVoice') }}</span>
            <div class="fh-field-row">
              <select v-model="createForm.voiceKind" class="fh-select" :disabled="creating">
                <option value="enum">{{ t('film.charVoiceEnum') }}</option>
                <option value="custom">{{ t('film.charVoiceCustom') }}</option>
                <option value="none">{{ t('film.charVoiceNone') }}</option>
              </select>
              <select v-if="createForm.voiceKind === 'enum'" v-model="createForm.voiceEnum" class="fh-select" :disabled="creating">
                <option v-for="v in OPENAI_VOICES" :key="v" :value="v">{{ v }}</option>
              </select>
              <input
                v-if="createForm.voiceKind === 'custom'"
                v-model="createForm.voiceCustom"
                type="text"
                class="fh-input"
                :placeholder="t('film.charVoiceCustomPh')"
                :disabled="creating"
              >
            </div>
          </div>
          <div v-if="createError" class="fh-error-box">{{ createError }}</div>
          <div class="fh-form-actions">
            <button class="fh-btn" type="button" :disabled="creating" @click="showCreate = false">{{ t('film.cancel') }}</button>
            <button class="fh-btn fh-btn-primary" type="button" :disabled="creating" @click="submitCreate">
              {{ creating ? t('film.creating') : t('film.create') }}
            </button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cast-obj-card {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 10px);
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 5px;
  cursor: pointer;
  min-width: 0;
  transition: border-color 0.15s ease, background 0.15s ease;
}
.cast-obj-card:hover { border-color: var(--accent, #E95420); }
.cast-obj-card.is-active {
  border-color: var(--accent, #E95420);
  background: var(--accent-soft, rgba(233, 84, 32, 0.08));
}
.cast-obj-row { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; min-width: 0; }
.cast-obj-name { font-weight: 700; font-size: 13px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cast-obj-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  line-height: 1.45;
}
.cast-obj-thumbs { display: flex; gap: 5px; flex-wrap: wrap; }
.cast-save-row { display: flex; align-items: center; gap: 10px; }
.cast-msg-err { color: #b91c1c; }
.cast-report-group { display: flex; flex-direction: column; gap: 5px; }
.cast-report-list { display: flex; flex-direction: column; gap: 4px; }
.cast-report-item {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: 12.5px;
  padding: 4px 8px;
  border-radius: var(--radius-sm, 8px);
  background: var(--border-soft, #FAFAFA);
  min-width: 0;
}
.cast-report-name { font-weight: 600; flex-shrink: 0; }
.cast-report-desc {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
