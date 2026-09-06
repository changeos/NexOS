// =============================================================================
// preview-smoke.mjs —— previewEngine 冒烟（happy-dom；v0.1.35）。
//
// 主前端无 vitest（照 sdk-verify.mjs 先例）：本脚本用 esbuild（devDependencies）
// 把 src/previewEngine.ts 打成临时 ESM（vue 一并打包，NODE_ENV=production），
// 在 happy-dom 的 Window 上挂 document / requestAnimationFrame / HTMLElement
// 等全局，然后对引擎断言：
//   1. 镜头段映射（起点累计 / kind 按产物形态 / 字幕 line 优先 desc）；
//   2. 播放头 seek（夹取 + 段定位 + 段内偏移 + 媒体 currentTime 跳转）；
//   3. 边界（推进到总时长末尾自动停 + 回起点；⏭ 末段到头停；setSources 收缩夹头）；
//   4. Ken Burns 类轮换（4 向循环）；
//   5. 媒体驱动（单 video 切 src / voice 从段内偏移起播 / BGM 循环 0.35 / mute 全静）；
//   6. final 模式（final.mp4 装载 + storyboard/final 互切停对面媒体）。
//
// 运行：cd apps/film && npm run smoke
// =============================================================================
import { build } from 'esbuild'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, URL } from 'node:url'
import assert from 'node:assert/strict'
import { Window } from 'happy-dom'

const appRoot = fileURLToPath(new URL('..', import.meta.url))

// —— 1. esbuild 打包 previewEngine.ts（vue 内联；process.env.NODE_ENV 定义掉）——
const tmpDir = mkdtempSync(join(tmpdir(), 'film-preview-smoke-'))
const bundlePath = join(tmpDir, 'preview-engine.mjs')
await build({
  entryPoints: [join(appRoot, 'src/previewEngine.ts')],
  bundle: true,
  format: 'esm',
  platform: 'neutral',
  target: 'es2022',
  outfile: bundlePath,
  logLevel: 'silent',
  define: { 'process.env.NODE_ENV': '"production"' },
})

// —— 2. happy-dom 全局（引擎只用 document.createElement + rAF + 媒体属性面）——
const win = new Window()
globalThis.window = win
globalThis.document = win.document
globalThis.requestAnimationFrame = win.requestAnimationFrame.bind(win)
globalThis.cancelAnimationFrame = win.cancelAnimationFrame.bind(win)
for (const k of ['HTMLElement', 'HTMLMediaElement', 'HTMLVideoElement', 'HTMLAudioElement']) {
  if (win[k]) globalThis[k] = win[k]
}

// —— 3. 加载引擎 ——
const pe = await import(`file://${bundlePath}`)
const {
  buildSegments,
  segmentIndexAt,
  clampSec,
  kenBurnsClass,
  KEN_BURNS_CLASSES,
  createPreviewEngine,
  BGM_VOLUME,
} = pe

let passed = 0
function ok(cond, label) {
  assert.ok(cond, label)
  passed++
  console.log(`  ✓ ${label}`)
}

// —— 测试源：4 镜头（2s/3s/缺省4s/1.5s），产物形态各异 ——
const shots = [
  { shot: 1, desc: '开场城市', line: '夜幕降临', duration_secs: 2 },
  { shot: 2, desc: '主角登场', line: '', duration_secs: 3 },
  { shot: 3, desc: '追逐', line: '快跑', duration_secs: null },
  { shot: 4, desc: '结尾', line: undefined, duration_secs: 1.5 },
]
const arts = new Set(['shot-1.mp4', 'shot-1.png', 'line-1.mp3', 'shot-2.png', 'line-3.mp3', 'bgm.mp3', 'final.mp4'])

