// =============================================================================
// hub-smoke-harness.ts —— FilmHub 显性大厅冒烟 harness（happy-dom 挂载，
// v0.1.1）。
//
// 由 scripts/hub-smoke.mjs 用 vite（plugin-vue 编译 .vue，vue/vue-i18n 真实
// 打包）构建为临时 ESM，在 happy-dom 全局就绪后 import 并调用 run()：
//   · HubLobby 大厅：品牌栏 / 项目卡（标题+ratio+五阶段进度点）/ 搜索过滤 /
//     丰富模式（成员 chips + 最近活动一句 + 成本小字）与逐项目静默降级 /
//     卡片操作 emit（打开 / Hub 浏览 / 删除）；
//   · HubBrowse 项目 Hub 浏览：文件树渲染与目录折叠 / 内容区四形态
//     （文本 pre / 图片 data URL / audio / video / 二进制提示）/ 面包屑 /
//     四 Tab（文件/活动/成本/接入指南 curl 三段）/「在工作台打开」互跳映射；
//     v0.1.37.1 初始定位：pendingHubFile（SideNav 树卡点击文件）→ 挂载即
//     选中该文件 + 加载内容 + 祖先目录自动展开；
//   · CastingPage 集成：pendingCastSelect（Hub 浏览 casting 路径 → 定妆页
//     切类 + 选中对象 + 消费清空）；
//   · SideNav 导航层级：顶部 🎬 FilmHub 回大厅项 + Hub 浏览项 + 树卡
//     注册表式（点击文件 emit file-click 完整路径；迷你预览浮层已删）。
// HTTP 走宿主桥 mock（globalThis.__NEXOS_HOST__.api，由 .mjs 注入 fixtures）。
// 断言在 .mjs（node:assert）。
// =============================================================================
import { computed, createApp, defineComponent, h, nextTick, ref, type App } from 'vue'
import { createI18n } from 'vue-i18n'
import HubLobby from '../src/flow/HubLobby.vue'
import HubBrowse from '../src/flow/HubBrowse.vue'
import SideNav from '../src/flow/SideNav.vue'
import CastingPage from '../src/flow/CastingPage.vue'
import zhCN from '../src/i18n/zh-CN.json'
import type { FilmProject } from '../src/api'
import { FLOW_CONTEXT_KEY, PROJECT_DEFAULT_KEY, type FlowContext } from '../src/flow/flowContext'
import type { FilmOwnership } from '../src/flow/collab'
import type { FilmStage, FlowView } from '../src/flow/flowTypes'

/** 与 .mjs 的共享观测面（mjs 注入）。 */
interface Smock {
  calls: { method: string; path: string; body?: unknown }[]
  viewSwitches: string[]
  ownershipPuts: FilmOwnership[]
}
function smock(): Smock {
  return (globalThis as { __FLOW_SMOKE__: Smock }).__FLOW_SMOKE__
}

/** 大厅测试项目 fixture（props 直传——列表本身由 FilmStudio 加载）。 */
function proj(
  id: string,
  title: string,
  idea: string,
  script: { shot: number }[],
  artifacts: { name: string; bytes: number }[],
  updated: string,
): FilmProject {
  return {
    id,
    title,
    idea,
    ratio: '16:9',
    dir: `/tank/film/${id}`,
    created_at: '2026-09-01T00:00:00Z',
    updated_at: updated,
    script,
    artifacts,
  } as FilmProject
}

const PROJECTS: FilmProject[] = [
  // p1：final.mp4 → 阶段推导 compose；丰富数据全命中
  proj('p1', '星际快递员', '一只柯基在月球快递站的一天', [{ shot: 1 }, { shot: 2 }], [
    { name: 'shot-1.png', bytes: 1000 },
    { name: 'shot-1.mp4', bytes: 9000 },
    { name: 'line-1.mp3', bytes: 800 },
    { name: 'bgm.mp3', bytes: 900 },
    { name: 'final.mp4', bytes: 50000 },
  ], '2026-09-06T09:00:00Z'),
  // p2：仅 shot png → casting；activity/cost 404 → 部分降级
  proj('p2', '深海灯塔', '守塔人与最后一束光', [{ shot: 1 }, { shot: 2 }, { shot: 3 }], [
    { name: 'shot-1.png', bytes: 1200 },
  ], '2026-09-05T09:00:00Z'),
  // p3：空 → story；丰富数据全 404 → 全降级素卡
  proj('p3', '无题草稿', '只有一个想法', [], [], '2026-09-04T09:00:00Z'),
]

