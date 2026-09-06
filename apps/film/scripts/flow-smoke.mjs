// =============================================================================
// flow-smoke.mjs —— FilmHub 流程化组件冒烟（happy-dom；v0.1.35）。
//
// 照 preview-smoke.mjs 先例（主前端无 vitest）：
//   1. vite JS API 构建 scripts/flow-smoke-harness.ts（plugin-vue 编译 .vue，
//      vue/vue-i18n 真实打包，@nexos/app-sdk 别名指主前端 SDK 源）→ 临时 ESM；
//   2. happy-dom 全局（window/document/Event/localStorage/navigator）+ 宿主桥
//      mock api（globalThis.__NEXOS_HOST__.api——fixtures 见下）+ 观测面
//      globalThis.__FLOW_SMOKE__（calls / viewSwitches / ownershipPuts）；
//   3. import 构建产物 → run() 挂载 SideNav/CastingPage/ComposePage/AudioPage/
//      SettingsPage 并交互 → node:assert 断言：
//      ① 左侧选项卡：七项渲染/阶段徽章（README stage）/点击流转/折叠；
//      ② 定妆页：六类 Tab/Tab 切换重载/对象卡/多视图五槽位（空槽虚线标记）/
//         对象级认领（PUT ownership.json）；
//      ③ 合成页：dist 版本列表（新版本在前）/cache 半成品 commit 调用（带
//         author）/「预览成片」切工作台；
//      ④ 音频页：BGM 表单校验（trigger 空不 POST；填后 POST 带 info+author）；
//      ⑤ 设置页：成员渲染/添加成员（PUT ownership）/分区认领/活动流。
//
// 运行：cd apps/film && npm run smoke:flow
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
const tmpDir = mkdtempSync(join(tmpdir(), 'film-flow-smoke-'))
const bundlePath = join(tmpDir, 'flow-smoke.mjs')
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
      entry: join(appRoot, 'scripts/flow-smoke-harness.ts'),
      formats: ['es'],
      fileName: () => 'flow-smoke.mjs',
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
// Node 22 的 navigator 是仅 getter 全局——用 defineProperty 覆盖
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

/** files 树 fixture（dist 成品 / cache 半成品 / sources 原文 / casting 视图）。 */
const TREE = [
  { path: 'README.md', bytes: 120 },
  { path: 'story.md', bytes: 900 },
  { path: 'sources/novel.txt', bytes: 5000 },
  { path: 'extraction.json', bytes: 300 },
  { path: 'casting/characters/小明/front.png', bytes: 40000 },
  { path: 'dist/final-v1.mp4', bytes: 1000000, mtime: '2026-09-05T10:00:00Z' },
  { path: 'dist/final-v2.mp4', bytes: 1200000, mtime: '2026-09-06T09:00:00Z' },
  { path: 'cache/try-bgm.mp3', bytes: 200000 },
  { path: 'cache/try-shot-1.mp4', bytes: 300000 },
  { path: 'ownership.json', bytes: 100 },
  { path: 'activity.json', bytes: 200 },
]

/** 单文件内容 fixture（files/<path> 的 b64 信封）。 */
const FILES = {
  'README.md': b64('---\ntitle: 冒烟项目\nstage: casting\n---\n\n# 冒烟项目\n'),
  'story.md': b64('---\nsource: novel.txt\n---\n\n月光落在月球快递站的穹顶上……'),
  'extraction.json': b64(
    JSON.stringify({
      characters: [{ name: '小明', desc: '柯基快递员', frequency: 12 }],
      props: [{ name: '长剑', desc: '星海长剑', frequency: 3 }],
    }),
  ),
  'casting/characters/小明/front.png': pngB64,
  'ownership.json': b64(
    JSON.stringify({
      members: ['小明', '小红'],
      sections: { story: { owner: '小红', claimed_at: '2026-09-05T01:00:00Z' } },
      casting_objects: { 'characters/小明': { owner: '小红', claimed_at: '2026-09-05T02:00:00Z' } },
    }),
  ),
  'activity.json': b64(
    JSON.stringify([
      { ts: '2026-09-06T10:00:00Z', author: '小红', action: 'story.generate', target: 'story.md' },
      { ts: 1788000000, author: 'anonymous', action: 'compose', target: 'dist/final-v2.mp4' },
    ]),
  ),
}

/** casting 六类 fixture。 */
const CASTING = {
  characters: [{ name: '小明', desc: '柯基快递员', voice: 'alloy', views: [{ view: 'front', path: 'casting/characters/小明/front.png' }] }],
  props: [{ name: '长剑', desc: '星海长剑', views: [] }],
  pets: [], formations: [], actions: [], scenes: [],
}

/** BGM 库 fixture。 */
const BGM = [
  { track: 'bgm-001', trigger: 'global', mood: '温馨', file: 'bgm-001.mp3', duration_secs: 92 },
  { track: 'bgm-002', trigger: '追逐', mood: '紧张' },
]

