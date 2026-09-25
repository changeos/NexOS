// =============================================================================
// toast-smoke-harness.ts —— 全局操作反馈系统冒烟 harness（v0.1.11，happy-dom）。
//
// 由 scripts/toast-smoke.mjs 用 vite（plugin-vue 编译 .vue）构建为临时 ESM，
// happy-dom 全局就绪后 import 并调用 run()：
//   ① toast 单例 API（push/patch/update/dismiss/堆叠上限/loading 双载/时长档）；
//   ② NxToast.vue 渲染（Teleport body 右上角堆叠/aria-live/标题正文/关闭钮）；
//   ③ TaskIndicator（计数徽章+spinner/popover 明细行 X-Y 与耗时/查看全部 emit）；
//   ④ taskToast 纯函数（taskOutcomeSummary 摘要解析 / firstErrorLine 首行）；
//   ⑤ 任务通知器闭环（真实 zh-CN i18n：提交 loading → 进度文案 → 终态 success
//      摘要 / error 首行 + 查看任务动作回调）；
//   ⑥ 保存类操作触发 toast（AudioPage 真挂载：BGM 建条目 → toast.saved）。
// 断言在 .mjs（node:assert）；观测面经返回值 / globalThis.__FLOW_SMOKE__。
// =============================================================================
import { computed, createApp, defineComponent, h, nextTick, ref, type Ref } from 'vue'
import { createI18n } from 'vue-i18n'
import NxToast from '../src/nx/NxToast.vue'
import TaskIndicator from '../src/flow/TaskIndicator.vue'
import AudioPage from '../src/flow/AudioPage.vue'
import zhCN from '../src/i18n/zh-CN.json'
import {
  TOAST_MAX,
  clearToasts,
  dismiss,
  toast,
  useToastState,
  TOAST_DURATION,
  TOAST_ERROR_DURATION,
} from '../src/nx/toast'
import {
  createTaskToastNotifier,
  firstErrorLine,
  taskOutcomeSummary,
} from '../src/flow/taskToast'
import { FLOW_CONTEXT_KEY, PROJECT_DEFAULT_KEY, type FlowContext } from '../src/flow/flowContext'
import type { FlowView } from '../src/flow/flowTypes'

/** 与 .mjs 的共享观测面（mjs 注入；calls 记 mock api 全部调用）。 */
interface Smock {
  calls: { method: string; path: string; body?: unknown }[]
  viewSwitches: string[]
}
function smock(): Smock {
  return (globalThis as { __FLOW_SMOKE__: Smock }).__FLOW_SMOKE__
}

/** i18n（真实 vue-i18n zh-CN——通知文案断言同一键径）。 */
const i18n = createI18n({
  legacy: false,
  locale: 'zh-CN',
  fallbackLocale: 'zh-CN',
  messages: { 'zh-CN': zhCN as Record<string, unknown> },
  missingWarn: false,
  fallbackWarn: false,
})

function mountApp(comp: Parameters<typeof createApp>[0], provides: [symbol | number, unknown][] = []) {
  const el = document.createElement('div')
  document.body.appendChild(el)
  const app = createApp(comp)
  app.use(i18n)
  for (const [k, v] of provides) app.provide(k as never, v)
  app.mount(el)
  return { app, el }
}

/** mock FlowContext（AudioPage 消费面最小集）。 */
const mockModelSel = { chat: 'llm:chat', image: 'local', video: 'ch:c1', tts: 'ch:c1', music: 'ch:c1' }
const navView = ref<FlowView>('audio')
const mockCtx: FlowContext = {
  project: ref({
    id: 'p1',
    title: '冒烟项目',
    idea: '',
    ratio: '16:9',
    dir: '/tank/film/p1',
    created_at: '',
    updated_at: '',
  }),
  optionsFor: () => [
    { label: '网关渠道', options: [{ key: 'ch:c1', label: 'mock-渠道', relay: false }] },
  ],
  hasOptionsFor: () => true,
  modelSel: mockModelSel,
  modelRefFor: (cap) => ({ source: 'channel', channel_id: 'c1', capability: cap as 'chat' }),
  addTracked: () => undefined,
  trackFilmTask: () => undefined,
  storyTasks: computed(() => []),
  errMsg: (e) => String(e),
  refreshTick: ref(0),
  reloadProject: async () => undefined,
  chatAvailable: computed(() => true),
  channelAvailable: computed(() => true),
  composeAvailable: computed(() => true),
  isOffline: computed(() => false),
  stage: ref(''),
  view: navView,
  setView: (v) => {
    navView.value = v
    smock().viewSwitches.push(v)
  },
  ownership: ref(null),
  author: ref('小明'),
  activity: ref([]),
  setAuthor: () => undefined,
  saveOwnership: async () => true,
  refreshCollab: async () => undefined,
  pendingCastSelect: ref(null),
  pendingStoryboardChapter: ref(null) as Ref<string | null>,
  pendingHubFile: ref(null),
  projectModels: ref(null),
  reloadProjectModels: async () => undefined,
  defaultModelSummary: () => '',
  modelSelReady: () => true,
  isProjectDefaultSel: (cap) => mockModelSel[cap] === PROJECT_DEFAULT_KEY,
}

