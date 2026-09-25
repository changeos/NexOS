<script setup lang="ts">
// =============================================================================
// CastCustomizer.vue —— 定妆定制器（FilmHub v0.1.39；v0.1.41 增动作/运镜区）。
//
// 六类对象统一进定制器（类型决定模板与主槽名）：
//   · 中央大图 = 主视图槽最新版本产物（人物=front 正面全身 / 宠物=body 全身
//     立姿（植物对象=全株）/ 武器=full 完整形态 / 场景=pano 全景 / 排列=group
//     全员站位图 / 动作=pose 动作序列主帧）；空=虚线占位 + 生成按钮；
//   · 右侧部件面板 = 每部件槽一行（槽名 + 当前值 + 点击展开预设 chips +
//     「自定义」自由输入；改动标记 dirty）；
//   · 底部「按当前部件生成主视图」（image model_ref 走项目默认或选择器；
//     dirty 时先随生成一并 PUT parts）；
//   · 主槽历史缩略（views 里 front/front-v2… 多版本保留，新→旧）；
//   · 「更多视图」折叠区收编现有五槽位辅助视图机制（生成/导入并存）；
//   · 动作/运镜区（v0.1.41 P0，仅 actions 类）：①动作参考——视频参考
//     （views/motion.mp4）/骨架参考（views/motion-skeleton.png）两槽（导入+
//     预览 video/img+清除）+ 动作备注；②运镜——8 预设 chips + 自定义描述。
//     保存走 PUT casting/actions/:name {motion, camera}；导入文件由页面持
//     选择器（emit import-motion）。
//
// 生成提示词由后端组装（parts 拼进主槽 prompt——替换部件后只改对应键再生成，
// 其它部件保持一致）；面板底部提示词预览与后端同模板（composePromptPreview）。
// =============================================================================
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { PROJECT_DEFAULT_KEY, useFlow } from './flowContext'
import { matchCastView, type CastViewSlot } from './flowFiles'
import {
  CAMERA_PRESETS,
  MOTION_VIEW_NAMES,
  PART_TEMPLATES,
  auxViewSlots,
  composePromptPreview,
  latestMainView,
  mainSlotOf,
  mainViewVersions,
  normalizeCamera,
  normalizeMotion,
  normalizeParts,
  partSlotsFor,
  partsEqual,
  type CameraCfg,
  type CastPartSlot,
  type MotionCfg,
} from './castParts'
import type { FilmCastType, FilmCastingObject } from '../api'

const props = defineProps<{
  type: FilmCastType
  obj: FilmCastingObject
  /** path → data URL 缩略缓存（与定妆页共享；无 path 条目回落 url 直连）。 */
  thumbs: Record<string, string>
  /** 生成按钮 busy（任务已提交等待终态）。 */
  generating?: boolean
  /** 部件保存 busy。 */
  partsSaving?: boolean
  /** 动作/运镜保存 busy（v0.1.41 P0）。 */
  motionSaving?: boolean
}>()

const emit = defineEmits<{
  /** 保存部件（整表替换；PUT casting/:type/:name {parts}）。 */
  (e: 'save-parts', parts: Record<string, string>): void
  /** 按当前部件生成主视图（携带当前 draft——页面 dirty 时先 PUT 再 POST）。 */
  (e: 'generate-main', parts: Record<string, string>): void
  /** Blender 渲染主视图（v0.1.44 spike，仅 scenes；chat 模型产 bpy 脚本
   *  → 本机 headless 渲染；prompt 可选——缺省由后端按 card 组装）。 */
  (e: 'generate-blender', payload: { parts: Record<string, string>; prompt?: string }): void
  /** 辅助视图生成（「更多视图」折叠区；prompt 可选覆写）。 */
  (e: 'generate-aux', payload: { view: string; prompt?: string }): void
  /** 视图导入（槽位视图名已解析；页面持文件选择器）。 */
  (e: 'import-view', view: string): void
  /** 动作参考导入（v0.1.41 P0；kind 决定槽位与文件类型，页面持选择器）。 */
  (e: 'import-motion', kind: 'video' | 'skeleton'): void
  /** 保存动作参考/运镜（v0.1.41 P0；PUT {motion, camera}——null=清空）。 */
  (e: 'save-motion-camera', payload: { motion: MotionCfg | null; camera: CameraCfg | null }): void
}>()

const { t } = useI18n()
const ctx = useFlow()

