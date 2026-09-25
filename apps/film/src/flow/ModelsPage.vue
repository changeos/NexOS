<script setup lang="ts">
// =============================================================================
// ModelsPage.vue —— 项目级模型设置页（v0.1.39：八能力位默认模型源 + 直填 API；
// v0.1.44 并入件：每能力位「⚙ 高级路由」折叠区——任务路由 + 失败 fallback 链）。
//
// 真值在 hub 树 models.json（文件即真值，agent 亦可 PUT files/models.json）；
// 本页 = GET :id/models 快照（八能力位行：图标 + 名称 + 用途 + 当前配置 +
// 可用性探测绿/灰点 + 「配置」按钮）+ 配置弹窗（模型源[未设置/本地/渠道🌐] +
// 模型名（可选）+ 备注 + 清除）→ PUT :id/models（单能力位更新，author 透传
// activity models.set）。
//
// v0.1.44 高级路由：能力位行尾「⚙ 高级路由 (n)」展开折叠区——路由规则表
// （行：任务下拉[常用任务枚举+自定义+兜底] / 模型源选择 / 渠道+模型名 /
// 上移下移排序 / 删除 / 逐条探测点）+「+添加路由」+ 保存。保存走 PUT
// /models 带 routes 字段（整组替换；主默认源字段原样回传=保留）。语义：
// 任务自上而下匹配（精确>最长前缀），执行失败自动切换下一候选，最后回落
// 默认模型源；agent 双通道（REST PUT routes / 直接 PUT files/models.json）
// 见折叠区底部说明行。
//
// v0.1.39 直填 API：页头「＋ 添加 API」弹窗（API 地址 + API Key（可空）+
// 模型名 + 能力（五位单选）+ 备注 + 名称（缺省=模型名·能力））→ POST
// /api/v1/gateway/channels 建渠道（provider 固定 openai=OpenAI 兼容缺省；
// 渠道无 capability 字段——能力归属由 models.json slot 承载）→ 成功后自动
// PUT :id/models 把该能力位设为 {source:channel, channel_id:新渠道,
// model:模型名}（一步到位：填完 API 即成为该能力默认）+ 刷新快照与渠道清单。
//
// 能力位八枚举冻结：chat/image/video/tts/music 为既有消费位（生成动作缺省链
// 已接入——各生成按钮选「项目默认」即不传 model_ref 走本页配置）；asr/emb/vl
// 为预留槽位（配置就绪、待消费场景——行尾灰徽章，可配置但注明未消费）。
//
// 模型源下拉数据复用既有宿主端点（llm/instances running + gateway/channels，
// 与工作台模型选择器同源；via_node 非空 = 🌐 联邦中继渠道）。
// =============================================================================
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  FILM_MODEL_CAPS,
  FILM_MODEL_CAPS_RESERVED,
  createGatewayChannel,
  filmSetProjectModel,
  fetchGatewayChannels,
  fetchLlmInstances,
  normalizeApiBaseUrl,
  sdkGatewayChannels,
  sdkLlmInstances,
  type FilmModelCap,
  type FilmModelCapView,
  type FilmModelRoute,
} from '../api'
import { useFlow } from './flowContext'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
// v0.1.11 全局操作反馈：能力位保存/清除/直填 API 成败 toast
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

// —— 能力位元数据（图标 + i18n 名称/用途键；显式映射不做运行时拼接） ——
const CAP_META: Record<FilmModelCap, { icon: string; nameKey: string; useKey: string }> = {
  chat: { icon: '💬', nameKey: 'models.capChat', useKey: 'models.useChat' },
  image: { icon: '🖼', nameKey: 'models.capImage', useKey: 'models.useImage' },
  video: { icon: '🎬', nameKey: 'models.capVideo', useKey: 'models.useVideo' },
  tts: { icon: '🔊', nameKey: 'models.capTts', useKey: 'models.useTts' },
  music: { icon: '🎵', nameKey: 'models.capMusic', useKey: 'models.useMusic' },
  asr: { icon: '🎙', nameKey: 'models.capAsr', useKey: 'models.useAsr' },
  emb: { icon: '🧬', nameKey: 'models.capEmb', useKey: 'models.useEmb' },
  vl: { icon: '👁', nameKey: 'models.capVl', useKey: 'models.useVl' },
}

const caps = computed<FilmModelCapView[]>(() => ctx?.projectModels.value?.capabilities ?? [])
const loadError = ref('')
const loading = ref(false)

