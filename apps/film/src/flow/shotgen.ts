// =============================================================================
// shotgen.ts —— 短片生成 ShotGen（v0.1.45 应用侧，film 0.1.13）。
//
// 对标海螺 H3 创作页的**直连 video 渠道单段生成器**：不建项目、不走
// 分镜/生图/合成管线——一次 POST /api/v1/film/shotgen（stage=shotgen 任务）
// 直出一段视频；历史=服务端作品流（GET /film/shotgen），不再用 localStorage。
//
// 本文件两层：
//   · 纯函数 / 常量（画幅五档、时长四档、提示词上限、种子解析、表单校验、
//     状态归一、作品筛选、复制 helper）——页面与 happy-dom 冒烟共用同一代码
//     路径（照 flowFiles.ts 先例）；
//   · createShotgenEngine（工厂闭包，reactive）：提交（防重复 in-flight 锁）→
//     乐观入列 → 本地轮询 GET :id 到终态 → 产物 url 惰性转 data URL 可播。
//     **toast 静默口径**：引擎全程不进任务中心（addTracked/trackFilmTask/
//     taskNotifier 一概不调）——子任务反馈只用作品卡状态标签 + 页内红条，
//     不落全局 toast（每个子任务一条 toast 对单段生成器太吵）。
//
// 引擎由 FilmStudio 创建并 provide（SHOTGEN_ENGINE_KEY）——跨「高级模式」
// 切换存活（模式切换不卸载根组件，轮询不中断）；deps 注入 HTTP 面 + 错误
// 文案 + 轮询间隔（冒烟 5ms）——冒烟与真机同一状态机。
// =============================================================================

