// =============================================================================
// hub-smoke.mjs —— FilmHub 显性大厅冒烟（happy-dom；v0.1.1）。
//
// 照 flow-smoke.mjs 先例（主前端无 vitest）：
//   1. vite JS API 构建 scripts/hub-smoke-harness.ts（plugin-vue 编译 .vue，
//      vue/vue-i18n 真实打包，@nexos/app-sdk 别名指主前端 SDK 源）→ 临时 ESM；
//   2. happy-dom 全局 + 宿主桥 mock api（fixtures 见下）+ 观测面
//      globalThis.__FLOW_SMOKE__（calls / viewSwitches / ownershipPuts）；
//   3. import 构建产物 → run() 挂载 HubLobby/HubBrowse/CastingPage/SideNav
//      并交互 → node:assert 断言：
//      ① 大厅：品牌栏 / 项目卡（标题+ratio+五阶段进度点——启发式推导）/
//         搜索过滤 / 丰富模式（成员 chips+最近活动一句+成本小字；p2 部分降级、
//         p3 全降级素卡）/ 开关关闭 / 卡片操作 emit；
//      ② Hub 浏览：文件树渲染与目录折叠（嵌套展开）/ 内容区四形态
//         （文本 pre / json / 图片 data URL / audio / video / 二进制提示）/
//         面包屑 / 四 Tab 切换（文件/活动/成本/接入指南 curl 三段）/
//         「在工作台打开」互跳映射（story.md→剧情；casting/*→定妆+对象选中）；
//      ③ 定妆页集成：pendingCastSelect 消费 + 对象选中；
//      ④ SideNav 导航层级：顶部 🎬 FilmHub 回大厅 + Hub 浏览项；
//      ⑤ 纯函数：buildHubTree / hubFileIcon / hubTargetView / hubCastSelect /
//         deriveStageFromProject / isHubTextPath。
//
// 运行：cd apps/film && npm run smoke:hub
// =============================================================================
import { build } from 'vite'
import vue from '@vitejs/plugin-vue'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, URL } from 'node:url'
import assert from 'node:assert/strict'
import { Window } from 'happy-dom'

process.env.NODE_ENV = 'production'

const appRoot = fileURLToPath(new URL('..', import.meta.url))
const srcDir = join(appRoot, 'src')
const sdkPath = join(appRoot, '../../crates/os-api/web/src/sdk/index.ts')

// —— 1. vite 构建 harness（.vue 经 plugin-vue 编译；产物临时目录）——
const tmpDir = mkdtempSync(join(tmpdir(), 'film-hub-smoke-'))
const bundlePath = join(tmpDir, 'hub-smoke.mjs')
await build({
  configFile: false,
  root: appRoot,
  plugins: [vue()],
  resolve: {
    alias: {
      '@': srcDir,
      '@nexos/app-sdk': sdkPath,
    },
  },
  build: {
    lib: {
      entry: join(appRoot, 'scripts/hub-smoke-harness.ts'),
      formats: ['es'],
      fileName: () => 'hub-smoke.mjs',
    },
    outDir: tmpDir,
    emptyOutDir: true,
    cssCodeSplit: false,
    minify: false,
  },
  logLevel: 'warn',
})

// —— 2. happy-dom 全局 + 宿主桥 mock ——
const win = new Window()
globalThis.window = win
globalThis.document = win.document
globalThis.localStorage = win.localStorage
globalThis.requestAnimationFrame = win.requestAnimationFrame.bind(win)
globalThis.cancelAnimationFrame = win.cancelAnimationFrame.bind(win)
Object.defineProperty(globalThis, 'navigator', { value: win.navigator, configurable: true })
for (const k of [
  'Event', 'CustomEvent', 'InputEvent', 'KeyboardEvent', 'MouseEvent',
  'Node', 'Element', 'HTMLElement', 'HTMLInputElement', 'HTMLButtonElement',
  'HTMLSelectElement', 'HTMLTextAreaElement', 'HTMLImageElement', 'SVGElement',
  'Document', 'DocumentFragment', 'ShadowRoot', 'Text', 'Comment',
]) {
  if (win[k]) globalThis[k] = win[k]
}

