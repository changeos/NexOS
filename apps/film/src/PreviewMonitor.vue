<script setup lang="ts">
// =============================================================================
// PreviewMonitor.vue —— 右侧栏可视化预览监视器（v0.1.35，参照剪映/PR 的
// Program Monitor）。
//
// 结构（自上而下）：
//   - 卡头：📺 预览监视器 + 模式徽章（分镜预览/成片）；
//   - 画面区（stage）：按项目 ratio 适配（六档预设泛化解析 a:b → 数值比例，
//     任意 "2.39:1" 形态均可），黑底 #000 圆角，容器内居中 letterbox；顶行叠加
//     镜头号徽章 + 时间码 mm:ss.f / 总时长；底部字幕条（当前镜头 line 优先
//     desc，白字黑描边，CC 可开关）；
//   - 播放控制条：⏮ 上一镜头 / ▶⏸ / ⏭ 下一镜头 + CC 开关 + 音量（静音切换）。
//
// 画面内容按 previewEngine 播放头位置渲染（引擎 provide/inject 注入）：
//   - 视频段：复用单个 <video>（引擎切 src + currentTime=段内偏移）；
//   - 图片段：<img> + Ken Burns 缓动（方向按镜头号轮换 4 向，暂停时冻结）；
//   - 无图段：黑底 + desc 大字占位；
//   - final 模式：final.mp4 整片播（独立 <video>，播放头=currentTime）。
//
// 媒体元素（storyboard video / voice audio / bgm audio / final video）由本组件
// 持有并 attach 给引擎驱动；卸载时 detach。⏮⏭ 同时 emit select-shot 联动
// 左侧镜头卡/面板选中。
// =============================================================================
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { kenBurnsClass, usePreviewEngine } from './previewEngine'
import { ratioValueOf } from './flow/flowTypes'

const emit = defineEmits<{
  (e: 'select-shot', shot: number): void
}>()

const { t } = useI18n()

const engine = usePreviewEngine()

// —— 媒体元素（模板 ref；引擎 attach 驱动）——
const videoEl = ref<HTMLVideoElement | null>(null)
const voiceEl = ref<HTMLAudioElement | null>(null)
const bgmEl = ref<HTMLAudioElement | null>(null)
const finalVideoEl = ref<HTMLVideoElement | null>(null)

onMounted(() => {
  if (!engine) return
  engine.attachVideo(videoEl.value)
  engine.attachShotAudio(voiceEl.value)
  engine.attachBgm(bgmEl.value)
  engine.attachFinalVideo(finalVideoEl.value)
})

onUnmounted(() => {
  if (!engine) return
  engine.attachVideo(null)
  engine.attachShotAudio(null)
  engine.attachBgm(null)
  engine.attachFinalVideo(null)
})

// —— 状态捷径（engine 必在——FilmStudio provide；模板里少写 .value）——
const mode = computed(() => engine?.mode.value ?? 'storyboard')
const playing = computed(() => engine?.playing.value ?? false)
const playheadSec = computed(() => engine?.playheadSec.value ?? 0)
const totalSecs = computed(() => engine?.totalSecs.value ?? 0)
const currentSeg = computed(() => engine?.currentSeg.value ?? null)
const segments = computed(() => engine?.segments.value ?? [])
const ccOn = computed(() => engine?.ccOn.value ?? false)
const mute = computed(() => engine?.mute.value ?? false)
const finalUrl = computed(() => engine?.finalUrl.value ?? '')

/**
 * 画面比例 → stage 内联样式（泛化解析任意 `a:b` 比例——六档预设及未来档位
 * 通吃；非法/缺省按 16:9 letterbox）。宽画幅（比例 ≥1）宽撑满、竖画幅高撑满，
 * 反向由 max-width/max-height 兜底居中。
 */
const stageStyle = computed(() => {
  const v = ratioValueOf(engine?.ratio.value ?? '16:9') ?? 16 / 9
  return {
    aspectRatio: String(v),
    ...(v >= 1 ? { width: '100%' } : { height: '100%', width: 'auto' }),
  }
})

/**
 * 画面层形态（按播放头派生，非按产物清单——视频 URL 未就绪时回退图片层）：
 * video / image / placeholder / empty（无分镜）/ final（整片）。
 */
