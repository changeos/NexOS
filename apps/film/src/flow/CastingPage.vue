<script setup lang="ts">
// =============================================================================
// CastingPage.vue —— 定妆页（流程 ③，FilmHub v0.1.39 定制器形态）。
//
// 顶部「AI 提取定妆对象」（chat model_ref → casting/extract；202 任务或 200
// 直出报告）→ extraction.json 报告展示（六类分组 name/desc/frequency，可折叠）。
// 六类 Tab（人物/武器/宠物/排列/动作/场景）——每类对象卡列表（信息 + 视图缩略
// 横排 + 负责人徽章：未认领=「待认领」+认领按钮，多人按对象分工）。
// 对象详情 = **定妆定制器**（CastCustomizer）：中央主体完整视图（主槽——人物
// 正面全身/宠物全身立姿（植物=全株）/武器完整形态/场景全景/排列站位图/动作
// 主帧）+ 右侧部件替换面板（按类型模板：预设 chips + 自定义）+ 底部「按当前
// 部件生成主视图」+ 主槽历史缩略（front-v2… 多版本保留）+「更多视图」折叠区
// （辅助视图生成/导入）。parts 存 card.md（PUT casting/:type/:name）；组合
// 提示词由后端组装（部件替换后只改对应键再生成=其它部件保持一致）。
// card.md 描述编辑 + voice（人物类）保留在定制器下方。新建对象表单
// （创建人自动成为对象 owner）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  FILM_CAST_TYPES,
  OPENAI_VOICES,
  b64ToText,
  fetchFileDataUrl,
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
import CastCustomizer from './CastCustomizer.vue'
import FlowPageHead from './FlowPageHead.vue'
import NxButton from '../nx/NxButton.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
import { PROJECT_DEFAULT_KEY, useFlow } from './flowContext'
import { mainSlotOf, normalizeMotion, normalizeParts, partsEqual, type CameraCfg, type MotionCfg } from './castParts'
import { readyViewCount } from './flowFiles'
// v0.1.11 全局操作反馈：保存/删除/导入/提交类成败 toast（任务类走通知闭环）
import { toast } from '../nx/toast'

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