/** 本地能力面（chat=运行中 LLM 实例；image=sd-turbo 内核；其余位无本地源）。 */
const LOCAL_CAPS: readonly FilmModelCap[] = ['chat', 'image']

/** 渠道/实例清单（弹窗模型源下拉；与工作台选择器同数据源）。 */
interface ChLite {
  id?: string
  name?: string
  provider?: string
  enabled?: boolean
  status?: string
  models?: string[]
  via_node?: string
  [k: string]: unknown
}
interface InstLite {
  id?: string
  name?: string
  model?: string
  status?: string
  config?: { served_model_name?: string }
  [k: string]: unknown
}
const channels = ref<ChLite[]>([])
const runningLlms = ref<InstLite[]>([])
const sourcesError = ref('')

async function loadSources(): Promise<void> {
  sourcesError.value = ''
  const errs: string[] = []
  try {
    const viaSdk = await sdkLlmInstances()
    const raw = viaSdk ?? (await fetchLlmInstances())
    const list = Array.isArray(raw) ? (raw as InstLite[]) : []
    runningLlms.value = list.filter((i) => i.status === 'running')
  } catch (e) {
    runningLlms.value = []
    errs.push(String(e))
  }
  try {
    const viaSdk = await sdkGatewayChannels()
    const raw = viaSdk ?? (await fetchGatewayChannels())
    channels.value = Array.isArray(raw) ? (raw as ChLite[]) : []
  } catch (e) {
    channels.value = []
    errs.push(String(e))
  }
  if (errs.length) sourcesError.value = errs.join('；')
}

/** 刷新快照（走 context.reloadProjectModels——与 FilmStudio 共享同一份
 *  projectModels 态；ModelsPage 捕获错误如实展示）。 */
async function reload(): Promise<void> {
  if (!ctx) return
  loading.value = true
  try {
    await ctx.reloadProjectModels()
    loadError.value = ''
  } catch (e) {
    loadError.value = ctx.errMsg(e)
  } finally {
    loading.value = false
  }
}

onMounted(() => {
  void reload()
  void loadSources()
})
watch(
  () => ctx?.refreshTick.value,
  () => void reload(),
)

// —— 行展示派生 ——

function isReserved(cap: FilmModelCap): boolean {
  return FILM_MODEL_CAPS_RESERVED.includes(cap)
}

/** 当前配置显示：本地 | 渠道名·模型 | 未设置。 */
function slotLabel(c: FilmModelCapView): string {
  if (c.source === 'local') return t('models.sourceLocal')
  if (c.source === 'channel') {
    const ch = channels.value.find((x) => x.id === c.channel_id)
    const name = ch?.name || c.channel_id || '?'
    const model = (c.model || ch?.models?.[0] || '').trim()
    return model ? `${name} · ${model}` : name
  }
  return t('models.notSet')
}

function availableOk(c: FilmModelCapView): boolean {
  return !!c.available?.ok
}

// —— 高级路由（v0.1.44）：任务路由 + 失败 fallback 链，每能力位一折叠区 ——

/** 常用任务枚举（与后端 stage 键冻结一致；story/casting/shot 为段前缀）。 */
const ROUTE_TASK_PRESETS: readonly string[] = [
  'story.generate',
  'story.clean',
  'story.chapterize',
  'story.profile',
  'story.embed',
  'storyboard.generate',
  'casting.extract',
  'casting.view',
  'shot.image',
  'shot.video',
  'shot.tts',
  'music.generate',
  'bgm.generate',
  'scene.render',
  'story',
  'casting',
  'shot',
]

/** 路由编辑行（draft；保存时归一为 FilmModelRoute[]）。 */
interface RouteRow {
  /** 任务名（''=兜底路由）。 */
  task: string
  /** 任务不在预设枚举中 → 自定义输入态。 */
  taskCustom: boolean
  source: 'local' | 'channel'
  channelId: string
  model: string
}

/** 折叠开合（capability → bool）。 */
const routeOpen = reactive<Record<string, boolean>>({})
/** 路由草稿（capability → 行；caps 快照变化时重建）。 */
const routeDrafts = reactive<Record<string, RouteRow[]>>({})
const routeBusy = ref(false)
const routeError = ref('')
/** 保存成功提示（capability；下次展开/保存时清）。 */
const routeSavedCap = ref('')

function rowFromRoute(r: FilmModelRoute): RouteRow {
  const task = (r.task ?? '').trim()
  return {
    task,
    taskCustom: !!task && !ROUTE_TASK_PRESETS.includes(task),
    source: r.source === 'local' ? 'local' : 'channel',
    channelId: r.channel_id ?? '',
    model: r.model ?? '',
  }
}