type StageKind = 'video' | 'image' | 'placeholder' | 'empty' | 'final'
const stageKind = computed<StageKind>(() => {
  if (mode.value === 'final') return 'final'
  const seg = currentSeg.value
  if (!seg) return 'empty'
  if (seg.kind === 'video' && engine?.videoUrls[seg.shot]) return 'video'
  if (seg.hasImage && engine?.imageUrls[seg.shot]) return 'image'
  return 'placeholder'
})

/** 当前字幕文本（line 优先 desc；CC 关或空则不渲染）。 */
const subtitleText = computed(() => currentSeg.value?.text ?? '')

/** Ken Burns 动画时长=段时长；暂停时冻结（animation-play-state）。 */
const kbStyle = computed(() => {
  const seg = currentSeg.value
  return {
    animationDuration: `${seg?.secs ?? 4}s`,
    animationPlayState: playing.value ? 'running' : 'paused',
  }
})

/** 时间码 mm:ss.f（一位小数）。 */
function fmtTimecode(sec: number): string {
  const s = Math.max(0, sec)
  const m = Math.floor(s / 60)
  const rest = s - m * 60
  const whole = Math.floor(rest)
  const tenth = Math.floor((rest - whole) * 10)
  return `${String(m).padStart(2, '0')}:${String(whole).padStart(2, '0')}.${tenth}`
}

// —— 播放控制（⏮⏭ 联动选中镜头：emit 给 FilmStudio）——
function togglePlay(): void {
  engine?.togglePlay()
}

function onPrevShot(): void {
  if (!engine) return
  engine.prevShot()
  const shot = engine.currentSeg.value?.shot
  if (shot) emit('select-shot', shot)
}

function onNextShot(): void {
  if (!engine) return
  engine.nextShot()
  const shot = engine.currentSeg.value?.shot
  if (shot) emit('select-shot', shot)
}

function toggleCc(): void {
  if (engine) engine.ccOn.value = !engine.ccOn.value
}

function toggleMute(): void {
  if (engine) engine.mute.value = !engine.mute.value
}
</script>

