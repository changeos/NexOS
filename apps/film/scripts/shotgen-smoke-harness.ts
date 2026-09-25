// =============================================================================
// shotgen-smoke-harness.ts —— 短片生成 ShotGen 冒烟 harness（v0.1.45，happy-dom）。
//
// 由 scripts/shotgen-smoke.mjs 用 vite（plugin-vue 编译 .vue）构建为临时 ESM，
// happy-dom 全局 + mock 宿主桥（__NEXOS_HOST__.api——shotgen 三端点 + files
// download 信封）就绪后 import 并调用 run()：
//   ① shotgen.ts 纯函数（状态归一 / 种子解析 / 表单校验 / 作品筛选 / 提示词
//      折叠 / 下载名 / 画幅五档含 21:9 / 时长四档）+ flowFiles 深链
//      view=shotgen 布尔位（p 并存语义）；
//   ② 引擎全链（真实 api 层 + mock 桥）：提交 body 契约（无 seed 不带字段/
//      文生档无 image_b64）→ 乐观入列 → 本地轮询到终态 → 产物 url 惰性转
//      data URL；**toast 静默**（全程 useToastState 空 + 不触 /film/tasks）；
//      防重复锁（生成中再 submit no-op）；任务失败归档（作品卡 error，可再
//      提交）；提交失败红条（POST 500）；
//   ③ 图生档：参考图类型/超限校验 + image_b64 随请求发出 + 无图校验拦截；
//   ④ ShotGen.vue 真挂载：双 Tab / 模型标识 / 模式切换 / 灵感 chips 回填
//      （结构化长提示词）/ 画幅时长选择 / 生成按钮（模型名 + 置灰口径）/
//      video 位未配黄条 + 去配置/高级模式 emit / 作品瀑布流（状态标签 /
//      点击装载播放器 http 与 data URL 两形态 / 失败红字 / 复制提示词 /
//      以此再生成回填）。
// 断言在 .mjs（node:assert）；观测面经返回值 / globalThis.__FLOW_SMOKE__。
// =============================================================================
import { createApp, nextTick, type Component } from 'vue'
import { createI18n } from 'vue-i18n'
import ShotGen from '../src/flow/ShotGen.vue'
import zhCN from '../src/i18n/zh-CN.json'
import { useToastState } from '../src/nx/toast'
import {
  SHOTGEN_DURATIONS,
  SHOTGEN_ENGINE_KEY,
  SHOTGEN_IMAGE_MAX_BYTES,
  SHOTGEN_INSPIRATIONS,
  SHOTGEN_PROMPT_MAX,
  SHOTGEN_RATIOS,
  createShotgenEngine,
  filterShotgenWorks,
  normalizeShotgenStatus,
  parseSeedInput,
  shotgenDownloadName,
  shotgenPromptLong,
  validateShotgenForm,
  type ShotgenEngine,
} from '../src/flow/shotgen'
import { parseStandaloneQuery } from '../src/flow/flowFiles'
import {
  fetchFileDataUrl,
  filmShotgenCreate,
  filmShotgenGet,
  filmShotgenList,
} from '../src/api'

/** 与 .mjs 的共享观测面（mjs 注入；calls 记 mock api 全部调用）。 */
interface Smock {
  calls: { method: string; path: string; body?: unknown }[]
  failNextCreate: boolean
}
function smock(): Smock {
  return (globalThis as { __FLOW_SMOKE__: Smock }).__FLOW_SMOKE__
}

/** i18n（真实 vue-i18n zh-CN——文案断言同一键径）。 */
const i18n = createI18n({
  legacy: false,
  locale: 'zh-CN',
  fallbackLocale: 'zh-CN',
  messages: { 'zh-CN': zhCN as Record<string, unknown> },
  missingWarn: false,
  fallbackWarn: false,
})

function mountApp(
  comp: Component,
  provides: [symbol, unknown][] = [],
  rootProps?: Record<string, unknown>,
) {
  const el = document.createElement('div')
  document.body.appendChild(el)
  const app = createApp(comp, rootProps)
  app.use(i18n)
  for (const [k, v] of provides) app.provide(k as never, v)
  app.mount(el)
  return { app, el }
}

async function flush(ms = 10): Promise<void> {
  await new Promise((r) => setTimeout(r, ms))
  await nextTick()
}