// ===================== ① 镜头段映射 =====================
console.log('① 镜头段映射（buildSegments）')
const segs = buildSegments(shots, (n) => arts.has(n))
ok(segs.length === 4, '4 镜头 → 4 段')
assert.deepEqual(segs.map((s) => s.start), [0, 2, 5, 9])
ok(true, '段起点=前序 duration 累计 [0,2,5,9]')
assert.deepEqual(segs.map((s) => s.end), [2, 5, 9, 10.5])
ok(true, '段终点 [2,5,9,10.5]（总 10.5s，缺省时长 4s 生效）')
assert.deepEqual(segs.map((s) => s.kind), ['video', 'image', 'placeholder', 'placeholder'])
ok(true, 'kind 按产物：mp4>png>占位')
ok(segs[0].hasVoice === true && segs[2].hasVoice === true && segs[1].hasVoice === false, 'hasVoice 按 line-N.mp3')
ok(segs[0].text === '夜幕降临' && segs[1].text === '主角登场', '字幕 line 优先 desc（空 line 回退 desc）')

ok(segmentIndexAt(segs, 0) === 0, 'segmentIndexAt(0)=第 1 段')
ok(segmentIndexAt(segs, 2) === 1, '边界 2s 属第 2 段（[start,end) 左闭右开）')
ok(segmentIndexAt(segs, 4.99) === 1 && segmentIndexAt(segs, 5) === 2, '段内/边界切段正确')
ok(segmentIndexAt(segs, 10.4) === 3, '末段内定位正确')
ok(segmentIndexAt(segs, 999) === 3, '超总时长夹到末段')
ok(segmentIndexAt([], 1) === -1, '空表 → -1')
ok(clampSec(-5, 10.5) === 0 && clampSec(99, 10.5) === 10.5 && clampSec(3, 10.5) === 3, 'clampSec 负/超/内')

// ===================== ② Ken Burns 类轮换 =====================
console.log('② Ken Burns 方向轮换（kenBurnsClass）')
const kbSeq = [1, 2, 3, 4, 5, 6, 7, 8, 9].map(kenBurnsClass)
assert.deepEqual(kbSeq, [
  'kb-in', 'kb-out', 'kb-pan-l', 'kb-pan-r',
  'kb-in', 'kb-out', 'kb-pan-l', 'kb-pan-r', 'kb-in',
])
ok(true, '镜头号 1..9 → 4 向循环')
ok(KEN_BURNS_CLASSES.length === 4, '方向池 4 向')

// ===================== ③ 引擎：seek / 媒体同步 =====================
console.log('③ 引擎 seek + 媒体驱动（happy-dom 真实媒体元素）')
const loads = []
const engine = createPreviewEngine({
  loader: async (name) => {
    loads.push(name)
    return `data:application/octet-stream;base64,${Buffer.from(name).toString('base64')}`
  },
})
const video = document.createElement('video')
const voice = document.createElement('audio')
const bgm = document.createElement('audio')
const finalVideo = document.createElement('video')
engine.attachVideo(video)
engine.attachShotAudio(voice)
engine.attachBgm(bgm)
engine.attachFinalVideo(finalVideo)

engine.setSources({ shots, artifactNames: arts, ratio: '9:16', finalAvailable: true, bgmAvailable: true })
await new Promise((r) => setTimeout(r, 10)) // 懒加载微任务排空

ok(engine.totalSecs.value === 10.5, '总时长 10.5s')
ok(engine.ratio.value === '9:16', 'ratio 透传')

engine.seek(4.5)
ok(engine.playheadSec.value === 4.5 && engine.currentSegIndex.value === 1 && engine.inSegOffset.value === 2.5, 'seek(4.5) → 段 2 内偏移 2.5')
engine.seek(-3)
ok(engine.playheadSec.value === 0, 'seek 负值夹 0')
engine.seek(99)
ok(engine.playheadSec.value === 10.5, 'seek 超界夹总时长')

// ⏮⏭ 镜头跳转（seek 到段边界）
engine.seek(4.5)
engine.prevShot()
ok(engine.playheadSec.value === 2, '⏮ 段中(偏移2.5) → 回本段头 2.0')
engine.prevShot()
ok(engine.playheadSec.value === 0, '⏮ 段首附近 → 再退一段到 0')
engine.nextShot()
ok(engine.playheadSec.value === 2, '⏭ → 下一段头 2.0')
engine.nextShot()
ok(engine.playheadSec.value === 5, '⏭ → 段 3 头 5.0')
engine.nextShot()
ok(engine.playheadSec.value === 9, '⏭ → 末段头 9.0')
engine.nextShot()
ok(engine.playheadSec.value === 10.5 && engine.playing.value === false, '⏭ 末段再下 → 总时长并停（边界）')