async function flush(ms = 15): Promise<void> {
  await new Promise((r) => setTimeout(r, ms))
  await nextTick()
}

export interface ToastSmokeResult {
  api: {
    stackAfterSix: string[]
    stackNewestFirst: boolean
    loadingSticky: boolean
    loadingPatch: boolean
    updateToSuccess: boolean
    updateMissingIdRepush: boolean
    dismissWorks: boolean
    durations: { success: number; error: number; loading: number }
    autoClosedAfter4s: boolean
    errorClosedManual: boolean
  }
  render: {
    teleported: boolean
    ariaLive: string
    roleError: string
    kindsRendered: string[]
    kindAfterUpdate: string
    titleAndMsg: { title: string; msg: string }
    closeWorks: boolean
    actionButtonWorks: boolean
  }
  indicator: {
    hiddenWhenEmpty: boolean
    badgeCount: string
    spinClass: boolean
    popoverOpens: boolean
    rows: string[]
    hasXy: boolean
    hasElapsed: boolean
    viewAllEmitted: boolean
  }
  summary: {
    storyboard: string
    clean: string
    chapterize: string
    profile: string
    embed: string
    outputFallback: string
    empty: string
    errorFirstLine: string
  }
  notifier: {
    submittedTitle: string
    submittedMsg: string
    progressMsg: string
    progressKindStillLoading: boolean
    terminalSuccessMsg: string
    terminalSuccessKind: string
    terminalSuccessTitle: string
    terminalErrorMsg: string
    terminalErrorTitle: string
    errorActionLabel: string
    actionFired: boolean
  }
  saveClass: {
    savedToastShown: boolean
    bgmPostBody: { info?: { trigger?: string } } | undefined
  }
}