// —— 部件 draft（dirty = 与对象已存 parts 不一致）——
const partsDraft = ref<Record<string, string>>({})
const expandedKey = ref('')
const customEditing = ref('')
const customDraft = ref('')

/** 对象已存 parts（宽容归一）。 */
const savedParts = computed<Record<string, string>>(() => normalizeParts(props.obj.parts))

watch(
  [() => props.obj, () => props.type],
  () => {
    partsDraft.value = { ...savedParts.value }
    expandedKey.value = ''
    customEditing.value = ''
    customDraft.value = ''
  },
  { immediate: true },
)

/** 当前生效部件槽集（pets 按形态切动物/植物槽集）。 */
const activeSlots = computed<CastPartSlot[]>(() => partSlotsFor(props.type, partsDraft.value))

const dirty = computed<boolean>(() => !partsEqual(partsDraft.value, savedParts.value))

/** 槽当前值（空=「未选」）。 */
function slotValue(key: string): string {
  return partsDraft.value[key] ?? ''
}

/** 当前值是否不在预设内（=自定义态）。 */
function isCustomValue(slot: CastPartSlot): boolean {
  const v = slotValue(slot.key)
  return v !== '' && !slot.presets.includes(v)
}

/** 选预设（再点同值=取消选中）；形态切换时清掉新槽集外的旧键。 */
function pickPart(slot: CastPartSlot, val: string): void {
  if (slotValue(slot.key) === val) {
    delete partsDraft.value[slot.key]
  } else {
    partsDraft.value[slot.key] = val
    if (slot.key === '形态' && props.type === 'pets') {
      const keep = new Set(partSlotsFor(props.type, partsDraft.value).map((s) => s.key))
      for (const k of Object.keys(partsDraft.value)) {
        if (!keep.has(k)) delete partsDraft.value[k]
      }
    }
  }
}

function toggleSlot(key: string): void {
  expandedKey.value = expandedKey.value === key ? '' : key
  customEditing.value = ''
  customDraft.value = ''
}

/** 打开自定义输入（预填当前自定义值）。 */
function startCustom(slot: CastPartSlot): void {
  customEditing.value = slot.key
  customDraft.value = isCustomValue(slot) ? slotValue(slot.key) : ''
}

/** 提交自定义值（空=取消选中）。 */
function commitCustom(slot: CastPartSlot): void {
  const v = customDraft.value.trim()
  if (v) partsDraft.value[slot.key] = v
  else delete partsDraft.value[slot.key]
  customEditing.value = ''
}

// —— 中央主视图（最新版本）与历史 ——
const mainSlot = computed<string>(() => mainSlotOf(props.type))
const mainLatest = computed(() => latestMainView(props.obj, props.type))
const mainVersions = computed(() => mainViewVersions(props.obj, props.type))

/** 视图条目 → img src（path/url 缩略缓存键与定妆页 loadThumb 同口径）。 */
function viewSrc(v: { path?: string | null; url?: string | null } | null): string {
  if (!v) return ''
  const key = v.path || v.url || ''
  return key ? props.thumbs[key] ?? '' : ''
}

/** 主槽显示名（五槽位沿用既有 i18n；新主槽另有键）。 */
function mainSlotLabel(): string {
  const map: Record<string, string> = {
    front: t('film.castViewFront'),
    side: t('film.castViewSide'),
    back: t('film.castViewBack'),
    action: t('film.castViewAction'),
    body: t('film.castViewBody'),
    full: t('film.castViewFull'),
    pano: t('film.castViewPano'),
    group: t('film.castViewGroup'),
    pose: t('film.castViewPose'),
  }
  return map[mainSlot.value] ?? mainSlot.value
}

/** 类型主视图描述（pets 按形态切全身立姿/全株）。 */
function mainDescLabel(): string {
  if (props.type === 'pets') {
    return partsDraft.value['形态'] === '植物'
      ? t('film.castCzMainDescPetsPlant')
      : t('film.castCzMainDescPetsAnimal')
  }
  const map: Record<string, string> = {
    characters: t('film.castCzMainDescCharacters'),
    props: t('film.castCzMainDescProps'),
    scenes: t('film.castCzMainDescScenes'),
    formations: t('film.castCzMainDescFormations'),
    actions: t('film.castCzMainDescActions'),
  }
  return map[props.type] ?? ''
}

/** 组合提示词预览（与后端 composed_view_prompt 同模板）。 */
const promptPreview = computed<string>(() =>
  composePromptPreview(props.type, props.obj.desc ?? '', mainSlot.value, partsDraft.value),
)