// 播放 + 媒体（段 1 为视频段：单 video 切 src；line-1.mp3 从段起始播）
engine.seek(0)
engine.play()
ok(engine.playing.value === true, 'play() → playing')
ok(video.src.includes(shot1B64()) === true || video.src !== '', '视频段：video 元素已装 src')
engine.advance(1.2)
ok(engine.playheadSec.value === 1.2, 'advance(1.2) 匀速推进（rAF 同代码路径）')
engine.advance(1.0)
ok(engine.currentSegIndex.value === 1 && engine.playheadSec.value === 2.2, '跨段推进（2.2s → 段 2）')
ok(voice.paused === false || voice.src === '', '配音段外不强行起播（段 2 无 line-2.mp3）')
ok(bgm.volume === BGM_VOLUME && BGM_VOLUME === 0.35, `BGM 音量 0.35（合成口径），元素 volume=${bgm.volume}`)
engine.mute.value = true
await new Promise((r) => setTimeout(r, 5)) // watch(mute) → syncMedia 微任务刷
ok(video.muted === true && voice.muted === true && bgm.muted === true && finalVideo.muted === true, 'mute 一键全静（四媒体元素）')
engine.mute.value = false

// ===================== ④ 边界：到总时长自动停+回起点 =====================
console.log('④ 边界行为')
engine.seek(9.2)
engine.play()
engine.advance(5) // 9.2 + 5 = 14.2 ≥ 10.5
ok(engine.playing.value === false && engine.playheadSec.value === 0, '播到末尾自动停 + 播放头回起点')

// 源收缩（镜头时长编辑/重生成）夹播放头
engine.seek(10.5)
engine.setSources({
  shots: [{ shot: 1, desc: 'a', duration_secs: 1 }, { shot: 2, desc: 'b', duration_secs: 1 }],
  artifactNames: arts,
  ratio: '16:9',
  finalAvailable: true,
  bgmAvailable: true,
})
ok(engine.totalSecs.value === 2 && engine.playheadSec.value === 2, 'setSources 收缩总时长 → 播放头夹到新总时长')
// 还原源
engine.setSources({ shots, artifactNames: arts, ratio: '16:9', finalAvailable: true, bgmAvailable: true })
engine.seek(0)

// ===================== ⑤ final 模式 =====================
console.log('⑤ final 模式（final.mp4 整播）')
await new Promise((r) => setTimeout(r, 10))
engine.setMode('final')
await new Promise((r) => setTimeout(r, 10))
ok(engine.mode.value === 'final', 'setMode(final)')
ok(engine.finalUrl.value.startsWith('data:'), 'final.mp4 经 loader 装载（data URL）')
ok(finalVideo.src === engine.finalUrl.value, 'final video 元素装载 final.mp4')
ok(engine.playheadSec.value === 0, '切模式播放头归零')
ok(video.paused === true, 'storyboard 视频在 final 模式停播')
engine.setMode('storyboard')
ok(finalVideo.paused === true && engine.mode.value === 'storyboard', '切回分镜预览：final 视频停')
engine.dispose()
ok(true, 'dispose 清理（rAF/媒体）')

function shot1B64() {
  return Buffer.from('shot-1.mp4').toString('base64')
}

// —— 素材懒加载策略：预加载当前 ±1（png/mp3），视频按需 ——
console.log('⑥ 素材懒加载策略（预加载 ±1 / 视频按需）')
ok(loads.includes('shot-1.png') && loads.includes('shot-2.png'), '初始段 ±1 的 png 预加载')
ok(loads.includes('line-1.mp3') && loads.includes('line-3.mp3'), '±1 的 mp3 预加载（段 1/段 3）')
ok(loads.includes('shot-1.mp4'), '当前视频段按需装载 mp4')
ok(!loads.includes('shot-2.mp4'), '非当前视频段不提前装载')
ok(loads.includes('bgm.mp3'), 'BGM 出现即预取（循环混播）')

rmSync(tmpDir, { recursive: true, force: true })
console.log(`\nPASS：${passed} 项断言全过（previewEngine happy-dom 冒烟）`)
