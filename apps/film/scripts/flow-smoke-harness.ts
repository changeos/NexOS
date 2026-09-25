// =============================================================================
// flow-smoke-harness.ts —— FilmHub 流程化组件冒烟 harness（happy-dom 挂载）。
//
// 由 scripts/flow-smoke.mjs 用 vite（plugin-vue 编译 .vue，vue/vue-i18n 真实
// 打包）构建为临时 ESM，在 happy-dom 全局就绪后 import 并调用 run()：
//   · SideNav 左侧选项卡（v0.1.5 排序：FilmHub/五阶段/工作台尾/设置/模型
//     设置——「Hub 浏览」项已删）+ 树卡标题=项目名 / 阶段徽章（README stage）/
//     点击流转 / 折叠；
//   · StoryboardPage：生成分镜配置面板（人物 chips + voice 下拉 PUT +
//     配置随 storyboard/generate body 发出）/「从剧情生成分镜」按钮；
//   · CastingPage：六类 Tab / Tab 切换重载 / 对象卡 / 多视图五槽位（空槽）/
//     对象级认领（PUT ownership.json）；
//   · ComposePage：dist 版本列表 / cache 半成品 commit 调用（带 author）/
//     预览成片切工作台；
//   · AudioPage：BGM 表单校验（trigger 空=错误；填后 POST 带作者）；
//   · SettingsPage：成员渲染 / 添加成员（PUT ownership）/ 分区认领 / 活动流。
// HTTP 走宿主桥 mock（globalThis.__NEXOS_HOST__.api，由 .mjs 注入）——全部
// 调用记录在 globalThis.__FLOW_SMOKE__.calls。断言在 .mjs（node:assert）。
// =============================================================================
import { computed, createApp, defineComponent, h, nextTick, reactive, ref } from 'vue'
import { createI18n } from 'vue-i18n'
import SideNav from '../src/flow/SideNav.vue'
import StoryboardPage from '../src/flow/StoryboardPage.vue'
import CastingPage from '../src/flow/CastingPage.vue'
import ComposePage from '../src/flow/ComposePage.vue'
import AudioPage from '../src/flow/AudioPage.vue'
import ModelsPage from '../src/flow/ModelsPage.vue'
import SettingsPage from '../src/flow/SettingsPage.vue'
import StoryPage from '../src/flow/StoryPage.vue'
import zhCN from '../src/i18n/zh-CN.json'
import { filmGetProjectModels, filmPutFile } from '../src/api'
import { FLOW_CONTEXT_KEY, PROJECT_DEFAULT_KEY, filmTaskStageLabel, type FlowContext } from '../src/flow/flowContext'
import { serializeOwnership, textToB64, type FilmOwnership } from '../src/flow/collab'
import type { FilmStage, FlowView } from '../src/flow/flowTypes'
import type { FilmProjectModels } from '../src/api'
import type { PipelineTaskSnapshot } from '../src/flow/pipelineStatus'

/** 与 .mjs 的共享观测面（mjs 注入）。 */
interface Smock {
  calls: { method: string; path: string; body?: unknown }[]
  viewSwitches: string[]
  ownershipPuts: FilmOwnership[]
  /** v0.1.39 直填 API：渠道创建模拟失败文案（mock 消费一次即清）。 */
  channelCreateFail: string
  /** v0.1.6 管线任务进任务中心：trackFilmTask(id, stage) 登记流水。 */
  tracked: { id: string; stage: string }[]
  /** v0.1.6 busy 态观测：下一次 POST story/chapterize 慢 150ms（中途读按钮态）。 */
  slowChapterize: boolean
}
function smock(): Smock {
  return (globalThis as { __FLOW_SMOKE__: Smock }).__FLOW_SMOKE__
}

/** b64 → UTF-8 JSON（node atob 不解多字节——Buffer 走字节面）。 */
function unb64Json(b64: string): Record<string, unknown> {
  const bin = typeof Buffer !== 'undefined' ? Buffer.from(b64, 'base64').toString('utf8') : atob(b64)
  return JSON.parse(bin) as Record<string, unknown>
}

/** mock FlowContext（真实类型面；数据经宿主桥 mock api 出入）。 */
const navView = ref<FlowView>('casting')
const mockOwnership = ref<FilmOwnership | null>({
  members: ['小明', '小红'],
  sections: { story: { owner: '小红', claimed_at: '2026-09-05T01:00:00Z' } },
  casting_objects: { 'characters/小明': { owner: '小红', claimed_at: '2026-09-05T02:00:00Z' } },
})
const mockAuthor = ref('小明')
const mockPendingCast = ref<{ type: 'characters' | 'props' | 'pets' | 'formations' | 'actions' | 'scenes'; name: string } | null>(null)
const mockPendingHub = ref<string | null>(null)
/** v0.1.44 章节卡「从此章生成分镜」→ StoryboardGenPanel 预填（挂载消费）。 */
const mockPendingSbChapter = ref<string | null>(null)
/** v0.1.38：模型下拉选择（含「项目默认」project key）与 models 快照。 */
const mockModelSel = reactive<Record<string, string>>({
  chat: 'llm:chat',
  image: 'local',
  video: 'ch:c1',
  tts: 'ch:c1',
  music: 'ch:c1',
})
const mockProjectModels = ref<FilmProjectModels | null>(null)
/** v0.1.7 管线任务快照（模拟 FilmStudio trackedTasks → storyTasks 投影）。 */
const mockStoryTasks = ref<PipelineTaskSnapshot[]>([])
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
  optionsFor: (cap) => [
    {
      label: '本地',
      options: [{ key: cap === 'image' ? 'local' : `llm:${cap}`, label: `mock-${cap}`, relay: false }],
    },
  ],
  hasOptionsFor: () => true,
  modelSel: mockModelSel,
  modelRefFor: (cap) =>
    mockModelSel[cap] === PROJECT_DEFAULT_KEY ? null : { source: 'local', capability: cap },
  addTracked: () => undefined,
  // v0.1.6 管线任务进任务中心（登记进观测面供 .mjs 断言 id+stage）；
  // v0.1.7 带 meta 的源关联任务同时进 storyTasks 快照（源行聚合徽章数据源）。
  // v0.1.11 源级互斥：mock 后端瞬时完成（status=completed）——真实行为下
  // 同源任务未终态时管线按钮被禁用，旧「同源连发多阶段」序列不再是合法路径
  //（①.58 聚合徽章用例直接整体替换 mockStoryTasks 自行播种运行/失败态）。
  trackFilmTask: (
    id: string,
    stage: string,
    meta?: { sourceFile?: string; createdAt?: number | null },
  ) => {
    smock().tracked.push({ id, stage })
    if (meta?.sourceFile) {
      mockStoryTasks.value = [
        ...mockStoryTasks.value,
        {
          id,
          stage,
          status: 'completed',
          sourceFile: meta.sourceFile,
          log: [],
          error: '',
          createdAt: meta.createdAt ?? null,
        },
      ]
    }
  },
  storyTasks: computed(() => mockStoryTasks.value),
  errMsg: (e) => String(e),
  refreshTick: ref(0),
  reloadProject: async () => undefined,
  chatAvailable: computed(() => true),
  channelAvailable: computed(() => true),
  composeAvailable: computed(() => true),
  isOffline: computed(() => false),
  stage: ref<FilmStage | ''>('casting'),
  view: navView,
  setView: (v: FlowView) => {
    navView.value = v
    smock().viewSwitches.push(v)
  },
  ownership: mockOwnership,
  author: mockAuthor,
  activity: ref([
    { ts: '2026-09-06T10:00:00Z', author: '小红', action: 'story.generate', target: 'story.md' },
    { ts: 1789000000, author: 'anonymous', action: 'compose', target: 'dist/final-v2.mp4' },
  ]),
  setAuthor: (name: string) => {
    mockAuthor.value = name
  },
  saveOwnership: async (next: FilmOwnership) => {
    // 与 FilmStudio.saveOwnership 同代码路径：PUT files/ownership.json（带作者）
    try {
      await filmPutFile('p1', 'ownership.json', textToB64(serializeOwnership(next)), mockAuthor.value)
      mockOwnership.value = next
      smock().ownershipPuts.push(next)
      return true
    } catch {
      return false
    }
  },
  refreshCollab: async () => undefined,
  pendingCastSelect: mockPendingCast,
  pendingStoryboardChapter: mockPendingSbChapter,
  pendingHubFile: mockPendingHub,
  // —— v0.1.38 项目级模型设置 ——
  projectModels: mockProjectModels,
  reloadProjectModels: async () => {
    mockProjectModels.value = await filmGetProjectModels('p1')
  },
  defaultModelSummary: (cap: string) => {
    const entry = mockProjectModels.value?.capabilities?.find((c) => c.capability === cap)
    if (!entry?.source) return ''
    if (entry.source === 'local') return '本地'
    const model = (entry.model || '').trim()
    return model ? `mock-渠道 · ${model}` : 'mock-渠道'
  },
  modelSelReady: () => true,
  isProjectDefaultSel: (cap: string) => mockModelSel[cap] === PROJECT_DEFAULT_KEY,
}