/** 观测面（harness 读写）。 */
globalThis.__FLOW_SMOKE__ = { calls: [], viewSwitches: [], ownershipPuts: [] }

function safeDecode(p) {
  try {
    return decodeURIComponent(p)
  } catch {
    return p
  }
}

/** mock 宿主 api：路由 fixtures；全部调用记录进 __FLOW_SMOKE__.calls。 */
async function handle(method, path, body) {
  globalThis.__FLOW_SMOKE__.calls.push({ method, path, body })
  const p = safeDecode(path)
  const base = '/api/v1/film/projects/p1'
  if (method === 'GET' && p === `${base}/files`) return TREE
  if (method === 'GET' && p.startsWith(`${base}/files/`)) {
    const key = p.slice(`${base}/files/`.length)
    if (key in FILES) return { content_b64: FILES[key], mime: key.endsWith('.png') ? 'image/png' : 'text/plain' }
    const err = new Error(`404 Not Found — ${key}`)
    throw err
  }
  if (method === 'GET' && p.startsWith(`${base}/casting/`)) {
    const ty = p.slice(`${base}/casting/`.length)
    return CASTING[ty] ?? []
  }
  if (method === 'GET' && p === `${base}/audio/bgm`) return BGM
  if (method === 'GET' && p === `${base}/cost`) {
    return { total: 3.14159, currency: '¥', calls: 12, events: 12, groups: [] }
  }
  if (method === 'GET' && p.startsWith(`${base}/cost?`)) {
    return { total: 3.14159, currency: '¥', groups: [{ key: 'story', cost: 1.5, events: 5 }] }
  }
  if (method === 'POST' && p === `${base}/audio/bgm`) return { track: 'bgm-003', trigger: '开场' }
  if (method === 'POST' && p.includes('/cache/') && p.endsWith('/commit')) return { committed: 'ok' }
  if (method === 'PUT' && p.endsWith('/files/ownership.json')) return { written: true, bytes: 100 }
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

console.log('① SideNav 左侧选项卡（阶段流转）')
ok(r.nav.items.length === 7, `七选项卡渲染（五阶段 + 工作台 + 设置）`)
ok(r.nav.items[0].includes('剧情') && r.nav.items[1].includes('分镜') && r.nav.items[2].includes('定妆'), '阶段标签：剧情/分镜/定妆')
ok(r.nav.items[3].includes('音频') && r.nav.items[4].includes('合成'), '阶段标签：音频/合成')
ok(r.nav.items[5].includes('工作台') && r.nav.items[6].includes('设置'), '工作台 + 设置/成员选项卡')
assert.deepEqual(r.nav.badges, ['✓', '✓', '③', '④', '⑤'])
ok(true, '阶段徽章（README stage=casting）：前两阶段 ✓、当前 ③')
ok(r.nav.badgeClasses[0].includes('is-done') && r.nav.badgeClasses[2].includes('is-current'), '徽章态 done/current')
ok(r.nav.activeIndexAfterClick === 1, '点击「分镜」→ active 流转到第 2 项')
ok(r.nav.viewSwitches.includes('storyboard'), '切换事件 emit（select=storyboard）')
ok(r.nav.collapsedAfterToggle === true, '折叠切换（is-collapsed）')

console.log('② CastingPage 定妆页（六类 Tab / 视图槽位 / 对象认领）')
ok(r.casting.tabs.length === 6, '六类 Tab 渲染')
ok(
  ['人物', '武器', '宠物', '排列', '动作', '场景'].every((x) => r.casting.tabs.some((t) => t.includes(x))),
  'Tab 标签：人物/武器/宠物/排列/动作/场景',
)
ok(r.casting.charactersCards.length === 1 && r.casting.charactersCards[0].includes('小明'), '对象卡渲染（characters: 小明）')
ok(r.casting.propsGetCalled, 'Tab 切换 → GET /casting/props 重载该类')
ok(r.casting.propsCardsAfterSwitch.length === 1 && r.casting.propsCardsAfterSwitch[0].includes('长剑'), 'props 对象卡（长剑）')
ok(r.casting.slotCount === 5, '多视图五槽位（front/side/back/action/custom）')
ok(r.casting.filledSlotCount === 1, 'front 视图已填（1/5 实槽）')
ok(r.casting.emptySlotLabels === 4, '空槽 4 个（虚线 + 空槽标记）')
ok(r.casting.ownerBadgeText.includes('小红'), '对象级认领徽章（characters/小明 → 负责人：小红）')
ok(r.casting.claimPuts >= 1, '未认领对象「认领」→ PUT ownership.json')
ok(r.casting.claimObjectOwner === '小明', '认领后 casting_objects[props/长剑].owner = 小明')

console.log('③ ComposePage 合成页（dist 版本 / cache commit）')
ok(r.compose.distRows.length === 2, 'dist 成品版本列表（2 个）')
ok(r.compose.distRows[0].includes('v2') && r.compose.distRows[1].includes('v1'), '版本倒序（v2 在前）')
ok(r.compose.cacheCards.length === 2, 'cache 半成品列表（2 个，与成品分离）')
ok(r.compose.cacheCards.some((c) => c.includes('try-bgm.mp3')), '半成品文件名渲染')
ok(
  !!r.compose.commitCall && safeDecode(r.compose.commitCall.path).endsWith('/cache/try-bgm.mp3/commit'),
  '「确认采用」→ POST /cache/try-bgm.mp3/commit',
)
ok(
  !!r.compose.commitCall && r.compose.commitCall.body?.author === '小明',
  'commit body 带 author（操作人）',
)
ok(r.compose.previewViewSwitch === 'workbench', '「预览成片」→ 切工作台（监视器）')

console.log('④ AudioPage 音频页（BGM 表单校验）')
ok(r.audio.emptyFormError === '触发（trigger）不可为空', '空 trigger 提交 → 校验错误展示')
ok(r.audio.emptyFormNoPost, '校验失败不发 POST')
ok(
  !!r.audio.postCall && r.audio.postCall.body?.info?.trigger === '开场',
  '填 trigger 提交 → POST /audio/bgm {info.trigger}',
)
ok(
  !!r.audio.postCall && r.audio.postCall.body?.author === '小明',
  'BGM 创建 body 带 author',
)
ok(!!r.audio.postCall && !('track_b64' in r.audio.postCall.body), '无文件时不带 track_b64')
ok(
  !!r.audio.genCall && /\/audio\/bgm\/bgm-003\/generate$/.test(r.audio.genCall.path),
  '建条目后链式 POST /audio/bgm/bgm-003/generate',
)
ok(
  !!r.audio.genCall && r.audio.genCall.body?.model_ref?.capability === 'music',
  'BGM 生成 body 带 model_ref（music 能力面）',
)

console.log('⑤ SettingsPage 设置/成员（认领 + 活动流）')
ok(r.settings.memberBadges.length === 2, '成员渲染（小明/小红）')
ok(r.settings.activityRows === 2, '活动流渲染（2 条流水）')
ok(
  Array.isArray(r.settings.ownershipPutMembers) && r.settings.ownershipPutMembers.includes('张三'),
  '添加成员「张三」→ PUT ownership.json（members 含新成员）',
)
ok(r.settings.sectionOwnerPut === '小明', '分区认领下拉 → sections.story.owner = 小明')

// —— 纯函数补充断言（flowFiles / flowTypes / collab 同一代码路径）——
console.log('⑥ 流程纯函数（树派生 / 校验 / 阶段解析 / ownership）')
{
  const tree = TREE
  const dvs = harness.distVersions(tree)
  assert.deepEqual(dvs.map((e) => e.path), ['dist/final-v2.mp4', 'dist/final-v1.mp4'])
  ok(true, 'distVersions：final-v*.mp4 过滤 + 版本倒序')
  ok(harness.distVersionOf('dist/final-v7.mp4') === 7 && harness.distVersionOf('dist/x.mp4') === -1, 'distVersionOf 版本号提取')
  assert.deepEqual(harness.cacheEntries(tree).map((e) => e.path), ['cache/try-bgm.mp3', 'cache/try-shot-1.mp4'])
  ok(true, 'cacheEntries：cache/ 半成品过滤（与成品分离）')
  assert.deepEqual(harness.storySources(tree).map((e) => e.path), ['sources/novel.txt'])
  ok(true, 'storySources：sources/ 原文派生')
  ok(harness.validateBgmForm('  ', true, false) === 'trigger', 'validateBgmForm：trigger 空 → trigger')
  ok(harness.validateBgmForm('global', true, false) === 'file', 'validateBgmForm：导入口径缺文件 → file')
  ok(harness.validateBgmForm('global', true, true) === '', 'validateBgmForm：齐备 → 通过')
  ok(harness.parseStageFromMarkdown('---\nstage: audio\n---\n') === 'audio', 'parseStageFromMarkdown：frontmatter stage')
  ok(harness.parseStageFromMarkdown('# 无 frontmatter') === '', 'parseStageFromMarkdown：缺省 → 未知')
  const own = harness.parseOwnership({ content_b64: FILES['ownership.json'] })
  ok(harness.sectionOwner(own, 'story') === '小红', 'sectionOwner：宽松形态（{owner}）')
  ok(harness.objectOwner(own, 'characters', '小明') === '小红', 'objectOwner：对象级认领键 <type>/<name>')
  const claimed = harness.claimCastingObject(own, 'scenes', '星海', '李四')
  ok(harness.objectOwner(claimed, 'scenes', '星海') === '李四', 'claimCastingObject：纯函数认领/释放')
  const act = harness.parseActivity({ content_b64: FILES['activity.json'] })
  ok(act.length === 2 && act[0].author === '小红', 'parseActivity：环形宽容 + 新→旧排序')
}

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（FilmHub 流程化组件 happy-dom 冒烟）`)