// —— 视图缩略（path/url → data URL 懒加载缓存；path=hub 相对路径走 files 信封，
//    url=后端回传 files/download 绝对路径走 fetchFileDataUrl）——
const viewThumbs = ref<Record<string, string>>({})
async function loadThumb(
  path: string | null,
  url: string | null,
  key: string,
): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || viewThumbs.value[key]) return
  try {
    viewThumbs.value[key] = path
      ? await filmFileDataUrl(pid, path)
      : await fetchFileDataUrl(url ?? '')
  } catch {
    /* 缩略失败保持无图 */
  }
}
/** 视图条目的缩略缓存键（path 优先；后端 GET 列表回传 url 不回传 path）。 */
function thumbKey(v: { path?: string | null; url?: string | null }): string {
  return v.path || v.url || ''
}
function ensureThumbs(): void {
  for (const o of objects.value) {
    for (const v of o.views ?? []) {
      const key = thumbKey(v)
      if (key) void loadThumb(v.path ?? null, v.url ?? null, key)
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
  const useDefault = ctx.isProjectDefaultSel('chat')
  const ref = ctx.modelRefFor('chat')
  if (!useDefault && !ref) {
    extractError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  extracting.value = true
  extractError.value = ''
  try {
    const res = await filmCastingExtract(ctx.project.value!.id, ref ?? undefined, ctx.author.value)
    if (isFilmTask(res)) {
      // 任务中心登记（v0.1.6：stage 显式传 casting；终态 → refreshTick → loadExtraction）
      ctx.trackFilmTask(res.id, 'casting')
    } else {
      extraction.value = res
      reportOpen.value = true
      await loadObjects()
      // v0.1.11：同步 200 直出报告（非任务路径无终态通知——这里即时 toast）
      toast.success(t('toast.reportReady'))
    }
  } catch (e) {
    extractError.value = t('film.castExtractFailed') + ctx.errMsg(e)
    toast.error(t('film.castExtractFailed') + ctx.errMsg(e))
  } finally {
    extracting.value = false
  }
}

// —— 对象详情（定制器：parts / 主视图生成 / 辅助视图 / 描述编辑 / voice）——
const descDraft = ref('')
const descDirty = ref(false)
const descSaving = ref(false)
const descMsg = ref('')
/** voice 表单（人物类）。 */
const voiceKind = ref<'enum' | 'custom' | 'none'>('none')
const voiceEnum = ref('alloy')
const voiceCustom = ref('')
/** 部件保存 busy / 回执（v0.1.39 定制器）。 */
const partsSaving = ref(false)
const partsMsg = ref('')
/** 动作/运镜保存 busy / 回执（v0.1.41 P0）。 */
const motionSaving = ref(false)
const motionMsg = ref('')
/** 导入中视图名（busy 标记）。 */
const importingView = ref('')
const viewInput = ref<HTMLInputElement | null>(null)
let pendingImportView = ''
/** 动作参考导入（v0.1.41 P0）：video=mp4 / skeleton=png；独立文件选择器。 */
const motionInput = ref<HTMLInputElement | null>(null)
let pendingMotionKind: 'video' | 'skeleton' = 'video'
const detailError = ref('')

function resetDraft(): void {
  descDraft.value = ''
  descDirty.value = false
  descMsg.value = ''
  partsMsg.value = ''
  motionMsg.value = ''
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
    toast.success(t('toast.saved'))
    await loadObjects()
    await ctx!.refreshCollab()
  } catch (e) {
    descMsg.value = t('film.castSaveFailed') + ctx!.errMsg(e)
    toast.error(t('film.castSaveFailed') + ctx!.errMsg(e))
  } finally {
    descSaving.value = false
  }
}

// —— 定制器动作（v0.1.39）——

/** 保存部件选中值（整表替换）——PUT casting/:type/:name {parts}。 */
async function saveParts(parts: Record<string, string>): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || partsSaving.value) return
  partsSaving.value = true
  partsMsg.value = ''
  try {
    await filmUpdateCasting(pid, activeType.value, o.name, {
      parts,
      author: ctx!.author.value,
    })
    partsMsg.value = t('film.castCzPartsSaved')
    toast.success(t('toast.saved'))
    await loadObjects()
    await ctx!.refreshCollab()
  } catch (e) {
    partsMsg.value = t('film.castCzPartsSaveFailed') + ctx!.errMsg(e)
    toast.error(t('film.castCzPartsSaveFailed') + ctx!.errMsg(e))
  } finally {
    partsSaving.value = false
  }
}

/** image 模型源校验（主视图/辅助视图生成共用；「项目默认」=不传 model_ref
 *  走 models.json image 缺省链）。返回 false 时已置 detailError。 */
function ensureImageModel(): boolean {
  if (!ctx) return false
  const useDefault = ctx.isProjectDefaultSel('image')
  const ref = ctx.modelRefFor('image')
  if (!useDefault && !ref) {
    detailError.value = ctx.hasOptionsFor('image') ? t('film.pickModel') : t('film.noSource')
    return false
  }
  return true
}

/**
 * 按当前部件生成主视图：dirty → 先 PUT parts（部件替换后只改对应键——其它
 * 部件由后端组合提示词原样拼入保持一致）→ POST views/generate（view=主槽，
 * **不带 prompt**——组合提示词由后端从 card parts 组装；已有主槽产物时后端
 * 自动版本化 front → front-v2 → …历史保留）。
 */