<template>
  <section class="card mon" :aria-label="t('film.previewMonitor')">
    <div class="col-head mon-head">
      <span>📺 {{ t('film.previewMonitor') }}</span>
      <span v-if="mode === 'final'" class="pill pill-blue mon-mode">
        🎬 {{ t('film.previewFinal') }}
      </span>
      <span class="muted small mono mon-count">{{ segments.length }}</span>
    </div>

    <!-- 画面区：ratio 适配（泛化 a:b 解析）+ 黑底圆角 + 居中 letterbox -->
    <div class="mon-body">
      <div class="mon-stage-wrap">
        <div class="mon-stage" :style="stageStyle">
          <!-- 分镜视频层（复用单元素，引擎切 src；final 模式隐藏但常驻保 ref） -->
          <video
            v-show="mode === 'storyboard' && stageKind === 'video'"
            ref="videoEl"
            class="mon-video"
            playsinline
            preload="auto"
          ></video>

          <!-- 成片层（final.mp4 整播；常驻元素，final 模式才显示） -->
          <video
            v-show="mode === 'final' && !!finalUrl"
            ref="finalVideoEl"
            class="mon-video"
            playsinline
            preload="auto"
          ></video>

          <!-- Ken Burns 分镜图层（方向按镜头号轮换；:key 换段重启动画） -->
          <img
            v-if="mode === 'storyboard' && stageKind === 'image' && currentSeg"
            :key="`kb-${currentSeg.shot}`"
            class="mon-img"
            :class="kenBurnsClass(currentSeg.shot)"
            :style="kbStyle"
            :src="engine?.imageUrls[currentSeg.shot]"
            :alt="currentSeg.text || t('film.previewMonitor')"
          >

          <!-- 黑底占位（无图镜头：desc 大字）/ 空态 / 成片未就绪 -->
          <div
            v-if="mode === 'storyboard' && stageKind === 'placeholder' && currentSeg"
            class="mon-ph"
          >
            <span class="mon-ph-shot mono">#{{ currentSeg.shot }}</span>
            <p class="mon-ph-text">{{ currentSeg.text || t('film.previewPlaceholder') }}</p>
          </div>
          <div v-if="mode === 'storyboard' && stageKind === 'empty'" class="mon-ph">
            <p class="mon-ph-text">{{ t('film.previewEmpty') }}</p>
          </div>
          <div v-if="mode === 'final' && !finalUrl" class="mon-ph">
            <p class="mon-ph-text">{{ t('film.previewFinalMissing') }}</p>
          </div>

          <!-- 顶行叠加：镜头号徽章 + 时间码 mm:ss.f / 总 -->
          <div v-if="mode === 'storyboard' && currentSeg" class="mon-topline">
            <span class="mon-badge mono">#{{ currentSeg.shot }}</span>
          </div>
          <div class="mon-timecode mono">
            {{ fmtTimecode(playheadSec) }} / {{ fmtTimecode(totalSecs) }}
          </div>

          <!-- 底部字幕条（line 优先 desc；CC 开关控制） -->
          <div
            v-if="mode === 'storyboard' && ccOn && subtitleText"
            class="mon-cc"
          >{{ subtitleText }}</div>
        </div>
      </div>

      <!-- 播放控制条：⏮ ▶/⏸ ⏭ + CC + 音量 -->
      <div class="mon-controls">
        <button
          class="mon-btn"
          type="button"
          :title="t('film.playerPrev')"
          :aria-label="t('film.playerPrev')"
          :disabled="!segments.length || mode === 'final'"
          @click="onPrevShot"
        >⏮</button>
        <button
          class="mon-btn mon-play"
          type="button"
          :title="playing ? t('film.playerPause') : t('film.playerPlay')"
          :aria-label="playing ? t('film.playerPause') : t('film.playerPlay')"
          :disabled="mode === 'final' ? !finalUrl : !segments.length"
          @click="togglePlay"
        >{{ playing ? '⏸' : '▶' }}</button>
        <button
          class="mon-btn"
          type="button"
          :title="t('film.playerNext')"
          :aria-label="t('film.playerNext')"
          :disabled="!segments.length || mode === 'final'"
          @click="onNextShot"
        >⏭</button>

        <span class="mon-ctrl-gap"></span>

        <button
          class="mon-btn mon-cc-btn"
          :class="{ on: ccOn }"
          type="button"
          :title="t('film.playerCc')"
          :aria-label="t('film.playerCc')"
          :aria-pressed="ccOn"
          @click="toggleCc"
        ><span class="mono">CC</span></button>
        <button
          class="mon-btn"
          type="button"
          :title="mute ? t('film.playerUnmute') : t('film.playerMute')"
          :aria-label="mute ? t('film.playerUnmute') : t('film.playerMute')"
          :aria-pressed="mute"
          @click="toggleMute"
        >{{ mute ? '🔇' : '🔊' }}</button>
      </div>
    </div>

    <!-- 隐藏音频（镜头配音 + BGM 循环混播；引擎驱动） -->
    <audio ref="voiceEl" preload="auto" class="mon-hidden"></audio>
    <audio ref="bgmEl" loop preload="auto" class="mon-hidden"></audio>
  </section>
</template>

<style scoped>
/* ===================== 卡骨架（flex 纵列；零 vh） ===================== */
.mon {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.mon-head { gap: 8px; }
.mon-mode { flex-shrink: 0; }
.mon-count { margin-left: auto; }
.mon-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  padding: 10px 12px 12px;
  gap: 10px;
}

/* —— 画面区：容器居中 letterbox，stage 按比例适配 —— */
.mon-stage-wrap {
  flex: 1;
  min-height: 0;
  display: flex;
  align-items: center;
  justify-content: center;
  overflow: hidden;
}
.mon-stage {
  position: relative;
  background: #000;
  border-radius: 10px;
  overflow: hidden;
  max-width: 100%;
  max-height: 100%;
  box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.06);
}
/* 画幅适配泛化（v0.1.37）：aspect-ratio 与宽/高撑满方向由 stageStyle 内联
 * 给出（任意 a:b 比例——16:9 / 9:16 / 1:1 / 2.39:1 / 1.85:1 / 4:3 通吃），
 * 超限由 max-width/max-height 兜底居中 letterbox。 */