/** i18n（真实 vue-i18n；zh-CN 全量键）。 */
const i18n = createI18n({
  legacy: false,
  locale: 'zh-CN',
  fallbackLocale: 'zh-CN',
  messages: { 'zh-CN': zhCN as Record<string, unknown> },
  missingWarn: false,
  fallbackWarn: false,
})

/** 挂载组件（app 级 provide FLOW_CONTEXT_KEY + i18n）。 */
function mountPage(comp: Parameters<typeof createApp>[0]): { app: ReturnType<typeof createApp>; el: HTMLElement } {
  const el = document.createElement('div')
  document.body.appendChild(el)
  const app = createApp(comp)
  app.use(i18n)
  app.provide(FLOW_CONTEXT_KEY, mockCtx)
  app.mount(el)
  return { app, el }
}

/** 微任务 + 渲染 flush（mock api promise 落地后再 nextTick）。 */
async function flush(ms = 15): Promise<void> {
  await new Promise((r) => setTimeout(r, ms))
  await nextTick()
}

// 纯函数再导出（.mjs 直接断言同一代码路径）
export {
  cacheEntries,
  distVersionOf,
  distVersions,
  hubFileEntries,
  hubFilePaths,
  storySourceStatus,
  storySources,
  validateBgmForm,
} from '../src/flow/flowFiles'
export {
  PART_TEMPLATES,
  composePromptPreview,
  latestMainView,
  mainSlotOf,
  normalizeParts,
  partSlotsFor,
  partsDiff,
  partsEqual,
} from '../src/flow/castParts'
export { parseStageFromMarkdown } from '../src/flow/flowTypes'
export {
  claimCastingObject,
  objectOwner,
  parseActivity,
  parseOwnership,
  sectionOwner,
} from '../src/flow/collab'
export { b64ToText } from '../src/api'
// v0.1.39 直填 API 地址规整（裸 host 补 /v1——.mjs 直断言同一纯函数）
export { normalizeApiBaseUrl } from '../src/api'
// v0.1.39 导入编码兼容（StoryPage 平级 <script> 具名导出——.mjs 直断言同一解码链）
export { bytesToB64, decodeSourceBytes } from '../src/flow/StoryPage.vue'
// v0.1.6 任务中心 stage 标签（纯函数再导出——.mjs 直断言白名单/键映射）
export { filmTaskStageKey, filmTaskStageLabel, FILM_TASK_STAGES } from '../src/flow/flowContext'
// v0.1.7 管线进度聚合纯函数（.mjs 直断言徽章聚合优先级 / X-Y 日志解析）
export {
  aggregateSourcePipeline,
  parseChunkProgress,
  STORY_PIPELINE_STAGES,
} from '../src/flow/pipelineStatus'

/** 路径安全解码（mock 路径段含百分号编码中文名；非法序列原样返回）。 */
function safePath(p: string): string {
  try {
    return decodeURIComponent(p)
  } catch {
    return p
  }
}

function fireInput(el: HTMLInputElement, value: string): void {
  el.value = value
  el.dispatchEvent(new Event('input', { bubbles: true }))
}
function fireChange(el: HTMLSelectElement, value: string): void {
  el.value = value
  el.dispatchEvent(new Event('change', { bubbles: true }))
}

export interface FlowSmokeResult {
  nav: {
    items: string[]
    badges: string[]
    badgeClasses: string[]
    activeIndexAfterClick: number
    collapsedAfterToggle: boolean
    viewSwitches: string[]
    /** v0.1.38：流程链说明行已移除（.fh-nav-hint 不存在）。 */
    hintGone: boolean
    /** v0.1.39：树卡标题（🗂 <项目标题>，不再是「Hub 树」）。 */
    treeCardTitle: string
  }
  storyboard: {
    /** 页头「从剧情生成分镜」按钮文案（含「生成分镜」）。 */
    genBtnLabel: string
    /** 折叠头文案（⚙ 生成分镜配置）。 */
    panelToggle: string
    /** 点折叠头后面板体出现。 */
    panelOpenAfterToggle: boolean
    /** 人物 chips（casting/characters 名单）。 */
    chipLabels: string[]
    chipActiveAfterPick: boolean
    /** 勾选后 voice 行下拉初值（card 既有 voice）。 */
    voiceSelectValue: string
    /** 改 voice → PUT casting/characters/:name {voice, author}。 */
    voicePut: { path: string; body: unknown } | undefined
    /** 生成 → POST storyboard/generate（配置字段随 body）。 */
    genCall: { path: string; body: unknown } | undefined
    /** v0.1.6 生成 202 → trackFilmTask(id, storyboard) 登记。 */
    trackedAfterGen: boolean
    /** v0.1.44 分镜章节下拉（面板首字段：全部剧情/第 N 章…）。 */
    chapterOptions: string[]
    /** v0.1.44 章节卡预填消费：挂载即自动展开面板 + 下拉选中该章。 */
    chapterPrefillOpen: boolean
    chapterPrefillValue: string
    /** v0.1.44 预填后生成 → body 带 chapter_range。 */
    chapterRangeGenCall: { path: string; body: unknown } | undefined
  }
  story: {
    sourceRows: string[]
    pipelineVisibleAfterSelect: boolean
    /** v0.1.44 并入件·源入库锁定：已入库源（novel，index.source 基名匹配）
     *  清理/分章禁用 + tooltip（重处理出口指引）；向量化不锁。 */
    ingestedLock: {
      cleanDisabled: boolean
      chapterizeDisabled: boolean
      embedDisabled: boolean
      cleanTitle: string
    }
    cleanCall: { path: string; body: unknown } | undefined
    cleanLlmCall: { path: string; body: unknown } | undefined
    /** v0.1.39.1「🧠 向量化」钮 → POST story/embed（无 model_ref=emb 缺省链）。 */
    embedCall: { path: string; body: unknown } | undefined
    /** v0.1.39.1 语义搜索框 → POST story/search；结果块卡（文本+行号+分）。 */
    searchCall: { path: string; body: unknown } | undefined
    searchHitTexts: string[]
    /** 结果卡点击「定位原文」→ GET files/<源>（左栏加载原文）。 */
    searchLocateCall: { path: string } | undefined
    tabs: string[]
    chapterRows: string[]
    chapterTextShown: boolean
    /** v0.1.44 章节卡「从此章生成分镜」→ pending 置位 + 跳分镜页。 */
    sbFromChapterPending: string | null
    sbFromChapterJumped: boolean
    profileCards: string[]
    toCastingCall: { path: string; body: unknown } | undefined
    draftShown: boolean
    /** v0.1.6 管线任务统一进任务中心：trackFilmTask(id, stage) 登记流水快照。 */
    tracked: { id: string; stage: string }[]
    /** v0.1.6 busy 态视觉：分章钮进行中 spinner + 「进行中…」文案；落地复位。 */
    busySpinnerShown: boolean
    busyLabel: string
    busyLabelBack: string
    /** v0.1.6 emb 探测失败：管线操作条上方提示行（文案/可关闭）+ 向量化钮 tooltip。 */
    embHintShown: boolean
    embHintText: string
    embedTitle: string
    embedDisabled: boolean
    embHintClosedWorks: boolean
    /** v0.1.7 管线进度重做：聚合徽章 / X-Y popover / 失败行内重试。 */
    pipe: {
      badgeNovel: string
      badgeZhuxian: string
      popoverOpenWorks: boolean
      popoverHasTasks: boolean
      popoverCloseWorks: boolean
      zhuxianFailed: boolean
      retryCall: { path: string; body: unknown } | undefined
    }
  }
  casting: {
    tabs: string[]
    charactersCards: string[]
    propsCardsAfterSwitch: string[]
    propsGetCalled: boolean
    /** v0.1.39 定制器：中央主槽视图态 / 部件行 / 历史缩略 / 更多视图槽位。 */
    czStageFilled: boolean
    czPlaceholderShown: boolean
    czCaption: string
    partRowCount: number
    partRowKeys: string[]
    historyThumbCount: number
    auxSlotCount: number
    auxSlotLabels: string[]
    /** 部件交互：预设 chips 展开 / 选中态 / dirty 徽章。 */
    chipCountOnExpand: number
    chipActiveAfterPick: boolean
    dirtyBadgeAfterPick: boolean
    /** 生成链路：PUT parts + POST views/generate（主槽、无 prompt）。 */
    partsPutBeforeGen: { path: string; body: unknown } | undefined
    mainGenCall: { path: string; body: unknown } | undefined
    secondPartsPut: { path: string; body: unknown } | undefined
    secondMainGenCall: { path: string; body: unknown } | undefined
    ownerBadgeText: string
    claimPuts: number
    claimObjectOwner: string | undefined
    /** v0.1.6「AI 提取定妆对象」202 → trackFilmTask(id, casting) 登记。 */
    extractTracked: boolean
  }
  compose: {
    distRows: string[]
    cacheCards: string[]
    commitCall: { path: string; body: unknown } | undefined
    previewViewSwitch: string | undefined
  }
  audio: {
    emptyFormError: string
    emptyFormNoPost: boolean
    secondError: string
    postCall: { path: string; body: unknown } | undefined
    genCall: { path: string; body: unknown } | undefined
  }
  settings: {
    memberBadges: string[]
    activityRows: number
    ownershipPutMembers: string[] | undefined
    sectionOwnerPut: string | undefined
  }
  /** v0.1.38 项目级模型设置（ModelsPage 八能力位行 + 配置弹窗 PUT）。 */
  models: {
    rows: string[]
    reservedRows: string[]
    okDots: number
    modalOpened: boolean
    putCall: { path: string; body: unknown } | undefined
    snapshotAfterPut: { source?: unknown; channel_id?: unknown } | undefined
    /** 「项目默认」生成：POST story/generate body 不含 model_ref 字段。 */
    projectDefaultGenCall: { path: string; body: unknown } | undefined
    /** v0.1.6「AI 写剧情」202 → trackFilmTask(id, story) 登记。 */
    storyGenTracked: boolean
    /** v0.1.39 直填 API：弹窗 / 自动名称联动 / 建渠道+PUT 链 / 失败红条。 */
    addApi: {
      dialogOpened: boolean
      autoName: string
      autoNameAfterCapSwitch: string
      createCall: { path: string; body: unknown } | undefined
      modelsPutAfterAdd: { path: string; body: unknown } | undefined
      notice: string
      modalClosedAfterDone: boolean
      failError: string
      failNoModelsPut: boolean
    }
  }
  /** v0.1.6 任务中心 stage 标签（taskLabel 同一纯函数 + zh-CN 真实 i18n）。 */
  taskLabels: Record<string, string>
}