export async function run(): Promise<ToastSmokeResult> {
  const state = useToastState()
  const out = {} as ToastSmokeResult

  // ============ ① 单例 API：堆叠 / 双载 loading / update / 时长档 ============
  {
    clearToasts()
    for (let i = 1; i <= 6; i++) toast.success(`第${i}条`)
    const stackAfterSix = state.items.map((x) => x.message)
    // 新推旧出：最多 4 条，且最新（第6条）在顶
    // loading 建 → patch（进度文案）→ update（终态）
    const id = toast.loading('已提交，完成后通知', '向量化')
    const loadingSticky = state.items[0].kind === 'loading' && state.items[0].duration === 0
    toast.loading(id, '向量化中 3/17')
    const loadingPatch = state.items[0].message === '向量化中 3/17' && state.items[0].kind === 'loading'
    toast.update(id, 'success', '完成：17 块')
    const updateToSuccess = state.items[0].kind === 'success' && state.items[0].message === '完成：17 块'
    // update 到已被关掉的 id → 重落一条（终态不丢）
    dismiss(state.items[0].id)
    toast.update(999999, 'error', '失败：超时')
    const updateMissingIdRepush =
      state.items[0].kind === 'error' && state.items[0].message === '失败：超时'
    const errId = state.items[0].id
    dismiss(errId)
    const dismissWorks = !state.items.some((x) => x.id === errId)
    // 时长档（不实际等 8s——档位断言）
    clearToasts()
    const sId = toast.success('x')
    const eId = toast.error('y')
    const lId = toast.loading('z')
    const durations = {
      success: state.items.find((x) => x.id === sId)!.duration,
      error: state.items.find((x) => x.id === eId)!.duration,
      loading: state.items.find((x) => x.id === lId)!.duration,
    }
    // 自动关（真实等 4s+）：success 到时自灭；error 档位 8s（不整等）
    const autoId = toast.success('自动关验证')
    await new Promise((r) => setTimeout(r, TOAST_DURATION + 350))
    const autoClosedAfter4s = !state.items.some((x) => x.id === autoId)
    const errId2 = toast.error('手动关验证')
    dismiss(errId2)
    const errorClosedManual = !state.items.some((x) => x.id === errId2)
    out.api = {
      stackAfterSix,
      stackNewestFirst: stackAfterSix[0] === '第6条',
      loadingSticky,
      loadingPatch,
      updateToSuccess,
      updateMissingIdRepush,
      dismissWorks,
      durations,
      autoClosedAfter4s,
      errorClosedManual,
    }
    clearToasts()
  }

  // ============ ② NxToast.vue 渲染（Teleport body / aria-live / 关闭钮） ============
  {
    clearToasts()
    const { app } = mountApp(NxToast)
    await flush()
    // 堆叠上限 4：连发 4 条（新在顶：loading 最先）
    toast.error('任务提交失败：404', '生成分镜')
    toast.warning('该文件正在执行〈清理中〉')
    toast.info('提示信息')
    const loadId = toast.loading('已提交，完成后通知', '向量化')
    await flush()
    const host = document.querySelector('.nx-toasts') as HTMLElement | null
    const toasts = Array.from(document.querySelectorAll('.nx-toast'))
    const kindsRendered = toasts.map((n) => (n.className.match(/nx-toast--(\w+)/) ?? [])[1] ?? '')
    const first = toasts[0]
    const titleAndMsg = {
      title: (first.querySelector('.nx-toast__title')?.textContent ?? '').trim(),
      msg: (first.querySelector('.nx-toast__msg')?.textContent ?? '').trim(),
    }
    // loading 翻 success（同一 DOM 条目换 kind 类名）
    toast.update(loadId, 'success', '完成：17 块')
    await flush(5)
    const kindAfterUpdate =
      (document.querySelector('.nx-toast')?.className.match(/nx-toast--(\w+)/) ?? [])[1] ?? ''
    // 关闭钮：关掉一条（success 那条，现在顶部）
    const before = document.querySelectorAll('.nx-toast').length
    ;(document.querySelector('.nx-toast .nx-toast__close') as HTMLButtonElement).click()
    await flush(5)
    const closeWorks = document.querySelectorAll('.nx-toast').length === before - 1
    // 动作钮：error 条带「查看任务」→ 点击触发回调并自关
    let fired = 0
    const actId = toast.error('失败：上游超时', '配音 · 镜头 2', {
      action: { label: '查看任务', onClick: () => { fired++ } },
    })
    await flush(5)
    const errEl = Array.from(document.querySelectorAll('.nx-toast')).find(
      (n) => (n.textContent ?? '').includes('失败：上游超时'),
    ) as HTMLElement
    ;(errEl.querySelector('.nx-toast__action') as HTMLButtonElement).click()
    await flush(5)
    const actionButtonWorks =
      fired === 1 && !useToastState().items.some((x) => x.id === actId)
    const roleError = Array.from(document.querySelectorAll('.nx-toast'))
      .find((n) => n.className.includes('nx-toast--error'))
      ?.getAttribute('role') ?? ''
    out.render = {
      teleported: !!host && !!document.body.contains(host),
      ariaLive: host?.getAttribute('aria-live') ?? '',
      roleError,
      kindsRendered,
      titleAndMsg,
      closeWorks,
      actionButtonWorks,
      kindAfterUpdate,
    }
    app.unmount()
    clearToasts()
  }

  // ============ ③ TaskIndicator（计数 / popover 明细 / 查看全部） ============
  {
    const viewAll: string[] = []
    const IndicatorHost = defineComponent({
      setup() {
        return () =>
          h(TaskIndicator, {
            tasks: [
              { id: 't1', label: '向量化 · sources/novel.txt', createdAt: Date.now() - 65_000, xy: '3/17' },
              { id: 't2', label: '配音 · 镜头 2', createdAt: Date.now() - 5_000, xy: '' },
            ],
            onViewAll: () => viewAll.push('workbench'),
          })
      },
    })
    const { app, el } = mountApp(IndicatorHost)
    await flush(1100) // 1s tick 至少一次（耗时口径刷新）
    const badge = el.querySelector('.task-ind') as HTMLElement | null
    const spinClass = !!badge?.querySelector('.task-ind__spin')
    const badgeCount = (badge?.querySelector('.task-ind__count')?.textContent ?? '').trim()
    ;(badge as HTMLElement).click()
    await flush(5)
    const panel = document.querySelector('.nx-pop__panel') as HTMLElement | null
    const rows = Array.from(panel?.querySelectorAll('.task-ind__row') ?? []).map((n) =>
      (n.textContent ?? '').trim(),
    )
    const allBtn = panel?.querySelector('.task-ind__all') as HTMLButtonElement | null
    allBtn?.click()
    await flush(5)
    const emptyHost = defineComponent({
      setup: () => () => h(TaskIndicator, { tasks: [], onViewAll: () => undefined }),
    })
    const empty = mountApp(emptyHost)
    await flush(5)
    out.indicator = {
      hiddenWhenEmpty: !empty.el.querySelector('.task-ind'),
      badgeCount,
      spinClass,
      popoverOpens: !!panel,
      rows,
      hasXy: rows.some((r) => r.includes('3/17')),
      hasElapsed: rows.some((r) => r.includes('已耗时')),
      viewAllEmitted: viewAll.length === 1,
    }
    app.unmount()
    empty.app.unmount()
  }

  // ============ ④ taskOutcomeSummary / firstErrorLine 纯函数 ============
  {
    out.summary = {
      storyboard: taskOutcomeSummary('storyboard', ['LLM 生成分镜完成：12 个镜头', '模型 local'], null),
      clean: taskOutcomeSummary('story.clean', ['原文 240000 字符', '清理完成：删除 1234 行广告'], null),
      chapterize: taskOutcomeSummary('story.chapterize', ['块 42/42 完成', '分章完成：共 42 章'], null),
      profile: taskOutcomeSummary('story.profile', ['人物梳理完成：识别 8 位人物'], null),
      embed: taskOutcomeSummary('story.embed', ['块 17/17 完成', '原文 240000 字符 → 17 块 × ≤800 字'], null),
      outputFallback: taskOutcomeSummary('compose', ['合成完成'], 'dist/final-v3.mp4'),
      empty: taskOutcomeSummary('story', ['剧情生成：模型 local'], null),
      errorFirstLine: firstErrorLine('LLM 深清块 3/17 失败：上游超时\n第二行堆栈\n第三行'),
    }
  }

  // ============ ⑤ 任务通知器闭环（真实 i18n：提交 → 进度 → 终态） ============
  {
    clearToasts()
    let viewTasksFired = 0
    const notifier = createTaskToastNotifier(i18n.global.t as never, () => {
      viewTasksFired++
    })
    const t = i18n.global.t as (key: string, params?: Record<string, unknown>) => string
    const id = notifier.submitted('story.embed')
    // 注意快照：items 内对象被 update 原地修改——这里立即拷贝字符串值
    const st0 = useToastState().items[0]
    const snap0 = { title: st0.title, message: st0.message }
    notifier.progress(id, 'story.embed', null, ['原文 240000 字符 → 17 块 × ≤800 字', '块 3/17 完成'])
    const st1 = useToastState().items.find((x) => x.id === id)!
    const snap1 = { kind: st1.kind, message: st1.message }
    notifier.terminal(id, {
      id: 'tk-1',
      stage: 'story.embed',
      shot: null,
      status: 'completed',
      logTail: ['块 17/17 完成', '原文 240000 字符 → 17 块 × ≤800 字'],
      output: null,
      error: null,
    })
    const st2 = useToastState().items.find((x) => x.id === id)!
    const snap2 = { kind: st2.kind, message: st2.message, title: st2.title }
    // 失败路径：新提交一条 → 终态 error（首行原因 + 查看任务动作）
    const id2 = notifier.submitted('tts', 2)
    notifier.terminal(id2, {
      id: 'tk-2',
      stage: 'tts',
      shot: 2,
      status: 'failed',
      logTail: [],
      output: null,
      error: '渠道 502 Bad Gateway\n上游超时',
    })
    const st3 = useToastState().items.find((x) => x.id === id2)!
    const snap3 = { kind: st3.kind, message: st3.message, title: st3.title, action: st3.action?.label ?? '' }
    st3.action?.onClick()
    out.notifier = {
      submittedTitle: snap0.title,
      submittedMsg: snap0.message,
      progressMsg: snap1.message,
      progressKindStillLoading: snap1.kind === 'loading',
      terminalSuccessMsg: snap2.message,
      terminalSuccessKind: snap2.kind,
      terminalSuccessTitle: snap2.title,
      terminalErrorMsg: snap3.message,
      terminalErrorTitle: snap3.title,
      errorActionLabel: snap3.action,
      actionFired: viewTasksFired === 1,
    }
    void t
    clearToasts()
  }

  // ============ ⑥ 保存类操作触发 toast（AudioPage 真挂载） ============
  {
    clearToasts()
    const { app, el } = mountApp(AudioPage, [[FLOW_CONTEXT_KEY, mockCtx]])
    await flush()
    // 右侧导入/新建表单：trigger 必填 →「建条目并 AI 生成」（免文件口径）
    const side = el.querySelector('.fh-col-side') as HTMLElement
    const trigger = side.querySelectorAll('input[type="text"]')[0] as HTMLInputElement
    trigger.value = '开场'
    trigger.dispatchEvent(new Event('input', { bubbles: true }))
    const genEntry = Array.from(side.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('✨'),
    ) as HTMLButtonElement
    genEntry.click()
    await flush(20)
    const savedToastShown = useToastState().items.some(
      (x) => x.kind === 'success' && x.message === i18n.global.t('toast.saved'),
    )
    out.saveClass = {
      savedToastShown,
      bgmPostBody: smock()
        .calls.find((c) => c.method === 'POST' && c.path.endsWith('/audio/bgm'))
        ?.body as { info?: { trigger?: string } } | undefined,
    }
    app.unmount()
    clearToasts()
  }

  void TOAST_MAX
  return out
}
