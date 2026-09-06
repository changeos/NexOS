// =============================================================================
// previewEngine.ts —— 预览播放引擎（v0.1.35 前端件，纯应用侧）。
//
// 参照剪映/PR 的 Program Monitor：把分散的镜头产物（shot-N.png / shot-N.mp4 /
// line-N.mp3 / bgm.mp3 / final.mp4）编排成「可播放的时间线」：
//
//   storyboard 模式（分镜预览）：
//     - 按各镜头 duration_secs 顺序排段（缺省 4s，与 TimelineTracks 同口径）；
//     - 段时钟 = requestAnimationFrame 匀速推进 playheadSec；
//     - 该镜头有 shot-N.mp4 → 复用单个 <video> 切 src 播该段（段到时切下镜头；
//       视频短于段时长时保持末帧等时钟）；
//     - 无视频有分镜图 → <img> + Ken Burns 缓动（CSS transform scale/translate，
//       方向按镜头号轮换 4 向：放大/缩小/左移/右移）营造「准视频」感；
//     - 无图 → 黑底 + desc 大字占位。
//   final 模式（成片预览）：final.mp4 整片播（视频 currentTime 即播放头，
//     ended → 停 + 回起点）。
//
// 音频同步：当前镜头有 line-N.mp3 → <audio> 从段起始播（seek 时
// currentTime=段内偏移）；bgm.mp3 存在 → 循环低音量混播（0.35，对齐合成口径）；
// mute 一键全静。
//
// 播放头双向联动：监视器（PreviewMonitor.vue）与时间轴（TimelineTracks.vue）
// 经 provide/inject 共享本引擎实例（PREVIEW_ENGINE_KEY）——时间轴点击/拖动 →
// seek()；引擎播放 → playheadSec 响应式驱动时间轴头移动。
//
// 素材策略：懒加载（files/download b64 信封 → data URL，loader 由 FilmStudio
// 注入）；预加载当前 ±1 镜头的 png/mp3；视频段按需加载（复用单 video 元素）。
//
// 纯函数（buildSegments / segmentIndexAt / clampSec / kenBurnsClass 等）独立
// 导出——scripts/preview-smoke.mjs 用 happy-dom 冒烟断言段映射/seek/边界/
// Ken Burns 轮换（媒体元素驱动也一并走真实代码路径）。
// =============================================================================
import { computed, inject, reactive, ref, watch, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import type { FilmShot } from './api'

/** 播放模式：storyboard 分镜预览 / final 成片整播。 */
export type PreviewMode = 'storyboard' | 'final'

/** 引擎消费的镜头输入（FilmShot 子集）。 */
export type PreviewShotInput = Pick<FilmShot, 'shot' | 'desc' | 'line' | 'duration_secs'>

/** 镜头缺省时长（duration_secs 缺失/非法时给标称 4s；与时间轴块宽同口径）。 */
export const FALLBACK_SECS = 4

/** 有效时长（>0 的 duration_secs，否则缺省）。 */
export function effSecs(s: PreviewShotInput): number {
  return typeof s.duration_secs === 'number' && s.duration_secs > 0 ? s.duration_secs : FALLBACK_SECS
}

/** 段画面形态：有视频 / 仅分镜图（Ken Burns）/ 黑底占位。 */
export type SegmentKind = 'video' | 'image' | 'placeholder'

/** 时间轴上一段（镜头的落位与画面/音频形态）。 */
export interface PreviewSegment {
  /** 段序（0 起，即 shots 数组下标）。 */
  index: number
  /** 镜头号（1 起，与产物文件名 shot-N 对应）。 */
  shot: number
  /** 段起始秒（前序镜头 duration 累计）。 */
  start: number
  /** 段时长秒（effSecs）。 */
  secs: number
  /** 段结束秒（start + secs）。 */
  end: number
  kind: SegmentKind
  hasImage: boolean
  hasVoice: boolean
  /** 字幕文本（line 优先，desc 兜底；皆空=''）。 */
  text: string
}

/**
 * 镜头 → 段映射（纯函数）：按 duration_secs 顺序无缝排布；kind 按产物形态
 * （mp4 > png > placeholder）；hasVoice 按 line-N.mp3。
 */
export function buildSegments(
  shots: PreviewShotInput[],
  hasArtifact: (name: string) => boolean,
): PreviewSegment[] {
  let acc = 0
  return shots.map((s, index) => {
    const secs = effSecs(s)
    const hasVideo = hasArtifact(`shot-${s.shot}.mp4`)
    const hasImage = hasArtifact(`shot-${s.shot}.png`)
    const seg: PreviewSegment = {
      index,
      shot: s.shot,
      start: acc,
      secs,
      end: acc + secs,
      kind: hasVideo ? 'video' : hasImage ? 'image' : 'placeholder',
      hasImage,
      hasVoice: hasArtifact(`line-${s.shot}.mp3`),
      text: (s.line ?? '').trim() || (s.desc ?? '').trim(),
    }
    acc += secs
    return seg
  })
}

/**
 * 播放头 → 段序（纯函数）：[start, end) 左闭右开；sec 超总时长夹到最后一段
 * （停机态播放头停在末尾仍显示末镜头）；空表返回 -1。
 */
export function segmentIndexAt(segments: PreviewSegment[], sec: number): number {
  if (!segments.length) return -1
  if (sec < segments[0].start) return 0
  for (let i = segments.length - 1; i >= 0; i--) {
    if (sec >= segments[i].start) return i
  }
  return 0
}

/** 播放头夹取（纯函数）：[0, total]。 */
export function clampSec(sec: number, total: number): number {
  if (!Number.isFinite(sec) || sec < 0) return 0
  return total > 0 ? Math.min(sec, total) : 0
}

/** Ken Burns 方向池（与 PreviewMonitor.vue 的 @keyframes 一一对应）。 */
export const KEN_BURNS_CLASSES = ['kb-in', 'kb-out', 'kb-pan-l', 'kb-pan-r'] as const
export type KenBurnsClass = (typeof KEN_BURNS_CLASSES)[number]

/** Ken Burns 方向轮换（纯函数）：按镜头号 4 向循环（同镜头恒定，跨镜头交替）。 */
export function kenBurnsClass(shot: number): KenBurnsClass {
  const n = KEN_BURNS_CLASSES.length
  const idx = ((shot - 1) % n + n) % n
  return KEN_BURNS_CLASSES[idx]
}

/** BGM 混播音量（与后端合成口径对齐：0.35）。 */
export const BGM_VOLUME = 0.35

/** 产物文件名 helpers（loader 契约 = 项目产物目录下的文件名）。 */
export function shotImageName(shot: number): string {
  return `shot-${shot}.png`
}
export function shotVideoName(shot: number): string {
  return `shot-${shot}.mp4`
}
export function shotVoiceName(shot: number): string {
  return `line-${shot}.mp3`
}
export const BGM_NAME = 'bgm.mp3'
export const FINAL_NAME = 'final.mp4'

/** 引擎持有/驱动的媒体元素集（监视器组件 attach；引擎只碰这些属性/方法）。 */
interface MediaSet {
  video: HTMLVideoElement | null
  voice: HTMLAudioElement | null
  bgm: HTMLAudioElement | null
  final: HTMLVideoElement | null
}

/** 引擎构造参数。 */
export interface PreviewEngineOptions {
  /** 产物文件名 → data URL（files/download b64 信封；FilmStudio 注入项目 dir）。 */
  loader: (filename: string) => Promise<string>
}

/** 引擎实例面（状态 + 派生 + 动作 + 媒体挂载）。 */
export interface PreviewEngine {
  // —— 状态 ——
  readonly mode: Ref<PreviewMode>
  readonly playheadSec: Ref<number>
  readonly playing: Ref<boolean>
  readonly mute: Ref<boolean>
  readonly ccOn: Ref<boolean>
  readonly ratio: Ref<string>
  readonly finalAvailable: Ref<boolean>
  // —— 派生（storyboard）——
  readonly segments: ComputedRef<PreviewSegment[]>
  readonly totalSecs: ComputedRef<number>
  readonly currentSegIndex: ComputedRef<number>
  readonly currentSeg: ComputedRef<PreviewSegment | null>
  readonly inSegOffset: ComputedRef<number>
  // —— 派生（final）——
  readonly finalDurationSecs: Ref<number>
  /** final 模式装载的产物文件名（默认 final.mp4；合成页版本预览可指 dist/final-v*.mp4）。 */
  readonly finalName: Ref<string>
  // —— 懒加载素材 URL（shot 号 → data URL；reactive 供模板直取）——
  readonly imageUrls: Record<number, string>
  readonly voiceUrls: Record<number, string>
  readonly videoUrls: Record<number, string>
  readonly finalUrl: Ref<string>
  readonly bgmUrl: Ref<string>
  readonly bgmAvailable: Ref<boolean>
  // —— 动作 ——
  setSources(input: {
    shots: PreviewShotInput[]
    artifactNames: Iterable<string>
    ratio: string
    finalAvailable: boolean
    bgmAvailable: boolean
  }): void
  preloadAround(index: number): void
  play(): void
  pause(): void
  togglePlay(): void
  seek(sec: number): void
  prevShot(): void
  nextShot(): void
  setMode(m: PreviewMode): void
  /** 指定 final 模式装载的文件（相对项目目录；'' 回默认 final.mp4）。 */
  setFinalName(name: string): void
  /** 时钟推进（rAF 循环内部同款；冒烟测试直接调）。 */
  advance(dtSec: number): void
  // —— 媒体挂载 ——
  attachVideo(el: HTMLVideoElement | null): void
  attachShotAudio(el: HTMLAudioElement | null): void
  attachBgm(el: HTMLAudioElement | null): void
  attachFinalVideo(el: HTMLVideoElement | null): void
  dispose(): void
}

/** provide/inject 键（FilmStudio provide；PreviewMonitor / TimelineTracks inject）。 */
export const PREVIEW_ENGINE_KEY: InjectionKey<PreviewEngine> = Symbol('film-preview-engine')

/** 便捷注入（缺引擎的宿主环境返回 null，播放头相关 UI 优雅降级）。 */
export function usePreviewEngine(): PreviewEngine | null {
  return inject(PREVIEW_ENGINE_KEY, null)
}

/** 安全 play（自动播放策略/未实现媒体栈拒绝时吞错——画面照走时钟）。 */
function safePlay(el: HTMLMediaElement): void {
  try {
    const p = el.play() as unknown as Promise<void> | undefined
    if (p && typeof p.catch === 'function') p.catch(() => undefined)
  } catch {
    /* happy-dom / 自动播放拦截：静默 */
  }
}

/** seek 目标时间夹取（媒体时长未知时原样；已知时留 50ms 尾防 ended 抖动）。 */
function clampMediaTime(sec: number, duration: number): number {
  if (!Number.isFinite(duration) || duration <= 0) return Math.max(0, sec)
  return Math.min(Math.max(0, sec), Math.max(0, duration - 0.05))
}

/**
 * 创建预览播放引擎（每工作室会话一个；FilmStudio 在 setup 建 + provide，
 * onUnmounted dispose）。媒体元素由监视器 attach，引擎 watch 自身状态驱动：
 * 切段（src 切换 + 精确 seek）/ 播放暂停 / 静音 / 模式切换。
 */
export function createPreviewEngine(opts: PreviewEngineOptions): PreviewEngine {
  const loader = opts.loader

  // —— 状态 ——
  const mode = ref<PreviewMode>('storyboard')
  const playheadSec = ref(0)
  const playing = ref(false)
  const mute = ref(false)
  const ccOn = ref(true)
  const ratio = ref('16:9')
  const finalAvailable = ref(false)
  const bgmAvailable = ref(false)

  // —— 源（shots + 产物名集合；setSources 更新）——
  const shotsRef = ref<PreviewShotInput[]>([])
  const artNames = ref<Set<string>>(new Set())

  // —— 派生 ——
  const segments = computed(() => buildSegments(shotsRef.value, (n) => artNames.value.has(n)))
  const storyboardTotal = computed(() =>
    segments.value.reduce((sum, s) => sum + s.secs, 0),
  )
  const totalSecs = computed(() =>
    mode.value === 'final' ? finalDurationSecs.value : storyboardTotal.value,
  )
  const currentSegIndex = computed(() => segmentIndexAt(segments.value, playheadSec.value))
  const currentSeg = computed(() => segments.value[currentSegIndex.value] ?? null)
  const inSegOffset = computed(() => {
    const seg = currentSeg.value
    return seg ? Math.min(Math.max(0, playheadSec.value - seg.start), seg.secs) : 0
  })

  // —— final 模式 ——
  const finalDurationSecs = ref(0)
  const finalUrl = ref('')
  const bgmUrl = ref('')
  /** final 模式装载的文件名（合成页版本预览可指 dist/final-v*.mp4）。 */
  const finalName = ref<string>(FINAL_NAME)

  /**
   * 装载 final 文件（懒）：默认 final.mp4 需 finalAvailable；显式版本名
   * （setFinalName 指定）直接尝试装载（loader 失败静默保持黑底）。
   * URL 就绪且已在 final 模式 → 即刻播（「预览成片 = 即刻播」口径）。
   */
  function loadFinal(): void {
    if (finalName.value === FINAL_NAME && !finalAvailable.value) return
    if (inFlight.has(finalName.value)) return
    inFlight.add(finalName.value)
    void loader(finalName.value)
      .then((u) => {
        finalUrl.value = u
        if (mode.value === 'final') {
          if (!playing.value) playing.value = true
          syncMedia()
        }
      })
      .catch(() => undefined)
      .finally(() => inFlight.delete(finalName.value))
  }

  /** 指定 final 装载文件（版本预览）：清旧 URL/时长/播放头，final 模式中立即重载。 */
  function setFinalName(name: string): void {
    const next = (name ?? '').trim() || FINAL_NAME
    if (finalName.value === next) return
    finalName.value = next
    finalUrl.value = ''
    finalDurationSecs.value = 0
    playheadSec.value = 0
    if (mode.value === 'final') {
      if (!finalUrl.value) playing.value = false
      loadFinal()
      syncMedia()
    }
  }

  // —— 懒加载素材 URL 缓存（shot → data URL）——
  const imageUrls = reactive<Record<number, string>>({})
  const voiceUrls = reactive<Record<number, string>>({})
  const videoUrls = reactive<Record<number, string>>({})
  /** 在途加载去重（文件名集合）。 */
  const inFlight = new Set<string>()

  function loadUrl(filename: string, into: (url: string) => void): void {
    if (inFlight.has(filename)) return
    inFlight.add(filename)
    void loader(filename)
      .then((url) => into(url))
      .catch(() => undefined)
      .finally(() => inFlight.delete(filename))
  }

  /** 预加载当前 ±1 镜头的 png/mp3（图片进画面/Ken Burns、音频即点即响）。 */
  function preloadAround(index: number): void {
    const segs = segments.value
    for (const i of [index - 1, index, index + 1]) {
      const seg = segs[i]
      if (!seg) continue
      if (seg.hasImage && !imageUrls[seg.shot]) {
        loadUrl(shotImageName(seg.shot), (u) => {
          imageUrls[seg.shot] = u
        })
      }
      if (seg.hasVoice && !voiceUrls[seg.shot]) {
        loadUrl(shotVoiceName(seg.shot), (u) => {
          voiceUrls[seg.shot] = u
        })
      }
    }
  }

  // —— 媒体元素 ——
  const media: MediaSet = { video: null, voice: null, bgm: null, final: null }
  /** 当前 storyboard video 元素上装载的镜头号（-1 = 空；用于切 src 判定）。 */
  const currentVideoShot = ref(-1)

  function stopStoryboardMedia(): void {
    media.video?.pause()
    media.voice?.pause()
    media.bgm?.pause()
    if (media.video) currentVideoShot.value = -1
  }

  function stopFinalMedia(): void {
    media.final?.pause()
  }

  /**
   * storyboard 媒体同步（切段 / seek / 播放态变化时调用）：
   * - video：有视频段且 URL 就绪 → 切 src + currentTime=段内偏移 + 按播放态
   *   play/pause；无视频段 → 暂停让位给 img/占位层；
   * - voice：播放中且有配音 → 从段内偏移起播；否则暂停；
   * - bgm：播放中且 bgm.mp3 → 循环 0.35 音量混播；否则暂停。
   */
  function syncStoryboardMedia(): void {
    const seg = currentSeg.value
    const v = media.video
    if (v) {
      const url = seg && seg.kind === 'video' ? videoUrls[seg.shot] : ''
      if (url && seg) {
        if (currentVideoShot.value !== seg.shot) {
          v.src = url
          currentVideoShot.value = seg.shot
        }
        const target = clampMediaTime(inSegOffset.value, v.duration)
        if (Math.abs(v.currentTime - target) > 0.3) v.currentTime = target
        if (playing.value) safePlay(v)
        else v.pause()
      } else if (currentVideoShot.value !== -1) {
        v.pause()
        currentVideoShot.value = -1
      }
      v.muted = mute.value
    }
    const a = media.voice
    if (a) {
      const url = seg?.hasVoice ? voiceUrls[seg.shot] : ''
      if (url && seg && playing.value) {
        const offset = clampMediaTime(inSegOffset.value, a.duration)
        if (!(a.duration && offset >= a.duration - 0.05)) {
          a.src = url
          a.currentTime = offset
          safePlay(a)
        } else {
          a.pause() // 段内偏移已越过配音长度（seek 到台词之后）
        }
      } else {
        a.pause()
      }
      a.muted = mute.value
    }
    const b = media.bgm
    if (b) {
      if (playing.value && bgmUrl.value) {
        if (!b.src) b.src = bgmUrl.value
        if (b.paused) safePlay(b)
      } else {
        b.pause()
      }
      b.muted = mute.value
      b.volume = BGM_VOLUME
    }
  }

  /** final 媒体同步（src 就绪时装载；播放头/播放态跟 video）。 */
  function syncFinalMedia(): void {
    const f = media.final
    if (!f) return
    if (finalUrl.value) {
      if (f.src !== finalUrl.value) f.src = finalUrl.value
      if (Math.abs(f.currentTime - playheadSec.value) > 0.3) f.currentTime = playheadSec.value
      if (playing.value) safePlay(f)
      else f.pause()
    } else {
      f.pause()
    }
    f.muted = mute.value
  }

  function syncMedia(): void {
    if (mode.value === 'final') syncFinalMedia()
    else syncStoryboardMedia()
  }

  // —— 时钟（storyboard：rAF 匀速；final：video timeupdate 驱动）——
  let rafId = 0
  let lastTs = 0

  function advance(dtSec: number): void {
    if (mode.value !== 'storyboard' || !playing.value) return
    const total = storyboardTotal.value
    const next = playheadSec.value + Math.max(0, dtSec)
    if (total <= 0) {
      playheadSec.value = 0
      playing.value = false
      syncMedia()
      return
    }
    if (next >= total) {
      // 边界：播到总时长末尾自动停 + 回到起点
      playheadSec.value = 0
      playing.value = false
      stopStoryboardMedia()
      syncMedia()
      return
    }
    playheadSec.value = next
  }

  function loop(ts: number): void {
    if (!playing.value || mode.value !== 'storyboard') {
      rafId = 0
      lastTs = 0
      return
    }
    if (lastTs) advance((ts - lastTs) / 1000)
    lastTs = ts
    rafId = requestAnimationFrame(loop)
  }

  function startClock(): void {
    if (mode.value !== 'storyboard' || rafId) return
    lastTs = 0
    rafId = requestAnimationFrame(loop)
  }

  function stopClock(): void {
    if (rafId) cancelAnimationFrame(rafId)
    rafId = 0
    lastTs = 0
  }

  // —— 动作 ——
  function play(): void {
    if (mode.value === 'final' && !finalUrl.value) return
    // 末尾再按播放 → 从头开始
    if (playheadSec.value >= totalSecs.value - 0.001) playheadSec.value = 0
    playing.value = true
    if (mode.value === 'storyboard') startClock()
    syncMedia()
  }

  function pause(): void {
    playing.value = false
    stopClock()
    syncMedia()
  }

  function togglePlay(): void {
    if (playing.value) pause()
    else play()
  }

  /** seek：画面 + 音频各自跳到对应偏移（storyboard=段内偏移；final=currentTime）。 */
  function seek(sec: number): void {
    const total = totalSecs.value
    playheadSec.value = clampSec(sec, total)
    syncMedia()
  }

  /** ⏮ 上一镜头：段首附近（≤0.25s）再退一段，否则回本段头。 */
  function prevShot(): void {
    const segs = segments.value
    const idx = currentSegIndex.value
    if (!segs.length || idx < 0) return
    const back = inSegOffset.value <= 0.25 && idx > 0 ? idx - 1 : idx
    seek(segs[back].start)
  }

  /** ⏭ 下一镜头：下一段头；已在末段 → 到总时长并停（边界）。 */
  function nextShot(): void {
    const segs = segments.value
    const idx = currentSegIndex.value
    if (!segs.length || idx < 0) return
    if (idx + 1 < segs.length) {
      seek(segs[idx + 1].start)
      return
    }
    seek(storyboardTotal.value)
    playing.value = false
    stopClock()
    syncMedia()
  }

  function setMode(m: PreviewMode): void {
    if (mode.value === m) return
    // 切模式：先停对面媒体，播放头归零，播放态保持语义（final 就绪才续播）
    if (m === 'final') {
      stopStoryboardMedia()
      stopClock()
      mode.value = 'final'
      playheadSec.value = 0
      if (!finalUrl.value) loadFinal()
      if (!playing.value && finalUrl.value) playing.value = true // 预览成片 = 即刻播
      else if (!finalUrl.value) playing.value = false
      syncMedia()
    } else {
      stopFinalMedia()
      mode.value = 'storyboard'
      playheadSec.value = 0
      playing.value = false
      syncMedia()
    }
  }

  /** 源同步（FilmStudio watch project 注入；生成新素材后重载段表并夹播放头）。 */
  function setSources(input: {
    shots: PreviewShotInput[]
    artifactNames: Iterable<string>
    ratio: string
    finalAvailable: boolean
    bgmAvailable: boolean
  }): void {
    shotsRef.value = input.shots.map((s) => ({
      shot: s.shot,
      desc: s.desc,
      line: s.line,
      duration_secs: s.duration_secs,
    }))
    artNames.value = new Set(input.artifactNames)
    ratio.value = input.ratio
    finalAvailable.value = input.finalAvailable
    bgmAvailable.value = input.bgmAvailable
    // BGM 出现时预取（循环混播即点即响）
    if (input.bgmAvailable && !bgmUrl.value && !inFlight.has(BGM_NAME)) {
      loadUrl(BGM_NAME, (u) => {
        bgmUrl.value = u
        if (playing.value && mode.value === 'storyboard') syncMedia()
      })
    }
    // 段表变化：夹播放头 + 保温当前段素材
    playheadSec.value = clampSec(playheadSec.value, storyboardTotal.value)
    preloadAround(currentSegIndex.value)
    // 当前段若变成视频段而 URL 未载 → 按需装载
    ensureCurrentVideo()
    syncMedia()
  }

  /** 视频段按需装载（复用单 video 元素；装载完成仍在本段才上屏）。 */
  function ensureCurrentVideo(): void {
    const seg = currentSeg.value
    if (!seg || seg.kind !== 'video' || videoUrls[seg.shot]) return
    const shotNo = seg.shot
    loadUrl(shotVideoName(shotNo), (u) => {
      videoUrls[shotNo] = u
      const cur = currentSeg.value
      if (cur && cur.shot === shotNo && mode.value === 'storyboard') syncMedia()
    })
  }

  // —— watcher：切段驱动（进入新镜头段 = 换画面/起音频 + 预热 ±1）——
  watch(currentSegIndex, (idx) => {
    if (mode.value !== 'storyboard' || idx < 0) return
    preloadAround(idx)
    ensureCurrentVideo()
    syncStoryboardMedia()
  })

  watch(playing, () => {
    if (playing.value && mode.value === 'storyboard') startClock()
  })

  watch(mute, (m) => {
    // 静音直接作用于全部已挂载媒体（含当前模式外的元素——切模式即静）
    for (const el of [media.video, media.voice, media.bgm, media.final]) {
      if (el) el.muted = m
    }
    syncMedia()
  })

  watch(
    () => mode.value,
    () => {
      if (mode.value === 'storyboard') stopFinalMedia()
    },
  )

  // —— 媒体挂载（监视器 mount/unmount 时注册/注销元素 + final 事件桥）——
  function onFinalTime(): void {
    const f = media.final
    if (!f || mode.value !== 'final' || !playing.value) return
    playheadSec.value = f.currentTime
  }
  function onFinalMeta(): void {
    const f = media.final
    if (f && Number.isFinite(f.duration)) finalDurationSecs.value = f.duration
  }
  function onFinalEnded(): void {
    if (mode.value !== 'final') return
    playheadSec.value = 0
    playing.value = false
    syncMedia()
  }

  function attachFinalVideo(el: HTMLVideoElement | null): void {
    const old = media.final
    if (old) {
      old.removeEventListener('timeupdate', onFinalTime)
      old.removeEventListener('loadedmetadata', onFinalMeta)
      old.removeEventListener('ended', onFinalEnded)
    }
    media.final = el
    if (el) {
      el.addEventListener('timeupdate', onFinalTime)
      el.addEventListener('loadedmetadata', onFinalMeta)
      el.addEventListener('ended', onFinalEnded)
      el.loop = false
      el.muted = mute.value
    }
    if (mode.value === 'final') syncFinalMedia()
  }

  function attachVideo(el: HTMLVideoElement | null): void {
    media.video = el
    if (el) {
      el.playsInline = true
      el.muted = mute.value
    }
    if (mode.value === 'storyboard') syncStoryboardMedia()
  }

  function attachShotAudio(el: HTMLAudioElement | null): void {
    media.voice = el
    if (el) el.muted = mute.value
    if (mode.value === 'storyboard') syncStoryboardMedia()
  }

  function attachBgm(el: HTMLAudioElement | null): void {
    media.bgm = el
    if (el) {
      el.loop = true
      el.volume = BGM_VOLUME
      el.muted = mute.value
    }
    if (mode.value === 'storyboard') syncStoryboardMedia()
  }

  function dispose(): void {
    stopClock()
    stopStoryboardMedia()
    stopFinalMedia()
    attachFinalVideo(null)
    media.video = null
    media.voice = null
    media.bgm = null
  }

  return {
    mode,
    playheadSec,
    playing,
    mute,
    ccOn,
    ratio,
    finalAvailable,
    segments,
    totalSecs,
    currentSegIndex,
    currentSeg,
    inSegOffset,
    finalDurationSecs,
    finalName,
    imageUrls,
    voiceUrls,
    videoUrls,
    finalUrl,
    bgmUrl,
    bgmAvailable,
    setSources,
    preloadAround,
    play,
    pause,
    togglePlay,
    seek,
    prevShot,
    nextShot,
    setMode,
    setFinalName,
    advance,
    attachVideo,
    attachShotAudio,
    attachBgm,
    attachFinalVideo,
    dispose,
  }
}