function rowsOf(cap: FilmModelCap): RouteRow[] {
  return routeDrafts[cap] ?? []
}

/** 快照 → 草稿（首次见到该能力位或行数与快照不一致且未在编辑时重建）。 */
watch(
  caps,
  (list) => {
    for (const c of list) {
      const rows = (c.routes ?? []).map(rowFromRoute)
      const cur = routeDrafts[c.capability]
      if (!cur || !routeOpen[c.capability]) routeDrafts[c.capability] = rows
    }
  },
  { immediate: true },
)

function toggleRoutes(c: FilmModelCapView): void {
  routeOpen[c.capability] = !routeOpen[c.capability]
  if (routeOpen[c.capability]) {
    routeDrafts[c.capability] = (c.routes ?? []).map(rowFromRoute)
    routeError.value = ''
  }
}

function addRouteRow(cap: FilmModelCap): void {
  rowsOf(cap).push({ task: '', taskCustom: false, source: 'channel', channelId: '', model: '' })
}

function removeRouteRow(cap: FilmModelCap, i: number): void {
  rowsOf(cap).splice(i, 1)
}

function moveRouteRow(cap: FilmModelCap, i: number, delta: -1 | 1): void {
  const rows = rowsOf(cap)
  const j = i + delta
  if (j < 0 || j >= rows.length) return
  const [row] = rows.splice(i, 1)
  rows.splice(j, 0, row)
}

/** 任务选择控件值（''=兜底；'__custom__'=自定义输入）。 */
const TASK_CUSTOM = '__custom__'

function taskSelectVal(row: RouteRow): string {
  if (row.taskCustom) return TASK_CUSTOM
  return row.task
}

function onTaskSelect(row: RouteRow, v: string): void {
  if (v === TASK_CUSTOM) {
    row.taskCustom = true
    row.task = ''
  } else {
    row.taskCustom = false
    row.task = v
  }
}

/** 保存路由（整组替换；主默认源原样回传——PUT 缺省字段=保留）。 */
async function saveRoutes(c: FilmModelCapView): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || routeBusy.value) return
  const rows = rowsOf(c.capability)
  for (const [i, r] of rows.entries()) {
    if (r.source === 'channel' && !r.channelId) {
      routeError.value = t('models.routeErrChannel', { n: i + 1 })
      return
    }
  }
  routeBusy.value = true
  routeError.value = ''
  try {
    const routes: FilmModelRoute[] = rows.map((r) => ({
      ...(r.task.trim() ? { task: r.task.trim() } : {}),
      source: r.source,
      ...(r.source === 'channel' && r.channelId ? { channel_id: r.channelId } : {}),
      ...(r.model.trim() ? { model: r.model.trim() } : {}),
    }))
    await filmSetProjectModel(pid, {
      capability: c.capability,
      source: (c.source as 'local' | 'channel' | null) ?? null,
      ...(c.source === 'channel' && c.channel_id ? { channel_id: c.channel_id } : {}),
      ...(c.model ? { model: c.model } : {}),
      ...(c.note ? { note: c.note } : {}),
      routes,
      author: ctx?.author.value,
    })
    routeSavedCap.value = c.capability
    await reload()
    toast.success(t('toast.saved'))
  } catch (e) {
    routeError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('models.saveFailed')
    toast.error(routeError.value)
  } finally {
    routeBusy.value = false
  }
}

/** 行级探测点（保存后快照与草稿行数一致时展示；编辑中隐藏）。 */
function routeRowAvailable(
  c: FilmModelCapView,
  i: number,
): { ok?: boolean; detail?: string } | null {
  const rows = rowsOf(c.capability)
  const snap = c.routes ?? []
  if (rows.length !== snap.length) return null
  return snap[i]?.available ?? null
}

// —— 配置弹窗 ——

const editing = ref<FilmModelCap | null>(null)
const saveBusy = ref(false)
const saveError = ref('')
const form = reactive({
  source: '' as '' | 'local' | 'channel',
  channelId: '',
  model: '',
  note: '',
})

function openConfigure(c: FilmModelCapView): void {
  editing.value = c.capability
  saveError.value = ''
  form.source = (c.source as 'local' | 'channel' | null) ?? ''
  form.channelId = c.channel_id ?? ''
  form.model = c.model ?? ''
  form.note = c.note ?? ''
}

/** 弹窗内渠道选项（启用中的渠道；🌐 中继标注）。 */
const channelOptions = computed(() =>
  channels.value.filter((c) => (c.status ?? (c.enabled ? 'enabled' : 'disabled')) === 'enabled'),
)

