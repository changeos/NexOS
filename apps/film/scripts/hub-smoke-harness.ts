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
//   · CastingPage 集成：pendingCastSelect（Hub 浏览 casting 路径 → 定妆页
//     切类 + 选中对象 + 消费清空）；
//   · SideNav 导航层级：顶部 🎬 FilmHub 回大厅项 + Hub 浏览项。
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
import { FLOW_CONTEXT_KEY, type FlowContext } from '../src/flow/flowContext'
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
  deriveStageFromProject,
  hubCastSelect,
  hubFileIcon,
  hubTargetView,
  isHubTextPath,
} from '../src/flow/flowFiles'

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
  }
  casting: {
    activeCardText: string
    pendingConsumed: boolean
  }
  nav: {
    homeText: string
    hubItemPresent: boolean
    homeEmits: number
    hubSelects: string[]
    treeCardRows: string[]
    treeCardAfterCollapseAll: number
    miniText: string
    miniTruncated: boolean
    miniImgPrefix: string
    miniAudioCount: number
    miniClosed: boolean
    browseEmits: number
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
    ;(cards[0].querySelector('.hub-actions .fh-btn-primary') as HTMLButtonElement).click()
    ;(cards[0].querySelectorAll('.hub-actions .fh-btn')[1] as HTMLButtonElement).click()
    ;(cards[0].querySelector('.hub-actions .fh-btn-danger') as HTMLButtonElement).click()
    await flush(5)
    out.lobby.openEmits = events.open
    out.lobby.browseEmits = events.browse
    out.lobby.deleteEmits = events.delete
    // 丰富模式开关：关 → 素卡（chips 消失）
    const richToggle = el.querySelector('.hub-rich-toggle') as HTMLButtonElement
    richToggle.click()
    await flush(5)
    out.lobby.chipsAfterRichOff = el.querySelectorAll('.hub-member-chip').length
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

  // ============ ④ SideNav：🎬 FilmHub 回大厅 + Hub 浏览项 ============
  {
    let homeEmits = 0
    const hubSelects: string[] = []
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
          })
      },
    })
    const { app, el } = mountPage(NavHost, {}, true)
    await flush(5)
    const items = Array.from(el.querySelectorAll('.fh-nav-item')) as HTMLButtonElement[]
    const home = el.querySelector('.fh-nav-home') as HTMLButtonElement
    home.click()
    await flush(5)
    ;(items.find((n) => (n.textContent ?? '').includes('浏览')) as HTMLButtonElement).click()
    await flush(5)
    // —— v0.1.36 底部常开树卡：渲染 / 全收 / 迷你预览三形态 / 完整浏览跳转 ——
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
    // 迷你预览①文本（notes/长文.md → 前 50 行 + 截断提示）
    ;(navRow('notes') as HTMLButtonElement).click()
    await flush(5)
    ;(navRow('长文.md') as HTMLButtonElement).click()
    await flush(10)
    const pop = () => el.querySelector('.hub-mini-pop')
    const miniText = (pop()?.querySelector('.hub-mini-text')?.textContent ?? '').trim()
    const miniTruncated = !!pop()?.querySelector('.hub-mini-note')
    ;(pop()?.querySelector('.hub-mini-close') as HTMLButtonElement | null)?.click()
    await flush(5)
    // 迷你预览②图片（casting/characters/小明/front.png → data URL 直显）
    for (const name of ['casting', 'characters', '小明', 'front.png']) {
      ;(navRow(name) as HTMLButtonElement).click()
      await flush(5)
    }
    const img = pop()?.querySelector('img.hub-mini-img') as HTMLImageElement | null
    const miniImgPrefix = img ? (img.getAttribute('src') ?? '').slice(0, 15) : ''
    // 迷你预览③音频（audio/bgm-001.mp3 → audio 控件）
    ;(navRow('audio') as HTMLButtonElement).click()
    await flush(5)
    ;(navRow('bgm-001.mp3') as HTMLButtonElement).click()
    await flush(10)
    const miniAudioCount = pop()?.querySelectorAll('audio').length ?? 0
    ;(pop()?.querySelector('.hub-mini-close') as HTMLButtonElement | null)?.click()
    await flush(5)
    const miniClosed = !pop()
    // 「完整浏览」链接 → select('hub')（跳 Hub 浏览页）
    const hubSelectsBeforeBrowse = hubSelects.length
    ;(el.querySelector('.hub-tree-browse') as HTMLButtonElement).click()
    await flush(5)
    out.nav = {
      homeText: (home.textContent ?? '').trim(),
      hubItemPresent: items.some((n) => (n.textContent ?? '').includes('Hub')),
      homeEmits,
      hubSelects,
      treeCardRows,
      treeCardAfterCollapseAll,
      miniText,
      miniTruncated,
      miniImgPrefix,
      miniAudioCount,
      miniClosed,
      browseEmits: hubSelects.length - hubSelectsBeforeBrowse,
    }
    app.unmount()
  }

  return out
}