// —— 生成 / 保存动作（HTTP 与重载在定妆页）——
function saveParts(): void {
  emit('save-parts', { ...partsDraft.value })
}

function generateMain(): void {
  emit('generate-main', { ...partsDraft.value })
}

// —— 生成源选择（v0.1.44 spike，仅 scenes：AI 生图[既有] / Blender 渲染）——
const genSource = ref<'image' | 'blender'>('image')
/** Blender 渲染场景提示（可选；缺省后端按 card desc/parts 组装）。 */
const blenderPrompt = ref('')

/** 场景类才显示源选择（人物/道具等其余五类暂只 AI 生图）。 */
const sourceSelectable = computed<boolean>(() => props.type === 'scenes')

/** 当前源生成按钮是否就绪（image 位 / chat 位模型选择）。 */
function genReady(): boolean {
  if (!ctx) return false
  return ctx.modelSelReady(genSource.value === 'blender' ? 'chat' : 'image')
}

function generateMainRouted(): void {
  if (genSource.value === 'blender') {
    emit('generate-blender', {
      parts: { ...partsDraft.value },
      ...(blenderPrompt.value.trim() ? { prompt: blenderPrompt.value.trim() } : {}),
    })
  } else {
    generateMain()
  }
}

// —— 「更多视图」折叠区（现有五槽位辅助机制；排除主槽）——
const moreOpen = ref(false)
/** custom 槽自定义视图名。 */
const customViewName = ref('custom')
/** 辅助视图生成提示词（可选覆写；主视图生成恒走后端 parts 组合，不吃此值）。 */
const auxPrompt = ref('')

const auxSlots = computed<CastViewSlot[]>(() => auxViewSlots(props.type))

function matchedAux(slot: CastViewSlot) {
  return matchCastView(props.obj, slot)
}

function auxViewLabel(slot: CastViewSlot): string {
  switch (slot) {
    case 'front': return t('film.castViewFront')
    case 'side': return t('film.castViewSide')
    case 'back': return t('film.castViewBack')
    case 'action': return t('film.castViewAction')
    case 'custom': return t('film.castViewCustom')
  }
}

/** 槽位视图名（custom 槽用输入值）。 */
function auxViewName(slot: CastViewSlot): string {
  return slot === 'custom' ? customViewName.value.trim() || 'custom' : slot
}

function genAux(slot: CastViewSlot): void {
  emit('generate-aux', {
    view: auxViewName(slot),
    ...(auxPrompt.value.trim() ? { prompt: auxPrompt.value.trim() } : {}),
  })
}

function importAux(slot: CastViewSlot): void {
  emit('import-view', auxViewName(slot))
}

// pets 模板两套槽集提示（形态切换后槽集换血）
const hasAltSlots = computed<boolean>(() => !!PART_TEMPLATES[props.type].altSlots)

// —— 动作参考 / 运镜区（v0.1.41 P0，仅 actions 类）——

/** 对象已存 motion/camera（宽容归一；旧后端/损坏卡 → null）。 */
const savedMotion = computed<MotionCfg | null>(() => normalizeMotion(props.obj.motion))
const savedCamera = computed<CameraCfg | null>(() => normalizeCamera(props.obj.camera))

/** 动作参考槽 draft（''=未选=保存时清空 motion）。 */
const motionSlot = ref<'' | 'video' | 'skeleton'>('')
/** 动作备注 draft。 */
const motionNote = ref('')
/** 运镜预设 draft（''=未选）。 */
const camPreset = ref('')
/** 运镜自定义描述 draft。 */
const camDescribe = ref('')

watch(
  [() => props.obj, () => props.type],
  () => {
    motionSlot.value = savedMotion.value?.kind ?? ''
    motionNote.value = savedMotion.value?.note ?? ''
    camPreset.value = savedCamera.value?.kind === 'enum' ? savedCamera.value.preset ?? '' : ''
    camDescribe.value = savedCamera.value?.kind === 'describe' ? savedCamera.value.describe ?? '' : ''
  },
  { immediate: true },
)

/** 保存态的 camera（draft 派生口径一致：chip 优先，其次非空描述，否则 null）。 */
function draftCamera(): CameraCfg | null {
  if (camPreset.value) return { kind: 'enum', preset: camPreset.value }
  const d = camDescribe.value.trim()
  if (d) return { kind: 'describe', describe: d }
  return null
}