async function generateMain(parts: Record<string, string>): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || !ctx) return
  if (!ensureImageModel()) return
  const ref = ctx.modelRefFor('image')
  detailError.value = ''
  try {
    if (!partsEqual(parts, normalizeParts(o.parts))) {
      await filmUpdateCasting(pid, activeType.value, o.name, {
        parts,
        author: ctx.author.value,
      })
      await loadObjects()
    }
    const task = await filmGenCastView(pid, activeType.value, o.name, {
      model_ref: ref ?? undefined,
      view: mainSlotOf(activeType.value),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (e) {
    detailError.value = t('film.actFailed') + ctx.errMsg(e)
    toast.error(t('film.actFailed') + ctx.errMsg(e))
  }
}

/** Blender 渲染主视图（v0.1.44 spike，scenes 定制器源选择）：chat 模型产
 * bpy 脚本 → 后端本机 headless 渲染（scene.render 两段任务）；prompt 可选
 * ——缺省后端按 card desc/parts 组装；dirty 时先 PUT parts 保持部件一致。 */
async function generateBlender(payload: { parts: Record<string, string>; prompt?: string }): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || !ctx) return
  // chat 位模型校验（「项目默认」=不传 model_ref 走 models.json chat 缺省链）
  const useDefault = ctx.isProjectDefaultSel('chat')
  const ref = ctx.modelRefFor('chat')
  if (!useDefault && !ref) {
    detailError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  detailError.value = ''
  try {
    if (!partsEqual(payload.parts, normalizeParts(o.parts))) {
      await filmUpdateCasting(pid, activeType.value, o.name, {
        parts: payload.parts,
        author: ctx.author.value,
      })
      await loadObjects()
    }
    const task = await filmGenCastView(pid, activeType.value, o.name, {
      source: 'blender',
      model_ref: ref ?? undefined,
      view: mainSlotOf(activeType.value),
      ...(payload.prompt ? { prompt: payload.prompt } : {}),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (e) {
    detailError.value = t('film.actFailed') + ctx.errMsg(e)
    toast.error(t('film.actFailed') + ctx.errMsg(e))
  }
}

/** 辅助视图生成（「更多视图」折叠区；prompt 可选覆写，缺省由后端模板构造）。 */
async function genAux(payload: { view: string; prompt?: string }): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || !ctx) return
  if (!ensureImageModel()) return
  const ref = ctx.modelRefFor('image')
  detailError.value = ''
  try {
    const task = await filmGenCastView(pid, activeType.value, o.name, {
      model_ref: ref ?? undefined,
      view: payload.view,
      ...(payload.prompt ? { prompt: payload.prompt } : {}),
      author: ctx.author.value,
    })
    ctx.addTracked(task)
  } catch (e) {
    detailError.value = t('film.actFailed') + ctx.errMsg(e)
    toast.error(t('film.actFailed') + ctx.errMsg(e))
  }
}

function importView(view: string): void {
  pendingImportView = view
  viewInput.value?.click()
}

async function onViewFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  const pid = ctx?.project.value?.id
  const o = selected.value
  const view = pendingImportView
  if (!file || !pid || !o || !ctx) return
  if (file.size > 10 * 1024 * 1024) {
    detailError.value = t('film.castViewTooLarge')
    return
  }
  importingView.value = view
  detailError.value = ''
  try {
    const { b64 } = splitDataUrl(await readFileAsDataUrl(file))
    await filmImportCastView(pid, activeType.value, o.name, {
      image_b64: b64,
      view,
      author: ctx.author.value,
    })
    await loadObjects()
    await ctx.refreshCollab()
    toast.success(t('toast.imported', { name: view }))
  } catch (err) {
    detailError.value = t('film.castViewImportFailed') + ctx.errMsg(err)
    toast.error(t('film.castViewImportFailed') + ctx.errMsg(err))
  } finally {
    importingView.value = ''
  }
}

/** 保存动作参考/运镜（v0.1.41 P0）——PUT casting/:type/:name {motion, camera}。 */
async function saveMotionCamera(payload: {
  motion: MotionCfg | null
  camera: CameraCfg | null
}): Promise<void> {
  const pid = ctx?.project.value?.id
  const o = selected.value
  if (!pid || !o || !ctx || motionSaving.value) return
  motionSaving.value = true
  motionMsg.value = ''
  try {
    await filmUpdateCasting(pid, activeType.value, o.name, {
      motion: payload.motion,
      camera: payload.camera,
      author: ctx.author.value,
    })
    motionMsg.value = t('film.castMotionSaved')
    toast.success(t('toast.saved'))
    await loadObjects()
    await ctx.refreshCollab()
  } catch (e) {
    motionMsg.value = t('film.castMotionSaveFailed') + ctx.errMsg(e)
    toast.error(t('film.castMotionSaveFailed') + ctx.errMsg(e))
  } finally {
    motionSaving.value = false
  }
}

/** 动作参考导入（v0.1.41 P0）：mp4 ≤50MB / png ≤10MB → views/import →
 *  按 import 回传路径回填 motion.asset（槽位自动选中该 kind）。 */
function importMotion(kind: 'video' | 'skeleton'): void {
  pendingMotionKind = kind
  motionInput.value?.click()
}

async function onMotionFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  const pid = ctx?.project.value?.id
  const o = selected.value
  const kind = pendingMotionKind
  if (!file || !pid || !o || !ctx) return
  const maxBytes = kind === 'video' ? 50 * 1024 * 1024 : 10 * 1024 * 1024
  if (file.size > maxBytes) {
    detailError.value =
      kind === 'video'
        ? t('film.castMotionVideoTooLarge')
        : t('film.castViewTooLarge')
    return
  }
  const view = kind === 'video' ? 'motion' : 'motion-skeleton'
  importingView.value = view
  detailError.value = ''
  try {
    const { b64 } = splitDataUrl(await readFileAsDataUrl(file))
    const resp = await filmImportCastView(pid, activeType.value, o.name, {
      image_b64: b64,
      view,
      mime: kind === 'video' ? 'video/mp4' : 'image/png',
      author: ctx.author.value,
    })
    // import 回传 path=casting/<type>/<slug>/views/<file> → asset=views/<file>
    const rel: string = (resp as unknown as { path?: string }).path ?? ''
    const asset = rel.includes('views/') ? `views/${rel.split('views/')[1]}` : ''
    if (asset) {
      await filmUpdateCasting(pid, activeType.value, o.name, {
        motion: {
          kind,
          asset,
          note: normalizeMotion(o.motion)?.note ?? '',
        },
        author: ctx.author.value,
      })
    }
    await loadObjects()
    await ctx.refreshCollab()
    toast.success(t('toast.imported', { name: kind === 'video' ? t('film.castMotionVideoSlot') : t('film.castMotionSkeletonSlot') }))
  } catch (err) {
    detailError.value = t('film.castMotionImportFailed') + ctx.errMsg(err)
    toast.error(t('film.castMotionImportFailed') + ctx.errMsg(err))
  } finally {
    importingView.value = ''
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
    toast.success(t('toast.created', { name: f.name.trim() }))
  } catch (e) {
    createError.value = t('film.castSaveFailed') + ctx.errMsg(e)
    toast.error(t('film.castSaveFailed') + ctx.errMsg(e))
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
    // v0.1.11 危险确认类：删除成功 toast
    toast.success(t('toast.deleted'))
  } catch (e) {
    objectsError.value = t('film.castDelFailed') + ctx.errMsg(e)
    toast.error(t('film.castDelFailed') + ctx.errMsg(e))
  }
}