/** 本地源说明小字（chat=运行中实例名；image=sd-turbo）。 */
const localHint = computed(() => {
  if (editing.value === 'chat') {
    const first = runningLlms.value[0]
    const name =
      first?.name || first?.config?.served_model_name || first?.model || t('models.noLocalLlm')
    return t('models.localChatHint', { name })
  }
  return t('models.localSdHint')
})

/** 弹窗选中渠道的 models 列表（模型名输入 datalist 提示）。 */
const formChannelModels = computed(() => {
  if (form.source !== 'channel') return []
  return channels.value.find((c) => c.id === form.channelId)?.models ?? []
})

function closeConfigure(): void {
  editing.value = null
}

async function save(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !editing.value || saveBusy.value) return
  saveBusy.value = true
  saveError.value = ''
  try {
    await filmSetProjectModel(pid, {
      capability: editing.value,
      source: form.source === '' ? null : form.source,
      ...(form.source === 'channel' && form.channelId ? { channel_id: form.channelId } : {}),
      ...(form.model.trim() ? { model: form.model.trim() } : {}),
      ...(form.note.trim() ? { note: form.note.trim() } : {}),
      author: ctx?.author.value,
    })
    await reload()
    editing.value = null
    // v0.1.11 保存类：能力位保存成功全局反馈（此前仅弹窗关闭）
    toast.success(t('toast.saved'))
  } catch (e) {
    saveError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('models.saveFailed')
    toast.error(saveError.value)
  } finally {
    saveBusy.value = false
  }
}

/** 清除（source=null；note 保留输入）。 */
async function clearSlot(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !editing.value || saveBusy.value) return
  saveBusy.value = true
  saveError.value = ''
  try {
    await filmSetProjectModel(pid, {
      capability: editing.value,
      source: null,
      ...(form.note.trim() ? { note: form.note.trim() } : {}),
      author: ctx?.author.value,
    })
    await reload()
    editing.value = null
    // v0.1.11 保存类：清除默认成功全局反馈
    toast.success(t('toast.saved'))
  } catch (e) {
    saveError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('models.saveFailed')
    toast.error(saveError.value)
  } finally {
    saveBusy.value = false
  }
}

// —— 直填 API 弹窗（v0.1.39：建网关渠道 + 一键设为该能力默认） ——

/** 直填可选能力（五位消费位；asr/emb/vl 预留槽不走直填入口）。 */
const ADD_API_CAPS: readonly FilmModelCap[] = ['chat', 'image', 'video', 'tts', 'music']

const adding = ref(false)
const addBusy = ref(false)
const addError = ref('')
/** 成功提示（页面级 ✓ 行；打开弹窗即清）。 */
const addNotice = ref('')
const addForm = reactive({
  baseUrl: '',
  apiKey: '',
  model: '',
  capability: 'chat' as (typeof ADD_API_CAPS)[number],
  note: '',
  name: '',
  /** 名称手动改过（此后不再随 模型名/能力 自动联动）。 */
  nameTouched: false,
})

/** 能力显示名（单选下拉/自动名称共用）。 */
function capLabel(cap: FilmModelCap): string {
  return t(CAP_META[cap].nameKey)
}

/** 自动名称 = 模型名 · 能力（模型名空则空——提交前校验兜底）。 */
function autoAddName(): string {
  const m = addForm.model.trim()
  return m ? `${m} · ${capLabel(addForm.capability)}` : ''
}

// 模型名/能力变化 → 未手改过名称时自动联动
watch(
  () => [addForm.model, addForm.capability],
  () => {
    if (!addForm.nameTouched) addForm.name = autoAddName()
  },
)

function openAdd(): void {
  adding.value = true
  addError.value = ''
  addNotice.value = ''
  addForm.baseUrl = ''
  addForm.apiKey = ''
  addForm.model = ''
  addForm.capability = 'chat'
  addForm.note = ''
  addForm.name = ''
  addForm.nameTouched = false
}

function closeAdd(): void {
  adding.value = false
}

/**
 * 提交直填：校验 → 规整地址（裸 host 补 /v1）→ POST gateway/channels 建渠道
 * （provider 固定 openai=OpenAI 兼容缺省）→ 取回 id 链式 PUT :id/models 设
 * 该能力位默认 → toast + 关弹窗 + 刷新（快照/渠道下拉）。失败红条透传后端
 * 文案（重名/地址非法等），不发 PUT。
 */