/** 保存态的 motion（槽 draft → asset：沿用已存同槽 asset，否则视图名缺省）。 */
function draftMotion(): MotionCfg | null {
  const kind = motionSlot.value
  if (kind !== 'video' && kind !== 'skeleton') return null
  const asset =
    savedMotion.value?.kind === kind
      ? savedMotion.value.asset
      : `views/${MOTION_VIEW_NAMES[kind]}.${kind === 'video' ? 'mp4' : 'png'}`
  return { kind, asset, note: motionNote.value.trim() }
}

/** 动作/运镜 dirty（保存按钮使能）。 */
const motionDirty = computed<boolean>(() => {
  const dm = draftMotion()
  const dc = draftCamera()
  const sameM =
    (dm === null && savedMotion.value === null) ||
    (dm !== null &&
      savedMotion.value !== null &&
      dm.kind === savedMotion.value.kind &&
      dm.asset === savedMotion.value.asset &&
      (dm.note ?? '') === (savedMotion.value.note ?? ''))
  const sameC =
    (dc === null && savedCamera.value === null) ||
    (dc !== null &&
      savedCamera.value !== null &&
      dc.kind === savedCamera.value.kind &&
      (dc.preset ?? '') === (savedCamera.value.preset ?? '') &&
      (dc.describe ?? '') === (savedCamera.value.describe ?? ''))
  return !(sameM && sameC)
})

/** 槽位视图条目（motion / motion-skeleton 视图名匹配；null=该槽无文件）。 */
function motionViewEntry(kind: 'video' | 'skeleton') {
  const want = MOTION_VIEW_NAMES[kind]
  return (props.obj.views ?? []).find((v) => (v?.view ?? '') === want) ?? null
}

/** 槽位预览 src（复用页面缩略缓存；video 槽给 <video>、skeleton 槽给 <img>）。 */
function motionSrc(kind: 'video' | 'skeleton'): string {
  const v = motionViewEntry(kind)
  const key = v?.path || v?.url || ''
  return key ? props.thumbs[key] ?? '' : ''
}

function pickMotionSlot(kind: 'video' | 'skeleton'): void {
  motionSlot.value = motionSlot.value === kind ? '' : kind
}

function pickCamPreset(p: string): void {
  camPreset.value = camPreset.value === p ? '' : p
}

function saveMotionCamera(): void {
  emit('save-motion-camera', { motion: draftMotion(), camera: draftCamera() })
}
</script>

