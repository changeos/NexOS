// =============================================================================
// flow-smoke-harness.ts —— FilmHub 流程化组件冒烟 harness（happy-dom 挂载）。
//
// 由 scripts/flow-smoke.mjs 用 vite（plugin-vue 编译 .vue，vue/vue-i18n 真实
// 打包）构建为临时 ESM，在 happy-dom 全局就绪后 import 并调用 run()：
//   · SideNav 左侧选项卡：七项渲染 / 阶段徽章（README stage）/ 点击流转 / 折叠；
//   · CastingPage：六类 Tab / Tab 切换重载 / 对象卡 / 多视图五槽位（空槽）/
//     对象级认领（PUT ownership.json）；
//   · ComposePage：dist 版本列表 / cache 半成品 commit 调用（带 author）/
//     预览成片切工作台；
//   · AudioPage：BGM 表单校验（trigger 空=错误；填后 POST 带作者）；
//   · SettingsPage：成员渲染 / 添加成员（PUT ownership）/ 分区认领 / 活动流。
// HTTP 走宿主桥 mock（globalThis.__NEXOS_HOST__.api，由 .mjs 注入）——全部
// 调用记录在 globalThis.__FLOW_SMOKE__.calls。断言在 .mjs（node:assert）。
// =============================================================================
import { computed, createApp, defineComponent, h, nextTick, ref } from 'vue'
import { createI18n } from 'vue-i18n'
import SideNav from '../src/flow/SideNav.vue'
import CastingPage from '../src/flow/CastingPage.vue'
import ComposePage from '../src/flow/ComposePage.vue'
import AudioPage from '../src/flow/AudioPage.vue'
import SettingsPage from '../src/flow/SettingsPage.vue'
import zhCN from '../src/i18n/zh-CN.json'
import { filmPutFile } from '../src/api'
import { FLOW_CONTEXT_KEY, type FlowContext } from '../src/flow/flowContext'
import { serializeOwnership, textToB64, type FilmOwnership } from '../src/flow/collab'
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
  modelSel: {
    chat: 'llm:chat',
    image: 'local',
    video: 'ch:c1',
    tts: 'ch:c1',
    music: 'ch:c1',
  },
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
  storySources,
  validateBgmForm,
} from '../src/flow/flowFiles'
export { parseStageFromMarkdown } from '../src/flow/flowTypes'
export {
  claimCastingObject,
  objectOwner,
  parseActivity,
  parseOwnership,
  sectionOwner,
} from '../src/flow/collab'
export { b64ToText } from '../src/api'

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
  }
  casting: {
    tabs: string[]
    charactersCards: string[]
    propsCardsAfterSwitch: string[]
    propsGetCalled: boolean
    slotCount: number
    filledSlotCount: number
    emptySlotLabels: number
    ownerBadgeText: string
    claimPuts: number
    claimObjectOwner: string | undefined
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
    }
    // 点击「分镜」→ active 流转 + view 切换
    items[1].dispatchEvent(new Event('click', { bubbles: true }))
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

  // ============ ② CastingPage：六类 Tab / 对象卡 / 视图槽位 / 对象认领 ============
  {
    const { app, el } = mountPage(CastingPage)
    await flush()
    const tabs = Array.from(el.querySelectorAll('.fh-tab'))
    const cards = Array.from(el.querySelectorAll('.cast-obj-card'))
    out.casting = {
      tabs: tabs.map((n) => (n.textContent ?? '').trim()),
      charactersCards: cards.map((n) => (n.textContent ?? '').trim()),
      propsCardsAfterSwitch: [],
      propsGetCalled: false,
      slotCount: 0,
      filledSlotCount: 0,
      emptySlotLabels: 0,
      ownerBadgeText: '',
      claimPuts: 0,
      claimObjectOwner: undefined,
    }
    // 选中对象（小明）→ 详情五槽位
    cards[0].dispatchEvent(new Event('click', { bubbles: true }))
    await flush(5)
    const slots = Array.from(el.querySelectorAll('.fh-view-slot'))
    out.casting.slotCount = slots.length
    out.casting.filledSlotCount = slots.filter((n) => n.classList.contains('is-filled')).length
    out.casting.emptySlotLabels = slots.filter(
      (n) => (n.textContent ?? '').includes('空槽'),
    ).length
    // 他人认领徽章（fixture：characters/小明 owner=小红）
    out.casting.ownerBadgeText = (el.querySelector('.cast-obj-card .fh-owner')?.textContent ?? '').trim()
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

  return out
}