async function submitAdd(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || addBusy.value) return
  addError.value = ''
  const baseUrl = normalizeApiBaseUrl(addForm.baseUrl)
  const model = addForm.model.trim()
  const name = addForm.name.trim() || autoAddName()
  if (!baseUrl) {
    addError.value = t('models.addApiErrUrl')
    return
  }
  if (!model) {
    addError.value = t('models.addApiErrModel')
    return
  }
  addBusy.value = true
  try {
    const ch = await createGatewayChannel({
      name,
      provider: 'openai',
      base_url: baseUrl,
      ...(addForm.apiKey.trim() ? { api_key: addForm.apiKey.trim() } : {}),
      models: [model],
    })
    const chId = (ch?.id ?? '').trim()
    if (!chId) throw new Error(t('models.addApiErrNoId'))
    await filmSetProjectModel(pid, {
      capability: addForm.capability,
      source: 'channel',
      channel_id: chId,
      model,
      ...(addForm.note.trim() ? { note: addForm.note.trim() } : {}),
      author: ctx?.author.value,
    })
    addNotice.value = t('models.addApiDone', {
      name: ch?.name || name,
      cap: capLabel(addForm.capability),
    })
    adding.value = false
    await Promise.all([reload(), loadSources()])
    // v0.1.11 创建类：直填 API 成功全局 toast（页内 ✓ 行保留）
    toast.success(addNotice.value)
  } catch (e) {
    addError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('models.addApiFailed')
    toast.error(addError.value)
  } finally {
    addBusy.value = false
  }
}

</script>