import { computed, inject, reactive, ref, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import {
  readFileAsDataUrl,
  splitDataUrl,
  type FilmShotgenCreateBody,
  type FilmShotgenWork,
  type FilmTask,
} from '../api'

// —— 常量（纯数据；页面与冒烟共用） ——

/** 创作页视图 Tab：create 创作 / works 作品。 */
export type ShotgenTab = 'create' | 'works'

/** 生成模式：text 文生视频 / image 图生视频（参考图）。 */
export type ShotgenMode = 'text' | 'image'

/** 作品状态（后端 done/error 归一为 completed/failed）。 */
export type ShotgenWorkStatus = 'queued' | 'running' | 'completed' | 'failed'

/** 画幅五档（16:9 / 9:16 / 1:1 / 4:3 / 21:9；width×height 为展示分辨率——
 *  实际输出画质由渠道原生决定，前端只作参数传递与图标比例）。 */
export interface ShotgenRatio {
  ratio: string
  width: number
  height: number
}

export const SHOTGEN_RATIOS: readonly ShotgenRatio[] = [
  { ratio: '16:9', width: 1920, height: 1080 },
  { ratio: '9:16', width: 1080, height: 1920 },
  { ratio: '1:1', width: 1080, height: 1080 },
  { ratio: '4:3', width: 1440, height: 1080 },
  { ratio: '21:9', width: 2520, height: 1080 },
]

/** 时长四档（秒）。 */
export const SHOTGEN_DURATIONS: readonly number[] = [5, 8, 10, 15]

/** 缺省画幅 / 缺省时长。 */
export const SHOTGEN_DEFAULT_RATIO = '16:9'
export const SHOTGEN_DEFAULT_DURATION = 5

/** 提示词上限（字符，码点口径由 v-model 天然保证；超限截断由 maxlength 兜底）。 */
export const SHOTGEN_PROMPT_MAX = 3000

/** 图生视频参考图上限（10MB，与定妆图/参考图导入同口径）。 */
export const SHOTGEN_IMAGE_MAX_BYTES = 10 * 1024 * 1024

/** 参考图允许类型（png/jpeg/webp；与既有上传口径一致）。 */
const SHOTGEN_IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/webp']

/**
 * 灵感 chips（点击回填预置示例提示词；含 2 个结构化长提示词——时间轴分镜式，
 * 即用户实际用法）。键尾 → i18n `shotgen.insp<Key>`（chip 名）/ `shotgen.insp<Key>Prompt`
 * （回填提示词正文，各语言本地化）。
 */
export const SHOTGEN_INSPIRATIONS = [
  { key: 'Road', structured: false },
  { key: 'Cyber', structured: true },
  { key: 'Whale', structured: false },
  { key: 'Xianxia', structured: true },
] as const

// —— 纯函数 ——

/** 后端原始状态 → 前端归一（done→completed / error→failed / 其余透传；空=queued）。 */
export function normalizeShotgenStatus(raw: string | null | undefined): ShotgenWorkStatus {
  const s = (raw ?? '').trim()
  if (s === 'completed' || s === 'done') return 'completed'
  if (s === 'failed' || s === 'error') return 'failed'
  if (s === 'running') return 'running'
  return 'queued'
}

/** 是否进行中（生成中口径：queued/running——筛选徽章与防重复锁共用）。 */
export function shotgenWorkActive(s: ShotgenWorkStatus): boolean {
  return s === 'queued' || s === 'running'
}

/** 种子输入解析：''/留空 → null（不传=随机）；'0' → 0（显式随机）；非负整数
 *  通过；其余（负数 / 小数 / 非数字）→ NaN（非法——表单红字）。纯函数。 */
export function parseSeedInput(raw: string): number | null {
  const t = raw.trim()
  if (!t) return null
  if (!/^\d+$/.test(t)) return Number.NaN
  return Number(t)
}

/** 表单校验（纯函数；返回错误码：''=通过 / 'prompt' / 'seed' / 'image'）。 */
export function validateShotgenForm(input: {
  prompt: string
  seedRaw: string
  mode: ShotgenMode
  imageReady: boolean
}): '' | 'prompt' | 'seed' | 'image' {
  if (!input.prompt.trim()) return 'prompt'
  if (Number.isNaN(parseSeedInput(input.seedRaw))) return 'seed'
  if (input.mode === 'image' && !input.imageReady) return 'image'
  return ''
}

/** 作品筛选（works 视图 Tab：all 全部 / generating 生成中）。 */
export function filterShotgenWorks(
  works: FilmShotgenWork[],
  filter: 'all' | 'generating',
): FilmShotgenWork[] {
  if (filter === 'all') return works
  return works.filter((w) => shotgenWorkActive(normalizeShotgenStatus(w.status)))
}

/** 提示词是否需要折叠（超长提示词卡内默认收 3 行，可展开——纯阈值口径）。 */
export function shotgenPromptLong(prompt: string): boolean {
  return Array.from(prompt ?? '').length > 60 || (prompt ?? '').split('\n').length > 2
}

/** 复制文本到剪贴板（navigator.clipboard 优先；降级 textarea+execCommand；
 *  返回是否成功——冒烟可断言；失败页面退化为「手动全选」提示不阻断）。 */
export async function copyText(text: string): Promise<boolean> {
  try {
    const nav = navigator as Navigator & { clipboard?: { writeText(t: string): Promise<void> } }
    if (nav.clipboard?.writeText) {
      await nav.clipboard.writeText(text)
      return true
    }
  } catch {
    /* 降级路径 */
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    const ok = document.execCommand('copy')
    ta.remove()
    return ok
  } catch {
    return false
  }
}

/** 下载名（shotgen-<id>.mp4；id 含非法文件名字符时折叠）。 */
export function shotgenDownloadName(id: string): string {
  const safe = id.replace(/[^\w.-]+/g, '_')
  return `shotgen-${safe || 'video'}.mp4`
}

// —— 引擎 ——

/** 引擎 HTTP 依赖（FilmStudio 注入真机 api；冒烟注入 mock）。 */
export interface ShotgenEngineDeps {
  /** POST /film/shotgen（提交生成）。 */
  create(body: FilmShotgenCreateBody): Promise<FilmTask>
  /** GET /film/shotgen（作品列表，服务端历史）。 */
  list(): Promise<FilmShotgenWork[]>
  /** GET /film/shotgen/:id（详情+产物 url；轮询）。 */
  get(id: string): Promise<FilmShotgenWork>
  /** 产物路径 → 可播 URL（http(s) 直通 / 路径走 files/download 信封）。 */
  resolveMedia(urlOrPath: string): Promise<string>
  /** 统一错误文案。 */
  errMsg(e: unknown): string
  /** 轮询间隔（缺省 3000ms；冒烟 5ms）。 */
  pollMs?: number
}

/** 表单态（创作页可编辑面）。 */
export interface ShotgenFormState {
  tab: ShotgenTab
  mode: ShotgenMode
  prompt: string
  ratio: string
  duration: number
  seedRaw: string
}

/** 引擎实例面（ShotGen.vue 消费；FilmStudio provide）。 */
export interface ShotgenEngine {
  // —— 表单（创作页）——
  readonly form: ShotgenFormState
  /** 图生参考图（预览 data URL；''=未选）。 */
  readonly imagePreview: Ref<string>
  /** 图生参考图 b64（无 data: 前缀；''=未选）。 */
  readonly imageB64: Ref<string>
  /** 参考图选取错误（超限/类型不符；''=无）。 */
  readonly imageError: Ref<string>
  // —— 提交面 ——
  /** 提交请求进行中（POST 在途）。 */
  readonly submitting: Ref<boolean>
  /** 提交失败文案（''=无；页内红条）。 */
  readonly submitError: Ref<string>
  /** 最近一次提交成功的工作 id（''=无；驱动「已入队」文案）。 */
  readonly lastQueuedId: Ref<string>
  // —— 作品流 ——
  /** 作品列表（归一后，新→旧）。 */
  readonly works: Ref<FilmShotgenWork[]>
  /** 列表加载中 / 列表错误。 */
  readonly worksLoading: Ref<boolean>
  readonly worksError: Ref<string>
  /** 作品筛选 Tab。 */
  readonly worksFilter: Ref<'all' | 'generating'>
  /** 已解析可播 URL 缓存（作品 id → data URL / http URL）。 */
  readonly videoUrls: Readonly<Record<string, string>>
  /** 展开提示词的作品 id 集合（卡内全文展开态）。 */
  readonly expanded: { has(id: string): boolean; toggle(id: string): void }
  // —— 派生 ——
  /** 进行中作品数（防重复锁 + Tab 徽章）。 */
  readonly generatingCount: ComputedRef<number>
  /** 表单校验错误码（''=通过）。 */
  readonly formError: ComputedRef<'' | 'prompt' | 'seed' | 'image'>
  /** 可提交（校验通过且不在 in-flight 锁内）。 */
  readonly canSubmit: ComputedRef<boolean>
  // —— 动作 ——
  /** 选/清参考图（File → 校验 → data URL 预览 + b64；null=清除）。 */
  setImage(file: File | null): Promise<void>
  /** 提交生成（防重复：submitting 或有进行中作品时静默 no-op）。 */
  submit(): Promise<void>
  /** 刷新作品列表（服务端历史）。 */
  refreshWorks(): Promise<void>
  /** 惰性解析作品视频 URL（缓存；失败抛出——调用方页内红字）。 */
  ensureVideoUrl(w: FilmShotgenWork): Promise<string>
  /** 下载作品视频（ensureVideoUrl → 浏览器另存）。 */
  download(w: FilmShotgenWork): Promise<void>
  /** 复制作品提示词（剪贴板；返回成败）。 */
  copyPrompt(w: FilmShotgenWork): Promise<boolean>
  /** 基于此作品再生成（回填创作页：提示词/画幅/时长/种子 + 切创作 Tab）。 */
  refill(w: FilmShotgenWork): void
  /** 启动（幂等：拉一次作品列表 + 恢复轮询进行中作品）。 */
  start(): void
  /** 释放轮询计时器。 */
  dispose(): void
}

/** provide/inject 键。 */
export const SHOTGEN_ENGINE_KEY: InjectionKey<ShotgenEngine> = Symbol('film-shotgen-engine')

/** 便捷注入（缺上下文返回 null——组件降级为空态不崩）。 */
export function useShotgenEngine(): ShotgenEngine | null {
  return inject(SHOTGEN_ENGINE_KEY, null)
}

/**
 * 建短片生成引擎（FilmStudio 调用一次并 provide；模式切换不销毁）。
 * toast 静默：本引擎不触碰任务中心 / 全局 toast——全部反馈落在页面内
 * （作品状态标签 / 红条文案），与 ShotGen 极简形态一致。
 */
export function createShotgenEngine(deps: ShotgenEngineDeps): ShotgenEngine {
  const pollMs = deps.pollMs ?? 3000

  const form = reactive<ShotgenFormState>({
    tab: 'create',
    mode: 'text',
    prompt: '',
    ratio: SHOTGEN_DEFAULT_RATIO,
    duration: SHOTGEN_DEFAULT_DURATION,
    seedRaw: '',
  })

  const imagePreview = ref('')
  const imageB64 = ref('')
  const imageError = ref('')

  const submitting = ref(false)
  const submitError = ref('')
  const lastQueuedId = ref('')

  const works = ref<FilmShotgenWork[]>([])
  const worksLoading = ref(false)
  const worksError = ref('')
  const worksFilter = ref<'all' | 'generating'>('all')
  const videoUrls = reactive<Record<string, string>>({})
  const expandedSet = reactive(new Set<string>())

  let pollTimer: ReturnType<typeof setInterval> | null = null
  let pollInFlight = false
  let started = false

  const norm = (w: FilmShotgenWork[]): FilmShotgenWork[] =>
    w
      .filter((x) => !!x && typeof x === 'object')
      .map((x) => ({ ...x, status: normalizeShotgenStatus(x.status) }))

  const generatingCount = computed(
    () => works.value.filter((w) => shotgenWorkActive(normalizeShotgenStatus(w.status))).length,
  )

  const formError = computed(() =>
    validateShotgenForm({
      prompt: form.prompt,
      seedRaw: form.seedRaw,
      mode: form.mode,
      imageReady: !!imageB64.value,
    }),
  )

  const canSubmit = computed(() => !submitting.value && generatingCount.value === 0 && formError.value === '')

  async function setImage(file: File | null): Promise<void> {
    imageError.value = ''
    if (!file) {
      imagePreview.value = ''
      imageB64.value = ''
      return
    }
    if (!SHOTGEN_IMAGE_TYPES.includes(file.type)) {
      imagePreview.value = ''
      imageB64.value = ''
      imageError.value = 'type'
      return
    }
    if (file.size > SHOTGEN_IMAGE_MAX_BYTES) {
      imagePreview.value = ''
      imageB64.value = ''
      imageError.value = 'size'
      return
    }
    try {
      const dataUrl = await readFileAsDataUrl(file)
      imagePreview.value = dataUrl
      imageB64.value = splitDataUrl(dataUrl).b64
    } catch (e) {
      imagePreview.value = ''
      imageB64.value = ''
      imageError.value = deps.errMsg(e)
    }
  }

  async function refreshWorks(): Promise<void> {
    worksLoading.value = true
    try {
      const raw = await deps.list()
      const list = Array.isArray(raw) ? norm(raw) : []
      // 保守合并：服务端列表为准，但保留本地乐观条目里服务端尚未收录的（提交后
      // 列表异步可见窗口）；同 id 以服务端详情字段较新者（非空）覆盖。
      const byId = new Map(list.map((w) => [w.id, w]))
      for (const local of works.value) {
        if (!byId.has(local.id)) byId.set(local.id, local)
      }
      works.value = [...byId.values()].sort((a, b) => timeOf(b) - timeOf(a))
      worksError.value = ''
      // 有进行中作品 → 起轮询；无 → 停
      if (generatingCount.value > 0) startPolling()
      else stopPolling()
    } catch (e) {
      worksError.value = deps.errMsg(e)
    } finally {
      worksLoading.value = false
    }
  }

  function timeOf(w: FilmShotgenWork): number {
    const v = w.created_at
    if (typeof v === 'number' && Number.isFinite(v)) return v
    if (typeof v === 'string' && v) {
      const ms = Date.parse(v)
      if (!Number.isNaN(ms)) return ms
    }
    return 0
  }

  function mergeDetail(cur: FilmShotgenWork, d: FilmShotgenWork): FilmShotgenWork {
    const merged: FilmShotgenWork = { ...cur }
    for (const k of ['prompt', 'ratio', 'duration_secs', 'seed', 'model', 'video_url', 'video_path', 'error', 'created_at', 'finished_at'] as const) {
      const v = (d as Record<string, unknown>)[k]
      if (v !== undefined && v !== null && v !== '') {
        ;(merged as Record<string, unknown>)[k] = v
      }
    }
    // 状态归一入库（done/error → completed/failed——works 恒为前端口径）
    if (d.status !== undefined && d.status !== null && d.status !== '') {
      merged.status = normalizeShotgenStatus(d.status)
    }
    if (Array.isArray(d.log)) merged.log = [...d.log]
    return merged
  }

  async function submit(): Promise<void> {
    if (!canSubmit.value) return
    const seed = parseSeedInput(form.seedRaw)
    submitting.value = true
    submitError.value = ''
    try {
      const body: FilmShotgenCreateBody = {
        prompt: form.prompt.trim(),
        ratio: form.ratio,
        duration_secs: form.duration,
        ...(seed !== null ? { seed } : {}),
        ...(form.mode === 'image' && imageB64.value ? { image_b64: imageB64.value } : {}),
      }
      const task = await deps.create(body)
      lastQueuedId.value = task.id
      // 乐观入列（服务端列表收录前的即时可见面）
      works.value = [
        {
          id: task.id,
          prompt: body.prompt,
          ratio: body.ratio,
          duration_secs: body.duration_secs,
          seed: body.seed ?? null,
          status: 'queued',
          created_at: Date.now(),
        },
        ...works.value.filter((w) => w.id !== task.id),
      ]
      startPolling()
    } catch (e) {
      submitError.value = deps.errMsg(e)
    } finally {
      submitting.value = false
    }
  }

  function startPolling(): void {
    if (pollTimer !== null) return
    pollTimer = setInterval(() => void tick(), pollMs)
  }

  function stopPolling(): void {
    if (pollTimer !== null) {
      clearInterval(pollTimer)
      pollTimer = null
    }
  }

  async function tick(): Promise<void> {
    if (pollInFlight) return
    const active = works.value.filter((w) => shotgenWorkActive(normalizeShotgenStatus(w.status)))
    if (active.length === 0) {
      stopPolling()
      return
    }
    pollInFlight = true
    try {
      for (const w of active) {
        try {
          const d = await deps.get(w.id)
          const idx = works.value.findIndex((x) => x.id === w.id)
          if (idx >= 0) works.value[idx] = mergeDetail(works.value[idx], d)
        } catch {
          /* 单次轮询失败（瞬时网络/后端重启）：保留条目，下轮再试 */
        }
      }
    } finally {
      pollInFlight = false
    }
    if (generatingCount.value === 0) stopPolling()
  }

  async function ensureVideoUrl(w: FilmShotgenWork): Promise<string> {
    if (videoUrls[w.id]) return videoUrls[w.id]
    const raw = w.video_url || w.video_path || (w as { output?: string | null }).output || ''
    if (!raw) throw new Error('video url missing')
    const url = /^https?:\/\//i.test(raw) ? raw : await deps.resolveMedia(raw)
    videoUrls[w.id] = url
    return url
  }

  async function download(w: FilmShotgenWork): Promise<void> {
    const url = await ensureVideoUrl(w)
    const a = document.createElement('a')
    a.href = url
    a.download = shotgenDownloadName(w.id)
    a.click()
  }

  async function copyPrompt(w: FilmShotgenWork): Promise<boolean> {
    return copyText((w.prompt ?? '').trim())
  }

  function refill(w: FilmShotgenWork): void {
    form.prompt = (w.prompt ?? '').trim()
    const r = SHOTGEN_RATIOS.find((x) => x.ratio === w.ratio)
    if (r) form.ratio = r.ratio
    const d = typeof w.duration_secs === 'number' ? SHOTGEN_DURATIONS.find((x) => x === w.duration_secs) : undefined
    if (d) form.duration = d
    form.seedRaw = typeof w.seed === 'number' && w.seed > 0 ? String(w.seed) : ''
    form.tab = 'create'
    submitError.value = ''
  }

  function start(): void {
    if (started) return
    started = true
    void refreshWorks()
  }

  function dispose(): void {
    stopPolling()
  }

  return {
    form,
    imagePreview,
    imageB64,
    imageError,
    submitting,
    submitError,
    lastQueuedId,
    works,
    worksLoading,
    worksError,
    worksFilter,
    videoUrls,
    expanded: {
      has: (id: string) => expandedSet.has(id),
      toggle: (id: string) => {
        if (expandedSet.has(id)) expandedSet.delete(id)
        else expandedSet.add(id)
      },
    },
    generatingCount,
    formError,
    canSubmit,
    setImage,
    submit,
    refreshWorks,
    ensureVideoUrl,
    download,
    copyPrompt,
    refill,
    start,
    dispose,
  }
}