/** 轮询等待谓词成立（超时抛错——测试口径）。 */
async function waitUntil(pred: () => boolean, tries = 80, ms = 10): Promise<void> {
  for (let i = 0; i < tries; i++) {
    if (pred()) return
    await flush(ms)
  }
  throw new Error('waitUntil 超时')
}

/** mock 引擎 deps：真实 api 层（走 mock 宿主桥）+ 5ms 轮询。 */
function makeDeps() {
  return {
    create: filmShotgenCreate,
    list: filmShotgenList,
    get: filmShotgenGet,
    resolveMedia: fetchFileDataUrl,
    errMsg: (e: unknown) => String(e),
    pollMs: 5,
  }
}

function postCalls(): { method: string; path: string; body?: unknown }[] {
  return smock().calls.filter((c) => c.method === 'POST' && c.path === '/api/v1/film/shotgen')
}

function findBtn(el: HTMLElement, text: string): HTMLButtonElement | undefined {
  return Array.from(el.querySelectorAll('button')).find((b) =>
    (b.textContent ?? '').includes(text),
  ) as HTMLButtonElement | undefined
}

export interface ShotgenSmokeResult {
  pure: {
    statusNorm: string[]
    seed: (number | null | typeof Number.NaN)[]
    form: string[]
    worksFilter: number
    promptLong: boolean
    downloadName: string
    ratios: string[]
    durations: number[]
    promptMax: number
    inspCount: number
    deepLink: { shotgenOnly: boolean; withProject: boolean; studioView: boolean }
  }
  engine: {
    initialWorks: number
    submitBody: Record<string, unknown>
    optimisticQueued: boolean
    dupBlocked: boolean
    completed: boolean
    videoDataUrl: string
    toastSilent: boolean
    noTaskCenterCalls: boolean
    clipboardText: string
    refill: { prompt: string; seed: string; tab: string }
    failedWork: { status: string; error: string }
    canSubmitAfterFail: boolean
    submitErrorShown: boolean
    worksAfterSubmitFail: number
  }
  imageMode: {
    badType: string
    tooLarge: string
    previewPrefix: string
    bodyHasImage: boolean
    noImageBlocked: boolean
  }
  render: {
    tabs: string[]
    ratioLabels: string[]
    activeRatio: string
    ratioSwitch: string
    durationLabels: string[]
    genLabel: string
    genDisabledWhenVideoMissing: boolean
    genEnabledAfterPrompt: boolean
    warnBar: boolean
    advancedEmitted: boolean
    goModelsEmitted: boolean
    inspChips: number
    inspFilled: boolean
    worksCards: number
    statusLabels: string[]
    playHttpUrl: string
    playDataUrl: string
    failedCardText: string
    copiedFeedback: string
    refillSwitched: boolean
  }
}