// —— Hub 浏览页互跳（v0.1.1）：pendingCastSelect 置位 → 切类 + 选中对象 ——
// HubBrowse「在工作台打开」casting/<type>/<name>/… 路径时置位（含组件未挂载
// 场景——本页挂载后 immediate 消费）；消费后清空，避免重复跳转。
// 声明置于 resetDraft/descDraft 等依赖之后（immediate 回调会触达它们）。
watch(
  () => ctx?.pendingCastSelect.value ?? null,
  (p) => {
    if (!p || !ctx) return
    ctx.pendingCastSelect.value = null
    if (!FILM_CAST_TYPES.includes(p.type)) return
    if (activeType.value !== p.type) {
      activeType.value = p.type
      selectedName.value = p.name
      resetDraft()
      void loadObjects()
    } else if (p.name) {
      selectedName.value = p.name
      resetDraft()
    }
  },
  { immediate: true },
)

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
  <div class="fh-page nx-page">
    <FlowPageHead stage="casting" :title="t('film.flowCasting')">
      <template #actions>
        <NxThemeToggle />
        <select v-if="ctx" v-model="ctx.modelSel.chat" class="fh-select" :title="t('film.model')">
          <option :value="PROJECT_DEFAULT_KEY">
            🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('chat') ? ' · ' + ctx.defaultModelSummary('chat') : '' }}
          </option>
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>{{ t('film.noRunningLlm') }}</option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">{{ o.label }}{{ o.relay ? ' 🌐' : '' }}</option>
          </optgroup>
        </select>
        <NxButton
          variant="primary"
          size="sm"
          :loading="extracting"
          :disabled="!ctx?.project.value || !ctx?.modelSelReady('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="runExtract"
        >{{ extracting ? t('film.btnBusy') : t('film.castExtract') }}</NxButton>
        <NxButton size="sm" @click="openCreate">+ {{ t('film.castNew') }}</NxButton>
      </template>
    </FlowPageHead>

    <div class="fh-page-scroll">
      <!-- 提取报告（六类分组 name/desc/frequency；可折叠） -->
      <div v-if="extraction" class="fh-card">
        <button
          class="fh-card-head cast-report-toggle"
          type="button"
          @click="reportOpen = !reportOpen"
        >
          <span>📋 {{ t('film.castExtractReport') }}</span>
          <span class="cast-report-caret" aria-hidden="true">{{ reportOpen ? '▾' : '▴' }}</span>
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
                  <img v-if="thumbKey(v) && viewThumbs[thumbKey(v)]" :src="viewThumbs[thumbKey(v)]" :alt="v.view ?? ''">
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

        <!-- 对象详情：定制器（中央主视图 + 部件面板 + 更多视图）+ card.md 描述/voice -->
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

            <!-- 定妆定制器（v0.1.39）：中央主体完整视图 + 右侧部件替换面板 +
                 底部按当前部件生成主视图 + 主槽历史缩略 + 「更多视图」折叠区；
                 v0.1.41 P0 动作类另加动作参考/运镜区 -->
            <CastCustomizer
              :type="activeType"
              :obj="selected"
              :thumbs="viewThumbs"
              :parts-saving="partsSaving"
              :motion-saving="motionSaving"
              @save-parts="saveParts"
              @generate-main="generateMain"
              @generate-blender="generateBlender"
              @generate-aux="genAux"
              @import-view="importView"
              @import-motion="importMotion"
              @save-motion-camera="saveMotionCamera"
            />
            <div v-if="partsMsg" class="fh-small" :class="{ 'cast-msg-err': partsMsg.startsWith(t('film.castCzPartsSaveFailed')) }">
              {{ partsMsg }}
            </div>
            <div v-if="motionMsg" class="fh-small" :class="{ 'cast-msg-err': motionMsg.startsWith(t('film.castMotionSaveFailed')) }">
              {{ motionMsg }}
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
    <!-- 动作参考导入（v0.1.41 P0）：视频槽=mp4（≤50MB）/骨架槽=png（≤10MB） -->
    <input ref="motionInput" type="file" accept="video/mp4,image/png" class="fh-hidden-input" @change="onMotionFile" />

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
/* v0.1.8 nx 化：定妆对象卡三态（静默/hover 描边加深/选中 accent soft） */
.cast-obj-card {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 5px;
  cursor: pointer;
  min-width: 0;
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  transition:
    border-color 0.15s ease,
    background 0.15s ease,
    box-shadow 0.15s ease;
}
.cast-obj-card:hover {
  border-color: var(--nx-line-strong);
  box-shadow: var(--nx-shadow-xs);
}
.cast-obj-card.is-active {
  border-color: var(--nx-accent);
  background: var(--nx-accent-soft);
}
.cast-obj-card:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.cast-obj-row { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; min-width: 0; }
.cast-obj-name {
  font-weight: var(--nx-font-weight-semibold);
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.cast-obj-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  line-height: 1.45;
}
.cast-obj-thumbs { display: flex; gap: 5px; flex-wrap: wrap; }
.cast-save-row { display: flex; align-items: center; gap: 10px; }
.cast-msg-err { color: var(--nx-danger); }
.cast-report-group { display: flex; flex-direction: column; gap: 5px; }
.cast-report-list { display: flex; flex-direction: column; gap: 4px; }
/* 提取报告折叠头：按钮化卡头（透明底 + divider 底线 + hover 换底） */
.cast-report-toggle {
  cursor: pointer;
  background: transparent;
  border: none;
  border-bottom: 0.5px solid var(--nx-line-divider);
  font-family: inherit;
  font-size: var(--nx-font-size-base);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  transition: background 0.12s ease;
}
.cast-report-toggle:hover { background: var(--nx-bg-hover); }
.cast-report-toggle:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: -2px;
}
.cast-report-caret { margin-left: auto; color: var(--nx-text-tertiary); }
.cast-report-item {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: var(--nx-font-size-xs);
  padding: 4px 8px;
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  min-width: 0;
}
.cast-report-name {
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  flex-shrink: 0;
}
.cast-report-desc {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