<template>
  <div class="cast-cz">
    <!-- 中央主体完整视图（主槽最新版本；空=虚线占位 + 生成按钮） -->
    <div class="cast-cz-main">
      <div class="cast-cz-stage" :class="{ 'is-filled': !!viewSrc(mainLatest) }">
        <img
          v-if="viewSrc(mainLatest)"
          class="cast-cz-img"
          :src="viewSrc(mainLatest)"
          :alt="mainSlotLabel()"
        >
        <div v-else class="cast-cz-ph">
          <span class="cast-cz-ph-plus">＋</span>
          <span class="fh-muted fh-small">{{ t('film.castCzMainEmpty') }}</span>
          <button
            class="fh-btn fh-btn-primary fh-btn-small"
            type="button"
            :disabled="!ctx?.modelSelReady('image') || ctx?.isOffline.value || generating"
            @click="generateMain"
          >✨ {{ t('film.castCzMainGen') }}</button>
        </div>
      </div>
      <div class="cast-cz-caption">
        <span class="cast-cz-caption-main">{{ t('film.castCzMainView') }} · {{ mainSlotLabel() }}</span>
        <span class="fh-muted fh-small">{{ mainDescLabel() }}</span>
        <span
          v-if="mainVersions.length > 1"
          class="fh-pill fh-pill-muted fh-pill-mini"
        >{{ t('film.castCzHistoryCount', { n: mainVersions.length }) }}</span>
      </div>
      <!-- 主槽历史缩略（多版本保留，新→旧；title=版本视图名） -->
      <div v-if="mainVersions.length" class="cast-cz-history">
        <span class="fh-muted fh-small">{{ t('film.castCzHistory') }}</span>
        <div
          v-for="(v, i) in mainVersions"
          :key="v.view ?? i"
          class="fh-thumb cast-cz-history-thumb"
          :class="{ 'is-latest': v === mainLatest }"
          :title="v.view ?? ''"
        >
          <img v-if="viewSrc(v)" :src="viewSrc(v)" :alt="v.view ?? ''">
          <span v-else>🖼</span>
        </div>
      </div>
    </div>

    <!-- 右侧部件面板（每槽一行：槽名 + 当前值 + 展开预设 chips/自定义） -->
    <aside class="cast-cz-parts">
      <div class="cast-cz-parts-head">
        <span class="cast-cz-parts-title">{{ t('film.castCzPartsTitle') }}</span>
        <span v-if="dirty" class="fh-pill fh-pill-amber fh-pill-mini">{{ t('film.castCzPartsDirty') }}</span>
        <button
          class="fh-btn fh-btn-mini"
          style="margin-left: auto"
          type="button"
          :disabled="!dirty || partsSaving"
          @click="saveParts"
        >{{ partsSaving ? t('film.saving') : t('film.castCzSaveParts') }}</button>
      </div>
      <p class="fh-muted fh-small cast-cz-parts-hint">
        {{ hasAltSlots ? t('film.castCzPartsHintAlt') : t('film.castCzPartsHint') }}
      </p>
      <div
        v-for="slot in activeSlots"
        :key="slot.key"
        class="cast-part-row"
        :class="{ 'is-dirty': dirty && slotValue(slot.key) !== (savedParts[slot.key] ?? '') }"
      >
        <button class="cast-part-head" type="button" @click="toggleSlot(slot.key)">
          <span class="cast-part-name">{{ slot.key }}</span>
          <span class="cast-part-value" :class="{ 'is-empty': !slotValue(slot.key) }">
            {{ slotValue(slot.key) || t('film.castViewEmpty') }}
          </span>
          <span class="cast-part-caret">{{ expandedKey === slot.key ? '▾' : '▸' }}</span>
        </button>
        <div v-if="expandedKey === slot.key" class="cast-part-opts">
          <button
            v-for="p in slot.presets"
            :key="p"
            class="cast-part-chip"
            :class="{ 'is-active': slotValue(slot.key) === p }"
            type="button"
            @click="pickPart(slot, p)"
          >{{ p }}</button>
          <button
            class="cast-part-chip"
            :class="{ 'is-active': isCustomValue(slot) && customEditing !== slot.key }"
            type="button"
            @click="startCustom(slot)"
          >{{ isCustomValue(slot) ? slotValue(slot.key) : t('film.castCzPartsCustom') }} ✎</button>
          <div v-if="customEditing === slot.key" class="cast-part-custom">
            <input
              v-model="customDraft"
              type="text"
              class="fh-input"
              :placeholder="t('film.castCzPartsCustomPh')"
              @keyup.enter="commitCustom(slot)"
            >
            <button class="fh-btn fh-btn-mini" type="button" @click="commitCustom(slot)">
              {{ t('film.castCzCustomOk') }}
            </button>
          </div>
        </div>
      </div>
      <div class="cast-cz-preview">
        <span class="fh-muted fh-small">{{ t('film.castCzPromptPreview') }}</span>
        <p class="cast-cz-preview-text">{{ promptPreview }}</p>
      </div>
    </aside>

    <!-- 底部：生成源选择（v0.1.44 spike，scenes）+ 模型选择器 + 生成主视图 -->
    <div class="cast-cz-footer">
      <select
        v-if="ctx && sourceSelectable"
        v-model="genSource"
        class="fh-select cast-cz-source"
        :title="t('film.castGenSource')"
      >
        <option value="image">🖼 {{ t('film.castGenSourceImage') }}</option>
        <option value="blender">🧊 {{ t('film.castGenSourceBlender') }}</option>
      </select>
      <select
        v-if="ctx && genSource === 'image'"
        v-model="ctx.modelSel.image"
        class="fh-select"
        :title="t('film.castViewGenModel')"
        style="flex: 1"
      >
        <option :value="PROJECT_DEFAULT_KEY">
          🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('image') ? ' · ' + ctx.defaultModelSummary('image') : '' }}
        </option>
        <option v-if="!ctx.hasOptionsFor('image')" value="" disabled>{{ t('film.noSource') }}</option>
        <optgroup v-for="g in ctx.optionsFor('image')" :key="g.label" :label="g.label">
          <option v-for="o in g.options" :key="o.key" :value="o.key">{{ o.label }}{{ o.relay ? ' 🌐' : '' }}</option>
        </optgroup>
      </select>
      <!-- Blender 源：chat 位产 bpy 脚本（模型选择器）+ 场景提示（示例占位） -->
      <template v-if="genSource === 'blender'">
        <select
          v-if="ctx"
          v-model="ctx.modelSel.chat"
          class="fh-select"
          :title="t('film.castBlenderChatModel')"
          style="flex: 1"
        >
          <option :value="PROJECT_DEFAULT_KEY">
            🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('chat') ? ' · ' + ctx.defaultModelSummary('chat') : '' }}
          </option>
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>{{ t('film.noSource') }}</option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">{{ o.label }}{{ o.relay ? ' 🌐' : '' }}</option>
          </optgroup>
        </select>
        <input
          v-model="blenderPrompt"
          type="text"
          class="fh-input cast-cz-blender-prompt"
          :placeholder="t('film.castBlenderPromptPh')"
          :title="t('film.castBlenderHint')"
        >
      </template>
      <button
        class="fh-btn fh-btn-primary fh-btn-small"
        type="button"
        :disabled="!genReady() || ctx?.isOffline.value || generating"
        :title="genSource === 'blender' ? t('film.castBlenderHint') : dirty ? t('film.castCzGenNote') : t('film.castViewGenTip')"
        @click="generateMainRouted"
      >{{ genSource === 'blender' ? '🧊' : '✨' }} {{ generating ? t('film.taskRunning') + '…' : genSource === 'blender' ? t('film.castCzGenBlender') : t('film.castCzGenMain') }}</button>
      <span v-if="dirty && genSource === 'image'" class="fh-muted fh-small">{{ t('film.castCzGenNote') }}</span>
    </div>

    <!-- 动作参考 / 运镜区（v0.1.41 P0，仅 actions 类）：视频参考/骨架参考两槽
         （导入+预览 video/img+清除）+ 动作备注；运镜 8 预设 chips + 自定义描述 -->
    <div v-if="type === 'actions'" class="cast-cz-motion">
      <div class="cast-cz-sec-title">🎞 {{ t('film.castMotionSection') }}</div>
      <div class="cast-motion-slots">
        <div
          v-for="kind in (['video', 'skeleton'] as const)"
          :key="kind"
          class="cast-motion-slot"
          :class="{ 'is-active': motionSlot === kind }"
          role="button"
          tabindex="0"
          @click="pickMotionSlot(kind)"
          @keydown.enter="pickMotionSlot(kind)"
        >
          <div class="cast-motion-slot-head">
            <span class="cast-motion-slot-name">
              {{ kind === 'video' ? t('film.castMotionVideoSlot') : t('film.castMotionSkeletonSlot') }}
            </span>
            <span
              v-if="motionSlot === kind"
              class="fh-pill fh-pill-ok fh-pill-mini"
            >✓</span>
          </div>
          <video
            v-if="kind === 'video' && motionSrc(kind)"
            class="cast-motion-preview cast-motion-preview-video"
            :src="motionSrc(kind)"
            controls
            muted
          ></video>
          <img
            v-else-if="kind === 'skeleton' && motionSrc(kind)"
            class="cast-motion-preview"
            :src="motionSrc(kind)"
            :alt="t('film.castMotionSkeletonSlot')"
          >
          <div v-else class="cast-motion-ph">{{ motionViewEntry(kind) ? '🎞' : '＋' }}</div>
          <div class="cast-motion-actions">
            <button
              class="fh-btn fh-btn-mini"
              type="button"
              @click.stop="emit('import-motion', kind)"
            >⬆ {{ t('film.castMotionImport') }}</button>
            <button
              v-if="motionSlot === kind"
              class="fh-btn fh-btn-mini"
              type="button"
              @click.stop="motionSlot = ''"
            >{{ t('film.castMotionClear') }}</button>
          </div>
        </div>
      </div>
      <label class="cast-motion-note">
        <span class="fh-muted fh-small">{{ t('film.castMotionNote') }}</span>
        <input
          v-model="motionNote"
          type="text"
          class="fh-input"
          :placeholder="t('film.castMotionNotePh')"
        >
      </label>
      <p class="fh-muted fh-small cast-motion-hint">{{ t('film.castMotionHint') }}</p>

      <div class="cast-cz-sec-title">🎥 {{ t('film.castCameraSection') }}</div>
      <div class="cast-camera-chips">
        <button
          v-for="p in CAMERA_PRESETS"
          :key="p"
          class="cast-part-chip"
          :class="{ 'is-active': camPreset === p }"
          type="button"
          @click="pickCamPreset(p)"
        >{{ p }}</button>
      </div>
      <label class="cast-motion-note">
        <span class="fh-muted fh-small">{{ t('film.castCameraDescribe') }}</span>
        <input
          v-model="camDescribe"
          type="text"
          class="fh-input"
          :placeholder="t('film.castCameraDescribePh')"
          :disabled="!!camPreset"
        >
      </label>
      <p class="fh-muted fh-small cast-motion-hint">{{ t('film.castCameraHint') }}</p>

      <div class="cast-save-row">
        <button
          class="fh-btn fh-btn-primary fh-btn-small"
          type="button"
          :disabled="!motionDirty || motionSaving"
          @click="saveMotionCamera"
        >{{ motionSaving ? t('film.saving') : t('film.castMotionSave') }}</button>
      </div>
    </div>

    <!-- 更多视图（现有五槽位辅助机制收进折叠区；排除已被中央占用的主槽） -->
    <div class="cast-cz-more">
      <button
        class="cast-cz-more-toggle"
        type="button"
        @click="moreOpen = !moreOpen"
      >{{ t('film.castCzMoreViews') }} {{ moreOpen ? '▾' : '▸' }}</button>
      <template v-if="moreOpen">
        <div class="fh-view-grid">
          <div
            v-for="slot in auxSlots"
            :key="slot"
            class="fh-view-slot"
            :class="{ 'is-filled': !!matchedAux(slot) }"
          >
            <div class="fh-view-label">
              <span>{{ auxViewLabel(slot) }}</span>
              <span v-if="matchedAux(slot)" class="fh-pill fh-pill-ok fh-pill-mini">✓</span>
              <span v-else class="fh-pill fh-pill-muted fh-pill-mini">{{ t('film.castViewEmpty') }}</span>
            </div>
            <img
              v-if="matchedAux(slot) && viewSrc(matchedAux(slot))"
              class="fh-view-img"
              :src="viewSrc(matchedAux(slot))"
              :alt="auxViewLabel(slot)"
            >
            <div v-else class="fh-view-ph">{{ matchedAux(slot) ? '🖼' : '＋' }}</div>
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
                :disabled="!ctx?.modelSelReady('image') || ctx?.isOffline.value"
                :title="t('film.castViewGenTip')"
                @click="genAux(slot)"
              >✨ {{ t('film.castViewGen') }}</button>
              <button class="fh-btn fh-btn-mini" type="button" @click="importAux(slot)">
                ⬆ {{ t('film.castViewImport') }}
              </button>
            </div>
          </div>
        </div>
        <input
          v-model="auxPrompt"
          type="text"
          class="fh-input cast-cz-aux-prompt"
          :placeholder="t('film.castViewPrompt')"
        >
      </template>
    </div>
  </div>
