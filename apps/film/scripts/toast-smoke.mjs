// =============================================================================
// toast-smoke.mjs —— 全局操作反馈系统冒烟（happy-dom；v0.1.11）。
//
// 照 flow-smoke.mjs 先例（主前端无 vitest）：
//   1. vite JS API 构建 scripts/toast-smoke-harness.ts（plugin-vue 编译 .vue）
//      → 临时 ESM；
//   2. happy-dom 全局 + 宿主桥 mock api（BGM 面 fixture）+ 观测面
//      globalThis.__FLOW_SMOKE__；
//   3. import 构建产物 → run() → node:assert 断言：
//      ① toast 单例 API：五档 push / 堆叠上限 4（新推旧出，新在顶）/
//         loading 双载（建 + id 更新进度文案）/ update 翻终态（id 失效重落）/
//         手动 dismiss / 时长档（success 4s / error 8s / loading 0）；
//      ② NxToast.vue 渲染：Teleport body / aria-live=polite / error role=alert /
//         五 kind 类名 / 标题+正文 / 关闭钮 / 动作钮（点击回调 + 自关）；
//      ③ TaskIndicator：空态不渲染 / ↻ N 计数徽章 + spinner / popover 展开 /
//         明细行（X/Y + 已耗时）/ 「查看全部任务」emit；
//      ④ taskOutcomeSummary 摘要解析（分镜 12 镜头 / 清理删 N 行 / 42 章 /
//         8 位人物 / 17 块 / output 兜底 / 解析不到空串）+ firstErrorLine 首行；
//      ⑤ 任务通知器闭环（真实 zh-CN i18n）：提交 loading（标题=stage 标签）→
//         进度文案「向量化中 3/17」→ 终态 success「完成：17 块」/ error 首行 +
//         「查看任务」动作回调；
//      ⑥ 保存类操作触发 toast：AudioPage 建条目 → toast.saved（POST 带 trigger）。
//
// 运行：cd apps/film && npm run smoke:toast
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
const tmpDir = mkdtempSync(join(tmpdir(), 'film-toast-smoke-'))
const bundlePath = join(tmpDir, 'toast-smoke.mjs')
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
      entry: join(appRoot, 'scripts/toast-smoke-harness.ts'),
      formats: ['es'],
      fileName: () => 'toast-smoke.mjs',
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

/** BGM 库 fixture（AudioPage 消费面）。 */
const BGM = [
  { track: 'bgm-001', trigger: 'global', mood: '温馨', file: 'bgm-001.mp3', duration_secs: 92 },
]

/** 观测面（mock api 调用记录）。 */
globalThis.__FLOW_SMOKE__ = { calls: [], viewSwitches: [] }