const b64 = (s) => Buffer.from(s, 'utf8').toString('base64')
const pngB64 = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'utf8',
).toString('base64')
const mp3B64 = b64('//uQxAAAAAAAAAAAAAAAAAAAAAAAWGluZwAAAA8AAAACAAACcQCAgICAgICAgICAg')
const mp4B64 = b64('AAAAIGZ0eXBpc29tAAACAGlzb21pc28yYXZjMW1wNDE=')
const iccB64 = b64('\x00\x00\x02\x30fake-icc-profile')

/** p1 项目文件树 fixture（hub 树渲染 + 四形态内容 + 嵌套目录折叠）。 */
const TREE = [
  { path: 'README.md', bytes: 120 },
  { path: 'story.md', bytes: 900 },
  { path: 'storyboard.json', bytes: 400 },
  { path: 'story/sources/novel.txt', bytes: 5000 },
  { path: 'casting/characters/小明/front.png', bytes: 40000 },
  { path: 'casting/props/长剑/card.md', bytes: 200 },
  { path: 'audio/bgm-001.mp3', bytes: 150000 },
  { path: 'assets/poster.icc', bytes: 3200 },
  { path: 'dist/final-v1.mp4', bytes: 1000000, mtime: '2026-09-05T10:00:00Z' },
  { path: 'dist/final-v2.mp4', bytes: 1200000, mtime: '2026-09-06T09:00:00Z' },
  { path: 'cache/try-bgm.mp3', bytes: 200000 },
  { path: 'notes/长文.md', bytes: 6000 },
  { path: 'ownership.json', bytes: 100 },
  { path: 'activity.json', bytes: 200 },
  { path: 'extraction.json', bytes: 300 },
]

/** 单文件内容 fixture（files/<path> 的 b64 信封；mime 按扩展）。 */
const FILES = {
  'README.md': b64('---\ntitle: 冒烟项目\nstage: casting\n---\n\n# 冒烟项目\n'),
  'story.md': b64('---\nsource: novel.txt\n---\n\n月光落在月球快递站的穹顶上……'),
  'storyboard.json': b64(JSON.stringify({ shots: [{ shot: 1, desc: '穹顶月光' }] })),
  'story/sources/novel.txt': b64('第一章 月球快递站……'),
  'casting/characters/小明/front.png': pngB64,
  'casting/props/长剑/card.md': b64('# 长剑\n星海长剑'),
  'audio/bgm-001.mp3': mp3B64,
  'assets/poster.icc': iccB64,
  'dist/final-v1.mp4': mp4B64,
  'dist/final-v2.mp4': mp4B64,
  'cache/try-bgm.mp3': mp3B64,
  'notes/长文.md': b64(Array.from({ length: 60 }, (_, i) => `第${i + 1}行：占位正文……`).join('\n')),
  'ownership.json': b64(JSON.stringify({ members: ['小明', '小红'] })),
  'activity.json': b64(
    JSON.stringify([
      { ts: '2026-09-06T10:00:00Z', author: '小红', action: 'story.generate', target: 'story.md' },
      { ts: 1788000000, author: 'anonymous', action: 'compose', target: 'dist/final-v2.mp4' },
    ]),
  ),
  'extraction.json': b64(
    JSON.stringify({ characters: [{ name: '小明', desc: '柯基快递员', frequency: 12 }] }),
  ),
}

/** p2（部分降级）/ p3（全降级）的丰富数据 fixture。 */
const P2 = {
  'ownership.json': b64(JSON.stringify({ members: ['阿礁'] })),
}
const P3 = {}

/** casting fixture（③ 集成用）。 */
const CASTING = {
  characters: [
    { name: '小明', desc: '柯基快递员', voice: 'alloy', views: [{ view: 'front', path: 'casting/characters/小明/front.png' }] },
  ],
  props: [], pets: [], formations: [], actions: [], scenes: [],
}

/** mime 按扩展推导。 */
function mimeOf(key) {
  if (key.endsWith('.png')) return 'image/png'
  if (key.endsWith('.mp3')) return 'audio/mpeg'
  if (key.endsWith('.mp4')) return 'video/mp4'
  if (key.endsWith('.icc')) return 'application/octet-stream'
  return 'text/plain'
}

/** 观测面（harness 读写）。 */
globalThis.__FLOW_SMOKE__ = { calls: [], viewSwitches: [], ownershipPuts: [] }

function safeDecode(p) {
  try {
    return decodeURIComponent(p)
  } catch {
    return p
  }
}