</template>

<style scoped>
.cast-cz {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 300px;
  grid-template-areas:
    'main parts'
    'footer parts'
    'motion motion'
    'more more';
  gap: 12px;
}
.cast-cz-main { grid-area: main; min-width: 0; }
.cast-cz-parts { grid-area: parts; min-width: 0; }
.cast-cz-footer { grid-area: footer; }
.cast-cz-motion { grid-area: motion; min-width: 0; }
.cast-cz-more { grid-area: more; }

.cast-cz-stage {
  border: 2px dashed var(--nx-line-strong);
  border-radius: var(--nx-radius-card);
  background: var(--nx-bg-card);
  min-height: 300px;
  display: flex;
  align-items: stretch;
  justify-content: center;
  overflow: hidden;
}
.cast-cz-stage.is-filled { border-style: solid; border-color: var(--nx-line-regular); }
.cast-cz-img {
  width: 100%;
  max-height: 460px;
  object-fit: contain;
  border-radius: var(--nx-radius-card);
  background: var(--nx-bg-subtle);
}
.cast-cz-ph {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  padding: 24px;
}
.cast-cz-ph-plus { font-size: 34px; opacity: 0.4; color: var(--nx-text-tertiary); }
.cast-cz-caption {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  margin-top: 8px;
}
.cast-cz-caption-main {
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
}
.cast-cz-history {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  margin-top: 8px;
}
.cast-cz-history-thumb { width: 40px; height: 40px; font-size: 16px; }
.cast-cz-history-thumb.is-latest { border-color: var(--nx-accent); }

