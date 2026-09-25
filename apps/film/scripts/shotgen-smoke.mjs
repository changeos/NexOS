// =============================================================================
// shotgen-smoke.mjs —— 短片生成 ShotGen 冒烟（happy-dom；v0.1.45 / film 0.1.13）。
//
// 照 toast-smoke.mjs 先例（主前端无 vitest）：
//   1. vite JS API 构建 scripts/shotgen-smoke-harness.ts（plugin-vue 编译 .vue）
//      → 临时 ESM；
//   2. happy-dom 全局 + mock 宿主桥 api（shotgen 三端点服务端形态模拟：
//      POST 202 任务乐观落服务端列表 / GET 列表 / GET :id 两拍 running→done，
//      prompt=电影公路 固定失败，failNextCreate 旗标 500 / files/download 信封）；
//   3. import 构建产物 → run() → node:assert 断言：
//      ① 纯函数：状态归一 done/error→completed/failed；种子解析（空/0/正数过、
//         负/小数/非数字 NaN）；表单四校验；作品筛 generating=2；提示词折叠；
//         下载名净化；画幅五档含 21:9；时长四档 5/8/10/15；灵感 4 枚；
//         深链 view=shotgen（p 并存 p 优先、studio 视图不误判）；
//      ② 引擎全链：提交 body 契约（seed=42 带、无 image_b64）；乐观 queued；
//         防重复锁（canSubmit false + 再 submit 不发 POST）；轮询到 completed；
//         产物 url 惰性转 data:video/mp4；**toast 静默**（useToastState 全程空
//         + 不触 /film/tasks 任务中心端点）；复制提示词走剪贴板；refill 回填
//         （提示词/种子/切创作 Tab）；任务失败归档（error 首行，可再提交）；
//         提交失败红条（POST 500，不乐观入列）；
//      ③ 图生档：gif 拒 / 10MB 超限拒 / png 预览 data URL + image_b64 随请求；
//         无图 submit 拦截（formError=image 不发 POST）；
//      ④ ShotGen.vue：创作/作品双 Tab；画幅 16:9 默认 + 21:9 可切；时长四档；
//         生成按钮含模型名「生成视频 · MiniMax H3」+ 空提示词置灰 + 输入后启用；
//         video 位未配黄条 + 去配置/高级模式双 emit + 生成置灰；灵感 chip 回填
//         结构化长提示词（0-3s）；作品瀑布流 4 卡（含失败卡红字）；点击装载
//         播放器（http 直连 + data URL 信封两形态）；复制提示词 ✓ 反馈；
//         以此再生成回填创作页；
//      ⑤ FilmStudio 接线契约（源码断言，照 toast-smoke ⑦ 先例）+ i18n ×4
//         shotgen.* 键齐平（键集一致、60 键非空）。
//
// 运行：cd apps/film && npm run smoke:shotgen
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

// —— 1. vite 构建 harness ——
const tmpDir = mkdtempSync(join(tmpdir(), 'film-shotgen-smoke-'))
const bundlePath = join(tmpDir, 'shotgen-smoke.mjs')
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
      entry: join(appRoot, 'scripts/shotgen-smoke-harness.ts'),
      formats: ['es'],
      fileName: () => 'shotgen-smoke.mjs',
    },
    outDir: tmpDir,
    emptyOutDir: true,
    cssCodeSplit: false,
    minify: false,
  },
  logLevel: 'warn',
})

// —— 2. happy-dom 全局 + mock 宿主桥（shotgen 服务端形态模拟） ——
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
  'File', 'Blob', 'FileReader',
]) {
  if (win[k]) globalThis[k] = win[k]
}

globalThis.__FLOW_SMOKE__ = { calls: [], failNextCreate: false }

/** mock 服务端：作品表（列表/详情同源）+ 任务两拍推进 + 固定失败剧本。 */
const serverWorks = new Map()
serverWorks.set('sg-0', {
  id: 'sg-0',
  prompt: '云上巨鲸：一头发光的巨鲸游过晚霞中的云海，背脊洒下星光',
  ratio: '21:9',
  duration_secs: 8,
  seed: null,
  status: 'done',
  model: 'H3',
  video_url: 'https://cdn.example.test/w.mp4',
  created_at: '2026-09-01T10:00:00Z',
})
let seq = 0
const pollCount = new Map()
const failIds = new Set()