/* 最小可用高度（窄屏堆叠时监视器不至于被压扁） */
.mon-stage-wrap { min-height: 160px; }

/* —— 画面层 —— */
.mon-video {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: contain;
  background: #000;
}
.mon-img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: cover;
  will-change: transform;
  animation-timing-function: linear;
  animation-fill-mode: forwards;
}

/* Ken Burns 缓动（方向按镜头号轮换；暂停时 animation-play-state 冻结） */
.mon-img.kb-in { animation-name: monKbIn; }
.mon-img.kb-out { animation-name: monKbOut; }
.mon-img.kb-pan-l { animation-name: monKbPanL; }
.mon-img.kb-pan-r { animation-name: monKbPanR; }
@keyframes monKbIn {
  from { transform: scale(1); }
  to { transform: scale(1.14); }
}
@keyframes monKbOut {
  from { transform: scale(1.14); }
  to { transform: scale(1); }
}
@keyframes monKbPanL {
  from { transform: scale(1.12) translateX(3%); }
  to { transform: scale(1.12) translateX(-3%); }
}
@keyframes monKbPanR {
  from { transform: scale(1.12) translateX(-3%); }
  to { transform: scale(1.12) translateX(3%); }
}

/* 黑底占位（无图镜头：desc 大字） */
.mon-ph {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 8px;
  padding: 18px 14px;
  text-align: center;
}
.mon-ph-shot {
  font-size: 13px;
  font-weight: 700;
  color: rgba(255, 255, 255, 0.55);
  background: rgba(255, 255, 255, 0.12);
  border-radius: 6px;
  padding: 1px 8px;
}
.mon-ph-text {
  margin: 0;
  font-size: 15px;
  line-height: 1.6;
  color: rgba(255, 255, 255, 0.82);
  display: -webkit-box;
  -webkit-line-clamp: 4;
  -webkit-box-orient: vertical;
  overflow: hidden;
}

/* 顶行叠加：镜头号徽章 + 时间码 */
.mon-topline {
  position: absolute;
  top: 8px;
  left: 8px;
  z-index: 2;
  pointer-events: none;
}
.mon-badge {
  display: inline-block;
  font-size: 11.5px;
  font-weight: 700;
  color: #fff;
  background: rgba(0, 0, 0, 0.55);
  border-radius: var(--radius-pill, 20px);
  padding: 1px 9px;
}
.mon-timecode {
  position: absolute;
  top: 8px;
  right: 8px;
  z-index: 2;
  font-size: 11px;
  color: rgba(255, 255, 255, 0.85);
  background: rgba(0, 0, 0, 0.55);
  border-radius: var(--radius-pill, 20px);
  padding: 1px 9px;
  pointer-events: none;
  white-space: nowrap;
}

/* 底部字幕条（白字黑描边；line 优先 desc） */
.mon-cc {
  position: absolute;
  left: 10%;
  right: 10%;
  bottom: 10px;
  z-index: 2;
  text-align: center;
  font-size: 14px;
  font-weight: 600;
  line-height: 1.5;
  color: #ffffff;
  -webkit-text-stroke: 3px #000;
  paint-order: stroke fill;
  text-shadow:
    0 0 3px #000,
    1px 1px 2px #000,
    -1px -1px 2px #000,
    1px -1px 2px #000,
    -1px 1px 2px #000;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  pointer-events: none;
}

/* —— 播放控制条 —— */
.mon-controls {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-shrink: 0;
}
.mon-ctrl-gap { flex: 1; }
.mon-btn {
  min-width: 32px;
  height: 30px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  padding: 0 8px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  flex-shrink: 0;
}
.mon-btn:hover { background: rgba(0, 0, 0, 0.04); }
.mon-btn:disabled { opacity: 0.45; cursor: not-allowed; }
.mon-btn .mono { font-family: var(--mono, monospace); font-weight: 700; font-size: 12px; }
.mon-play {
  min-width: 42px;
  background: var(--accent, #E95420);
  border-color: var(--accent, #E95420);
  color: #fff;
}
.mon-play:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.mon-cc-btn.on {
  border-color: var(--accent, #E95420);
  color: var(--accent, #E95420);
  background: var(--accent-soft, rgba(233, 84, 32, 0.08));
}

.mon-hidden { display: none; }
</style>