export async function run(): Promise<ShotgenSmokeResult> {
  const out = {} as ShotgenSmokeResult

  // ============ ① 纯函数（shotgen.ts + flowFiles 深链） ============
  {
    out.pure = {
      statusNorm: [
        normalizeShotgenStatus('done'),
        normalizeShotgenStatus('error'),
        normalizeShotgenStatus('running'),
        normalizeShotgenStatus(''),
      ],
      seed: [
        parseSeedInput(''),
        parseSeedInput('0'),
        parseSeedInput('42'),
        parseSeedInput('-1'),
        parseSeedInput('1.5'),
        parseSeedInput('abc'),
      ],
      form: [
        validateShotgenForm({ prompt: '  ', seedRaw: '', mode: 'text', imageReady: false }),
        validateShotgenForm({ prompt: 'ok', seedRaw: 'x', mode: 'text', imageReady: false }),
        validateShotgenForm({ prompt: 'ok', seedRaw: '', mode: 'image', imageReady: false }),
        validateShotgenForm({ prompt: 'ok', seedRaw: '8', mode: 'image', imageReady: true }),
      ],
      worksFilter: filterShotgenWorks(
        [
          { id: 'a', status: 'completed' },
          { id: 'b', status: 'running' },
          { id: 'c', status: 'queued' },
          { id: 'd', status: 'error' },
        ],
        'generating',
      ).length,
      promptLong: shotgenPromptLong(`0-3s x\n3-6s y\n6-10s z`),
      downloadName: shotgenDownloadName('sg 1/2'),
      ratios: SHOTGEN_RATIOS.map((r) => r.ratio),
      durations: [...SHOTGEN_DURATIONS],
      promptMax: SHOTGEN_PROMPT_MAX,
      inspCount: SHOTGEN_INSPIRATIONS.length,
      deepLink: {
        shotgenOnly: parseStandaloneQuery('?view=shotgen').shotgen,
        withProject: (() => {
          const d = parseStandaloneQuery('?p=x1&view=shotgen')
          return d.projectId === 'x1' && d.shotgen && d.view === ''
        })(),
        studioView: (() => {
          const d = parseStandaloneQuery('?p=x2&view=workbench')
          return !d.shotgen && d.view === 'workbench'
        })(),
      },
    }
  }

  // 剪贴板 mock（copyText 优先走 navigator.clipboard）
  let clipText = ''
  try {
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: async (txt: string) => { clipText = txt } },
      configurable: true,
    })
  } catch {
    /* execCommand 降级路径兜底 */
  }

  // ============ ② 引擎全链（真实 api 层 + mock 桥；toast 静默口径） ============
  {
    const engine = createShotgenEngine(makeDeps())
    engine.start()
    await waitUntil(() => !engine.worksLoading.value)
    const initialWorks = engine.works.value.length

    engine.form.prompt = '雪山之巅的仙侠对决'
    engine.form.duration = 10
    engine.form.seedRaw = '42'
    const before = postCalls().length
    await engine.submit()
    const submitBody = postCalls()[before]?.body as Record<string, unknown>
    const optimistic = engine.works.value.find((w) => w.prompt === '雪山之巅的仙侠对决')
    const optimisticQueued = normalizeShotgenStatus(optimistic?.status) === 'queued'
    const dupLocked = !engine.canSubmit.value
    await engine.submit() // 生成中防重复：静默 no-op
    const dupNoPost = postCalls().length === before + 1

    await waitUntil(() => {
      const w = engine.works.value.find((x) => x.prompt === '雪山之巅的仙侠对决')
      return !!w && normalizeShotgenStatus(w.status) === 'completed'
    })
    const doneWork = engine.works.value.find((w) => w.prompt === '雪山之巅的仙侠对决')!
    const videoDataUrl = await engine.ensureVideoUrl(doneWork)
    const toastSilent = useToastState().items.length === 0
    const noTaskCenterCalls = !smock().calls.some((c) => c.path.startsWith('/api/v1/film/tasks'))

    await engine.copyPrompt(doneWork)
    if (!clipText) clipText = 'COPY_FAILED'

    engine.form.tab = 'works'
    engine.refill(doneWork)
    const refill = {
      prompt: engine.form.prompt,
      seed: engine.form.seedRaw,
      tab: engine.form.tab,
    }

    // 任务失败路径（mock：prompt=电影公路 的任务固定失败）
    engine.form.prompt = '电影公路'
    engine.form.seedRaw = ''
    await engine.submit()
    await waitUntil(() => {
      const w = engine.works.value.find((x) => x.prompt === '电影公路')
      return !!w && normalizeShotgenStatus(w.status) === 'failed'
    })
    const failedWork = engine.works.value.find((w) => w.prompt === '电影公路')!
    const canSubmitAfterFail = engine.canSubmit.value

    // 提交失败路径（POST 500 → 页内红条，不乐观入列）
    smock().failNextCreate = true
    engine.form.prompt = '再来一条'
    const worksBefore = engine.works.value.length
    await engine.submit()
    const submitErrorShown = engine.submitError.value.length > 0
    const worksAfterSubmitFail = engine.works.value.length - worksBefore
    smock().failNextCreate = false

    out.engine = {
      initialWorks,
      submitBody,
      optimisticQueued,
      dupBlocked: dupLocked && dupNoPost,
      completed: normalizeShotgenStatus(doneWork.status) === 'completed',
      videoDataUrl,
      toastSilent,
      noTaskCenterCalls,
      clipboardText: clipText,
      refill,
      failedWork: { status: failedWork.status ?? '', error: (failedWork.error ?? '').split('\n')[0] },
      canSubmitAfterFail,
      submitErrorShown,
      worksAfterSubmitFail,
    }
    engine.dispose()
  }

  // ============ ③ 图生档（参考图校验 + image_b64 契约） ============
  {
    const engine = createShotgenEngine(makeDeps())
    engine.start()
    await waitUntil(() => !engine.worksLoading.value)

    const badType = new File([new Uint8Array([1])], 'a.gif', { type: 'image/gif' })
    await engine.setImage(badType)
    const badTypeErr = engine.imageError.value

    const big = new File([new Uint8Array(SHOTGEN_IMAGE_MAX_BYTES + 1)], 'big.png', { type: 'image/png' })
    await engine.setImage(big)
    const tooLargeErr = engine.imageError.value

    const png = new File([new Uint8Array([137, 80, 78, 71])], 'ref.png', { type: 'image/png' })
    await engine.setImage(png)
    const previewPrefix = engine.imageError.value === '' ? engine.imagePreview.value.slice(0, 22) : 'ERR'

    engine.form.mode = 'image'
    engine.form.prompt = '图生视频：巨鲸跃出云海'
    const before = postCalls().length
    await engine.submit()
    const bodyHasImage = !!(postCalls()[before]?.body as { image_b64?: string } | undefined)?.image_b64
    await waitUntil(() => {
      const w = engine.works.value.find((x) => x.prompt === '图生视频：巨鲸跃出云海')
      return !!w && normalizeShotgenStatus(w.status) === 'completed'
    })

    // 无图校验：清图 + 换提示词 → formError='image'，submit no-op
    engine.form.prompt = '另一条'
    await engine.setImage(null)
    const postBefore = postCalls().length
    await engine.submit()
    const noImageBlocked = engine.formError.value === 'image' && postCalls().length === postBefore

    out.imageMode = {
      badType: badTypeErr,
      tooLarge: tooLargeErr,
      previewPrefix,
      bodyHasImage,
      noImageBlocked,
    }
    // 组件段复用本引擎（全部终态、无进行中）
    out.render = await renderSection(engine)
    engine.dispose()
  }

  return out
}