/** mock FlowContext（HubBrowse / CastingPage 消费；数据经宿主桥 mock api）。 */
const navView = ref<FlowView>('hub')
const mockOwnership = ref<FilmOwnership | null>({
  members: ['小明', '小红'],
})
const mockAuthor = ref('小明')
const mockPendingCast = ref<{ type: 'characters' | 'props' | 'pets' | 'formations' | 'actions' | 'scenes'; name: string } | null>(null)
const mockPendingHub = ref<string | null>(null)
/** v0.1.44 章节卡「从此章生成分镜」→ StoryboardGenPanel 预填（挂载消费）。 */
const mockPendingSbChapter = ref<string | null>(null)
const mockProject = ref<FilmProject>(PROJECTS[0])
const mockCtx: FlowContext = {
  project: mockProject,
  optionsFor: (cap) => [
    { label: '本地', options: [{ key: cap === 'image' ? 'local' : `llm:${cap}`, label: `mock-${cap}`, relay: false }] },
  ],
  hasOptionsFor: () => true,
  modelSel: { chat: 'llm:chat', image: 'local', video: 'ch:c1', tts: 'ch:c1', music: 'ch:c1' },
  modelRefFor: (cap) => ({ source: 'local', capability: cap }),
  addTracked: () => undefined,
  // v0.1.7 FlowContext 扩展面（本 harness 未挂 StoryPage——空快照即可）
  trackFilmTask: () => undefined,
  storyTasks: computed(() => []),
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
  saveOwnership: async () => true,
  refreshCollab: async () => undefined,
  pendingCastSelect: mockPendingCast,
  pendingStoryboardChapter: mockPendingSbChapter,
  pendingHubFile: mockPendingHub,
  // —— v0.1.38 项目级模型设置（页面模板渲染 defaultModelSummary 等字段） ——
  projectModels: ref(null),
  reloadProjectModels: async () => undefined,
  defaultModelSummary: () => '',
  modelSelReady: () => true,
  isProjectDefaultSel: (cap: string) => mockCtx.modelSel[cap] === PROJECT_DEFAULT_KEY,
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

/** 挂载（app 级 provide FLOW_CONTEXT_KEY + i18n；opts 控制是否注入 ctx）。 */
function mountPage(
  comp: Parameters<typeof createApp>[0],
  props: Record<string, unknown> = {},
  withCtx = true,
): { app: App; el: HTMLElement } {
  const el = document.createElement('div')
  document.body.appendChild(el)
  const app = createApp(comp, props)
  app.use(i18n)
  if (withCtx) app.provide(FLOW_CONTEXT_KEY, mockCtx)
  app.mount(el)
  return { app, el }
}

/** 微任务 + 渲染 flush（mock api promise 落地后再 nextTick）。 */
async function flush(ms = 20): Promise<void> {
  await new Promise((r) => setTimeout(r, ms))
  await nextTick()
}

// 纯函数再导出（.mjs 直接断言同一代码路径）
export {
  buildHubTree,
  buildStandaloneUrl,
  deriveStageFromProject,
  hubCastSelect,
  hubFileIcon,
  hubTargetView,
  isHubTextPath,
  parseStandaloneQuery,
} from '../src/flow/flowFiles'
// v0.1.7 UI 重设计：大厅表头筛选纯函数（搜索 × 阶段 × 类别）
export { filterHubProjects, hubRatioOptions } from '../src/flow/hubFilters'

function fireInput(el: HTMLInputElement, value: string): void {
  el.value = value
  el.dispatchEvent(new Event('input', { bubbles: true }))
}

function rowByText(el: HTMLElement, text: string): HTMLElement | null {
  return (
    Array.from(el.querySelectorAll('.hub-tree-row')).find((n) =>
      (n.querySelector('.hub-tree-name')?.textContent ?? '').trim() === text,
    ) ?? null
  )
}

export interface HubSmokeResult {
  lobby: {
    brandText: string
    cardTitles: string[]
    ratioPills: string[]
    cardCount: number
    dotCountCard0: number
    dotStatesCard0: string[]
    dotStatesCard2: string[]
    searchFiltered: number
    memberChipsCard0: string[]
    latestCard0: string
    costCard0: string
    memberChipsCard1: string[]
    latestCard1: boolean
    memberChipsCard2: number
    latestCard2: boolean
    chipsAfterRichOff: number
    openEmits: number
    browseEmits: number
    deleteEmits: number
    /** v0.1.7 UI 重设计：卡片三态类名 / 表头筛选 popover 开合与过滤。 */
    nx: {
      cardInteractiveCount: number
      cardSelectedByFocus: boolean
      filterOpenWorks: boolean
      filterCloseWorks: boolean
      stageFiltered: number
      stageReset: number
    }
  }
  browse: {
    treeRows: string[]
    rowsBeforeCollapse: number
    rowsAfterCollapse: number
    rowsAfterExpandNested: number
    textContent: string
    jsonContent: string
    imgSrcPrefix: string
    audioCount: number
    videoCount: number
    binaryNote: boolean
    crumbsText: string
    tabs: string[]
    activityRows: number
    costTables: number
    curlBlocks: number
    curlHasPutPath: boolean
    openFlowViews: string[]
    pendingCastSet: { type: string; name: string } | null
    /** v0.1.5：HubBrowse 左栏树卡非 expandable——无「⤢ 展开」钮。 */
    treeExpandAbsent: boolean
  }
  /** v0.1.37.1 初始定位（SideNav 树卡点击文件 → pendingHubFile → HubBrowse）。 */
  locate: {
    contentText: string
    selectedRowPath: string | null
    ancestorExpanded: boolean
    pendingConsumed: boolean
    crumbsText: string
  }
  casting: {
    activeCardText: string
    pendingConsumed: boolean
  }
  nav: {
    homeText: string
    /** v0.1.5：「Hub 浏览」导航项已删（视图保留——树卡/深链到达）。 */
    hubItemAbsent: boolean
    /** 视图项行尾 ⧉ 外链钮数（五阶段+工作台+协作+设置+模型 = 9；FilmHub 项除外）。 */
    extCount: number
    /** ⧉ 点击捕获的 window.open URL（嵌入桌面模式 /apps-assets/film/ 前缀）。 */
    extOpens: string[]
    /** ⧉ 点击不触发本行切页（stopPropagation）。 */
    extClickNotSelect: boolean
    homeEmits: number
    hubSelects: string[]
    treeCardRows: string[]
    treeCardAfterCollapseAll: number
    fileClicks: string[]
    popAbsent: boolean
    browseEmits: number
    /** v0.1.5 树卡原位大展开（上树 55% / 下内容 45% 三形态 + 收起恢复）。 */
    expand: {
      classesOn: boolean
      insideNav: boolean
      previewPresent: boolean
      text: string
      clicksNotEmitted: boolean
      img: boolean
      audio: number
      complex: boolean
      collapsedBack: boolean
    }
  }
}

export async function run(): Promise<HubSmokeResult> {
  const out = {} as HubSmokeResult

  // ============ ① HubLobby 大厅：卡渲染 / 阶段进度 / 搜索 / 丰富模式 ============
  {
    const events = { open: 0, browse: 0, delete: 0 }
    const { app, el } = mountPage(
      HubLobby,
      {
        projects: PROJECTS,
        loading: false,
        error: '',
        onRefresh: () => undefined,
        onCreate: () => undefined,
        onOpen: () => {
          events.open++
        },
        onBrowse: () => {
          events.browse++
        },
        onDelete: () => {
          events.delete++
        },
      },
      false,
    )
    await flush(30)
    const cards = Array.from(el.querySelectorAll('.hub-card'))
    const dots0 = Array.from(cards[0].querySelectorAll('.hub-stage-dot'))
    const dots2 = Array.from(cards[2].querySelectorAll('.hub-stage-dot'))
    const stateOf = (n: Element) =>
      n.classList.contains('is-done')
        ? 'done'
        : n.classList.contains('is-current')
          ? 'current'
          : 'todo'
    out.lobby = {
      brandText: (el.querySelector('.hub-brand-name')?.textContent ?? '').trim(),
      cardTitles: cards.map((c) => (c.querySelector('.hub-card-title')?.textContent ?? '').trim()),
      ratioPills: cards.map((c) => (c.querySelector('.hub-pill-ratio')?.textContent ?? '').trim()),
      cardCount: cards.length,
      dotCountCard0: dots0.length,
      dotStatesCard0: dots0.map(stateOf),
      dotStatesCard2: dots2.map(stateOf),
      searchFiltered: 0,
      memberChipsCard0: [],
      latestCard0: '',
      costCard0: '',
      memberChipsCard1: [],
      latestCard1: false,
      memberChipsCard2: 0,
      latestCard2: false,
      chipsAfterRichOff: 0,
      openEmits: 0,
      browseEmits: 0,
      deleteEmits: 0,
    }
    // 丰富数据已落地（mock api → richMap）
    out.lobby.memberChipsCard0 = Array.from(cards[0].querySelectorAll('.hub-member-chip')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    out.lobby.latestCard0 = (cards[0].querySelector('.hub-latest')?.textContent ?? '').trim()
    out.lobby.costCard0 = (cards[0].querySelector('.hub-card-foot')?.textContent ?? '').trim()
    out.lobby.memberChipsCard1 = Array.from(cards[1].querySelectorAll('.hub-member-chip')).map((n) =>
      (n.textContent ?? '').trim(),
    )
    out.lobby.latestCard1 = !!cards[1].querySelector('.hub-latest')
    out.lobby.memberChipsCard2 = cards[2].querySelectorAll('.hub-member-chip').length
    out.lobby.latestCard2 = !!cards[2].querySelector('.hub-latest')
    // 搜索过滤（标题/idea）
    const search = el.querySelector('.hub-search') as HTMLInputElement
    fireInput(search, '柯基')
    await flush(5)
    out.lobby.searchFiltered = el.querySelectorAll('.hub-card').length
    fireInput(search, '')
    await flush(5)
    // 卡片操作 emit（打开 / Hub 浏览 / 删除）
    // v0.1.7 重设计：操作条按钮 = NxButton（nx-btn 基座类）
    ;(cards[0].querySelector('.hub-actions .nx-btn--primary') as HTMLButtonElement).click()
    ;(cards[0].querySelectorAll('.hub-actions .nx-btn')[1] as HTMLButtonElement).click()
    ;(cards[0].querySelector('.hub-actions .nx-btn--destructive') as HTMLButtonElement).click()
    await flush(5)
    out.lobby.openEmits = events.open
    out.lobby.browseEmits = events.browse
    out.lobby.deleteEmits = events.delete
    // 丰富模式开关：关 → 素卡（chips 消失）
    const richToggle = el.querySelector('.hub-rich-toggle') as HTMLButtonElement
    richToggle.click()
    await flush(5)
    out.lobby.chipsAfterRichOff = el.querySelectorAll('.hub-member-chip').length

    // —— v0.1.7 UI 重设计：卡片三态类名 / 表头筛选 popover 开合 + 过滤 ——
    const nx = {
      cardInteractiveCount: el.querySelectorAll('.hub-card.nx-card.is-interactive').length,
      cardSelectedByFocus: false,
      filterOpenWorks: false,
      filterCloseWorks: false,
      stageFiltered: 0,
      stageReset: 0,
    }
    // 键盘聚焦（focusin）→ is-selected 态挂载（卡片第三态）
    cards[0].dispatchEvent(new Event('focusin', { bubbles: false }))
    await flush(5)
    nx.cardSelectedByFocus = cards[0].classList.contains('is-selected')
    // 表头筛选 popover：点类别触发 → 面板开（Teleport 到 body）→ Esc 关
    const filters = Array.from(el.querySelectorAll('.hub-filters .nx-pop__trigger')) as HTMLElement[]
    ;(filters[0] as HTMLElement).click()
    await flush(5)
    nx.filterOpenWorks = !!document.querySelector('.nx-pop__panel')
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' }))
    await flush(5)
    nx.filterCloseWorks = !document.querySelector('.nx-pop__panel')
    // 阶段筛选：开阶段下拉 → 点「定妆」（flowCasting）→ 只剩 p2（casting）→ 复位全部
    ;(filters[1] as HTMLElement).click()
    await flush(5)
    const stagePanel = document.querySelector('.nx-pop__panel')
    const optOf = (txt: string): HTMLElement | null =>
      (Array.from((stagePanel ?? document).querySelectorAll('.hub-filter-opt')).find((n) =>
        (n.textContent ?? '').trim().includes(txt),
      ) ?? null) as HTMLElement | null
    ;(optOf('定妆') as HTMLElement).click()
    await flush(5)
    nx.stageFiltered = el.querySelectorAll('.hub-card').length
    ;(filters[1] as HTMLElement).click()
    await flush(5)
    ;(optOf('全部') as HTMLElement).click()
    await flush(5)
    nx.stageReset = el.querySelectorAll('.hub-card').length
    out.lobby.nx = nx
    app.unmount()
  }

  // ============ ② HubBrowse：文件树 / 内容四形态 / Tab / 互跳 ============
  {
    const { app, el } = mountPage(HubBrowse)
    await flush(30)
    const rows = () => Array.from(el.querySelectorAll('.hub-tree-row'))
    out.browse = {
      treeRows: rows().map((n) => (n.querySelector('.hub-tree-name')?.textContent ?? '').trim()),
      rowsBeforeCollapse: rows().length,
      rowsAfterCollapse: 0,
      rowsAfterExpandNested: 0,
      textContent: '',
      jsonContent: '',
      imgSrcPrefix: '',
      audioCount: 0,
      videoCount: 0,
      binaryNote: false,
      crumbsText: '',
      tabs: Array.from(el.querySelectorAll('.fh-tab')).map((n) => (n.textContent ?? '').trim()),
      activityRows: 0,
      costTables: 0,
      curlBlocks: 0,
      curlHasPutPath: false,
      openFlowViews: [],
      pendingCastSet: null,
    }
    // 折叠 story/ 目录 → 子行隐藏；再展开 → 恢复；展开 story/sources → novel.txt 出现
    const storyDir = rowByText(el, 'story') as HTMLButtonElement
    storyDir.click()
    await flush(5)
    out.browse.rowsAfterCollapse = rows().length
    storyDir.click()
    await flush(5)
    const sourcesDir = rowByText(el, 'sources') as HTMLButtonElement
    sourcesDir.click()
    await flush(5)
    out.browse.rowsAfterExpandNested = rows().length
    // 文本（story.md → pre）
    ;(rowByText(el, 'story.md') as HTMLButtonElement).click()
    await flush(10)
    out.browse.textContent = (el.querySelector('.hub-view-body pre')?.textContent ?? '').trim()
    out.browse.crumbsText = (el.querySelector('.hub-crumbs')?.textContent ?? '').replace(/\s+/g, ' ').trim()
    // 「在工作台打开」：story.md → 剧情页
    ;(el.querySelector('.hub-open-flow') as HTMLButtonElement).click()
    await flush(5)
    // json 文本（storyboard.json）
    ;(rowByText(el, 'storyboard.json') as HTMLButtonElement).click()
    await flush(10)
    out.browse.jsonContent = (el.querySelector('.hub-view-body pre')?.textContent ?? '').trim()
    // 图片（casting/characters/小明/front.png → data URL 直显）+ casting 互跳
    //（嵌套目录逐级展开：characters → 小明 → front.png）
    ;(rowByText(el, 'characters') as HTMLButtonElement).click()
    await flush(5)
    ;(rowByText(el, '小明') as HTMLButtonElement).click()
    await flush(5)
    ;(rowByText(el, 'front.png') as HTMLButtonElement).click()
    await flush(10)
    const img = el.querySelector('.hub-view-body img') as HTMLImageElement | null
    out.browse.imgSrcPrefix = img ? (img.getAttribute('src') ?? '').slice(0, 15) : ''

    ;(el.querySelector('.hub-open-flow') as HTMLButtonElement).click()
    await flush(5)
    // 音频（audio/bgm-001.mp3 → audio 标签）
    ;(rowByText(el, 'bgm-001.mp3') as HTMLButtonElement).click()
    await flush(10)
    out.browse.audioCount = el.querySelectorAll('.hub-view-body audio').length
    // 视频（dist/final-v2.mp4 → video 标签）
    ;(rowByText(el, 'final-v2.mp4') as HTMLButtonElement).click()
    await flush(10)
    out.browse.videoCount = el.querySelectorAll('.hub-view-body video').length
    // 二进制（assets/poster.icc → 不支持预览提示）
    ;(rowByText(el, 'poster.icc') as HTMLButtonElement).click()
    await flush(10)
    out.browse.binaryNote = (el.querySelector('.hub-view-body')?.textContent ?? '').includes('暂不支持预览')
    // v0.1.5：HubBrowse 左栏树卡非 expandable（无「⤢ 展开」钮——大展开是
    // SideNav 底部树卡形态专属，本页内容区在右侧）
    out.browse.treeExpandAbsent = !el.querySelector('.hub-tree-expand')
    // 活动流 Tab（activity.json 时间线）
    const tabs = Array.from(el.querySelectorAll('.fh-tab')) as HTMLButtonElement[]
    ;(tabs.find((n) => (n.textContent ?? '').includes('活动')) as HTMLButtonElement).click()
    await flush(10)
    out.browse.activityRows = el.querySelectorAll('.hub-act-row').length
    // 成本 Tab（by stage/channel 两表）
    ;(tabs.find((n) => (n.textContent ?? '').includes('成本')) as HTMLButtonElement).click()
    await flush(10)
    out.browse.costTables = el.querySelectorAll('.fh-cost-table').length
    // 接入指南 Tab（curl 三段）
    ;(tabs.find((n) => (n.textContent ?? '').includes('接入指南')) as HTMLButtonElement).click()
    await flush(5)
    out.browse.curlBlocks = el.querySelectorAll('.hub-curl').length
    out.browse.curlHasPutPath = Array.from(el.querySelectorAll('.hub-curl')).some((n) =>
      (n.textContent ?? '').includes('/files/storyboard.json'),
    )
    out.browse.openFlowViews = [...smock().viewSwitches]
    out.browse.pendingCastSet = mockPendingCast.value
      ? { type: mockPendingCast.value.type, name: mockPendingCast.value.name }
      : null
    app.unmount()
  }

  // ============ ②b 初始定位（v0.1.37.1）：pendingHubFile → 挂载即选中加载 ============
  {
    mockPendingHub.value = 'casting/props/长剑/card.md'
    const { app, el } = mountPage(HubBrowse)
    await flush(30)
    const selRow = el.querySelector('.hub-tree-row.is-selected') as HTMLElement | null
    out.locate = {
      contentText: (el.querySelector('.hub-view-body pre')?.textContent ?? '').trim(),
      selectedRowPath: selRow ? (selRow.getAttribute('title') ?? null) : null,
      ancestorExpanded: !!rowByText(el, 'card.md'),
      pendingConsumed: mockPendingHub.value === null,
      crumbsText: (el.querySelector('.hub-crumbs')?.textContent ?? '').replace(/\s+/g, ' ').trim(),
    }
    app.unmount()
    mockPendingHub.value = null
  }

  // ============ ③ CastingPage 集成：pendingCastSelect 选中对象 ============
  {
    // ② 已置位 pendingCastSelect={characters, 小明}——挂载定妆页消费
    const { app, el } = mountPage(CastingPage)
    await flush(30)
    const active = el.querySelector('.cast-obj-card.is-active')
    out.casting = {
      activeCardText: (active?.textContent ?? '').trim(),
      pendingConsumed: mockPendingCast.value === null,
    }
    app.unmount()
  }

  // ============ ④ SideNav：🎬 FilmHub 回大厅（v0.1.5「Hub 浏览」项已删） + ④c 树卡大展开 ============
  {
    let homeEmits = 0
    const hubSelects: string[] = []
    const fileClicks: string[] = []
    // v0.1.5 视图外链：mock window.open 捕获（块尾还原）
    const extOpens: string[] = []
    const winObj = globalThis.window as unknown as {
      open?: (...a: unknown[]) => unknown
    }
    const origOpen = typeof winObj.open === 'function' ? winObj.open : undefined
    winObj.open = (u?: unknown) => {
      extOpens.push(String(u))
      return null
    }
    const NavHost = defineComponent({
      setup() {
        return () =>
          h(SideNav, {
            view: 'hub' as FlowView,
            stage: 'casting' as FilmStage,
            onSelect: (v: FlowView) => {
              hubSelects.push(v)
            },
            onHome: () => {
              homeEmits++
            },
            onFileClick: (path: string) => {
              fileClicks.push(path)
            },
          })
      },
    })
    const { app, el } = mountPage(NavHost, {}, true)
    await flush(5)
    const items = Array.from(el.querySelectorAll('.fh-nav-item')) as HTMLButtonElement[]
    const home = el.querySelector('.fh-nav-home') as HTMLButtonElement
    home.click()
    await flush(5)
    // v0.1.5：「Hub 浏览」导航项已删（与左下树卡重复）——选项卡不再列 hub
    const hubItemAbsent = !items.some((n) => (n.textContent ?? '').includes('浏览'))
    // v0.1.5 视图外链 ⧉：每个视图项行尾（五阶段+工作台+设置+模型；FilmHub 除外）
    const extCount = el.querySelectorAll('.fh-nav-item .fh-nav-ext').length
    const sbItem = items.find((n) => (n.textContent ?? '').includes('分镜')) as HTMLButtonElement
    ;(sbItem.querySelector('.fh-nav-ext') as HTMLElement).click()
    await flush(5)
    const extClickNotSelect = hubSelects.length === 0 // ⧉ 不触发本行切页
    // —— v0.1.36 底部常开树卡：渲染 / 全收 / 点击跳转 ——
    const navTree = () => Array.from(el.querySelectorAll('.fh-nav-tree .hub-tree-row'))
    const navRow = (text: string): HTMLButtonElement | null =>
      (navTree().find((n) =>
        (n.querySelector('.hub-tree-name')?.textContent ?? '').trim() === text,
      ) ?? null) as HTMLButtonElement | null
    const treeCardRows = navTree().map((n) =>
      (n.querySelector('.hub-tree-name')?.textContent ?? '').trim(),
    )
    // 全收：所有目录折叠 → 只剩根级行
    ;(el.querySelector('.hub-tree-collapseall') as HTMLButtonElement).click()
    await flush(5)
    const treeCardAfterCollapseAll = navTree().length
    // v0.1.37.1 注册表式（紧凑态）：点击文件 emit file-click（完整路径）——根级文件
    ;(navRow('story.md') as HTMLButtonElement).click()
    await flush(5)
    // 深层文件：逐级展开 casting/characters/小明 → front.png（emit 完整路径）
    for (const name of ['casting', 'characters', '小明']) {
      ;(navRow(name) as HTMLButtonElement).click()
      await flush(5)
    }
    ;(navRow('front.png') as HTMLButtonElement).click()
    await flush(10)
    // 迷你预览浮层已删（点击后不出现 .hub-mini-pop）
    const popAbsent = !el.querySelector('.hub-mini-pop')
    // 「完整浏览」链接 → select('hub')（跳 Hub 浏览页）
    const hubSelectsBeforeBrowse = hubSelects.length
    ;(el.querySelector('.hub-tree-browse') as HTMLButtonElement).click()
    await flush(5)

    // —— v0.1.5 树卡原位大展开：⤢ → 卡片撑满左栏剩余全高 + 卡内下区内容 ——
    const expandBtn = el.querySelector('.hub-tree-expand') as HTMLButtonElement
    expandBtn.click()
    await flush(5)
    const navTreeWrap = el.querySelector('.fh-nav-tree') as HTMLElement
    const cardEl = navTreeWrap.querySelector('.hub-tree-card') as HTMLElement
    const navRoot = el.querySelector('.fh-nav') as HTMLElement
    const expandClassesOn =
      cardEl.classList.contains('is-expanded') &&
      navTreeWrap.classList.contains('is-tree-expanded') &&
      navRoot.classList.contains('is-tree-expanded')
    // 原位放大：卡片仍在 .fh-nav 内（无弹层/portal）——宽度限左栏、绝不向右溢出
    const expandInsideNav = !!cardEl.closest('.fh-nav')
    const previewPresent = !!el.querySelector('.hub-tree-preview')
    // 展开态点击 story.md → 下区文本 pre（不再 emit file-click / 不跳 Hub 浏览）
    const fileClicksBeforeExpand = fileClicks.length
    ;(navRow('story.md') as HTMLButtonElement).click()
    await flush(10)
    const expandText = (el.querySelector('.hub-tree-preview pre')?.textContent ?? '').trim()
    const expandClicksNotEmitted = fileClicks.length === fileClicksBeforeExpand
    // 图片（casting/characters/小明 已展开）→ img contain 直显
    ;(navRow('front.png') as HTMLButtonElement).click()
    await flush(10)
    const expandImg = !!el.querySelector('.hub-tree-preview img')
    // 音频（audio/ 目录折叠过——先展开再点 bgm-001.mp3 → audio 控件）
    ;(navRow('audio') as HTMLButtonElement).click()
    await flush(5)
    ;(navRow('bgm-001.mp3') as HTMLButtonElement).click()
    await flush(10)
    const expandAudio = el.querySelectorAll('.hub-tree-preview audio').length
    // 视频（dist/final-v2.mp4 → 复杂内容提示——「完整浏览」出口）
    ;(navRow('dist') as HTMLButtonElement).click()
    await flush(5)
    ;(navRow('final-v2.mp4') as HTMLButtonElement).click()
    await flush(10)
    const expandComplex = (el.querySelector('.hub-tree-preview')?.textContent ?? '').includes(
      '完整浏览',
    )
    // 收起 → 恢复紧凑态（is-expanded / 下区内容区消失）
    expandBtn.click()
    await flush(5)
    const expandCollapsedBack =
      !cardEl.classList.contains('is-expanded') && !el.querySelector('.hub-tree-preview')

    out.nav = {
      homeText: (home.textContent ?? '').trim(),
      hubItemAbsent,
      extCount,
      extOpens,
      extClickNotSelect,
      homeEmits,
      hubSelects,
      treeCardRows,
      treeCardAfterCollapseAll,
      fileClicks,
      popAbsent,
      browseEmits: hubSelects.length - hubSelectsBeforeBrowse,
      expand: {
        classesOn: expandClassesOn,
        insideNav: expandInsideNav,
        previewPresent,
        text: expandText,
        clicksNotEmitted: expandClicksNotEmitted,
        img: expandImg,
        audio: expandAudio,
        complex: expandComplex,
        collapsedBack: expandCollapsedBack,
      },
    }
    if (origOpen) winObj.open = origOpen
    else delete winObj.open
    app.unmount()
  }

  return out
}