export async function run(): Promise<FlowSmokeResult> {
  const out = {} as FlowSmokeResult

  // ============ ① SideNav：选项卡渲染 / 阶段徽章 / 点击流转 / 折叠 ============
  {
    const NavHost = defineComponent({
      setup() {
        return () =>
          h(SideNav, {
            view: navView.value,
            stage: 'casting' as FilmStage,
            onSelect: (v: FlowView) => {
              navView.value = v
              smock().viewSwitches.push(v)
            },
          })
      },
    })
    const { app, el } = mountPage(NavHost)
    await flush()
    const items = Array.from(el.querySelectorAll('.fh-nav-item'))
    const badges = Array.from(el.querySelectorAll('.fh-nav-badge'))
    out.nav = {
      items: items.map((n) => (n.textContent ?? '').trim()),
      badges: badges.map((n) => (n.textContent ?? '').trim()),
      badgeClasses: badges.map((n) => n.className),
      activeIndexAfterClick: -1,
      collapsedAfterToggle: false,
      viewSwitches: [],
      hintGone: !el.querySelector('.fh-nav-hint'),
      // v0.1.39 树卡标题 = 项目标题（mock ctx title=冒烟项目）
      treeCardTitle: (el.querySelector('.hub-tree-card-title')?.textContent ?? '').trim(),
    }
    // 点击「分镜」→ active 流转 + view 切换
    //（v0.1.5 顺序：0 FilmHub / 1 剧情 / 2 分镜 / 3 定妆 / 4 音频 /
    //  5 合成 / 6 工作台 / 7 设置 / 8 模型设置——「Hub 浏览」项已删）
    items[2].dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    out.nav.activeIndexAfterClick = items.findIndex((n) => n.classList.contains('is-active'))
    // 折叠
    const toggle = el.querySelector('.fh-nav-toggle') as HTMLButtonElement
    toggle.click()
    await flush(5)
    out.nav.collapsedAfterToggle = (el.querySelector('.fh-nav') as HTMLElement).classList.contains('is-collapsed')
    out.nav.viewSwitches = [...smock().viewSwitches]
    app.unmount()
  }

  // ============ ①.5 StoryPage：文档管线（源区/管线操作条/三 Tab/转定妆） ============
  {
    // v0.1.39.1：先载 models 快照（StoryPage 的 emb 位就绪态/向量化钮启用判据）
    await mockCtx.reloadProjectModels()
    const { app, el } = mountPage(StoryPage)
    await flush()
    // 左栏源列表（fixture：sources/novel.txt + 状态徽章）
    const leftCol = el.querySelector('.fh-two-col > .fh-col') as HTMLElement
    const sourceRows = Array.from(leftCol.querySelectorAll('.fh-card')[0].querySelectorAll('.fh-row')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    // 选中源 → 管线操作条出现
    const firstRow = leftCol.querySelectorAll('.fh-card')[0].querySelector('.fh-row') as HTMLElement
    firstRow.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(20)
    const pipelineCard = Array.from(leftCol.querySelectorAll('.fh-card'))[1] as HTMLElement | undefined
    const pipelineVisibleAfterSelect = !!pipelineCard && (pipelineCard.textContent ?? '').includes('文档管线')
    // v0.1.44 并入件·源入库锁定：novel 已分章（index.source 基名匹配）→ 已入库
    // ——🧹清理/📖分章禁用 + tooltip（重处理出口指引）；🧠向量化不锁（入库后的
    // 操作，novel 有对应章节 → 前置分章门放行）
    const lockBtn = (word: string) =>
      Array.from((pipelineCard ?? leftCol).querySelectorAll('button')).find((b) =>
        (b.textContent ?? '').includes(word),
      ) as HTMLButtonElement | undefined
    const ingestedLock = {
      cleanDisabled: lockBtn('清理')?.disabled ?? false,
      chapterizeDisabled: lockBtn('分章')?.disabled ?? false,
      embedDisabled: lockBtn('向量化')?.disabled ?? false,
      cleanTitle: lockBtn('清理')?.getAttribute('title') ?? '',
    }
    // 「🧠 向量化」→ POST story/embed（无 model_ref=走 emb 位缺省链；带 author）
    const embedBtn = lockBtn('向量化') as HTMLButtonElement
    embedBtn.click()
    await flush(20)
    const embedCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path.endsWith('/story/embed'),
    )
    // 右栏三 Tab + 章节卡清单（v0.1.44 卡片化：每章一卡）+ 点卡头展开正文
    const rightCard = el.querySelector('.fh-two-col > section.fh-card') as HTMLElement
    const tabs = Array.from(rightCard.querySelectorAll('.fh-tab')).map((n) => (n.textContent ?? '').trim())
    const chaptersTab = tabs.findIndex((x) => x.includes('章节'))
    ;(rightCard.querySelectorAll('.fh-tab')[chaptersTab] as HTMLElement).dispatchEvent(
      new Event('click', { bubbles: true }),
    )
    await flush(20)
    const chapterRows = Array.from(rightCard.querySelectorAll('.story-ch-card')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    const firstChapterHead = rightCard.querySelector('.story-ch-card .story-ch-head') as HTMLElement
    firstChapterHead.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(20)
    const chapterTextShown = (rightCard.querySelector('.story-ch-body .fh-pre')?.textContent ?? '')
      .includes('风起正文')
    // v0.1.44 章节卡快捷钮：「从此章生成分镜」→ pendingStoryboardChapter 置位
    // + 跳分镜页（面板挂载消费——①.6.5 验证）
    const sbFromChBtn = Array.from(rightCard.querySelectorAll('.story-ch-actions button')).find(
      (b) => (b.textContent ?? '').includes('生成分镜'),
    ) as HTMLButtonElement
    sbFromChBtn.click()
    await flush(5)
    const sbFromChapterPending = mockPendingSbChapter.value
    const sbFromChapterJumped = smock().viewSwitches.includes('storyboard')
    // 消费测试归 ①.6.5——此处复位 pending，防 ①.6 面板挂载即自动展开干扰既有断言
    mockPendingSbChapter.value = null
    // 人物档案 Tab：卡片 + 转定妆
    const profileTabIdx = tabs.findIndex((x) => x.includes('人物档案'))
    ;(rightCard.querySelectorAll('.fh-tab')[profileTabIdx] as HTMLElement).dispatchEvent(
      new Event('click', { bubbles: true }),
    )
    await flush(5)
    const profileCards = Array.from(rightCard.querySelectorAll('.story-profile-card')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    const toCastingBtn = rightCard.querySelector('.story-profile-card .nx-btn') as HTMLButtonElement
    toCastingBtn.click()
    await flush(20)
    const toCastingCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path.endsWith('/casting/characters'),
    )
    // v0.1.39.1 语义搜索（右栏「章节」Tab 上方）：输入 → POST story/search →
    // 结果块卡（文本+行号）→ 点击定位原文（左栏加载源文件）
    const searchInput = rightCard.querySelector('input.fh-input') as HTMLInputElement
    fireInput(searchInput, '猫在霓虹巷口')
    const searchBtn = Array.from(rightCard.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('检索'),
    ) as HTMLButtonElement
    searchBtn.click()
    await flush(20)
    const searchCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path.endsWith('/story/search'),
    )
    const searchHitEls = Array.from(rightCard.querySelectorAll('.story-search-hit'))
    const searchHitTexts = searchHitEls.map((n) => (n.textContent ?? '').trim())
    const getsBeforeLocate = smock().calls.filter((c) => c.method === 'GET').length
    ;(searchHitEls[0] as HTMLElement).dispatchEvent(new Event('click', { bubbles: true }))
    await flush(20)
    const searchLocateCall = smock().calls
      .filter((c) => c.method === 'GET')
      .slice(getsBeforeLocate)
      .find((c) => c.path.includes('/files/sources/novel.txt'))
    // 正稿 Tab：story.md 展示（v0.1.35 功能保留）
    const draftTabIdx = tabs.findIndex((x) => x.includes('正稿'))
    ;(rightCard.querySelectorAll('.fh-tab')[draftTabIdx] as HTMLElement).dispatchEvent(
      new Event('click', { bubbles: true }),
    )
    await flush(5)
    const draftShown = (rightCard.querySelector('.fh-pre')?.textContent ?? '').includes('月光落在月球快递站')
    // v0.1.44 并入件：管线操作改在**未入库源**（诛仙——index.source 指向
    // novel，基名不匹配 → 不锁）上验证：🧹清理（rules/llm）与 📖分章照常可点
    const rows = Array.from(leftCol.querySelectorAll('.fh-card')[0].querySelectorAll('.fh-row')) as HTMLElement[]
    const zhuxianRow = rows.find((r) => (r.textContent ?? '').includes('诛仙')) as HTMLElement
    zhuxianRow.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(20)
    // 「🧹 清理」（rules 缺省）→ POST story/clean {source_file, mode, author}
    const cleanBtn = Array.from((pipelineCard ?? leftCol).querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('清理'),
    ) as HTMLButtonElement
    cleanBtn.click()
    await flush(20)
    const callsAfterClean = [...smock().calls]
    const cleanCall = callsAfterClean.find(
      (c) => c.method === 'POST' && c.path.endsWith('/story/clean') && (c.body as { mode?: string })?.mode === 'rules',
    )
    // 切 LLM 深清 → 再点清理（带 model_ref）
    const modeSelect = (pipelineCard ?? leftCol).querySelector('select.fh-select') as HTMLSelectElement
    fireChange(modeSelect, 'llm')
    await flush(5)
    cleanBtn.click()
    await flush(20)
    const cleanLlmCall = smock().calls
      .filter((c) => c.method === 'POST' && c.path.endsWith('/story/clean'))
      .find((c) => (c.body as { mode?: string })?.mode === 'llm')
    // v0.1.6 busy 态视觉：慢 150ms 的 story/chapterize 中途观测「分章」钮——
    // ↻ spinner + 「进行中…」文案；落地后复位「分章」
    smock().slowChapterize = true
    const chapBtn = Array.from((pipelineCard ?? leftCol).querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('分章'),
    ) as HTMLButtonElement
    chapBtn.click()
    await flush(10) // 慢响应 pending 中
    // v0.1.7 重设计：busy spinner = NxButton 环形 .nx-btn__spin（兼挂 fh-spin 钩子）
    const busySpinnerShown = !!el.querySelector('.fh-card button .nx-btn__spin.is-spinning')
    const busyBtn = Array.from((pipelineCard ?? leftCol).querySelectorAll('button')).find((b) =>
      b.querySelector('.fh-spin'),
    ) as HTMLButtonElement | undefined
    const busyLabel = (busyBtn?.textContent ?? '').trim()
    await flush(250) // 慢响应落地 + busy 复位
    const busyLabelBack = (chapBtn.textContent ?? '').trim()
    out.story = {
      sourceRows,
      pipelineVisibleAfterSelect,
      ingestedLock,
      cleanCall,
      cleanLlmCall,
      embedCall,
      searchCall,
      searchHitTexts,
      searchLocateCall,
      tabs,
      chapterRows,
      chapterTextShown,
      sbFromChapterPending,
      sbFromChapterJumped,
      profileCards,
      toCastingCall,
      draftShown,
      tracked: [...smock().tracked],
      busySpinnerShown,
      busyLabel,
      busyLabelBack,
      embHintShown: false,
      embHintText: '',
      embedTitle: '',
      embedDisabled: false,
      embHintClosedWorks: false,
    }
    app.unmount()

    // —— v0.1.6 emb 探测失败可见性：快照翻 ok:false → 重挂 StoryPage ——
    //    管线操作条上方提示行（文案含人话指引、可关闭）+ 向量化钮 tooltip 强化
    const embRow = mockProjectModels.value?.capabilities?.find((c) => c.capability === 'emb')
    const origEmbAvail = embRow?.available
    if (embRow) {
      embRow.available = { ok: false, detail: '本地实例 127.0.0.1:8000（bge-m3）无 /v1/embeddings' }
    }
    const embStory = mountPage(StoryPage)
    await flush(20)
    const embLeftCol = embStory.el.querySelector('.fh-two-col > .fh-col') as HTMLElement
    const embSourceRow = embLeftCol.querySelectorAll('.fh-card')[0].querySelector('.fh-row') as HTMLElement
    embSourceRow.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(20)
    const embHint = embStory.el.querySelector('.story-emb-hint') as HTMLElement | null
    out.story.embHintShown = !!embHint
    out.story.embHintText = (embHint?.textContent ?? '').trim()
    const embPipeline = Array.from(embLeftCol.querySelectorAll('.fh-card'))[1] as HTMLElement | undefined
    const embBtn2 = Array.from((embPipeline ?? embLeftCol).querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('向量化'),
    ) as HTMLButtonElement | undefined
    out.story.embedTitle = embBtn2?.getAttribute('title') ?? ''
    out.story.embedDisabled = embBtn2?.disabled ?? false
    ;(embHint?.querySelector('button') as HTMLButtonElement | undefined)?.click()
    await flush(5)
    out.story.embHintClosedWorks = !embStory.el.querySelector('.story-emb-hint')
    embStory.app.unmount()
    if (embRow && origEmbAvail) embRow.available = origEmbAvail

    // ============ ①.58 管线进度重做（v0.1.7：聚合徽章 / X-Y popover / 失败重试） ============
    {
      const NOW = Date.now()
      mockStoryTasks.value = [
        {
          id: 'pt-clean',
          stage: 'story.clean',
          status: 'completed',
          sourceFile: 'sources/novel.txt',
          log: ['原文 240000 字符 → 17 块 × ≤6K（重叠上下文 200 字符）', '块 17/17 完成'],
          error: '',
          createdAt: NOW - 600_000,
        },
        {
          id: 'pt-emb',
          stage: 'story.embed',
          status: 'running',
          sourceFile: 'sources/novel.txt',
          log: ['原文 240000 字符 → 17 块 × ≤800 字（并发≤8）', '块 3/17 完成'],
          error: '',
          createdAt: NOW - 65_000,
        },
        {
          id: 'pt-chap',
          stage: 'story.chapterize',
          status: 'failed',
          sourceFile: 'story/source-诛仙.txt',
          log: ['原文 90000 字符 → 9 块 × ≤6K', '块 2/9 完成'],
          error: 'LLM 分章块 2/9 失败：上游超时',
          createdAt: NOW - 300_000,
        },
      ]
      const pipe = mountPage(StoryPage)
      await flush(20)
      const pipeLeft = pipe.el.querySelector('.fh-two-col > .fh-col') as HTMLElement
      const pipeRows = Array.from(pipeLeft.querySelectorAll('.fh-row')) as HTMLElement[]
      const novelRow = pipeRows.find((r) => (r.textContent ?? '').includes('novel.txt'))
      const zhuxianRow = pipeRows.find((r) => (r.textContent ?? '').includes('诛仙'))
      const badgeOf = (row: HTMLElement | undefined) =>
        (row?.querySelector('.nx-badge')?.textContent ?? '').trim()
      // 聚合徽章：novel = 运行中 embed（清理已完成）→「向量化中」+ X/Y 3/17
      const badgeNovel = badgeOf(novelRow)
      // popover 开合：点徽章触发 → 面板（Teleport body）含各任务「完成 X/Y」
      ;(novelRow?.querySelector('.nx-pop__trigger') as HTMLElement | undefined)?.click()
      await flush(5)
      const panel = document.querySelector('.nx-pop__panel')
      const panelText = (panel?.textContent ?? '').trim()
      document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
      await flush(5)
      const panelClosed = !document.querySelector('.nx-pop__panel')
      // 失败行：红态类 + 失败徽章词；行内重试 → POST story/chapterize（原源）
      const zhuxianFailed = !!zhuxianRow?.classList.contains('is-failed')
      const postsBeforeRetry = smock().calls.length
      ;(zhuxianRow?.querySelector('.story-retry') as HTMLElement | undefined)?.click()
      await flush(20)
      const retryCall = smock().calls
        .slice(postsBeforeRetry)
        .find(
          (c) =>
            c.method === 'POST' &&
            c.path.endsWith('/story/chapterize') &&
            (c.body as { source_file?: string })?.source_file === 'story/source-诛仙.txt',
        )
      out.story.pipe = {
        badgeNovel,
        badgeZhuxian: badgeOf(zhuxianRow),
        popoverOpenWorks: !!panel,
        popoverHasTasks: panelText.includes('清理') && panelText.includes('完成 17/17') && panelText.includes('完成 3/17'),
        popoverCloseWorks: panelClosed,
        zhuxianFailed,
        retryCall: retryCall as { path: string; body: unknown } | undefined,
      }
      pipe.app.unmount()
      mockStoryTasks.value = []
    }
  }

  // ============ ①.6 StoryboardPage：生成分镜配置面板（v0.1.39） ============
  {
    const { app, el } = mountPage(StoryboardPage)
    await flush()
    const genBtn = Array.from(el.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('生成分镜'),
    ) as HTMLButtonElement
    const toggle = el.querySelector('.sbg-toggle') as HTMLElement
    out.storyboard = {
      genBtnLabel: (genBtn?.textContent ?? '').trim(),
      panelToggle: (toggle?.textContent ?? '').trim(),
      panelOpenAfterToggle: false,
      chipLabels: [],
      chipActiveAfterPick: false,
      voiceSelectValue: '',
      voicePut: undefined,
      genCall: undefined,
      trackedAfterGen: false,
      chapterOptions: [],
      chapterPrefillOpen: false,
      chapterPrefillValue: '',
      chapterRangeGenCall: undefined,
    }
    // 折叠头点开 → 面板体（人物 chips = casting/characters 名单）
    toggle.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(15)
    out.storyboard.panelOpenAfterToggle = !!el.querySelector('.sbg-body')
    const chips = Array.from(el.querySelectorAll('.sbg-chip')) as HTMLElement[]
    out.storyboard.chipLabels = chips.map((c) => (c.textContent ?? '').trim())
    // 勾选「小明」→ chip 高亮 + voice 行（初值=card 既有 voice alloy）
    chips[0].dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    out.storyboard.chipActiveAfterPick = chips[0].classList.contains('is-active')
    const voiceSel = el.querySelector('.sbg-voice-select') as HTMLSelectElement
    out.storyboard.voiceSelectValue = voiceSel?.value ?? ''
    // 改 voice=nova → PUT casting/characters/小明 {voice, author}
    fireChange(voiceSel, 'nova')
    await flush(20)
    out.storyboard.voicePut = smock().calls.find(
      (c) => c.method === 'PUT' && /\/casting\/characters\/[^/]+$/.test(safePath(c.path)),
    )
    // 镜头数 8 + 总时长提示 → 生成（配置字段随 storyboard/generate body）
    const shotInput = el.querySelector(
      '.sbg-body input[type="number"]',
    ) as HTMLInputElement
    fireInput(shotInput, '8')
    const hintInput = el.querySelector(
      '.sbg-body input[type="text"]',
    ) as HTMLInputElement
    fireInput(hintInput, '约 60 秒')
    await flush(5)
    genBtn.click()
    await flush(20)
    out.storyboard.genCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path.endsWith('/storyboard/generate'),
    )
    // v0.1.6：生成 202 → trackFilmTask(id=ft-sb, stage=storyboard)
    out.storyboard.trackedAfterGen = smock().tracked.some(
      (x) => x.id === 'ft-sb' && x.stage === 'storyboard',
    )
    // v0.1.44 分镜章节下拉：首字段选项（全部剧情 + 第 N 章 · 标题——chapters
    // index 派生；fixture 两章）
    const chapterSelect = el.querySelector('.sbg-body select') as HTMLSelectElement
    out.storyboard.chapterOptions = Array.from(chapterSelect?.options ?? []).map(
      (o) => o.textContent ?? '',
    )
    app.unmount()
  }

  // ============ ①.6.5 分镜章节预填（v0.1.44：章节卡「从此章生成分镜」） ============
  {
    // pending 置位后挂载分镜页 → 面板自动展开 + 下拉选中该章；生成请求带
    // chapter_range（面板挂载消费即清空 pending）
    mockPendingSbChapter.value = '2'
    const { app, el } = mountPage(StoryboardPage)
    await flush(20)
    out.storyboard.chapterPrefillOpen = !!el.querySelector('.sbg-body')
    const chapterSelect = el.querySelector('.sbg-body select') as HTMLSelectElement
    out.storyboard.chapterPrefillValue = chapterSelect?.value ?? ''
    const pendingConsumed = mockPendingSbChapter.value === null
    const genBtn = Array.from(el.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('生成分镜'),
    ) as HTMLButtonElement
    genBtn.click()
    await flush(20)
    const calls = smock().calls.filter(
      (c) => c.method === 'POST' && c.path.endsWith('/storyboard/generate'),
    )
    out.storyboard.chapterRangeGenCall = calls[calls.length - 1] as {
      path: string
      body: unknown
    }
    out.storyboard.chapterPrefillValue = pendingConsumed
      ? out.storyboard.chapterPrefillValue
      : `(未消费: ${mockPendingSbChapter.value})`
    app.unmount()
  }

  // ============ ② CastingPage：定制器（六类 Tab / 部件面板 / 主视图生成） ============
  {
    const { app, el } = mountPage(CastingPage)
    await flush()
    // v0.1.6「AI 提取定妆对象」页头钮 → POST casting/extract 202 →
    // trackFilmTask(id=ft-extract, stage=casting)
    const extractBtn = Array.from(el.querySelectorAll('.fh-head-actions button')).find((b) =>
      (b.textContent ?? '').includes('提取'),
    ) as HTMLButtonElement | undefined
    extractBtn?.click()
    await flush(20)
    const extractTracked = smock().tracked.some(
      (x) => x.id === 'ft-extract' && x.stage === 'casting',
    )
    const tabs = Array.from(el.querySelectorAll('.fh-tab'))
    const cards = Array.from(el.querySelectorAll('.cast-obj-card'))
    // 选中对象（小明；fixture：parts 脸型=瓜子脸/发型=长发 + front/front-v2 两版）
    cards[0].dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    // 中央主槽视图（front-v2 最新 → 已填态 + 历史缩略 2 版）
    const stage = el.querySelector('.cast-cz-stage') as HTMLElement
    const cz: FlowSmokeResult['casting'] = {
      tabs: tabs.map((n) => (n.textContent ?? '').trim()),
      charactersCards: cards.map((n) => (n.textContent ?? '').trim()),
      propsCardsAfterSwitch: [],
      propsGetCalled: false,
      czStageFilled: stage.classList.contains('is-filled'),
      czPlaceholderShown: !!el.querySelector('.cast-cz-ph'),
      czCaption: (el.querySelector('.cast-cz-caption')?.textContent ?? '').trim(),
      partRowCount: 0,
      partRowKeys: [],
      historyThumbCount: el.querySelectorAll('.cast-cz-history-thumb').length,
      auxSlotCount: 0,
      auxSlotLabels: [],
      chipCountOnExpand: 0,
      chipActiveAfterPick: false,
      dirtyBadgeAfterPick: false,
      partsPutBeforeGen: undefined,
      mainGenCall: undefined,
      secondPartsPut: undefined,
      secondMainGenCall: undefined,
      ownerBadgeText: '',
      claimPuts: 0,
      claimObjectOwner: undefined,
      extractTracked,
    }
    const partRows = Array.from(el.querySelectorAll('.cast-part-row'))
    cz.partRowCount = partRows.length
    cz.partRowKeys = partRows.map((n) => (n.querySelector('.cast-part-name')?.textContent ?? '').trim())
    // 展开发型槽 → 预设 chips（6 预设 + 自定义）→ 点「短发」（当前=长发 → dirty）
    const hairRow = partRows.find((n) =>
      (n.querySelector('.cast-part-name')?.textContent ?? '').includes('发型'),
    ) as HTMLElement
    ;(hairRow.querySelector('.cast-part-head') as HTMLElement).dispatchEvent(
      new Event('click', { bubbles: true }),
    )
    await flush(5)
    const chips = Array.from(hairRow.querySelectorAll('.cast-part-chip')) as HTMLElement[]
    cz.chipCountOnExpand = chips.length
    const shortChip = chips.find((c) => (c.textContent ?? '').trim() === '短发') as HTMLElement
    shortChip.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    cz.chipActiveAfterPick = shortChip.classList.contains('is-active')
    cz.dirtyBadgeAfterPick = !!el.querySelector('.cast-cz-parts-head .fh-pill-amber')
    // 「按当前部件生成主视图」→ PUT parts（脸型保持/发型=短发）→ POST
    // views/generate {view: front, 无 prompt}（组合提示词由后端组装）
    const postsBefore = smock().calls.length
    const genBtn = el.querySelector('.cast-cz-footer .fh-btn-primary') as HTMLButtonElement
    genBtn.click()
    await flush(25)
    const callsAfterGen = smock().calls.slice(postsBefore)
    cz.partsPutBeforeGen = callsAfterGen.find(
      (c) => c.method === 'PUT' && /\/casting\/characters\/[^/]+$/.test(safePath(c.path)),
    )
    cz.mainGenCall = callsAfterGen.find(
      (c) => c.method === 'POST' && c.path.endsWith('/views/generate'),
    )
    // 再改「脸型」为圆脸 → 二次生成（两次 PUT parts 仅发型/脸型对应键差异）
    const faceRow = Array
      .from(el.querySelectorAll('.cast-part-row'))
      .find((n) => (n.querySelector('.cast-part-name')?.textContent ?? '').includes('脸型')) as HTMLElement
    ;(faceRow.querySelector('.cast-part-head') as HTMLElement).dispatchEvent(
      new Event('click', { bubbles: true }),
    )
    await flush(5)
    const faceChips = Array.from(faceRow.querySelectorAll('.cast-part-chip')) as HTMLElement[]
    const roundChip = faceChips.find((c) => (c.textContent ?? '').trim() === '圆脸') as HTMLElement
    roundChip.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    const postsBefore2 = smock().calls.length
    genBtn.click()
    await flush(25)
    const callsAfterGen2 = smock().calls.slice(postsBefore2)
    cz.secondPartsPut = callsAfterGen2.find(
      (c) => c.method === 'PUT' && /\/casting\/characters\/[^/]+$/.test(safePath(c.path)),
    )
    cz.secondMainGenCall = callsAfterGen2.find(
      (c) => c.method === 'POST' && c.path.endsWith('/views/generate'),
    )
    // 「更多视图」折叠区展开 → 辅助槽（characters 排除主槽 front：4 槽）
    const moreToggle = el.querySelector('.cast-cz-more-toggle') as HTMLElement
    moreToggle.dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    const auxSlots = Array.from(el.querySelectorAll('.fh-view-grid .fh-view-slot'))
    cz.auxSlotCount = auxSlots.length
    cz.auxSlotLabels = auxSlots.map(
      (n) => (n.querySelector('.fh-view-label span')?.textContent ?? '').trim(),
    )
    // 他人认领徽章（fixture：characters/小明 owner=小红）
    cz.ownerBadgeText = (el.querySelector('.cast-obj-card .fh-owner')?.textContent ?? '').trim()
    out.casting = cz
    // 切 props Tab → 重载该类对象；长剑未认领 → 「认领」按钮 → PUT ownership
    const propsTab = tabs.find((n) => (n.textContent ?? '').includes('武器')) as HTMLElement
    propsTab.dispatchEvent(new Event('click', { bubbles: true }))
    await flush()
    out.casting.propsGetCalled = smock().calls.some(
      (c) => c.method === 'GET' && c.path.endsWith('/casting/props'),
    )
    out.casting.propsCardsAfterSwitch = Array.from(el.querySelectorAll('.cast-obj-card')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    const claimBtn = el.querySelector(
      '.cast-obj-card .fh-btn-mini:not(.fh-btn-danger)',
    ) as HTMLButtonElement
    claimBtn.click()
    await flush(5)
    out.casting.claimPuts = smock().ownershipPuts.length
    out.casting.claimObjectOwner = smock().ownershipPuts[0]?.casting_objects?.['props/长剑']?.owner
    app.unmount()
  }

  // ============ ③ ComposePage：dist 版本 / cache commit / 预览切工作台 ============
  {
    const { app, el } = mountPage(ComposePage)
    await flush()
    out.compose = {
      distRows: Array.from(el.querySelectorAll('.fh-card')[0].querySelectorAll('.fh-row')).map((n) =>
        (n.textContent ?? '').trim(),
      ),
      cacheCards: Array.from(el.querySelectorAll('.cp-cache-card')).map((n) =>
        (n.textContent ?? '').trim(),
      ),
      commitCall: undefined,
      previewViewSwitch: undefined,
    }
    // 「确认采用」→ POST cache/:file/commit（带 author）
    const commitBtn = el.querySelector('.cp-cache-card .fh-btn-primary') as HTMLButtonElement
    commitBtn.click()
    await flush(5)
    out.compose.commitCall = smock().calls.find((c) => c.method === 'POST' && c.path.includes('/cache/') && c.path.endsWith('/commit'))
    // 「预览成片」→ 切工作台（引擎缺失环境仅切视图）
    const previewBtn = Array.from(el.querySelectorAll('.fh-btn-mini')).find((n) =>
      (n.textContent ?? '').includes('预览成片'),
    ) as HTMLButtonElement
    const before = smock().viewSwitches.length
    previewBtn.click()
    await flush(5)
    out.compose.previewViewSwitch = smock().viewSwitches[before]
    app.unmount()
  }

  // ============ ④ AudioPage：BGM 表单校验 + 提交带作者 ============
  {
    const { app, el } = mountPage(AudioPage)
    await flush()
    const side = el.querySelector('.fh-col-side') as HTMLElement
    const trigger = side.querySelectorAll('input[type="text"]')[0] as HTMLInputElement
    const submit = side.querySelector('.fh-btn-primary') as HTMLButtonElement
    // 空 trigger 提交 → 校验错误（不发 POST）
    const postsBefore = smock().calls.filter((c) => c.method === 'POST').length
    submit.click()
    await flush(5)
    const errBox = side.querySelector('.fh-error-box') as HTMLElement
    out.audio = {
      emptyFormError: (errBox?.textContent ?? '').trim(),
      emptyFormNoPost: smock().calls.filter((c) => c.method === 'POST').length === postsBefore,
      secondError: '',
      postCall: undefined,
      genCall: undefined,
    }
    // 填 trigger 点「建条目并 AI 生成」→ POST audio/bgm {info,author}（无文件
    // 不带 track_b64）+ 链式 POST :track/generate {model_ref,author}
    fireInput(trigger, '开场')
    const genEntryBtn = Array.from(side.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('✨'),
    ) as HTMLButtonElement
    genEntryBtn.click()
    await flush(5)
    out.audio.secondError = (
      (side.querySelector('.fh-error-box') as HTMLElement | null)?.textContent ?? ''
    ).trim()
    out.audio.postCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path.endsWith('/audio/bgm'),
    )
    out.audio.genCall = smock().calls.find(
      (c) => c.method === 'POST' && /\/audio\/bgm\/[^/]+\/generate$/.test(c.path),
    )
    app.unmount()
  }

  // ============ ⑤ SettingsPage：成员 / 分区认领 / 活动流 ============
  {
    const { app, el } = mountPage(SettingsPage)
    await flush()
    const leftCard = el.querySelectorAll('.fh-card')[0]
    out.settings = {
      memberBadges: Array.from(leftCard.querySelectorAll('.fh-pill-blue')).map((n) =>
        (n.textContent ?? '').trim(),
      ),
      activityRows: el.querySelectorAll('.fh-card')[1].querySelectorAll('.fh-row').length,
      ownershipPutMembers: undefined,
      sectionOwnerPut: undefined,
    }
    // 添加成员「张三」→ PUT ownership.json（saveOwnership 本地态）
    const inputs = leftCard.querySelectorAll('input.fh-input')
    fireInput(inputs[1] as HTMLInputElement, '张三')
    const addBtn = leftCard.querySelector('.fh-btn-primary') as HTMLButtonElement
    addBtn.click()
    await flush(5)
    out.settings.ownershipPutMembers = smock().calls
      .filter((c) => c.method === 'PUT' && c.path.endsWith('/files/ownership.json'))
      .map((c) => (c.body as { content_b64?: string })?.content_b64)
      .filter(Boolean)
      .map((b64) => unb64Json(b64 as string).members as string[])
      .pop()
    // 分区认领：story 行下拉选「小明」→ PUT
    const storySelect = leftCard.querySelectorAll('select.fh-select')[0] as HTMLSelectElement
    fireChange(storySelect, '小明')
    await flush(5)
    const putBodies = smock().calls
      .filter((c) => c.method === 'PUT' && c.path.endsWith('/files/ownership.json'))
      .map((c) => (c.body as { content_b64?: string })?.content_b64)
      .filter(Boolean)
      .map((b64) => unb64Json(b64 as string))
    out.settings.sectionOwnerPut = (putBodies[putBodies.length - 1] as { sections?: Record<string, { owner?: string }> })?.sections?.story?.owner
    app.unmount()
  }

  // ============ ⑥ ModelsPage 项目级模型设置（v0.1.38）+「项目默认」生成 ============
  {
    const { app, el } = mountPage(ModelsPage)
    await flush(25)
    const rows = Array.from(el.querySelectorAll('.models-row'))
    out.models = {
      rows: rows.map((n) => (n.textContent ?? '').trim()),
      reservedRows: rows.filter((n) => (n.textContent ?? '').includes('预留槽位')).map((n) =>
        (n.textContent ?? '').trim(),
      ),
      okDots: rows.filter((n) => n.querySelector('.models-dot.ok')).length,
      modalOpened: false,
      putCall: undefined,
      snapshotAfterPut: undefined,
      projectDefaultGenCall: undefined,
      storyGenTracked: false,
      routing: {
        panelOpened: false,
        hintShown: false,
        agentHintShown: false,
        rowsBefore: 0,
        rowsAfterAdd: 0,
        putCall: undefined as { path: string; body: unknown } | undefined,
        savedMark: false,
        rowDots: 0,
      },
      addApi: {
        dialogOpened: false,
        autoName: '',
        autoNameAfterCapSwitch: '',
        createCall: undefined,
        modelsPutAfterAdd: undefined,
        notice: '',
        modalClosedAfterDone: false,
        failError: '',
        failNoModelsPut: false,
      },
    }
    // 点第一行（chat）「配置」→ 弹窗开 → 源选渠道 → 选渠道 → 保存 → PUT /models
    //（v0.1.44 行内按钮两枚：⚙ 高级路由在前——配置取最后一个 .fh-btn）
    const configureBtn = (Array.from(rows[0]?.querySelectorAll('.fh-btn') ?? []).pop() ??
      rows[0]?.querySelector('.fh-btn')) as HTMLButtonElement
    configureBtn.click()
    await flush(5)
    const modal = el.querySelector('.fh-modal') as HTMLElement | null
    out.models.modalOpened = !!modal
    const selects = Array.from(el.querySelectorAll('.fh-modal select')) as HTMLSelectElement[]
    fireChange(selects[0], 'channel')
    await flush(5)
    const chSelects = Array.from(el.querySelectorAll('.fh-modal select')) as HTMLSelectElement[]
    fireChange(chSelects[1], 'ch-1')
    // 等 v-model 落地 + 重渲染（否则保存按钮仍处 disabled，点击无效）
    await flush(5)
    const saveBtn = el.querySelector('.fh-form-actions .fh-btn-primary') as HTMLButtonElement
    saveBtn.click()
    await flush(20)
    out.models.putCall = smock().calls.find(
      (c) => c.method === 'PUT' && c.path.endsWith('/film/projects/p1/models'),
    )
    // PUT 后快照回显（GET models 再读——mock 渠道已写入 chat 位）
    const chatEntry = mockProjectModels.value?.capabilities?.find((c) => c.capability === 'chat')
    out.models.snapshotAfterPut = chatEntry as { source?: unknown; channel_id?: unknown } | undefined

    // —— v0.1.44 高级路由：⚙ 展开折叠区 → 添加路由（任务下拉选 story.clean /
    //    渠道 ch-1）→ 再加一条兜底 → 保存 → PUT /models 带 routes（整组替换）——
    const routeToggle = Array.from(rows[0]?.querySelectorAll('button') ?? []).find((b) =>
      (b.textContent ?? '').includes('高级路由'),
    ) as HTMLButtonElement
    routeToggle.click()
    await flush(5)
    const routePanel = el.querySelector('.models-route-panel') as HTMLElement | null
    out.models.routing = {
      panelOpened: !!routePanel,
      hintShown: !!(routePanel?.textContent ?? '').includes('自上而下匹配'),
      agentHintShown: !!(routePanel?.textContent ?? '').includes('files/models.json'),
      rowsBefore: el.querySelectorAll('.models-route-row').length,
      putCall: undefined,
      savedMark: false,
      rowDots: 0,
    }
    const addRouteBtn = Array.from(el.querySelectorAll('button')).find((b) =>
      (b.textContent ?? '').includes('添加路由'),
    ) as HTMLButtonElement
    addRouteBtn.click()
    await flush(3)
    addRouteBtn.click()
    await flush(3)
    const routeRows = Array.from(el.querySelectorAll('.models-route-row'))
    out.models.routing.rowsAfterAdd = routeRows.length
    // 第一行：任务下拉选 story.clean（预设枚举）→ 源选渠道 → 渠道下拉选 ch-1
    //（select 顺序 [task, source]；source=channel 重渲染后追加 channel 下拉）
    const firstSelects = routeRows[0].querySelectorAll('select.fh-select')
    fireChange(firstSelects[0] as HTMLSelectElement, 'story.clean')
    await flush(3)
    fireChange(firstSelects[1] as HTMLSelectElement, 'channel')
    await flush(3)
    const chSel = routeRows[0].querySelectorAll('select.fh-select')
    fireChange(chSel[chSel.length - 1] as HTMLSelectElement, 'ch-1')
    await flush(3)
    // 第二行：任务留缺省（兜底路由）+ 源渠道 + 渠道 ch-1
    const secondSelects = routeRows[1].querySelectorAll('select.fh-select')
    fireChange(secondSelects[1] as HTMLSelectElement, 'channel')
    await flush(3)
    const chSel2 = routeRows[1].querySelectorAll('select.fh-select')
    fireChange(chSel2[chSel2.length - 1] as HTMLSelectElement, 'ch-1')
    await flush(3)
    const routeSaveBtn = Array.from(el.querySelectorAll('.models-route-actions button')).find(
      (b) => (b.textContent ?? '').includes('保存路由'),
    ) as HTMLButtonElement
    routeSaveBtn.click()
    await flush(25)
    out.models.routing.putCall = [...smock().calls]
      .reverse()
      .find((c) => c.method === 'PUT' && c.path.endsWith('/film/projects/p1/models'))
    out.models.routing.savedMark = !!(routePanel?.textContent ?? '').includes('路由已保存')
    out.models.routing.rowDots = el.querySelectorAll('.models-route-row .models-dot').length
    routeToggle.click() // 收起（后续断言基于快照）
    await flush(3)

    // —— v0.1.39 直填 API：页头「＋ 添加 API」→ 弹窗表单 → POST gateway/
    //    channels 建渠道 → 链式 PUT :id/models 设能力位默认 → ✓ 提示 ——
    const addBtn = Array.from(el.querySelectorAll('.fh-head button')).find((b) =>
      (b.textContent ?? '').includes('添加 API'),
    ) as HTMLButtonElement
    addBtn.click()
    await flush(5)
    const addModal = (el.querySelector('#film-models-add-title')?.closest('.fh-modal') ??
      null) as HTMLElement | null
    out.models.addApi.dialogOpened = !!addModal
    const addInputs = Array.from(
      (addModal ?? el).querySelectorAll('input.fh-input'),
    ) as HTMLInputElement[]
    fireInput(addInputs[0], 'http://192.168.1.5:8000') // API 地址（裸 host——提交规整补 /v1）
    fireInput(addInputs[1], 'sk-test-123') // API Key（password 输入）
    fireInput(addInputs[2], 'cosyvoice-v2') // 模型名 → 名称自动联动（缺省 chat 位）
    await flush(5)
    out.models.addApi.autoName = addInputs[3].value
    const capSelect = (addModal ?? el).querySelector('select.fh-select') as HTMLSelectElement
    fireChange(capSelect, 'tts')
    await flush(5)
    out.models.addApi.autoNameAfterCapSwitch = addInputs[3].value
    const addSubmit = (addModal ?? el).querySelector(
      '.fh-form-actions .fh-btn-primary',
    ) as HTMLButtonElement
    addSubmit.click()
    await flush(25)
    out.models.addApi.createCall = smock().calls.find(
      (c) => c.method === 'POST' && c.path === '/api/v1/gateway/channels',
    )
    out.models.addApi.modelsPutAfterAdd = [...smock().calls]
      .reverse()
      .find((c) => c.method === 'PUT' && c.path.endsWith('/film/projects/p1/models'))
    out.models.addApi.notice = (el.querySelector('.models-add-notice')?.textContent ?? '').trim()
    out.models.addApi.modalClosedAfterDone = !el.querySelector('#film-models-add-title')

    // —— 失败路径：后端校验（渠道重名等）→ 红条透传、不发 PUT /models ——
    addBtn.click()
    await flush(5)
    const failModal = (el.querySelector('#film-models-add-title')?.closest('.fh-modal') ??
      null) as HTMLElement | null
    const failInputs = Array.from(
      (failModal ?? el).querySelectorAll('input.fh-input'),
    ) as HTMLInputElement[]
    fireInput(failInputs[0], 'http://10.0.0.9:9000/v1') // 已带 /v1——原样保留
    fireInput(failInputs[2], 'gpt-4o')
    await flush(5)
    smock().channelCreateFail = '渠道名称已存在：gpt-4o · 文本（LLM）'
    const putsBeforeFail = smock().calls.filter(
      (c) => c.method === 'PUT' && c.path.endsWith('/film/projects/p1/models'),
    ).length
    const failSubmit = (failModal ?? el).querySelector(
      '.fh-form-actions .fh-btn-primary',
    ) as HTMLButtonElement
    failSubmit.click()
    await flush(15)
    out.models.addApi.failError = (
      (failModal ?? el).querySelector('.fh-error-box')?.textContent ?? ''
    ).trim()
    out.models.addApi.failNoModelsPut =
      smock().calls.filter(
        (c) => c.method === 'PUT' && c.path.endsWith('/film/projects/p1/models'),
      ).length === putsBeforeFail
    app.unmount()

    // —— 「项目默认」生成：StoryPage chat 下拉选 project → AI 写剧情 →
    //    POST story/generate body **不含 model_ref 字段** ——
    const story2 = mountPage(StoryPage)
    await flush(20)
    // 页头两个下拉：第 1=改编底稿（source_file）、第 2=chat 模型源——选后者
    const chatSelect = story2.el.querySelectorAll('.fh-head select.fh-select')[1] as HTMLSelectElement
    fireChange(chatSelect, 'project')
    await flush(5)
    const genBtn = Array.from(story2.el.querySelectorAll('.fh-head button')).find((b) =>
      (b.textContent ?? '').includes('AI 写剧情'),
    ) as HTMLButtonElement
    const postsBefore = smock().calls.filter((c) => c.method === 'POST').length
    genBtn.click()
    await flush(20)
    const genPosts = smock().calls.filter((c) => c.method === 'POST').slice(postsBefore)
    out.models.projectDefaultGenCall = genPosts.find((c) => c.path.endsWith('/story/generate'))
    // v0.1.6：「AI 写剧情」202 → trackFilmTask(id=ft-story, stage=story)
    out.models.storyGenTracked = smock().tracked.some(
      (x) => x.id === 'ft-story' && x.stage === 'story',
    )
    story2.app.unmount()
  }

  // ============ ⑦ 任务中心 stage 标签（v0.1.6 taskLabel 同一纯函数） ============
  {
    const tt = (key: string) => i18n.global.t(key)
    out.taskLabels = Object.fromEntries(
      [
        'script',
        'storyboard',
        'story',
        'story.clean',
        'story.chapterize',
        'story.profile',
        'story.embed',
        'casting',
        'image',
        'video',
        'tts',
        'music',
        'compose',
        'portrait',
      ].map((s) => [s, filmTaskStageLabel(s, tt)]),
    )
  }

  return out
}
