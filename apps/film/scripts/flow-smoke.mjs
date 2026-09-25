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
//      ⑤ 设置页：成员渲染/添加成员（PUT ownership）/分区认领/活动流；
//      ⑥ v0.1.6 管线任务进度可见性：trackFilmTask(id, stage) 登记（清理/分章/
//         向量化/写剧情/生成分镜/定妆提取）+ busy 态 ↻ spinner「进行中…」+
//         taskLabel 新 stage 白名单（story 系/casting）+ emb 不可用 tooltip/提示行。
//
// 运行：cd apps/film && npm run smoke:flow
// =============================================================================
import { build } from 'vite'
import vue from '@vitejs/plugin-vue'
import { mkdtempSync, readFileSync, rmSync } from 'node:fs'
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

/** files 树 fixture（dist 成品 / cache 半成品 / sources 原文 / casting 视图）。
 *  v0.1.39.1 起按后端真形态返回 **{root, files} 信封**（此前裸数组掩盖了
 *  「e.filter is not a function」崩点——归一函数 hubFileEntries 消费面）。 */
const TREE = [
  { path: 'README.md', bytes: 120 },
  { path: 'story.md', bytes: 900 },
  { path: 'sources/novel.txt', bytes: 5000 },
  // v0.1.39.1 真机补充：story/import 实际落点 story/source-<slug>.txt
  //（源列表派生须认——106 复现时该形态漏认致列表空）
  { path: 'story/source-诛仙.txt', bytes: 260000 },
  // v0.1.37 story 文档管线产物（cleaned / chapters / profile / hub 口径 story.md）
  { path: 'story/cleaned-novel.txt', bytes: 4200 },
  { path: 'story/chapters/index.json', bytes: 400 },
  { path: 'story/chapters/ch-01.md', bytes: 2100 },
  { path: 'story/chapters/ch-02.md', bytes: 1900 },
  { path: 'story/characters-profile.json', bytes: 800 },
  { path: 'story/story.md', bytes: 900 },
  // v0.1.39.1 语义检索缓存（源列表「已向量化」徽章派生）
  { path: 'story/vectors-novel.json', bytes: 24000 },
  { path: 'extraction.json', bytes: 300 },
  { path: 'casting/characters/小明/front.png', bytes: 40000 },
  { path: 'dist/final-v1.mp4', bytes: 1000000, mtime: '2026-09-05T10:00:00Z' },
  { path: 'dist/final-v2.mp4', bytes: 1200000, mtime: '2026-09-06T09:00:00Z' },
  { path: 'cache/try-bgm.mp3', bytes: 200000 },
  { path: 'cache/try-shot-1.mp4', bytes: 300000 },
  // v0.1.39 目录占位说明文件（cache 空目录撑层级用——派生面须排除）
  { path: 'cache/_about.md', bytes: 80 },
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
  // v0.1.39 定制器主槽历史第二版
  'casting/characters/小明/front-v2.png': pngB64,
  // v0.1.37 story 管线产物（chapters index/章文件/人物档案/hub 口径 story.md）
  'sources/novel.txt': b64('小说正文：猫在霓虹巷口醒来。\n'.repeat(60)),
  'story/chapters/index.json': b64(
    JSON.stringify({
      version: 1,
      source: 'story/source-novel.txt',
      auto: false,
      chapters: [
        { no: 1, title: '第一章 风起', start_line: 1, end_line: 40, words: 2034, file: 'story/chapters/ch-01.md' },
        { no: 2, title: '第二章 云涌', start_line: 41, end_line: 80, words: 1876, file: 'story/chapters/ch-02.md' },
      ],
    }),
  ),
  'story/chapters/ch-01.md': b64('---\nno: 1\ntitle: 第一章 风起\nwords: 2034\n---\n\n风起正文：猫在霓虹巷口醒来。\n'),
  'story/chapters/ch-02.md': b64('---\nno: 2\ntitle: 第二章 云涌\nwords: 1876\n---\n\n云涌正文：雨夜追逐。\n'),
  'story/characters-profile.json': b64(
    JSON.stringify([
      {
        name: '小明',
        aliases: ['明明'],
        gender: '男',
        age: '17',
        appearance: '黑短发，背旧书包',
        personality: '沉静',
        relations: [{ name: '阿蓝', relation: '挚友' }],
        first_chapter: 1,
        arc: '寻找回家的路',
      },
      { name: '阿蓝', gender: '女', personality: '爽朗', first_chapter: 2, arc: '同行者' },
    ]),
  ),
  'story/story.md': b64('---\nsource: novel.txt\nwords: 800\n---\n\n月光落在月球快递站的穹顶上……\n'),
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

/** casting 六类 fixture（v0.1.39 定制器：parts + 主槽多版本 front/front-v2）。 */
const CASTING = {
  characters: [{
    name: '小明',
    desc: '柯基快递员',
    voice: 'alloy',
    parts: { 脸型: '瓜子脸', 发型: '长发' },
    views: [
      { view: 'front', path: 'casting/characters/小明/front.png' },
      { view: 'front-v2', path: 'casting/characters/小明/front-v2.png' },
    ],
  }],
  props: [{ name: '长剑', desc: '星海长剑', views: [] }],
  pets: [], formations: [], actions: [], scenes: [],
}

/** BGM 库 fixture。 */
const BGM = [
  { track: 'bgm-001', trigger: 'global', mood: '温馨', file: 'bgm-001.mp3', duration_secs: 92 },
  { track: 'bgm-002', trigger: '追逐', mood: '紧张' },
]

/** v0.1.38 项目级模型设置 fixture（GET :id/models 八能力位快照；PUT 原位更新）。 */
const MODELS_SNAP = {
  path: 'models.json',
  capabilities: [
    { capability: 'chat', source: null, channel_id: null, model: null, note: '', reserved: false, consumed: true, available: { ok: false, detail: '未设置' } },
    { capability: 'image', source: 'local', channel_id: null, model: null, note: '', reserved: false, consumed: true, available: { ok: true, detail: '本地 sd-turbo 生图内核（按需拉起，无常驻）' } },
    { capability: 'video', source: 'channel', channel_id: 'ch-1', model: 'vid-x', note: '', reserved: false, consumed: true, available: { ok: true, detail: '渠道 mock-渠道（vid-x）' } },
    { capability: 'tts', source: null, channel_id: null, model: null, note: '', reserved: false, consumed: true, available: { ok: false, detail: '未设置' } },
    { capability: 'music', source: null, channel_id: null, model: null, note: '', reserved: false, consumed: true, available: { ok: false, detail: '未设置' } },
    { capability: 'asr', source: null, channel_id: null, model: null, note: '', reserved: true, consumed: false, available: { ok: false, detail: '未设置' } },
    // v0.1.39.1：emb 入消费位（story/embed·search 消费；本地探测 ok——须
    // --task embed 实例才有 /v1/embeddings 的提示在 detail）
    { capability: 'emb', source: 'local', channel_id: null, model: 'bge-m3', note: '', reserved: false, consumed: true, available: { ok: true, detail: '本地实例 127.0.0.1:8000（bge-m3；须 --task embed 启动才有 /v1/embeddings）' } },
    { capability: 'vl', source: 'channel', channel_id: 'ch-1', model: null, note: '留待图文理解', reserved: true, consumed: false, available: { ok: true, detail: '渠道 mock-渠道' } },
  ],
}

/** 本地 LLM 实例 / 网关渠道 fixture（模型源下拉数据源）。 */
const LLM_INSTANCES = [
  { id: 'llm-1', name: 'qwen-9b', model: 'qwen', status: 'running', config: {} },
]
const CHANNELS = [
  { id: 'ch-1', name: 'mock-渠道', provider: 'openai', enabled: true, status: 'enabled', models: ['gpt-x'], via_node: '' },
]

/** 观测面（harness 读写；channelCreateFail=渠道创建模拟失败文案，置位一次即清）。 */
globalThis.__FLOW_SMOKE__ = { calls: [], viewSwitches: [], ownershipPuts: [], channelCreateFail: '', tracked: [], slowChapterize: false }

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
  if (method === 'GET' && p === `${base}/files`) return { root: '/tank/film/p1', files: TREE }
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
  if (method === 'GET' && p === `${base}/models`) return MODELS_SNAP
  if (method === 'PUT' && p === `${base}/models`) {
    // 原位更新快照（capability 匹配行合并 slot 字段）——PUT 后 GET 回显
    const row = MODELS_SNAP.capabilities.find((c) => c.capability === body?.capability)
    if (row) {
      row.source = body?.source ?? null
      row.channel_id = body?.channel_id ?? null
      row.model = body?.model ?? null
      row.note = body?.note ?? ''
      if (body && 'routes' in body) row.routes = body.routes ?? []
      row.available = row.source
        ? { ok: true, detail: row.source === 'local' ? '本地' : `渠道 mock-渠道（${row.model ?? 'gpt-x'}）` }
        : { ok: false, detail: '未设置' }
    }
    return { capability: body?.capability, slot: { source: body?.source ?? null, channel_id: body?.channel_id ?? null, model: body?.model ?? null, note: body?.note ?? '' } }
  }
  if (method === 'GET' && p === '/api/v1/llm/instances') return LLM_INSTANCES
  if (method === 'GET' && p === '/api/v1/gateway/channels') return CHANNELS
  // v0.1.39 直填 API：POST gateway/channels 建渠道（id=ch-9，并入 CHANNELS
  // 使下拉/行标签可见）；smock.channelCreateFail 置位时模拟后端校验失败
  //（渠道重名等——错误文案透传前端红条，不发 PUT models）。
  if (method === 'POST' && p === '/api/v1/gateway/channels') {
    if (globalThis.__FLOW_SMOKE__.channelCreateFail) {
      const msg = globalThis.__FLOW_SMOKE__.channelCreateFail
      globalThis.__FLOW_SMOKE__.channelCreateFail = ''
      throw new Error(msg)
    }
    const ch = {
      id: 'ch-9',
      name: body?.name ?? '',
      provider: body?.provider ?? 'openai',
      base_url: body?.base_url ?? '',
      models: body?.models ?? [],
      enabled: true,
      status: 'enabled',
    }
    CHANNELS.push(ch)
    return ch
  }
  if (method === 'POST' && p === `${base}/story/generate`) {
    return { id: 'ft-story', project_id: 'p1', stage: 'story', status: 'queued', log: [], created_at: 0 }
  }
  // v0.1.39 生成分镜配置面板：配置字段随 body（后端 v1 忽略未知字段）
  if (method === 'POST' && p === `${base}/storyboard/generate`) {
    return { id: 'ft-sb', project_id: 'p1', stage: 'storyboard', status: 'queued', log: [], created_at: 0 }
  }
  if (method === 'GET' && p === `${base}/cost`) {
    return { total: 3.14159, currency: '¥', calls: 12, events: 12, groups: [] }
  }
  if (method === 'GET' && p.startsWith(`${base}/cost?`)) {
    return { total: 3.14159, currency: '¥', groups: [{ key: 'story', cost: 1.5, events: 5 }] }
  }
  if (method === 'POST' && p === `${base}/story/clean`) {
    return { id: 'ft-clean', project_id: 'p1', stage: 'story.clean', status: 'queued', log: [], created_at: 0 }
  }
  if (method === 'POST' && p === `${base}/story/chapterize`) {
    // v0.1.6 busy 态观测：slowChapterize 置位时本次慢 150ms（harness 中途读按钮态）
    if (globalThis.__FLOW_SMOKE__.slowChapterize) {
      globalThis.__FLOW_SMOKE__.slowChapterize = false
      await new Promise((r) => setTimeout(r, 150))
    }
    return { id: 'ft-chap', project_id: 'p1', stage: 'story.chapterize', status: 'queued', log: [], created_at: 0 }
  }
  if (method === 'POST' && p === `${base}/story/profile`) {
    return { id: 'ft-prof', project_id: 'p1', stage: 'story.profile', status: 'queued', log: [], created_at: 0 }
  }
  // v0.1.39.1 语义检索：embed 202 任务 / search 同步 200 top_k 结果
  if (method === 'POST' && p === `${base}/story/embed`) {
    return { id: 'ft-emb', project_id: 'p1', stage: 'story.embed', status: 'queued', log: [], created_at: 0 }
  }
  if (method === 'POST' && p === `${base}/story/search`) {
    return {
      query: body?.query ?? '',
      source: 'sources/novel.txt',
      vectors: 'story/vectors-novel.json',
      model: 'bge-m3',
      top_k: 5,
      results: [
        { i: 3, text: '猫在霓虹巷口醒来，抖落一身雨水。', score: 0.9876, start_line: 12 },
        { i: 17, text: '它沿着发光的招牌一路向北。', score: 0.8123, start_line: 61 },
      ],
    }
  }
  if (method === 'POST' && p === `${base}/casting/characters`) {
    return { type: 'characters', name: body?.name, slug: 'x', desc: body?.desc, views: [] }
  }
  // v0.1.6 定妆提取 202 任务（stage=casting → trackFilmTask 登记）
  if (method === 'POST' && p === `${base}/casting/extract`) {
    return { id: 'ft-extract', project_id: 'p1', stage: 'casting', status: 'queued', log: [], created_at: 0 }
  }
  // v0.1.39 定制器：PUT casting/:type/:name（parts 整表替换）+ views/generate
  if (method === 'PUT' && /\/casting\/characters\/[^/]+$/.test(p)) {
    return { type: 'characters', name: '小明', desc: '柯基快递员', parts: body?.parts ?? {}, views: CASTING.characters[0].views }
  }
  if (method === 'POST' && p.endsWith('/views/generate')) {
    return { id: 'ft-view', project_id: 'p1', stage: 'view', status: 'queued', log: [], created_at: 0 }
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

console.log('① SideNav 左侧选项卡（v0.1.5：删「Hub 浏览」项——与左下树卡重复）')
ok(r.nav.items.length === 10, `十选项卡渲染（🎬 FilmHub + 五阶段 + 工作台 + 协作 + 设置 + 模型设置；Hub 浏览项已删）`)
ok(r.nav.items[0].includes('FilmHub'), '品牌项：🎬 FilmHub（回大厅，第一项）')
ok(r.nav.items[1].includes('剧情') && r.nav.items[2].includes('分镜') && r.nav.items[3].includes('定妆'), '阶段标签：剧情/分镜/定妆')
ok(r.nav.items[4].includes('音频') && r.nav.items[5].includes('合成'), '阶段标签：音频/合成')
ok(r.nav.items[6].includes('工作台'), '工作台：合成之后、流程尾部普通项（v0.1.39 回流程尾部）')
ok(r.nav.items[7].includes('协作') && r.nav.items[8].includes('设置') && r.nav.items[9].includes('模型设置'), '协作 + 设置/成员 + 🤖 模型设置选项卡')
ok(!r.nav.items.some((x) => x.includes('浏览')), '「Hub 浏览」导航项不再渲染（hub 视图保留：树卡「完整浏览」/点文件/深链 view=hub）')
assert.deepEqual(r.nav.badges, ['✓', '✓'])
ok(true, '阶段徽章只保留完成态 ✓（README stage=casting：剧情/分镜已过）')
ok(
  r.nav.badges.every((b) => !/[①②③④⑤0-9]/.test(b)),
  '序号徽章已去掉（无 ①②③④⑤ 排序数字）',
)
ok(r.nav.badgeClasses[0].includes('is-done'), '完成徽章态 is-done')
ok(r.nav.activeIndexAfterClick === 2, '点击「分镜」→ active 流转到第 3 项（v0.1.39 顺序：FilmHub 后即阶段区）')
ok(r.nav.viewSwitches.includes('storyboard'), '切换事件 emit（select=storyboard）')
ok(r.nav.collapsedAfterToggle === true, '折叠切换（is-collapsed）')
ok(r.nav.hintGone, '流程链说明行已移除（剧情 → 分镜 → … 文案不再渲染）')
ok(r.nav.treeCardTitle.includes('冒烟项目'), '树卡标题 = 项目标题「🗂 冒烟项目」（不再是「Hub 树」）')

console.log('①.5 StoryPage 剧情页（文档管线：清理/分章/人物档案/转定妆 + v0.1.39.1 向量化/语义搜索）')
ok(
  r.story.sourceRows.length === 2 &&
    r.story.sourceRows[0].includes('novel.txt') &&
    r.story.sourceRows[1].includes('诛仙'),
  '左栏源列表（sources/novel.txt + story/source-诛仙.txt——{root,files} 信封经 hubFileEntries 归一不崩）',
)
ok(r.story.sourceRows[0].includes('已清理') && r.story.sourceRows[0].includes('已入库') && !r.story.sourceRows[0].includes('已分章'), '源状态徽章：已清理 + 已入库（v0.1.44 并入件最高显示档，替代已分章）')
ok(r.story.sourceRows[0].includes('已向量化'), '源状态徽章：已向量化（vectors-novel.json 在树）')
ok(r.story.pipelineVisibleAfterSelect, '选中源 → 管线操作条出现（清理/分章/人物梳理/向量化）')
ok(
  r.story.ingestedLock.cleanDisabled && r.story.ingestedLock.chapterizeDisabled &&
    r.story.ingestedLock.cleanTitle.includes('已入库') && r.story.ingestedLock.cleanTitle.includes('chapters/'),
  'v0.1.44 并入件：已入库源 🧹清理/📖分章按钮禁用 + tooltip（先删 chapters/ 可重新处理）',
)
ok(!r.story.ingestedLock.embedDisabled, '已入库源 🧠向量化不锁（入库后的操作）')
ok(r.story.cleanCall?.path.endsWith('/story/clean') && r.story.cleanCall.body?.source_file === 'story/source-诛仙.txt', '「清理」→ POST story/clean（未入库源诛仙，rules 带 source_file）')
ok(r.story.cleanCall.body?.author === '小明', '清理任务带 author')
ok(r.story.cleanLlmCall?.body?.mode === 'llm' && r.story.cleanLlmCall.body?.model_ref, 'LLM 深清模式 → mode=llm + model_ref')
ok(
  !!r.story.embedCall && r.story.embedCall.path.endsWith('/story/embed') &&
    r.story.embedCall.body?.source_file === 'sources/novel.txt' &&
    r.story.embedCall.body?.model_ref === undefined,
  '「🧠 向量化」→ POST story/embed（带 source_file；无 model_ref=emb 位缺省链）',
)
ok(r.story.embedCall?.body?.author === '小明', '向量化任务带 author')
ok(
  !!r.story.searchCall && r.story.searchCall.path.endsWith('/story/search') &&
    r.story.searchCall.body?.query === '猫在霓虹巷口' &&
    r.story.searchCall.body?.source_file === 'sources/novel.txt',
  '语义搜索框 → POST story/search（query + 选中源已向量化则限定 source_file）',
)
ok(
  r.story.searchHitTexts.length === 2 && r.story.searchHitTexts[0].includes('猫在霓虹巷口醒来') &&
    r.story.searchHitTexts[0].includes('L12'),
  '搜索结果块卡（片段文本 + 行号 L12 + 分数）',
)
ok(!!r.story.searchLocateCall, '结果卡点击 → 左栏定位原文（GET files/sources/novel.txt）')
ok(r.story.tabs.length === 3 && r.story.tabs.some((x) => x.includes('章节')) && r.story.tabs.some((x) => x.includes('人物档案')) && r.story.tabs.some((x) => x.includes('正稿')), '右栏三 Tab：章节 / 人物档案 / 正稿')
ok(r.story.chapterRows.length === 2 && r.story.chapterRows[0].includes('第一章 风起') && r.story.chapterRows[0].includes('字'), '章节卡清单（index.json 派生 + 每章字数；v0.1.44 卡片化）')
ok(r.story.chapterTextShown, '点章节卡头 → 折叠展开 ch-01.md 正文')
ok(r.story.sbFromChapterPending === '1' && r.story.sbFromChapterJumped, '章节卡「从此章生成分镜」→ pending 置位 + 跳分镜页（v0.1.44）')
ok(r.story.profileCards.length === 2 && r.story.profileCards[0].includes('小明') && r.story.profileCards[0].includes('明明'), '人物档案卡（别名渲染）')
ok(r.story.profileCards[0].includes('黑短发') && r.story.profileCards[0].includes('寻找回家的路'), '人物卡：外貌 + 成长线')
ok(r.story.toCastingCall?.path.endsWith('/casting/characters') && r.story.toCastingCall.body?.name === '小明', '「转定妆对象」→ POST casting/characters {name}')
ok(
  typeof r.story.toCastingCall.body?.desc === 'string' && r.story.toCastingCall.body.desc.includes('黑短发') && r.story.toCastingCall.body.desc.includes('沉静'),
  '转定妆 desc = 外貌+性格拼接',
)
ok(r.story.draftShown, '正稿 Tab：story.md 展示（旧功能保留）')

console.log('①.55 管线任务统一进任务中心 + busy 态视觉（v0.1.6）')
ok(
  r.story.tracked.some((x) => x.id === 'ft-clean' && x.stage === 'story.clean'),
  '「清理」202 → trackFilmTask(id, stage=story.clean)——任务中心可见',
)
ok(
  r.story.tracked.some((x) => x.id === 'ft-chap' && x.stage === 'story.chapterize'),
  '「分章」202 → trackFilmTask(story.chapterize)',
)
ok(
  r.story.tracked.some((x) => x.id === 'ft-emb' && x.stage === 'story.embed'),
  '「🧠 向量化」202 → trackFilmTask(story.embed)',
)
ok(
  r.story.busySpinnerShown && r.story.busyLabel.includes('进行中'),
  'busy 态视觉：管线钮进行中 = nx-btn__spin 环形 spinner + 「进行中…」文案（慢响应中途观测；v0.1.7 重设计 spinner 形态）',
)
ok(
  r.story.busyLabelBack.includes('分章'),
  '任务落地后按钮文案复位「📖 分章」（busy 不残留）',
)
console.log('①.56 任务中心 stage 标签白名单（v0.1.6：story 系 + casting）')
assert.deepEqual(
  r.taskLabels,
  {
    script: '分镜',
    storyboard: '分镜',
    story: 'AI 剧情',
    'story.clean': '清理',
    'story.chapterize': '分章',
    'story.profile': '人物梳理',
    'story.embed': '向量化',
    casting: '定妆提取',
    image: '分镜图',
    video: '视频',
    tts: '配音',
    music: 'BGM',
    compose: '合成',
    portrait: '定妆图',
  },
)
ok(true, 'taskLabel 新 stage 全部落本地化标签（story/story.clean/chapterize/profile/embed/casting——不再裸显 stage 串）')
ok(harness.filmTaskStageKey('story.clean') === 'film.kStoryClean', 'stage→i18n 键点分驼峰（story.clean → film.kStoryClean）')
ok(harness.filmTaskStageLabel('unknown.stage', (k) => k) === 'unknown.stage', '白名单外 stage 原样展示不崩')
console.log('①.57 emb 不可用可见性（v0.1.6 tooltip 强化 + 提示行）')
ok(r.story.embHintShown, 'emb 探测 false → 管线操作条上方提示行显示')
ok(
  r.story.embHintText.includes('--task embed') && r.story.embHintText.includes('bge-m3'),
  '提示行文案 = 探测详情 + 人话指引（本地 --task embed 实例 / 渠道 /embeddings API）',
)
ok(r.story.embHintClosedWorks, '提示行可关闭（× 后不再渲染）')
ok(
  r.story.embedTitle.includes('bge-m3') && r.story.embedTitle.includes('--task embed'),
  '向量化钮 disabled tooltip：探测详情 + 指引（两段合成）',
)
ok(r.story.embedDisabled, 'emb 探测 false → 向量化钮保持禁用')

console.log('①.58 管线进度重做（v0.1.7：聚合徽章 / X-Y popover / 失败行内重试）')
ok(
  r.story.pipe.badgeNovel.includes('向量化中') && r.story.pipe.badgeNovel.includes('3/17'),
  '源行聚合徽章：novel 运行中（清理已完）→「向量化中」+ X/Y 3/17（日志解析）',
)
ok(r.story.pipe.badgeZhuxian.includes('失败'), '源行聚合徽章：诛仙分章失败 →「失败」')
ok(r.story.pipe.zhuxianFailed, '失败行红态：fh-row is-failed 类挂载')
ok(r.story.pipe.popoverOpenWorks, '徽章 popover：点击展开（.nx-pop__panel 面板出现）')
ok(
  r.story.pipe.popoverHasTasks,
  'popover 任务明细：各任务「完成 X/Y」离散计数（清理 17/17 + 向量化 3/17）',
)
ok(r.story.pipe.popoverCloseWorks, '徽章 popover：Esc 关闭')
ok(
  !!r.story.pipe.retryCall && r.story.pipe.retryCall.path.endsWith('/story/chapterize'),
  '失败行内重试 → POST story/chapterize（原源 story/source-诛仙.txt）',
)

// —— 管线聚合纯函数（pipelineStatus 同一代码路径）——
console.log('⑩ 管线进度聚合纯函数（X-Y 日志解析 / 徽章聚合优先级）')
{
  const pp = harness.parseChunkProgress
  ok(
    JSON.stringify(pp(['原文 240000 字符 → 17 块 × ≤6K（重叠上下文 200 字符）', '块 3/17 完成'])) ===
      '{"done":3,"total":17}',
    'parseChunkProgress：进度行「块 X/Y 完成」→ 3/17（多条取最后）',
  )
  ok(
    JSON.stringify(pp(['块 2/9 完成', '块 8/9 完成（2 个章节断点）'])) === '{"done":8,"total":9}',
    'parseChunkProgress：后缀注释（章节断点数）不干扰解析',
  )
  ok(
    JSON.stringify(pp(['LLM 深清：17 块 × ≤6K 字符（并发≤8）'])) === '{"done":0,"total":17}',
    'parseChunkProgress：仅计划行 → 0/17（开跑即有总量）',
  )
  ok(pp(['剧情生成：模型 local', '正稿 800 字 → story/story.md']) === null, 'parseChunkProgress：无分块日志 → null（不造假进度）')
  const agg = harness.aggregateSourcePipeline
  const T = (stage, status, log = []) => ({
    id: `${stage}-${status}`, stage, status, sourceFile: 'a.txt', log, error: '', createdAt: 1,
  })
  ok(agg([], 'a.txt').key === 'idle', 'aggregateSourcePipeline：无任务 → idle（无徽章）')
  ok(
    agg([T('story.embed', 'completed'), T('story.clean', 'failed', ['块 1/2 完成'])], 'a.txt').key === 'failed' &&
      agg([T('story.embed', 'completed'), T('story.clean', 'failed', ['块 1/2 完成'])], 'a.txt').failedStages[0] === 'story.clean',
    '聚合优先级：failed 压过 completed（失败阶段收进 failedStages 供重试）',
  )
  ok(
    agg([T('story.embed', 'running'), T('story.chapterize', 'running')], 'a.txt').stage === 'story.chapterize',
    '聚合优先级：多任务 running → 取管线序最靠前（分章先于向量化）',
  )
  ok(agg([T('story.profile', 'queued')], 'a.txt').key === 'queued', 'queued-only → 排队态')
  ok(
    agg([T('story.clean', 'completed'), T('story.embed', 'completed')], 'a.txt').key === 'done',
    '全部 completed → 成功态',
  )
  ok(
    agg([T('story.clean', 'running')], 'b.txt').key === 'idle',
    '源关联过滤：他源任务不串行（b.txt 无徽章）',
  )
}
console.log('①.6 StoryboardPage 分镜页（v0.1.39 生成分镜配置面板）')
ok(r.storyboard.genBtnLabel.includes('生成分镜'), '页头按钮文案含「生成分镜」（从剧情生成分镜）')
ok(r.storyboard.panelToggle.includes('生成分镜配置'), '折叠头「⚙ 生成分镜配置」渲染')
ok(r.storyboard.panelOpenAfterToggle, '点折叠头 → 面板体展开')
assert.deepEqual(r.storyboard.chipLabels, ['小明'])
ok(true, '出场人物 chips = casting/characters 对象名单（小明）')
ok(r.storyboard.chipActiveAfterPick, '勾选人物 → chip 高亮（多选态）')
ok(r.storyboard.voiceSelectValue === 'alloy', '勾选后 voice 行下拉初值 = card 既有 voice（alloy）')
ok(
  !!r.storyboard.voicePut &&
    safeDecode(r.storyboard.voicePut.path).endsWith('/casting/characters/小明') &&
    r.storyboard.voicePut.body?.voice === 'nova' &&
    r.storyboard.voicePut.body?.author === '小明',
  '改声线 → PUT casting/characters/小明 {voice: nova, author}（既有端点即时落卡）',
)
ok(
  !!r.storyboard.genCall && r.storyboard.genCall.path.endsWith('/storyboard/generate'),
  '生成 → POST storyboard/generate',
)
const sbBody = r.storyboard.genCall?.body
ok(
  !!sbBody &&
    Array.isArray(sbBody.characters) &&
    sbBody.characters[0] === '小明' &&
    sbBody.voices?.['小明'] === 'nova',
  '生成 body 带出场人物 characters + 声线 voices（前端拼装；后端已消费）',
)
ok(
  !!sbBody && sbBody.shot_count === 8 && sbBody.duration_hint === '约 60 秒',
  '生成 body 带镜头数 shot_count=8 + 总时长提示 duration_hint',
)
ok(
  !!sbBody && sbBody.chapter_range === undefined,
  '章节缺省（全部剧情）→ body 不带 chapter_range（全文语料现行为）',
)
ok(
  r.storyboard.chapterOptions.length === 3 &&
    r.storyboard.chapterOptions[0].includes('全部剧情') &&
    r.storyboard.chapterOptions.some((x) => x.includes('第 1 章')) &&
    r.storyboard.chapterOptions.some((x) => x.includes('第 2 章')),
  '面板分镜章节下拉：全部剧情[缺省] + 第 N 章…（chapters index 派生；v0.1.44）',
)
console.log('①.6.5 分镜章节预填（v0.1.44：章节卡「从此章生成分镜」→ 面板消费）')
ok(r.storyboard.chapterPrefillOpen, 'pending 置位挂载 → 面板自动展开')
ok(r.storyboard.chapterPrefillValue === '2', '章节下拉自动选中该章（第 2 章）')
const sbChBody = r.storyboard.chapterRangeGenCall?.body
ok(
  !!sbChBody && sbChBody.chapter_range === '2',
  '预填后生成 → body 带 chapter_range="2"（按章分镜语料）',
)
ok(
  !!sbBody && sbBody.model_ref?.capability === 'chat' && sbBody.author === '小明',
  '生成 body 仍带 model_ref + author（原契约不变）',
)
ok(r.storyboard.trackedAfterGen, 'v0.1.6 生成分镜 202 → trackFilmTask(id, stage=storyboard)——任务中心可见')

console.log('② CastingPage 定妆页（v0.1.39 定制器：六类 Tab / 部件面板 / 主视图生成）')
ok(r.casting.tabs.length === 6, '六类 Tab 渲染')
ok(
  ['人物', '武器', '宠物', '排列', '动作', '场景'].every((x) => r.casting.tabs.some((t) => t.includes(x))),
  'Tab 标签：人物/武器/宠物/排列/动作/场景',
)
ok(r.casting.charactersCards.length === 1 && r.casting.charactersCards[0].includes('小明'), '对象卡渲染（characters: 小明）')
ok(r.casting.czStageFilled && !r.casting.czPlaceholderShown, '中央主体完整视图=主槽最新版本（front-v2 已填、无占位）')
ok(
  r.casting.czCaption.includes('主视图') && r.casting.czCaption.includes('正面全身'),
  '主视图标题：主视图 · 正面 + 类型主视图描述（正面全身）',
)
assert.deepEqual(
  r.casting.partRowKeys,
  ['脸型', '发型', '服装', '配饰', '体型', '气质'],
)
ok(true, '人物部件槽 6 行（脸型/发型/服装/配饰/体型/气质——按类型模板渲染）')
ok(r.casting.historyThumbCount === 2, '主槽历史缩略 2 版（front/front-v2 多版本保留）')
ok(r.casting.chipCountOnExpand === 7, '发型槽展开=6 预设 chips + 自定义')
ok(r.casting.chipActiveAfterPick, '选中「短发」→ chip 高亮（部件选中态）')
ok(r.casting.dirtyBadgeAfterPick, '改动 → dirty 徽章（已修改）')
const partsPut1 = r.casting.partsPutBeforeGen?.body
ok(!!r.casting.partsPutBeforeGen, '生成前 PUT casting（未保存部件改动随生成一并保存）')
ok(
  !!partsPut1 && partsPut1.parts?.['脸型'] === '瓜子脸' && partsPut1.parts?.['发型'] === '短发',
  'PUT parts：发型替换为短发、脸型原样保留',
)
const mainGen1 = r.casting.mainGenCall?.body
ok(
  !!r.casting.mainGenCall && mainGen1?.view === 'front' && !('prompt' in (mainGen1 ?? {})),
  'POST views/generate：view=主槽 front、不带 prompt（组合提示词由后端按 parts 组装）',
)
const partsPut2 = r.casting.secondPartsPut?.body
ok(
  !!r.casting.secondPartsPut && partsPut2?.parts?.['发型'] === '短发' && partsPut2?.parts?.['脸型'] === '圆脸',
  '二次生成 PUT parts：脸型=圆脸、发型保持短发',
)
assert.deepEqual(
  harness.partsDiff(partsPut1?.parts ?? {}, partsPut2?.parts ?? {}),
  ['脸型'],
)
ok(true, '两次生成 parts 仅「脸型」一键差异（其它部件保持一致——用户核心诉求）')
ok(r.casting.secondMainGenCall?.body?.view === 'front', '二次生成仍打主槽 front（后端自动版本化 -v3）')
ok(
  r.casting.auxSlotCount === 4 && !r.casting.auxSlotLabels.includes('正面'),
  '「更多视图」折叠区 4 辅助槽（side/back/action/custom——排除主槽 front）',
)
ok(r.casting.propsGetCalled, 'Tab 切换 → GET /casting/props 重载该类')
ok(r.casting.propsCardsAfterSwitch.length === 1 && r.casting.propsCardsAfterSwitch[0].includes('长剑'), 'props 对象卡（长剑）')
ok(r.casting.ownerBadgeText.includes('小红'), '对象级认领徽章（characters/小明 → 负责人：小红）')
ok(r.casting.claimPuts >= 1, '未认领对象「认领」→ PUT ownership.json')
ok(r.casting.claimObjectOwner === '小明', '认领后 casting_objects[props/长剑].owner = 小明')
ok(r.casting.extractTracked, 'v0.1.6「AI 提取定妆对象」202 → trackFilmTask(id, stage=casting)——任务中心可见')

console.log('③ ComposePage 合成页（dist 版本 / cache commit）')
ok(r.compose.distRows.length === 2, 'dist 成品版本列表（2 个）')
ok(r.compose.distRows[0].includes('v2') && r.compose.distRows[1].includes('v1'), '版本倒序（v2 在前）')
ok(r.compose.cacheCards.length === 2, 'cache 半成品列表（2 个，与成品分离；_about.md 占位不显示）')
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

console.log('⑤.5 ModelsPage 项目级模型设置（v0.1.38 八能力位 + 配置弹窗 + 项目默认生成）')
ok(r.models.rows.length === 8, '八能力位行渲染（chat/image/video/tts/music/asr/emb/vl）')
ok(
  ['文本', '生图', '图生视频', '配音', '音乐', '语音识别', '向量化', '多模态'].every((x) =>
    r.models.rows.some((row) => row.includes(x)),
  ),
  '能力位名称：文本/生图/图生视频/配音/音乐/语音识别/向量化/多模态',
)
ok(r.models.reservedRows.length === 2, 'asr/vl 两行带「预留槽位」灰徽章（v0.1.39.1 emb 入消费位）')
ok(
  r.models.reservedRows.every((row) => row.includes('预留槽位')),
  '预留行文案含待消费说明',
)
ok(r.models.okDots === 4, '可用性绿点 4 个（image 本地 + video 渠道 + vl 渠道 + emb 本地；未设置位灰）')
ok(r.models.modalOpened, '点「配置」→ 配置弹窗打开')
ok(
  !!r.models.putCall && safeDecode(r.models.putCall.path).endsWith('/film/projects/p1/models'),
  '弹窗保存 → PUT /film/projects/p1/models',
)
ok(
  !!r.models.putCall &&
    r.models.putCall.body?.capability === 'chat' &&
    r.models.putCall.body?.source === 'channel' &&
    r.models.putCall.body?.channel_id === 'ch-1',
  'PUT body：单能力位 {capability: chat, source: channel, channel_id}',
)
ok(!!r.models.putCall && r.models.putCall.body?.author === '小明', 'PUT body 带 author（操作人）')
ok(
  !!r.models.snapshotAfterPut && r.models.snapshotAfterPut.source === 'channel',
  'PUT 后 GET 快照回显（chat 位 channel 源）',
)
ok(
  !!r.models.projectDefaultGenCall && r.models.projectDefaultGenCall.path.endsWith('/story/generate'),
  '「项目默认」选中 → AI 写剧情仍发 POST story/generate',
)
ok(
  !!r.models.projectDefaultGenCall && r.models.projectDefaultGenCall.body?.model_ref === undefined,
  '「项目默认」= 不传 model_ref 字段（后端读 models.json 缺省链）',
)
ok(r.models.storyGenTracked, 'v0.1.6「AI 写剧情」202 → trackFilmTask(id, stage=story)——任务中心可见')

console.log('⑤.5b ModelsPage 高级路由（v0.1.44：任务路由 + 失败 fallback 链）')
{
  const rt = r.models.routing
  ok(rt.panelOpened, '「⚙ 高级路由」→ 折叠区展开（无规则空态）')
  ok(rt.rowsBefore === 0, '初始无路由规则（空态文案）')
  ok(rt.hintShown, '说明行：自上而下匹配 / 失败自动切换 / 回落默认')
  ok(rt.agentHintShown, 'agent 双通道说明（REST PUT routes / PUT files/models.json）')
  ok(rt.rowsAfterAdd === 2, '「+ 添加路由」×2 → 两行规则')
  const body = rt.putCall?.body
  ok(
    !!rt.putCall && Array.isArray(body?.routes) && body.routes.length === 2,
    '保存 → PUT /models 带 routes（整组替换，两条）',
  )
  ok(
    !!body && body.routes?.[0]?.task === 'story.clean' && body.routes?.[0]?.source === 'channel'
      && body.routes?.[0]?.channel_id === 'ch-1',
    '规则 ①：{task: story.clean, source: channel, channel_id}（任务枚举下拉）',
  )
  ok(
    !!body && !('task' in (body.routes?.[1] ?? {})) && body.routes?.[1]?.channel_id === 'ch-1',
    '规则 ②：task 缺省=兜底路由（任务留空）',
  )
  ok(
    !!body && body?.capability === 'chat' && body?.source === 'channel',
    'PUT body 保留主默认源字段（只改路由不动默认）',
  )
  ok(rt.savedMark, '保存成功 ✓ 路由已保存提示')
  ok(rt.rowDots >= 2, '保存后快照逐条可用性点回显')
}

console.log('⑤.6 ModelsPage 直填 API（v0.1.39：建渠道 + 一键设默认 + 失败红条）')
{
  const a = r.models.addApi
  ok(a.dialogOpened, '页头「＋ 添加 API」→ 直填弹窗打开')
  ok(a.autoName === 'cosyvoice-v2 · 文本（LLM）', '名称自动联动 = 模型名 · 能力（未手改时）')
  ok(a.autoNameAfterCapSwitch === 'cosyvoice-v2 · 配音', '切能力（chat→tts）→ 名称随能力联动')
  const cc = a.createCall
  ok(
    !!cc && cc.path === '/api/v1/gateway/channels' && cc.body?.name === 'cosyvoice-v2 · 配音',
    '提交 → POST /api/v1/gateway/channels（name=自动名称）',
  )
  ok(
    !!cc && cc.body?.provider === 'openai' && cc.body?.base_url === 'http://192.168.1.5:8000/v1',
    'body：provider=openai（OpenAI 兼容缺省）+ 裸 host 地址规整补 /v1',
  )
  ok(
    !!cc && cc.body?.api_key === 'sk-test-123' && JSON.stringify(cc.body?.models) === '["cosyvoice-v2"]',
    'body：api_key（password 输入）+ models=[模型名]（渠道无 capability 字段——能力归属走 models.json）',
  )
  const ap = a.modelsPutAfterAdd?.body
  ok(
    !!a.modelsPutAfterAdd &&
      ap?.capability === 'tts' &&
      ap?.source === 'channel' &&
      ap?.channel_id === 'ch-9' &&
      ap?.model === 'cosyvoice-v2',
    '成功后链式 PUT /models：{capability: tts, source: channel, channel_id: 新渠道 id, model}',
  )
  ok(!!ap && ap?.author === '小明', 'PUT body 带 author（操作人）')
  ok(
    a.notice.includes('cosyvoice-v2') && a.notice.includes('配音'),
    '成功 ✓ 提示（渠道名 + 能力位）',
  )
  ok(a.modalClosedAfterDone, '成功后弹窗关闭 + 快照/渠道清单刷新')
  ok(a.failError.includes('渠道名称已存在'), '失败红条透传后端校验文案（渠道重名）')
  ok(a.failNoModelsPut, '建渠道失败不发 PUT /models（链式中断）')
  ok(
    harness.normalizeApiBaseUrl('https://api.openai.com') === 'https://api.openai.com/v1' &&
      harness.normalizeApiBaseUrl('http://10.0.0.9:9000/v1/') === 'http://10.0.0.9:9000/v1' &&
      harness.normalizeApiBaseUrl('  ') === '',
    'normalizeApiBaseUrl 纯函数：裸域名补 /v1 / 已带路径去尾斜杠保留 / 空串',
  )
}

// —— 纯函数补充断言（flowFiles / flowTypes / collab 同一代码路径）——
console.log('⑥ 流程纯函数（树派生 / 校验 / 阶段解析 / ownership）')
{
  const tree = TREE
  const dvs = harness.distVersions(tree)
  assert.deepEqual(dvs.map((e) => e.path), ['dist/final-v2.mp4', 'dist/final-v1.mp4'])
  ok(true, 'distVersions：final-v*.mp4 过滤 + 版本倒序')
  ok(harness.distVersionOf('dist/final-v7.mp4') === 7 && harness.distVersionOf('dist/x.mp4') === -1, 'distVersionOf 版本号提取')
  assert.deepEqual(harness.cacheEntries(tree).map((e) => e.path), ['cache/try-bgm.mp3', 'cache/try-shot-1.mp4'])
  ok(true, 'cacheEntries：cache/ 半成品过滤（与成品分离；_about.md 占位说明不算半成品）')
  assert.deepEqual(harness.storySources(tree).map((e) => e.path), [
    'sources/novel.txt',
    'story/source-诛仙.txt',
  ])
  ok(true, 'storySources：sources/ + story/source-*.txt 原文派生（hub 落点兼容）')
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

// —— 定妆定制器纯函数（v0.1.39：模板/主槽/组合提示词预览与后端同契约）——
console.log('⑥.5 定制器纯函数（六类模板 / 主槽映射 / 组合提示词 / parts 差异）')
{
  const mains = ['characters', 'pets', 'props', 'scenes', 'formations', 'actions'].map(
    (ty) => harness.mainSlotOf(ty),
  )
  assert.deepEqual(mains, ['front', 'body', 'full', 'pano', 'group', 'pose'])
  ok(true, '六类主槽映射：front/body/full/pano/group/pose')
  ok(
    harness.composePromptPreview('characters', '黑发少年', 'front', { 脸型: '瓜子脸', 发型: '长发' }) ===
      '黑发少年，脸型：瓜子脸，发型：长发（与其它镜头严格同一定妆对象），正面全身',
    '组合提示词预览=后端同模板（desc，键：值…（一致性锚），类型主视图描述）',
  )
  ok(
    harness.composePromptPreview('pets', '庭院月桂树', 'body', { 形态: '植物', 株型: '乔木' }).endsWith('全株'),
    'pets 形态=植物 → 主视图描述切换「全株」',
  )
  const plantSlots = harness.partSlotsFor('pets', { 形态: '植物' })
  ok(
    plantSlots.some((s) => s.key === '株型') && !plantSlots.some((s) => s.key === '毛色'),
    'pets 槽集切换：形态=植物 → 株型/叶形/花色/果实/姿态（动物槽收起）',
  )
  const latest = harness.latestMainView(
    { views: [{ view: 'front' }, { view: 'front-v3' }, { view: 'front-v2' }, { view: 'side' }] },
    'characters',
  )
  ok(latest?.view === 'front-v3', '主槽最新版本派生（front-v3；side 非主槽不掺入）')
  assert.deepEqual(harness.partsDiff({ 发型: '长发', 脸型: '瓜子脸' }, { 发型: '短发', 脸型: '瓜子脸' }), ['发型'])
  ok(true, 'partsDiff：仅差异键（替换部件各自独立）')
  const tplKeys = Object.values(harness.PART_TEMPLATES).map((tpl) => tpl.slots.length)
  ok(tplKeys.every((n) => n >= 3 && n <= 7), '六类模板槽行数 3-7（formations 3 / characters·pets 6-7）')
}

// —— 工作台合成区纯总装（v0.1.36）：源码契约断言（FilmStudio 为应用根组件，
//    宿主桥依赖重不宜整挂——以源码契约防生成入口回归 + 验证 BGM 下拉数据源）——
console.log('⑦ 工作台合成区纯总装（无 AI 生成入口 / BGM 下拉=音频页库）')
{
  const src = readFileSync(join(appRoot, 'src/FilmStudio.vue'), 'utf8')
  ok(!src.includes('filmGenMusic'), '工作台无「生成 BGM」入口（filmGenMusic 不再引入）')
  ok(!src.includes('musicPrompt') && !src.includes('genMusic('), 'BGM 提示词表单已移除（musicPrompt/genMusic 无残留）')
  ok(src.includes('filmListBgm'), 'BGM 下拉数据源 = filmListBgm（与音频页/合成页同源库）')
  ok(src.includes('filmCompose(cur.id, bgmTrack.value || undefined'), '「合成成片」body 带选中 bgm_track')
  ok(src.includes("@click=\"setFlowView('audio')\""), '「管理 BGM」链接 → 音频页（setFlowView(audio)）')
  ok(src.includes("@click=\"setFlowView('compose')\""), '「完整版本」链接 → 合成页（setFlowView(compose)）')
  ok(src.includes('latestVersion') && src.includes('previewDistVersion') && src.includes('downloadDistVersion'), '成片版本迷你列表（最新一版 + 预览/下载）')
  // v0.1.5 视图深链：启动解析 location.search（p+view 直达）+ 定位参数 pending 链
  ok(
    src.includes('parseStandaloneQuery') && src.includes('applyBootDeepLink'),
    '启动深链：location.search 解析 p/view（standalone.html?p=&view= 直达工作室跳大厅）',
  )
  ok(
    src.includes('pendingCastSelect.value = bootDeepLink.cast') &&
      src.includes('pendingHubFile.value = bootDeepLink.file'),
    '深链定位参数：&cast= → 定妆对象 / &file= → Hub 文件（pending 链页面消费）',
  )
}

// —— 原文导入编码兼容（v0.1.39）：fake ArrayBuffer 直断言解码链 ——
//    （GBK 双字节对 fatal UTF-8 必非法 → 落 GB18030 转码；0xFF 引导字节
//     对两条链均非法 → 二进制拒绝路径）
console.log('⑧ 原文导入编码兼容（UTF-8 直通 / GB18030 转码 / 二进制拒绝）')
{
  // 「中文」的 GBK 双字节序列 D6 D0 CE C4（GBK/GB2312/GB18030 同码位）
  const gbkBuf = new ArrayBuffer(4)
  new Uint8Array(gbkBuf).set([0xd6, 0xd0, 0xce, 0xc4])
  const g = harness.decodeSourceBytes(gbkBuf)
  ok(g.transcoded === true, 'GBK 字节 → GB18030 转码链（transcoded=true）')
  ok(new TextDecoder('utf-8').decode(g.bytes) === '中文', '转码产物 = UTF-8「中文」字节')
  ok(
    harness.bytesToB64(g.bytes) === Buffer.from('中文', 'utf8').toString('base64'),
    '转码字节 → 标准 b64（= 后端收到 UTF-8 content_b64）',
  )
  // 真 UTF-8 原文 → fatal 校验通过 → 原字节直通（不转码）
  const u = harness.decodeSourceBytes(new TextEncoder().encode('中文小说').buffer)
  ok(u.transcoded === false && new TextDecoder('utf-8').decode(u.bytes) === '中文小说', '真 UTF-8 → 原字节直通（transcoded=false）')
  // 二进制（0xFF 对 UTF-8 与 GB18030 均非法）→ 抛错 → 调用方维持原错误提示
  let binaryThrew = false
  try {
    harness.decodeSourceBytes(new Uint8Array([0xff, 0xd0, 0x81, 0x40, 0x00]).buffer)
  } catch {
    binaryThrew = true
  }
  ok(binaryThrew, '二进制字节 → 双链解码失败抛错（前端本地拒绝，不再发请求吃 400）')
}

// —— files 树响应归一三态（v0.1.39.1 崩点修复：{root,files} 信封 / 裸数组 /
//    undefined——hubFileEntries/hubFilePaths 纯函数直断言同一代码路径） ——
console.log('⑨ files 树响应归一三态（信封 / 裸数组 / undefined）')
{
  const envelope = { root: '/tank/film/p1', files: [{ path: 'a.txt', bytes: 1 }, { path: 'b/c.json' }] }
  ok(
    JSON.stringify(harness.hubFilePaths(envelope)) === JSON.stringify(['a.txt', 'b/c.json']),
    '信封 {root,files:[对象]} → 路径数组（对象取 path）',
  )
  ok(
    JSON.stringify(harness.hubFilePaths(['x.md', 'dist/y.mp4'])) === JSON.stringify(['x.md', 'dist/y.mp4']),
    '字符串数组直通（零转换）',
  )
  ok(
    harness.hubFilePaths(undefined).length === 0 && harness.hubFileEntries(undefined).length === 0,
    'undefined → 空数组（不抛 e.filter is not a function）',
  )
  ok(
    harness.hubFileEntries({ files: undefined }).length === 0 &&
      harness.hubFileEntries({}).length === 0,
    '信封 files 缺失/非数组 → 空数组',
  )
  ok(
    JSON.stringify(hubFileEntriesStrArr(harness)) === JSON.stringify([{ path: 's.md' }]),
    '字符串条目归一为 {path} 对象（混布宽容）',
  )
  // 向量徽章派生（story/vectors-<base>.json 在树 → embedded）
  const st = harness.storySourceStatus(
    harness.hubFileEntries(envelope).concat([{ path: 'story/vectors-a.json' }]),
    'story/source-a.txt',
    null,
  )
  ok(st.embedded === true && st.cleaned === false, 'storySourceStatus：vectors-<base>.json 在树 → embedded=true')
  ok(
    harness.storySourceStatus(harness.hubFileEntries(envelope), 'story/source-a.txt', null).embedded === false,
    '无向量缓存 → embedded=false',
  )
}
function hubFileEntriesStrArr(h) {
  return h.hubFileEntries({ files: ['s.md'] })
}

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（FilmHub 流程化组件 happy-dom 冒烟）`)