<template>
  <div class="fh-page nx-page">
    <!-- 页头（非流程阶段页——通用 head 样式；右侧「+ 添加 API」与刷新） -->
    <div class="fh-head">
      <span class="fh-head-title">🤖 {{ t('models.title') }}</span>
      <div class="fh-head-actions">
        <NxThemeToggle />
        <span class="fh-pill fh-pill-muted fh-pill-mini">{{ t('models.pathPill') }}</span>
        <button
          class="fh-btn fh-btn-small fh-btn-primary"
          type="button"
          :disabled="!ctx?.project.value"
          :title="t('models.addApiTitle')"
          @click="openAdd()"
        >＋ {{ t('models.addApi') }}</button>
        <button
          class="fh-btn fh-btn-small"
          type="button"
          :disabled="loading || !ctx?.project.value"
          :title="t('models.reloadTip')"
          @click="reload()"
        >↻</button>
      </div>
    </div>
    <div class="fh-collab-tip">{{ t('models.tip') }}</div>
    <div v-if="addNotice" class="fh-muted fh-small models-add-notice">✓ {{ addNotice }}</div>
    <div v-if="loadError" class="fh-error-box">{{ t('models.loadFailed') }}{{ loadError }}</div>

    <div class="fh-page-scroll">
      <section class="fh-card">
        <div class="fh-card-head">
          <span>{{ t('models.slotList') }}</span>
          <span class="fh-muted fh-small">
            {{ FILM_MODEL_CAPS.length - FILM_MODEL_CAPS_RESERVED.length }} / {{ FILM_MODEL_CAPS.length }}
          </span>
        </div>
        <div class="fh-card-body">
          <div v-if="!caps.length && !loadError" class="fh-empty">
            {{ loading ? t('film.loading') : t('models.empty') }}
          </div>
          <!-- 八能力位行：图标 + 名称 + 用途 + 当前配置 + 可用性点 + 配置按钮
               （asr/emb/vl 行加「预留槽位」灰徽章）+ ⚙ 高级路由折叠区 -->
          <template v-for="c in caps" :key="c.capability">
            <div class="fh-row models-row">
              <span class="models-icon" aria-hidden="true">{{ CAP_META[c.capability].icon }}</span>
              <div class="models-name">
                <div>{{ t(CAP_META[c.capability].nameKey) }}</div>
                <div class="fh-muted fh-small">{{ t(CAP_META[c.capability].useKey) }}</div>
              </div>
              <span
                v-if="isReserved(c.capability)"
                class="fh-pill fh-pill-muted fh-pill-mini"
                :title="t('models.reservedTip')"
              >{{ t('models.reserved') }}</span>
              <span
                class="fh-pill fh-pill-mini"
                :class="availableOk(c) ? 'fh-pill-ok' : 'fh-pill-muted'"
                :title="c.available?.detail || t('models.notSet')"
              >
                <span class="models-dot" :class="{ ok: availableOk(c) }" aria-hidden="true" />
                {{ availableOk(c) ? t('models.availOk') : t('models.availOff') }}
              </span>
              <span class="fh-small models-slot fh-ellipsis" :title="c.available?.detail || ''">
                {{ slotLabel(c) }}
              </span>
              <button
                class="fh-btn fh-btn-small models-route-toggle"
                type="button"
                :title="t('models.routeToggleTip')"
                @click="toggleRoutes(c)"
              >⚙ {{ t('models.routeToggle') }}{{ (c.routes ?? []).length ? ` (${(c.routes ?? []).length})` : '' }}</button>
              <button
                class="fh-btn fh-btn-small"
                style="flex-shrink: 0"
                type="button"
                :disabled="!ctx?.project.value"
                @click="openConfigure(c)"
              >{{ t('models.configure') }}</button>
            </div>
            <!-- ⚙ 高级路由折叠区（v0.1.44）：路由规则表 + 说明 + agent 双通道 -->
            <div v-if="routeOpen[c.capability]" class="models-route-panel">
              <div class="fh-muted fh-small models-route-hint">{{ t('models.routeHint') }}</div>
              <div v-if="!rowsOf(c.capability).length" class="fh-muted fh-small models-route-empty">
                {{ t('models.routeEmpty') }}
              </div>
              <div
                v-for="(row, i) in rowsOf(c.capability)"
                :key="i"
                class="fh-row models-route-row"
              >
                <label class="fh-field models-route-task">
                  <span class="fh-field-label">{{ t('models.routeTask') }}</span>
                  <select
                    class="fh-select"
                    :value="taskSelectVal(row)"
                    @change="onTaskSelect(row, ($event.target as HTMLSelectElement).value)"
                  >
                    <option value="">{{ t('models.routeTaskFallback') }}</option>
                    <option v-for="task in ROUTE_TASK_PRESETS" :key="task" :value="task">
                      {{ task }}
                    </option>
                    <option :value="TASK_CUSTOM">{{ t('models.routeTaskCustom') }}</option>
                  </select>
                  <input
                    v-if="row.taskCustom"
                    v-model="row.task"
                    type="text"
                    class="fh-input"
                    :placeholder="t('models.routeTaskPh')"
                  >
                </label>
                <label class="fh-field models-route-src">
                  <span class="fh-field-label">{{ t('models.routeSource') }}</span>
                  <select v-model="row.source" class="fh-select">
                    <option v-if="LOCAL_CAPS.includes(c.capability)" value="local">
                      {{ t('models.sourceLocal') }}
                    </option>
                    <option value="channel">{{ t('models.sourceChannel') }}</option>
                  </select>
                </label>
                <template v-if="row.source === 'channel'">
                  <label class="fh-field models-route-ch">
                    <span class="fh-field-label">🌐 {{ t('models.channel') }}</span>
                    <select v-model="row.channelId" class="fh-select">
                      <option v-if="!channelOptions.length" value="" disabled>
                        {{ t('models.noChannels') }}
                      </option>
                      <option
                        v-for="ch in channelOptions"
                        :key="ch.id ?? ''"
                        :value="ch.id ?? ''"
                      >
                        {{ ch.name || ch.id || '?' }}{{ ch.via_node ? ' 🌐' : '' }}
                      </option>
                    </select>
                  </label>
                  <label class="fh-field models-route-model">
                    <span class="fh-field-label">{{ t('models.modelName') }}</span>
                    <input
                      v-model="row.model"
                      type="text"
                      class="fh-input"
                      :placeholder="t('models.modelPh')"
                    >
                  </label>
                </template>
                <div v-else class="fh-muted fh-small models-route-local-hint">
                  {{ c.capability === 'chat' ? localHint : t('models.localSdHint') }}
                </div>
                <span
                  class="fh-pill fh-pill-mini"
                  :class="routeRowAvailable(c, i)?.ok ? 'fh-pill-ok' : 'fh-pill-muted'"
                  :title="routeRowAvailable(c, i)?.detail || ''"
                >
                  <span
                    class="models-dot"
                    :class="{ ok: routeRowAvailable(c, i)?.ok }"
                    aria-hidden="true"
                  />
                </span>
                <div class="models-route-ops">
                  <button
                    class="fh-btn fh-btn-small"
                    type="button"
                    :disabled="i === 0"
                    :title="t('models.routeUp')"
                    @click="moveRouteRow(c.capability, i, -1)"
                  >↑</button>
                  <button
                    class="fh-btn fh-btn-small"
                    type="button"
                    :disabled="i === rowsOf(c.capability).length - 1"
                    :title="t('models.routeDown')"
                    @click="moveRouteRow(c.capability, i, 1)"
                  >↓</button>
                  <button
                    class="fh-btn fh-btn-small"
                    type="button"
                    :title="t('models.routeDelete')"
                    @click="removeRouteRow(c.capability, i)"
                  >✕</button>
                </div>
              </div>
              <div class="models-route-actions">
                <button
                  class="fh-btn fh-btn-small"
                  type="button"
                  @click="addRouteRow(c.capability)"
                >{{ t('models.routeAdd') }}</button>
                <button
                  class="fh-btn fh-btn-small fh-btn-primary"
                  type="button"
                  :disabled="routeBusy || !ctx?.project.value"
                  @click="saveRoutes(c)"
                >{{ routeBusy ? '…' : t('models.routeSave') }}</button>
                <span
                  v-if="routeSavedCap === c.capability"
                  class="fh-muted fh-small"
                >✓ {{ t('models.routeSaved') }}</span>
              </div>
              <div v-if="routeError" class="fh-error-box models-route-error">{{ routeError }}</div>
              <div class="fh-muted fh-small models-route-agent">
                {{ t('models.routeAgentHint') }}
              </div>
            </div>
          </template>
          <div v-if="sourcesError" class="fh-muted fh-small">
            {{ t('models.sourcesFailed') }}{{ sourcesError }}
          </div>
        </div>
      </section>
    </div>

    <!-- 配置弹窗（单能力位：模型源 + 模型名（可选）+ 备注 + 清除） -->
    <div v-if="editing" class="fh-modal-backdrop" @click.self="closeConfigure">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-models-edit-title">
        <div class="fh-modal-head">
          <h3 id="film-models-edit-title">
            {{ CAP_META[editing].icon }} {{ t('models.editTitle', { cap: t(CAP_META[editing].nameKey) }) }}
          </h3>
          <button class="fh-modal-close" type="button" @click="closeConfigure">×</button>
        </div>
        <div class="fh-modal-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.source') }}</span>
            <select v-model="form.source" class="fh-select">
              <option value="">{{ t('models.sourceUnset') }}</option>
              <option v-if="LOCAL_CAPS.includes(editing)" value="local">
                {{ t('models.sourceLocal') }}
              </option>
              <option value="channel">{{ t('models.sourceChannel') }}</option>
            </select>
          </label>
          <div v-if="form.source === 'local'" class="fh-muted fh-small">{{ localHint }}</div>
          <template v-if="form.source === 'channel'">
            <label class="fh-field">
              <span class="fh-field-label">🌐 {{ t('models.channel') }}</span>
              <select v-model="form.channelId" class="fh-select">
                <option v-if="!channelOptions.length" value="" disabled>
                  {{ t('models.noChannels') }}
                </option>
                <option v-for="c in channelOptions" :key="c.id ?? ''" :value="c.id ?? ''">
                  {{ c.name || c.id || '?' }}{{ c.via_node ? ' 🌐' : '' }}
                </option>
              </select>
            </label>
            <label class="fh-field">
              <span class="fh-field-label">{{ t('models.modelName') }}</span>
              <input
                v-model="form.model"
                type="text"
                class="fh-input"
                list="film-models-channel-models"
                :placeholder="formChannelModels[0] || t('models.modelPh')"
              >
              <datalist id="film-models-channel-models">
                <option v-for="m in formChannelModels" :key="m" :value="m" />
              </datalist>
            </label>
          </template>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.note') }}</span>
            <input
              v-model="form.note"
              type="text"
              class="fh-input"
              :placeholder="t('models.notePh')"
            >
          </label>
          <div v-if="saveError" class="fh-error-box">{{ saveError }}</div>
        </div>
        <div class="fh-form-actions">
          <button class="fh-btn" type="button" :disabled="saveBusy" @click="closeConfigure">
            {{ t('models.cancel') }}
          </button>
          <button
            class="fh-btn"
            type="button"
            :disabled="saveBusy || (form.source === 'channel' && !form.channelId)"
            @click="clearSlot"
          >{{ t('models.clear') }}</button>
          <button
            class="fh-btn fh-btn-primary"
            type="button"
            :disabled="saveBusy || (form.source === 'channel' && !form.channelId)"
            @click="save"
          >{{ saveBusy ? '…' : t('models.save') }}</button>
        </div>
      </div>
    </div>

    <!-- 直填 API 弹窗（v0.1.39：POST gateway/channels 建渠道 → 链式 PUT 该
         能力位默认；失败红条透传后端文案） -->
    <div v-if="adding" class="fh-modal-backdrop" @click.self="closeAdd">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-models-add-title">
        <div class="fh-modal-head">
          <h3 id="film-models-add-title">＋ {{ t('models.addApiTitle') }}</h3>
          <button class="fh-modal-close" type="button" @click="closeAdd">×</button>
        </div>
        <div class="fh-modal-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.addApiUrl') }}</span>
            <input
              v-model="addForm.baseUrl"
              type="text"
              class="fh-input"
              :placeholder="t('models.addApiUrlPh')"
            >
            <span class="fh-muted fh-small">{{ t('models.addApiUrlHint') }}</span>
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.addApiKey') }}</span>
            <input
              v-model="addForm.apiKey"
              type="password"
              class="fh-input"
              autocomplete="off"
              :placeholder="t('models.addApiKeyPh')"
            >
          </label>
          <div class="fh-field-row">
            <label class="fh-field" style="flex: 1 1 180px">
              <span class="fh-field-label">{{ t('models.addApiModel') }}</span>
              <input
                v-model="addForm.model"
                type="text"
                class="fh-input"
                :placeholder="t('models.addApiModelPh')"
              >
            </label>
            <label class="fh-field" style="flex: 0 1 150px">
              <span class="fh-field-label">{{ t('models.addApiCap') }}</span>
              <select v-model="addForm.capability" class="fh-select">
                <option v-for="c in ADD_API_CAPS" :key="c" :value="c">
                  {{ CAP_META[c].icon }} {{ capLabel(c) }}
                </option>
              </select>
            </label>
          </div>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.addApiName') }}</span>
            <input
              v-model="addForm.name"
              type="text"
              class="fh-input"
              :placeholder="autoAddName()"
              @input="addForm.nameTouched = true"
            >
            <span class="fh-muted fh-small">{{ t('models.addApiNameHint') }}</span>
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('models.note') }}</span>
            <input
              v-model="addForm.note"
              type="text"
              class="fh-input"
              :placeholder="t('models.addApiNotePh')"
            >
          </label>
          <div v-if="addError" class="fh-error-box">{{ addError }}</div>
          <div class="fh-muted fh-small">{{ t('models.addApiFootnote') }}</div>
        </div>
        <div class="fh-form-actions">
          <button class="fh-btn" type="button" :disabled="addBusy" @click="closeAdd">
            {{ t('models.cancel') }}
          </button>
          <button
            class="fh-btn fh-btn-primary"
            type="button"
            :disabled="addBusy"
            @click="submitAdd()"
          >{{ addBusy ? '…' : t('models.addApiSubmit') }}</button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* v0.1.8 nx 化：视觉（卡面/三态/可用性点/槽位字色）在 theme.css
   .nx-page .models-row 系列——此处仅布局。 */