.cast-cz-parts {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  padding: 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  background: var(--nx-bg-card);
  box-shadow: var(--nx-shadow-xs);
}
.cast-cz-parts-head { display: flex; align-items: center; gap: 6px; }
.cast-cz-parts-title {
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
}
.cast-cz-parts-hint { margin: 0; line-height: 1.5; }
.cast-part-row {
  border: 0.5px solid var(--nx-line-divider);
  border-radius: var(--nx-radius-input);
  overflow: hidden;
}
/* 部件行 dirty：warning 浅底刻面 */
.cast-part-row.is-dirty {
  border-color: var(--nx-warning-border);
  background: var(--nx-warning-bg);
}
.cast-part-head {
  width: 100%;
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 7px 9px;
  background: transparent;
  border: none;
  cursor: pointer;
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  text-align: left;
}
.cast-part-head:hover { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.cast-part-head:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: -2px;
}
.cast-part-name {
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-primary);
  flex-shrink: 0;
}
.cast-part-value {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--nx-text-primary);
}
.cast-part-value.is-empty { color: var(--nx-text-tertiary); }
.cast-part-caret { color: var(--nx-text-tertiary); flex-shrink: 0; }
.cast-part-opts {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
  padding: 6px 9px 9px;
  border-top: 1px dashed var(--nx-line-divider);
}
/* 部件预设 chip：药丸 + accent 选中档（badge 圆角刻面） */
.cast-part-chip {
  border: 1px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-badge);
  background: var(--nx-bg-card);
  color: var(--nx-text-secondary);
  padding: 3px 11px;
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    background 0.15s ease,
    color 0.15s ease;
}
.cast-part-chip:hover {
  border-color: var(--nx-line-strong);
  background: var(--nx-bg-hover);
  color: var(--nx-text-primary);
}
.cast-part-chip.is-active {
  border-color: var(--nx-accent-border);
  background: var(--nx-accent-soft);
  color: var(--nx-accent);
  font-weight: var(--nx-font-weight-semibold);
}
.cast-part-chip:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.cast-part-custom { display: flex; gap: 6px; width: 100%; }
.cast-cz-preview {
  border-top: 1px dashed var(--nx-line-divider);
  padding-top: 8px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.cast-cz-preview-text {
  margin: 0;
  font-size: 11px;
  line-height: 1.6;
  color: var(--nx-text-tertiary);
  word-break: break-all;
  max-height: 84px;
  overflow-y: auto;
}

.cast-cz-footer { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
/* 生成源选择（v0.1.44 spike，scenes）：窄列固定宽，模型选择器占余量 */
.cast-cz-source { flex: 0 0 auto; min-width: 128px; }
.cast-cz-blender-prompt { flex: 1 1 220px; min-width: 180px; }

/* —— 动作参考/运镜区（v0.1.41 P0）：区卡 + 两槽 + chips + 备注/描述 —— */
.cast-cz-motion {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  background: var(--nx-bg-card);
  box-shadow: var(--nx-shadow-xs);
}
.cast-cz-sec-title {
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
}
.cast-motion-slots {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  gap: 10px;
}
.cast-motion-slot {
  border: 1px dashed var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
  padding: 8px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  cursor: pointer;
  min-width: 0;
  transition:
    border-color 0.15s ease,
    background 0.15s ease;
}
.cast-motion-slot:hover { border-color: var(--nx-accent-border); }
.cast-motion-slot.is-active {
  border-style: solid;
  border-color: var(--nx-accent);
  background: var(--nx-accent-soft);
}
.cast-motion-slot:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.cast-motion-slot-head { display: flex; align-items: center; gap: 6px; }
.cast-motion-slot-name {
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
}
.cast-motion-preview {
  width: 100%;
  max-height: 180px;
  object-fit: contain;
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
}
.cast-motion-preview-video { display: block; }
.cast-motion-ph {
  height: 90px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 26px;
  opacity: 0.45;
  color: var(--nx-text-tertiary);
  background: var(--nx-bg-subtle);
  border-radius: var(--nx-radius-input);
}
.cast-motion-actions { display: flex; gap: 6px; }
.cast-motion-note { display: flex; flex-direction: column; gap: 4px; }
.cast-motion-hint { margin: 0; line-height: 1.5; }
.cast-camera-chips { display: flex; gap: 6px; flex-wrap: wrap; }
.cast-cz-more-toggle {
  background: transparent;
  border: none;
  padding: 2px 0;
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-tertiary);
  cursor: pointer;
}
.cast-cz-more-toggle:hover { color: var(--nx-accent); }
.cast-cz-more-toggle:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 2px; border-radius: 4px; }
.cast-cz-aux-prompt { width: 100%; }

@media (max-width: 1080px) {
  .cast-cz {
    grid-template-columns: 1fr;
    grid-template-areas:
      'main'
      'parts'
      'footer'
      'motion'
      'more';
  }
}
</style>