async function handle(method, path, body) {
  globalThis.__FLOW_SMOKE__.calls.push({ method, path, body })
  if (method === 'GET' && path === '/api/v1/film/shotgen') {
    return [...serverWorks.values()]
  }
  if (method === 'POST' && path === '/api/v1/film/shotgen') {
    if (globalThis.__FLOW_SMOKE__.failNextCreate) {
      globalThis.__FLOW_SMOKE__.failNextCreate = false
      throw new Error('500 Internal Server Error')
    }
    const id = `sg-${++seq}`
    if (body?.prompt === '电影公路') failIds.add(id)
    serverWorks.set(id, {
      id,
      prompt: body?.prompt ?? '',
      ratio: body?.ratio ?? '16:9',
      duration_secs: body?.duration_secs ?? 5,
      seed: body?.seed ?? null,
      status: 'queued',
      created_at: Date.now(),
    })
    return { id, status: 'queued', kind: 'shotgen', stage: 'shotgen', created_at: Date.now() }
  }
  const m = /^\/api\/v1\/film\/shotgen\/(.+)$/.exec(path)
  if (method === 'GET' && m) {
    const id = decodeURIComponent(m[1])
    const cur = serverWorks.get(id)
    if (!cur) throw new Error('404 not found')
    if (failIds.has(id)) {
      const w = { ...cur, status: 'error', error: '渠道 502 Bad Gateway\n上游超时' }
      serverWorks.set(id, w)
      return w
    }
    const n = (pollCount.get(id) ?? 0) + 1
    pollCount.set(id, n)
    const w = n >= 2
      ? { ...cur, status: 'done', video_url: `/tank/shotgen/${id}.mp4` }
      : { ...cur, status: 'running' }
    serverWorks.set(id, w)
    return w
  }
  if (method === 'GET' && path.startsWith('/api/v1/files/download')) {
    return { mime_type: 'video/mp4', content_base64: 'AAAA' }
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

console.log('① shotgen.ts 纯函数 + view=shotgen 深链')
assert.deepEqual(r.pure.statusNorm, ['completed', 'failed', 'running', 'queued'])
ok(true, '状态归一：done→completed / error→failed / running 透传 / 空→queued')
assert.deepEqual(r.pure.seed, [null, 0, 42, Number.NaN, Number.NaN, Number.NaN])
ok(true, '种子解析：空→null（不传）/ 0 与正整数过 / 负·小数·非数字 NaN 拦截')
assert.deepEqual(r.pure.form, ['prompt', 'seed', 'image', ''])
ok(true, '表单校验：空提示词 / 非法种子 / 图生缺参考图 / 全过')
assert.equal(r.pure.worksFilter, 2)
ok(true, '作品筛选：generating 只留 queued/running（2 条）')
assert.equal(r.pure.promptLong, true)
ok(true, '提示词折叠阈值：多行结构化提示词判长（卡内 3 行收起可展开）')
assert.equal(r.pure.downloadName, 'shotgen-sg_1_2.mp4')
ok(true, '下载名净化（非法文件名字符折叠）')
assert.deepEqual(r.pure.ratios, ['16:9', '9:16', '1:1', '4:3', '21:9'])
ok(true, '画幅五档：16:9/9:16/1:1/4:3/21:9（含 21:9）')
assert.deepEqual(r.pure.durations, [5, 8, 10, 15])
ok(true, '时长四档：5/8/10/15s')
assert.equal(r.pure.promptMax, 3000)
ok(true, '提示词上限 3000（0/3000 计数口径）')
assert.equal(r.pure.inspCount, 4)
ok(true, '灵感 chips 4 枚（含 2 个结构化长提示词）')
ok(r.pure.deepLink.shotgenOnly, '深链 view=shotgen → 布尔位（默认首屏直达）')
ok(r.pure.deepLink.withProject, '深链 p+view=shotgen 并存 → p 优先（projectId 保住、view 归空）')
ok(r.pure.deepLink.studioView, '深链 view=workbench 照旧（shotgen 位不误判）')

console.log('② 引擎全链（提交契约 / 防重复 / 轮询终态 / toast 静默 / 失败路径）')
assert.equal(r.engine.initialWorks, 1)
ok(true, 'start() 拉服务端作品流（预存 1 条完成作品）')
{
  const b = r.engine.submitBody
  ok(b.prompt === '雪山之巅的仙侠对决' && b.ratio === '16:9' && b.duration_secs === 10,
    'POST /film/shotgen body：prompt + ratio（默认 16:9）+ duration_secs')
  ok(b.seed === 42 && !('image_b64' in b) && !('model_ref' in b),
    'body 契约：seed=42 带 / 文生档无 image_b64 / model_ref 缺省（走 video 位默认）')
}
ok(r.engine.optimisticQueued, '提交 202 → 乐观入列（queued 即刻可见）')
ok(r.engine.dupBlocked, '防重复锁：生成中 canSubmit=false + 再 submit 不发 POST')
ok(r.engine.completed, '本地轮询 GET :id → running→done 到终态（不进全局任务中心）')
assert.equal(r.engine.videoDataUrl, 'data:video/mp4;base64,AAAA')
ok(true, '产物 url 惰性解析：产物路径 → files/download 信封 → data URL 可播')
ok(r.engine.toastSilent, 'toast 静默：全程 useToastState 为空（反馈只在页内）')
ok(r.engine.noTaskCenterCalls, '不触 /film/tasks 任务中心端点（不走 trackFilmTask/addTracked）')
assert.equal(r.engine.clipboardText, '雪山之巅的仙侠对决')
ok(true, '复制提示词走剪贴板（承接上段直接复制生成的工作流）')
assert.deepEqual(r.engine.refill, { prompt: '雪山之巅的仙侠对决', seed: '42', tab: 'create' })
ok(true, 'refill 回填：提示词 + 种子 + 切回创作 Tab')
assert.equal(r.engine.failedWork.status, 'failed')
assert.equal(r.engine.failedWork.error, '渠道 502 Bad Gateway')
ok(true, '任务失败归档：作品卡 failed + 首行原因（生成可继续）')
ok(r.engine.canSubmitAfterFail, '失败终态释放防重复锁（可再提交）')
ok(r.engine.submitErrorShown && r.engine.worksAfterSubmitFail === 0,
  '提交失败（POST 500）：页内红条 + 不乐观入列')

console.log('③ 图生档（参考图校验 + image_b64 契约）')
assert.equal(r.imageMode.badType, 'type')
ok(true, '参考图类型不符（gif）→ 拒（png/jpeg/webp）')
assert.equal(r.imageMode.tooLarge, 'size')
ok(true, '参考图超 10MB → 拒')
assert.equal(r.imageMode.previewPrefix, 'data:image/png;base64,')
ok(true, '参考图预览 data URL（FileReader→b64 拆分）')
ok(r.imageMode.bodyHasImage, '图生档提交带 image_b64（首帧参考随请求发出）')
ok(r.imageMode.noImageBlocked, '图生档无图：formError=image 拦截 submit（不发 POST）')

console.log('④ ShotGen.vue 真挂载（创作/作品双页 + 降级 + 播放器）')
assert.deepEqual(r.render.tabs, ['创作', '作品'])
ok(true, '顶栏双 Tab：创作｜作品')
assert.deepEqual(r.render.ratioLabels, ['16:9', '9:16', '1:1', '4:3', '21:9'])
assert.equal(r.render.activeRatio, '16:9')
ok(true, '画幅五档图标组，默认 16:9 选中')
assert.equal(r.render.ratioSwitch, '21:9')
ok(true, '画幅可切（点击 21:9 生效）')
assert.deepEqual(r.render.durationLabels, ['5s', '8s', '10s', '15s'])
ok(true, '时长四档 5/8/10/15s')
ok(r.render.genLabel.includes('生成视频') && r.render.genLabel.includes('MiniMax H3'),
  '大按钮「✦ 生成视频 · MiniMax H3」（当前模型标识随按钮）')
ok(r.render.genEnabledAfterPrompt, '生成按钮：空提示词置灰 → 输入后启用')
ok(r.render.genDisabledWhenVideoMissing, 'video 位未配：生成按钮置灰（黄条引导去配置）')
ok(r.render.warnBar, 'video 位未配黄条「生视频需配置 video 渠道模型」')
ok(r.render.goModelsEmitted && r.render.advancedEmitted, '「去配置」/「高级模式 →」双 emit（FilmStudio 接线跳转）')
assert.equal(r.render.inspChips, 4)
ok(r.render.inspFilled, '灵感 chip 回填结构化长提示词（含 0-3s 时间轴分镜式）')
assert.equal(r.render.worksCards, 4)
ok(true, '作品瀑布流 4 卡（预存 1 + 会话生成 3）')
ok(r.render.statusLabels.some((s) => s.includes('已完成')) && r.render.statusLabels.some((s) => s.includes('失败')),
  '状态标签：已完成 / 失败（生成中态在 ② 轮询段覆盖）')
assert.equal(r.render.playHttpUrl, 'https://cdn.example.test/w.mp4')
ok(true, '点击装载播放器：http(s) URL 直连形态')
assert.equal(r.render.playDataUrl, 'data:video/mp4;base64,AAAA')
ok(true, '点击装载播放器：产物路径 → data URL 信封形态')
assert.equal(r.render.failedCardText, '渠道 502 Bad Gateway')
ok(true, '失败卡红字：error 首行（title 悬浮看全文）')
assert.equal(r.render.copiedFeedback, '已复制')
ok(true, '复制提示词 → 「✓ 已复制」短时反馈')
ok(r.render.refillSwitched, '「以此再生成」→ 回创作页并回填提示词')

// —— 源码契约断言（FilmStudio 为应用根组件，宿主桥依赖重不宜整挂——照
//    toast-smoke ⑦ 先例以源码契约防接线回归）+ i18n ×4 键齐平 ——
console.log('⑤ FilmStudio 接线契约 + i18n ×4 键齐平')
{
  const src = readFileSync(join(appRoot, 'src/FilmStudio.vue'), 'utf8')
  ok(src.includes("ref<'shotgen' | 'list' | 'studio'>('shotgen')"), "默认首屏 mode='shotgen'（三态：shotgen/list/studio）")
  ok(src.includes('<ShotGen') && src.includes('@advanced="goShotgenAdvanced"') && src.includes('@go-models="goShotgenModels"'),
    'ShotGen 挂载 + 高级模式/去配置事件接线')
  ok(src.includes('provide(SHOTGEN_ENGINE_KEY, shotgenEngine)'), '引擎 provide（跨模式切换存活——切高级模式轮询不中断）')
  ok(src.includes('shotgenEngine.start()') && src.includes('shotgenEngine.dispose()'),
    'onMounted start / onUnmounted dispose（作品流首拉 + 计时器释放）')
  ok(src.includes('mode.value = studioReturnMode.value'), '工作室返回按来路（shotgen 去配置进模型设置 → 返回回短片生成）')
  const shotgenSrc = readFileSync(join(appRoot, 'src/flow/ShotGen.vue'), 'utf8')
  ok(!shotgenSrc.includes('createShotgenEngine(') && shotgenSrc.includes('useShotgenEngine()'),
    'ShotGen 经 inject 消费引擎（不自建——模式切换不丢状态）')
  ok(!shotgenSrc.includes('addTracked') && !shotgenSrc.includes('trackFilmTask') && !shotgenSrc.includes('toast.'),
    'ShotGen 页零任务中心/零全局 toast 调用（静默口径在引擎层成立）')

  const locales = ['zh-CN', 'en-US', 'ja-JP', 'zh-TW'].map((l) => {
    const j = JSON.parse(readFileSync(join(appRoot, `src/i18n/${l}.json`), 'utf8'))
    return { l, keys: Object.keys(j.shotgen ?? {}).sort() }
  })
  assert.equal(locales[0].keys.length, 60)
  ok(true, 'i18n shotgen.* 60 键（zh-CN）')
  for (const { l, keys } of locales.slice(1)) {
    assert.deepEqual(keys, locales[0].keys)
  }
  ok(true, '×4 语言键集齐平（en-US / ja-JP / zh-TW 与 zh-CN 同 60 键）')
}

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（短片生成 ShotGen happy-dom 冒烟）`)