.models-row { align-items: center; flex-wrap: nowrap; gap: 10px; }
.models-icon { font-size: 18px; flex-shrink: 0; }
.models-name { min-width: 150px; flex-shrink: 0; }
.models-slot {
  flex: 1 1 auto;
  min-width: 120px;
  text-align: right;
}
.models-dot {
  display: inline-block;
  width: 8px;
  height: 8px;
  border-radius: 50%;
  margin-right: 4px;
  vertical-align: middle;
}
/* —— ⚙ 高级路由折叠区（v0.1.44）—— */
.models-route-toggle { margin-left: auto; flex-shrink: 0; }
.models-route-panel {
  flex: 1 1 100%;
  margin: 2px 0 10px 28px;
  padding: 10px 12px;
  border-left: 2px solid var(--nx-border, rgba(128, 128, 128, 0.35));
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.models-route-hint { margin-bottom: 2px; }
.models-route-empty { padding: 2px 0; }
.models-route-row {
  align-items: flex-end;
  flex-wrap: wrap;
  gap: 8px;
}
.models-route-task { flex: 1 1 210px; }
.models-route-src { flex: 0 1 110px; }
.models-route-ch { flex: 1 1 170px; }
.models-route-model { flex: 1 1 150px; }
.models-route-local-hint { flex: 1 1 auto; align-self: center; }
.models-route-ops { display: flex; gap: 4px; flex-shrink: 0; }
.models-route-actions { display: flex; align-items: center; gap: 8px; }
.models-route-agent { margin-top: 2px; }
@media (max-width: 720px) {
  .models-row { flex-wrap: wrap; }
  .models-name { min-width: 120px; }
  .models-slot { text-align: left; flex-basis: 100%; }
  .models-route-panel { margin-left: 0; }
}
</style>