async function handle(method, path, body) {
  globalThis.__FLOW_SMOKE__.calls.push({ method, path, body })
  const base = '/api/v1/film/projects/p1'
  if (method === 'GET' && path === `${base}/audio/bgm`) return BGM
  if (method === 'POST' && path === `${base}/audio/bgm`) {
    return { track: 'bgm-003', trigger: body?.info?.trigger ?? '' }
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

console.log('① toast 单例 API（v0.1.11：模块级状态面）')
assert.equal(r.api.stackAfterSix.length, 4)
ok(true, '堆叠上限 4 条（连发 6 条只留 4）')
ok(r.api.stackNewestFirst, '新在顶（第 6 条最先——新推旧出）')
ok(r.api.loadingSticky, 'loading 不自动关（duration=0，等终态 update）')
ok(r.api.loadingPatch, 'toast.loading(id, msg) 就地更新进度文案（仍 loading 态）')
ok(r.api.updateToSuccess, 'toast.update(id, success) 翻终态（loading → success）')
ok(r.api.updateMissingIdRepush, 'update 到已失效 id → 重落一条（终态不丢）')
ok(r.api.dismissWorks, '手动 dismiss 移除')
assert.deepEqual(r.api.durations, { success: 4000, error: 8000, loading: 0 })
ok(true, '时长档：success 4s / error 8s（+手动关）/ loading 0')
ok(r.api.autoClosedAfter4s, 'success 4s 自动关（真实计时）')
ok(r.api.errorClosedManual, 'error 可手动提前关')

console.log('② NxToast.vue 渲染器（Teleport body 右上角堆叠）')
ok(r.render.teleported, 'Teleport 到 body（.nx-toasts 容器在 body 下）')
ok(r.render.ariaLive === 'polite', '容器 aria-live=polite（读屏播报）')
ok(r.render.roleError === 'alert', 'error 条 role=alert（断言级即时播报）')
assert.deepEqual(r.render.kindsRendered, ['loading', 'info', 'warning', 'error'])
ok(true, '四 kind 类名齐（loading/info/warning/error——新在顶；堆叠上限 4）')
ok(r.render.kindAfterUpdate === 'success', 'update 翻终态：同一 DOM 条目换 kind 类名（loading → success）')
ok(
  r.render.titleAndMsg.title === '向量化' && r.render.titleAndMsg.msg === '已提交，完成后通知',
  '标题（stage 标签）+ 正文两行结构',
)
ok(r.render.closeWorks, '关闭钮生效（× 移除一条）')
ok(r.render.actionButtonWorks, '动作钮「查看任务」：回调触发 + 点击后自关')

console.log('③ TaskIndicator 顶栏全局任务指示器')
ok(r.indicator.hiddenWhenEmpty, '无进行中任务 → 整体不渲染')
ok(r.indicator.badgeCount === '2' && r.indicator.spinClass, '↻ spinner + 计数徽章（N=2）')
ok(r.indicator.popoverOpens, '点击展开 NxPopover（.nx-pop__panel 出现）')
ok(r.indicator.rows.length === 2, '进行中任务列表 2 行（标签 + meta）')
ok(r.indicator.hasXy, '明细行带 X/Y 分块进度（3/17）')
ok(r.indicator.hasElapsed, '明细行带已耗时（mm:ss，1s tick 自刷）')
ok(r.indicator.viewAllEmitted, '「查看全部任务」→ view-all 事件（跳工作台任务中心）')

console.log('④ taskOutcomeSummary 终态摘要解析（纯函数）')
ok(r.summary.storyboard === '12 镜头', '分镜 →「12 镜头」（日志尾「12 个镜头」）')
ok(r.summary.clean === '删 1234 行', '清理 →「删 1234 行」（「删除 1234 行广告」）')
ok(r.summary.chapterize === '42 章', '分章 →「42 章」')
ok(r.summary.profile === '8 位人物', '人物梳理 →「8 位人物」')
ok(r.summary.embed === '17 块', '向量化 →「17 块」（计划行兜底；块优先于字）')
ok(r.summary.outputFallback === 'final-v3.mp4', '日志无产出 → output 路径 basename 兜底')
ok(r.summary.empty === '', '均解析不到 → 空串（纯标签通知，不造假）')
ok(r.summary.errorFirstLine === 'LLM 深清块 3/17 失败：上游超时', 'firstErrorLine 多行只取首行')

console.log('⑤ 任务通知器闭环（提交 loading → 进度 → 终态）')
ok(r.notifier.submittedTitle === '向量化' && r.notifier.submittedMsg === '已提交，完成后通知', '提交 → loading toast（标题=stage 中文标签）')
ok(r.notifier.progressMsg === '向量化中 3/17' && r.notifier.progressKindStillLoading, '轮询日志「块 3/17 完成」→ 进度文案「向量化中 3/17」（仍 loading 态）')
ok(r.notifier.terminalSuccessMsg === '完成：17 块' && r.notifier.terminalSuccessKind === 'success' && r.notifier.terminalSuccessTitle === '向量化', '终态 success →「完成：17 块」（同条翻终态，标题不变）')
ok(r.notifier.terminalErrorMsg === '失败：渠道 502 Bad Gateway' && r.notifier.terminalErrorTitle === '配音 · 镜头 2', '终态 error →「失败：」+ 错误首行（标签含关联镜头）')
ok(r.notifier.errorActionLabel === '查看任务', 'error toast 带「查看任务」动作钮')
ok(r.notifier.actionFired, '动作钮点击 → onViewTasks 回调（跳工作台任务中心）')

console.log('⑥ 保存类操作触发 toast（AudioPage 真挂载）')
ok(r.saveClass.savedToastShown, 'BGM 建条目成功 → toast.success（已保存）')
ok(r.saveClass.bgmPostBody?.info?.trigger === '开场', 'POST /audio/bgm 照常（info.trigger=开场；反馈层不影响请求面）')

// —— 源码契约断言（FilmStudio 为应用根组件，宿主桥依赖重不宜整挂——以源码
//    契约防通知闭环/指示器接线回归，照 flow-smoke ⑦ 先例）——
console.log('⑦ FilmStudio 接线契约（轮询通知 / 指示器挂载 / 源级互斥）')
{
  const src = readFileSync(join(appRoot, 'src/FilmStudio.vue'), 'utf8')
  ok(src.includes('taskNotifier.terminal(item.toastId'), 'pollTasks 终态 → taskNotifier.terminal（loading → success/error 闭环）')
  ok(src.includes('taskNotifier.progress(item.toastId'), 'pollTasks 轮询 → taskNotifier.progress（「块 X/Y」→ 进度文案刷新）')
  ok(src.includes('taskNotifier.submitted('), 'addTracked/trackFilmTask 提交 → loading toast（持有 toastId）')
  ok(src.includes('<NxToast />'), 'NxToast 挂应用根（桌面嵌入与 standalone 两载体同享）')
  ok((src.match(/<TaskIndicator /g) ?? []).length === 2, 'TaskIndicator 双挂载（工作室顶栏 + 大厅 head-extra——全部页面可见）')
  ok(!src.includes('film-toast'), '旧组件内固定浮层（film-toast）已移除（统一走 NxToast）')
  ok(src.includes("toast.success(t('toast.saved'))"), '保存类操作（镜头/角色/导出路径/绑定等）→ toast.success')
  const storySrc = readFileSync(join(appRoot, 'src/flow/StoryPage.vue'), 'utf8')
  ok(storySrc.includes('activeSourceTask') && storySrc.includes('storySourceBusyTip'), 'StoryPage 源级互斥（activeSourceTask + tooltip storySourceBusyTip）')
  ok(storySrc.includes('toast.warning(msg)'), '管线提交 409 → toast.warning（后端同源互斥文案透传）')
}

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（全局操作反馈系统 happy-dom 冒烟）`)