// ============ ④ ShotGen.vue 真挂载（引擎注入 + happy-dom 交互） ============
async function renderSection(engine: ShotgenEngine): Promise<ShotgenSmokeResult['render']> {
  // 挂载前复位创作表单（③ 段引擎停在图生档无图态——formError 会误压按钮断言）
  engine.form.mode = 'text'
  engine.form.prompt = ''
  engine.form.seedRaw = ''
  engine.form.tab = 'create'
  const advanced: string[] = []
  const goModels: string[] = []
  const rootProps = (videoReady: boolean) => ({
    videoReady,
    modelName: 'MiniMax H3',
    isOffline: false,
    onAdvanced: () => advanced.push('x'),
    onGoModels: () => goModels.push('x'),
  })
  const mount = (videoReady: boolean) =>
    mountApp(ShotGen as unknown as Component, [[SHOTGEN_ENGINE_KEY, engine]], rootProps(videoReady))

  // —— 降级挂载（video 位未配）：黄条 + 双 emit + 生成置灰 ——
  const deg = mount(false)
  await flush()
  const warnBar = !!deg.el.querySelector('.sg-warn')
  const genDisabledWhenVideoMissing = findBtn(deg.el, '生成视频')?.disabled === true
  findBtn(deg.el, '去配置')?.click()
  findBtn(deg.el, '高级模式')?.click()
  await flush()
  deg.app.unmount()

  // —— 正常挂载：创作页全量 ——
  const m = mount(true)
  await flush()
  const el = m.el
  const tabs = Array.from(el.querySelectorAll('.sg-tab')).map((n) => (n.textContent ?? '').trim())
  const ratioBtns = Array.from(el.querySelectorAll('.sg-ratio')) as HTMLButtonElement[]
  const ratioLabels = ratioBtns.map((b) => (b.querySelector('.sg-ratio-label')?.textContent ?? '').trim())
  const activeRatio = (
    ratioBtns.find((b) => b.classList.contains('active'))?.querySelector('.sg-ratio-label')?.textContent ?? ''
  ).trim()
  const durationLabels = Array.from(el.querySelectorAll('.sg-dur')).map((b) => (b.textContent ?? '').trim())
  const gen = findBtn(el, '生成视频')
  const genLabel = (gen?.textContent ?? '').trim()

  // 提示词空 → 置灰；输入后 → 可用
  const ta = el.querySelector('.sg-prompt') as HTMLTextAreaElement
  const disabledEmpty = gen?.disabled === true
  ta.value = '云上巨鲸：一头发光的巨鲸游过晚霞中的云海'
  ta.dispatchEvent(new Event('input', { bubbles: true }))
  await flush()
  const genEnabledAfterPrompt = findBtn(el, '生成视频')?.disabled === false && disabledEmpty

  // 灵感 chips：点「赛博雨夜」回填结构化长提示词（含 0-3s 时间轴）
  const chips = Array.from(el.querySelectorAll('.sg-chip')) as HTMLButtonElement[]
  const cyberChip = chips.find((b) => (b.textContent ?? '').includes('赛博雨夜'))
  cyberChip?.click()
  await flush()
  const inspFilled = ta.value.includes('0-3s') && ta.value.includes('赛博朋克雨夜')

  // 画幅切换（21:9 生效）
  const r219 = ratioBtns.find(
    (b) => (b.querySelector('.sg-ratio-label')?.textContent ?? '').trim() === '21:9',
  )
  r219?.click()
  await flush()
  const ratioSwitch = engine.form.ratio

  // —— 作品页 ——
  const worksTab = Array.from(el.querySelectorAll('.sg-tab')).find((b) =>
    (b.textContent ?? '').includes('作品'),
  ) as HTMLButtonElement
  worksTab.click()
  await flush()
  const cards = Array.from(el.querySelectorAll('.sg-card')) as HTMLElement[]
  const statusLabels = cards.map((c) => (c.querySelector('.sg-media-status')?.textContent ?? '').trim())
  // sg-0（http URL 直连）点击装载 → <video src=https://…>
  const httpCard = cards.find((c) =>
    (c.querySelector('.sg-prompt-text')?.textContent ?? '').includes('云上巨鲸：一头'),
  )
  ;(httpCard?.querySelector('.sg-media') as HTMLElement).click()
  await flush(30)
  const playHttpUrl = (httpCard?.querySelector('video')?.getAttribute('src') ?? '').trim()
  // sg-1（产物路径 → files/download 信封 → data URL；引擎侧已缓存）
  const dataCard = cards.find((c) =>
    (c.querySelector('.sg-prompt-text')?.textContent ?? '').includes('雪山之巅'),
  )
  ;(dataCard?.querySelector('.sg-media') as HTMLElement).click()
  await flush(30)
  const playDataUrl = (dataCard?.querySelector('video')?.getAttribute('src') ?? '').trim()
  // 失败卡：首行原因红字
  const failedCard = cards.find((c) => c.classList.contains('is-failed'))
  const failedCardText = (failedCard?.querySelector('.sg-card-err')?.textContent ?? '').trim()
  // 复制提示词 → ✓ 已复制
  const copyBtn = findBtn(cards[0], '复制提示词')
  copyBtn?.click()
  await flush(20)
  const copiedFeedback = (copyBtn?.textContent ?? '').includes('已复制') ? '已复制' : ''
  // 以此再生成 → 回创作页 + 回填
  const reBtn = findBtn(dataCard as HTMLElement, '以此再生成')
  reBtn?.click()
  await flush()
  const createTabActive =
    (
      Array.from(el.querySelectorAll('.sg-tab')).find((b) =>
        (b.textContent ?? '').includes('创作'),
      )?.classList.contains('active') ?? false
    ) && (el.querySelector('.sg-prompt') as HTMLTextAreaElement).value.includes('雪山之巅')

  const result: ShotgenSmokeResult['render'] = {
    tabs,
    ratioLabels,
    activeRatio,
    ratioSwitch,
    durationLabels,
    genLabel,
    genDisabledWhenVideoMissing,
    genEnabledAfterPrompt,
    warnBar,
    advancedEmitted: advanced.length === 1,
    goModelsEmitted: goModels.length === 1,
    inspChips: chips.length,
    inspFilled,
    worksCards: cards.length,
    statusLabels,
    playHttpUrl,
    playDataUrl,
    failedCardText,
    copiedFeedback,
    refillSwitched: createTabActive,
  }
  m.app.unmount()
  return result
}