/** mock 宿主 api：按项目 id 路由 fixtures；调用记录进 __FLOW_SMOKE__.calls。 */
async function handle(method, path, body) {
  globalThis.__FLOW_SMOKE__.calls.push({ method, path, body })
  const p = safeDecode(path)
  for (const pid of ['p1', 'p2', 'p3']) {
    const base = `/api/v1/film/projects/${pid}`
    if (!p.startsWith(`${base}/`) && p !== `${base}`) continue
    const rest = p.slice(base.length).replace(/^\//, '')
    // —— p1：文件树 / 单文件 / casting / cost 全命中 ——
    if (pid === 'p1') {
      if (method === 'GET' && rest === 'files') return TREE
      if (method === 'GET' && rest.startsWith('files/')) {
        const key = rest.slice('files/'.length)
        if (key in FILES) return { content_b64: FILES[key], mime: mimeOf(key) }
        throw new Error(`404 Not Found — ${key}`)
      }
      if (method === 'GET' && rest.startsWith('casting/')) return CASTING[rest.slice('casting/'.length)] ?? []
      if (method === 'GET' && rest === 'cost') return { total: 3.14, currency: '¥', calls: 12, events: 12, groups: [] }
      if (method === 'GET' && rest.startsWith('cost?')) {
        return { total: 3.14, currency: '¥', groups: [{ key: 'story', cost: 1.5, events: 5 }, { key: 'compose', cost: 1.64, events: 7 }] }
      }
    }
    // —— p2：仅 ownership（activity/cost 404 → 部分降级）——
    if (pid === 'p2') {
      if (method === 'GET' && rest === 'files/ownership.json') {
        return { content_b64: P2['ownership.json'], mime: 'text/plain' }
      }
      throw new Error(`404 Not Found — ${p}`)
    }
    // —— p3：全 404（素卡降级）——
    if (pid === 'p3') throw new Error(`404 Not Found — ${p}`)
  }
  return {}
}

globalThis.__NEXOS_HOST__ = {
  api: {
    get: (p) => handle('GET', p),
    post: (p, b) => handle('POST', p, b),
    del: (p) => handle('DELETE', p),
    request: (p, o) => handle(o?.method ?? 'GET', p, o?.body),
  },
}

// —— 3. 加载 harness 并断言 ——
const harness = await import(`file://${bundlePath}`)
const r = await harness.run()
let passed = 0
function ok(cond, label) {
  assert.ok(cond, label)
  passed++
  console.log(`  ✓ ${label}`)
}

console.log('① HubLobby 大厅（品牌 / 项目卡 / 阶段进度 / 搜索 / 丰富模式）')
ok(r.lobby.brandText === 'FilmHub', '品牌栏：🎬 FilmHub（副标题=影片项目中心）')
ok(r.lobby.cardCount === 3, '三张项目卡渲染')
ok(r.lobby.cardTitles[0] === '星际快递员' && r.lobby.cardTitles[1] === '深海灯塔', '卡片标题（星际快递员/深海灯塔/无题草稿）')
ok(r.lobby.ratioPills.every((x) => x === '16:9'), 'ratio pill（16:9）')
ok(r.lobby.dotCountCard0 === 5, '五阶段进度点（story→storyboard→casting→audio→compose）')
assert.deepEqual(r.lobby.dotStatesCard0, ['done', 'done', 'done', 'done', 'current'])
ok(true, '阶段推导（final.mp4 → compose）：前四 ✓、当前=合成')
assert.deepEqual(r.lobby.dotStatesCard2, ['current', 'todo', 'todo', 'todo', 'todo'])
ok(true, '阶段推导（空项目 → story）：当前=剧情')
ok(r.lobby.searchFiltered === 1, '搜索「柯基」→ 命中 1 张（idea 过滤）')
ok(r.lobby.memberChipsCard0.length === 2 && r.lobby.memberChipsCard0.some((x) => x.includes('小红')), '丰富模式：成员 chips（ownership 小明/小红）')
ok(r.lobby.latestCard0.includes('小红') && r.lobby.latestCard0.includes('story.generate'), '丰富模式：最近活动一句（谁+动作+对象）')
ok(r.lobby.costCard0.includes('12'), '丰富模式：成本小字（cost calls=12）')
ok(r.lobby.memberChipsCard1.length === 1 && r.lobby.memberChipsCard1[0].includes('阿礁'), '部分降级：p2 ownership 命中（activity/cost 404）')
ok(r.lobby.latestCard1 === false, '部分降级：p2 无最近活动行（404 静默）')
ok(r.lobby.memberChipsCard2 === 0 && r.lobby.latestCard2 === false, '全降级：p3 素卡（三项全 404）')
ok(r.lobby.chipsAfterRichOff === 0, '丰富模式开关关闭 → 素卡')
ok(r.lobby.openEmits === 1 && r.lobby.browseEmits === 1 && r.lobby.deleteEmits === 1, '卡片操作 emit：打开 / Hub 浏览 / 删除')

console.log('② HubBrowse 项目 Hub 浏览（文件树 / 内容四形态 / Tab / 互跳）')
ok(r.browse.treeRows.includes('README.md') && r.browse.treeRows.includes('storyboard.json'), '文件树根级文件渲染（README/storyboard.json）')
ok(r.browse.treeRows.includes('audio') && r.browse.treeRows.includes('casting') && r.browse.treeRows.includes('dist'), '文件树根级目录渲染（audio/casting/dist…）')
ok(!r.browse.treeRows.includes('novel.txt'), '嵌套目录缺省折叠（story/sources/novel.txt 不可见）')
ok(r.browse.rowsAfterCollapse === r.browse.rowsBeforeCollapse - 1, '目录折叠：点 story → 子行隐藏')
ok(r.browse.rowsAfterExpandNested === r.browse.rowsAfterCollapse + 2, '嵌套展开：story + sources → novel.txt 出现')
ok(r.browse.textContent.includes('月光落在月球快递站'), '内容形态①文本：story.md 等宽 pre')
ok(r.browse.jsonContent.includes('shots'), '内容形态①json：storyboard.json pre')
ok(r.browse.imgSrcPrefix === 'data:image/png;', '内容形态②图片：front.png data URL 直显')
ok(r.browse.audioCount === 1, '内容形态③音频：bgm-001.mp3 audio 标签')
ok(r.browse.videoCount === 1, '内容形态④视频：final-v2.mp4 video 标签')
ok(r.browse.binaryNote, '二进制：poster.icc 不支持预览提示')
ok(r.browse.crumbsText.includes('story.md'), '面包屑：当前路径 story.md')
ok(r.browse.tabs.length === 4, '四 Tab（文件/活动/成本/接入指南）')
ok(r.browse.activityRows === 2, '活动流 Tab：activity.json 时间线 2 条')
ok(r.browse.costTables === 2, '成本 Tab：by stage/channel 两表')
ok(r.browse.curlBlocks === 3 && r.browse.curlHasPutPath, '接入指南 Tab：curl 三段（含 PUT storyboard.json）')
ok(r.browse.openFlowViews[0] === 'story', '互跳：story.md →「在工作台打开」= 剧情页')
ok(r.browse.openFlowViews.includes('casting'), '互跳：casting/* → 定妆页')
ok(r.browse.pendingCastSet?.type === 'characters' && r.browse.pendingCastSet?.name === '小明', '互跳：casting 路径带对象选中（characters/小明）')

console.log('③ CastingPage 集成（pendingCastSelect 选中对象）')
ok(r.casting.activeCardText.includes('小明'), '定妆页挂载即选中「小明」（对象卡 is-active）')
ok(r.casting.pendingConsumed, 'pendingCastSelect 消费后清空')

console.log('④ SideNav 导航层级（FilmHub 大厅 → 项目 → Hub 浏览）')
ok(r.nav.homeText.includes('FilmHub'), '顶部 🎬 FilmHub 品牌项（回大厅）')
ok(r.nav.hubItemPresent, '「Hub 浏览」选项卡（第八视图）')
ok(r.nav.homeEmits === 1, '点品牌项 → home 事件（回大厅）')
ok(r.nav.hubSelects.includes('hub'), '点「Hub 浏览」→ select=hub（项目内切视图）')

console.log('④b SideNav 底部常开树卡（渲染 / 全收 / 迷你预览三形态 / 跳转）')
ok(r.nav.treeCardRows.includes('story') && r.nav.treeCardRows.includes('README.md'), '常开树卡渲染（选项卡列表之下的 🗂 Hub 树）')
ok(
  r.nav.treeCardAfterCollapseAll < r.nav.treeCardRows.length,
  `「全收」钮：目录全折叠（${r.nav.treeCardRows.length} → ${r.nav.treeCardAfterCollapseAll} 行）`,
)
ok(r.nav.miniText.includes('第1行') && r.nav.miniText.includes('第50行') && !r.nav.miniText.includes('第60行'), '迷你预览①文本：前 50 行等宽（第60行截断）')
ok(r.nav.miniTruncated, '迷你预览①截断提示（仅前 50 行）')
ok(r.nav.miniImgPrefix === 'data:image/png;', '迷你预览②图片：front.png data URL 直显')
ok(r.nav.miniAudioCount === 1, '迷你预览③音频：bgm-001.mp3 audio 控件')
ok(r.nav.miniClosed, '关闭钮：浮层消失')
ok(r.nav.browseEmits === 1 && r.nav.hubSelects.filter((v) => v === 'hub').length >= 2, '「完整浏览」链接 → select=hub（跳 Hub 浏览页）')

// —— 纯函数补充断言（flowFiles 同一代码路径）——
console.log('⑤ Hub 纯函数（树构建 / 图标 / 互跳映射 / 阶段推导）')
{
  const tree = harness.buildHubTree(TREE)
  ok(tree[0].isDir && tree[0].name === 'assets', 'buildHubTree：目录在前且按名排序（assets 首位）')
  const dirCount = tree.filter((n) => n.isDir).length
  ok(
    tree.slice(0, dirCount).every((n) => n.isDir) && tree.slice(dirCount).every((n) => !n.isDir),
    'buildHubTree：目录块在前、文件块在后',
  )
  const story = tree.find((n) => n.name === 'story')
  ok(!!story && story.children[0].name === 'sources' && story.children[0].children[0].name === 'novel.txt', 'buildHubTree：嵌套目录递归（story/sources/novel.txt）')
  ok(
    harness.hubFileIcon('a.md') === '📝' && harness.hubFileIcon('a.json') === '🧾' &&
      harness.hubFileIcon('a.png') === '🖼' && harness.hubFileIcon('a.mp3') === '🎵' &&
      harness.hubFileIcon('a.mp4') === '▶' && harness.hubFileIcon('a.icc') === '📄',
    'hubFileIcon：类型映射（md📝/json🧾/图🖼/音🎵/视频▶/余📄）',
  )
  ok(
    harness.hubTargetView('story.md') === 'story' && harness.hubTargetView('sources/n.txt') === 'story' &&
      harness.hubTargetView('storyboard.json') === 'storyboard' &&
      harness.hubTargetView('casting/props/长剑/card.md') === 'casting' &&
      harness.hubTargetView('extraction.json') === 'casting' &&
      harness.hubTargetView('audio/bgm-001.mp3') === 'audio' &&
      harness.hubTargetView('dist/final-v1.mp4') === 'compose' && harness.hubTargetView('cache/x.mp4') === 'compose' &&
      harness.hubTargetView('shot-1.mp4') === 'workbench',
    'hubTargetView：路径→流程视图映射（八路径口径）',
  )
  const cs = harness.hubCastSelect('casting/props/长剑/front.png')
  ok(cs?.type === 'props' && cs?.name === '长剑', 'hubCastSelect：casting 路径 → 对象定位')
  ok(harness.hubCastSelect('story.md') === null, 'hubCastSelect：非 casting 路径 → null')
  ok(
    harness.deriveStageFromProject({ script: [], artifacts: [{ name: 'final.mp4', bytes: 1 }] }) === 'compose' &&
      harness.deriveStageFromProject({ script: [], artifacts: [{ name: 'line-1.mp3', bytes: 1 }] }) === 'audio' &&
      harness.deriveStageFromProject({ script: [], artifacts: [{ name: 'shot-1.png', bytes: 1 }] }) === 'casting' &&
      harness.deriveStageFromProject({ script: [{ shot: 1 }], artifacts: [] }) === 'storyboard' &&
      harness.deriveStageFromProject({ script: [], artifacts: [] }) === 'story',
    'deriveStageFromProject：产物启发式（compose/audio/casting/storyboard/story）',
  )
  ok(harness.isHubTextPath('a.md') && harness.isHubTextPath('b.json') && !harness.isHubTextPath('c.png'), 'isHubTextPath：文本扩展判定')
}

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（FilmHub 显性大厅 happy-dom 冒烟）`)
