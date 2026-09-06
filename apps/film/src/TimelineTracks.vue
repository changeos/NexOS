<script setup lang="ts">
// =============================================================================
// TimelineTracks.vue —— 工作室底部多轨时间轴（剪映/CapCut 风格，v0.1.34）。
//
// 四轨（自上而下）：
//   ① 🎞 视频轨：每镜头一块（块宽 = duration_secs × pxPerSec；已生成分镜图时
//      img 背景（files/download 信封懒加载 data URL），缺失态斜纹；mp4 就绪=
//      实色边框、仅有图=半就绪色）；
//   ② 🎙 配音轨：每镜头台词音频块（mp3 生成后实色 + 小波形图标 + 时长；
//      无台词/未生成=空槽虚线块）；
//   ③ 🎵 BGM 轨：单块横贯（未生成=虚线槽；生成后按任务 output 落 bgm.mp3；
//      artifacts 仅 bytes 无时长元数据 → 时长未知给标称块=视频轨总时长）；
//   ④ 💬 字幕轨：每镜头 desc 块（有台词优先台词文本，无则 desc 截断）。
//
// 交互：
//   - 块点击 = 选中镜头（emit select → FilmStudio 联动左侧镜头卡/面板滚动）；
//   - pxPerSec 缩放滑杆（20–120px/s）+「适应宽度」按钮；
//   - 轨道头固定列（轨道名+图标+该轨就绪计数 x/y）不随横向滚动；
//   - 可折叠：展开 ~180px，折叠成 28px 条；
//   - 播放头（v0.1.35）：标尺区点击=定位播放头（seek previewEngine）；拖动
//     （pointerdown+move，pointer capture）连续 seek；播放时引擎 rAF 时钟驱动
//     playheadSec 匀速前进 → 头随 pxPerSec 移动；镜头块红色描边高亮当前段
//     （与选中镜头的橙色区分）；播放中当前块自动滚入视口。
//
// 时间标尺：顶部秒刻度，步进按 pxPerSec 在 1/2/5/10s 中自适应（最小刻度间距
// ~56px）；镜头块间无缝排布（各轨块 x = 前序镜头 duration 累计），总时长尾标。
//
// 状态刷新：块状态全部由 props（shots/artifacts）派生——FilmStudio 任务轮询
// 终态后 reloadProject() 刷新 artifacts，本组件 computed 自动重算变色。
//
// 未尽事项（v1 明确不做）：
//   - 拖拽调序：需后端 script 重排端点配合（当前 PUT 仅按镜头号合并字段）；
//   - 波形式音频/真实时长探测：需后端补音频元数据（采样/时长）端点。
// =============================================================================
import { computed, nextTick, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { fetchFileDataUrl, type FilmArtifact, type FilmShot } from './api'
import { effSecs, usePreviewEngine } from './previewEngine'

const props = defineProps<{
  /** 分镜列表（顺序即轨道顺序）。 */
  shots: FilmShot[]
  /** 产物清单（shot-N.png / shot-N.mp4 / line-N.mp3 / bgm.mp3 存在性判定）。 */
  artifacts: FilmArtifact[]
  /** 当前选中镜头序号（1 起；外部变化时轨道区滚到对应块）。 */
  selectedShot: number
  /** 项目产物目录（拼 files/download 读取路径；空=无项目）。 */
  projectDir: string
}>()

const emit = defineEmits<{
  (e: 'select', shot: number): void
}>()

const { t } = useI18n()

// —— 预览引擎（FilmStudio provide；播放头双向：此处 seek ↔ 引擎播放移动）——
const engine = usePreviewEngine()

// —— 折叠态（展开 ~180px / 折叠 28px 条，条上仍可点击展开）——
const collapsed = ref(false)

// —— 缩放（px/s；滑杆 20–120）——
const pxPerSec = ref(50)

// 镜头缺省时长（duration_secs 缺失/非法时给标称 4s——块宽需有值）；
// effSecs/FALLBACK_SECS 取自 previewEngine（与播放段映射同一事实源）。

// —— 产物存在性（Set 派生，artifacts 变化即重算）——
const artNames = computed(() => new Set(props.artifacts.map((a) => a.name)))
function artOn(name: string): boolean {
  return artNames.value.has(name)
}

/** 秒数格式化（整数不带小数，非整数保留一位）。 */
function fmtSecs(n: number): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(1)
}

// —— 四轨数据映射（x/w 由前序镜头 duration 累计派生，无缝排布）——
interface TrackBlock {
  shot: number
  /** 左端 px（= 前序镜头 duration 合计 × pxPerSec）。 */
  x: number
  /** 块宽 px（= duration × pxPerSec；极窄下限保可点击）。 */
  w: number
  secs: number
  hasImg: boolean
  hasVideo: boolean
  hasVoice: boolean
  hasLine: boolean
  /** 分镜图缩略 data URL（未就绪/加载失败=undefined→斜纹缺失态）。 */
  thumb?: string
  /** 字幕文本（台词优先，无则 desc；两者皆空=''）。 */
  text: string
}

const blocks = computed<TrackBlock[]>(() => {
  const px = pxPerSec.value
  let acc = 0
  return props.shots.map((s) => {
    const secs = effSecs(s)
    const x = acc * px
    acc += secs
    return {
      shot: s.shot,
      x,
      w: Math.max(secs * px, 14),
      secs,
      hasImg: artOn(`shot-${s.shot}.png`),
      hasVideo: artOn(`shot-${s.shot}.mp4`),
      hasVoice: artOn(`line-${s.shot}.mp3`),
      hasLine: !!(s.line && s.line.trim()),
      thumb: thumbs[s.shot],
      text: (s.line && s.line.trim()) || (s.desc || '').trim(),
    }
  })
})

const totalSecs = computed(() =>
  props.shots.reduce((sum, s) => sum + effSecs(s), 0),
)
const totalPx = computed(() => totalSecs.value * pxPerSec.value)
/** 内容宽（总时长 + 尾标余量；min-width:100% 兜底短内容）。 */
const contentWidth = computed(() => totalPx.value + 56)

// —— BGM 轨（单块；时长未知 → 标称块 = 视频轨总时长，见头部注释）——
const bgmOn = computed(() => artOn('bgm.mp3'))
const bgmSecs = computed(() => totalSecs.value)

// —— 轨道头就绪计数 x/y ——
const videoReady = computed(() => blocks.value.filter((b) => b.hasVideo).length)
const voiceReady = computed(() => blocks.value.filter((b) => b.hasVoice).length)
const voiceDenom = computed(() => blocks.value.filter((b) => b.hasLine).length)
const subReady = computed(() => blocks.value.filter((b) => b.text).length)

// —— 时间标尺（步进 1/2/5/10s 自适应：最小刻度间距 ≥56px）——
const RULER_STEPS = [1, 2, 5, 10] as const
const rulerStep = computed(() => {
  for (const step of RULER_STEPS) {
    if (step * pxPerSec.value >= 56) return step
  }
  return RULER_STEPS[RULER_STEPS.length - 1]
})
const rulerTicks = computed(() => {
  const ticks: number[] = []
  for (let s = 0; s <= totalSecs.value; s += rulerStep.value) ticks.push(s)
  return ticks
})

// —— 分镜图缩略（files/download 信封 → data URL；串行懒加载防并发洪峰）——
const thumbs = reactive<Record<number, string>>({})

async function loadThumbs(): Promise<void> {
  const dir = props.projectDir.replace(/\/$/, '')
  if (!dir) return
  for (const s of props.shots) {
    if (!artOn(`shot-${s.shot}.png`) || thumbs[s.shot]) continue
    try {
      // await 串行：后端 b64 信封逐个取，避免一次打满宿主 api
      thumbs[s.shot] = await fetchFileDataUrl(`${dir}/shot-${s.shot}.png`)
    } catch {
      /* 加载失败保持斜纹缺失态（下次 artifacts 变化再试） */
    }
  }
}

watch(
  () => [props.projectDir, artNames.value, props.shots.length] as const,
  () => void loadThumbs(),
  { immediate: true },
)

// —— 横向滚动容器：适应宽度 + 选中块滚入 ——
const scrollEl = ref<HTMLElement | null>(null)

/** 适应宽度：按滚动区可视宽反解 pxPerSec（夹到 20–120）。 */
function fitWidth(): void {
  const el = scrollEl.value
  if (!el || totalSecs.value <= 0) return
  const avail = el.clientWidth - 24
  if (avail <= 0) return
  const fit = Math.round((avail / totalSecs.value) * 10) / 10
  pxPerSec.value = Math.min(120, Math.max(20, Math.round(fit)))
}

/** 把指定镜头块滚进视口（选中联动 / 播放中当前段跟随共用）。 */
function scrollToShotBlock(n: number): void {
  void nextTick(() => {
    const sc = scrollEl.value
    const el = sc?.querySelector<HTMLElement>(`[data-shot="${n}"]`)
    if (!sc || !el) return
    const left = el.offsetLeft
    if (left < sc.scrollLeft + 8 || left + el.offsetWidth > sc.scrollLeft + sc.clientWidth - 8) {
      sc.scrollLeft = Math.max(0, left - sc.clientWidth / 2)
    }
  })
}

/** 选中镜头变化（左侧镜头卡/面板 → 时间轴反向联动）时滚入对应块。 */
watch(
  () => props.selectedShot,
  (n) => {
    if (!collapsed.value) scrollToShotBlock(n)
  },
)

// =============================================================================
// 播放头（v0.1.35）：与 previewEngine 双向联动——
//   - 引擎播放 → playheadSec 匀速前进（引擎 rAF 时钟）→ 此处换算 px 渲染；
//   - 标尺点击 / 播放头拖动（pointerdown+move+capture）→ engine.seek。
// =============================================================================
/** 播放头是否可见（有分镜可播；final 模式跟随成片 currentTime）。 */
const playheadVisible = computed(
  () => !!engine && (engine.segments.value.length > 0 || engine.mode.value === 'final'),
)

/** 播放头位置 px（playheadSec × pxPerSec；final 模式=currentTime 同式换算）。 */
const playheadPx = computed(() =>
  engine ? engine.playheadSec.value * pxPerSec.value : 0,
)

/** 播放中当前段镜头号（红色描边高亮；-1=未播）。 */
const playingShot = computed(() => engine?.currentSeg.value?.shot ?? -1)

/** 拖动中标记（pointer capture 期间 pointermove 连续 seek）。 */
const draggingPh = ref(false)

/** 指针事件 → 播放头秒（滚动容器坐标 + scrollLeft 修正）。 */
function phSeek(e: PointerEvent): void {
  const sc = scrollEl.value
  if (!engine || !sc) return
  const rect = sc.getBoundingClientRect()
  const x = e.clientX - rect.left + sc.scrollLeft
  engine.seek(x / pxPerSec.value)
}

function onPlayheadDown(e: PointerEvent): void {
  if (!engine) return
  draggingPh.value = true
  const el = e.currentTarget as HTMLElement
  el.setPointerCapture(e.pointerId)
  phSeek(e)
}

function onPlayheadMove(e: PointerEvent): void {
  if (draggingPh.value) phSeek(e)
}

function onPlayheadUp(e: PointerEvent): void {
  if (!draggingPh.value) return
  draggingPh.value = false
  const el = e.currentTarget as HTMLElement
  if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId)
}

/** 播放中当前段块自动滚入视口（跟随镜头推进）。 */
watch(
  () => engine?.currentSeg.value?.shot,
  (n) => {
    if (n && engine?.playing.value && !collapsed.value) scrollToShotBlock(n)
  },
)
</script>

<template>
  <section class="tl-bar" :class="{ collapsed }" :aria-label="t('film.timelineTracks')">
    <!-- 工具条（折叠态即 28px 条本体：仍可点击展开） -->
    <div class="tl-toolbar">
      <button
        class="tl-toggle"
        type="button"
        :title="collapsed ? t('film.timelineExpand') : t('film.timelineCollapse')"
        :aria-label="collapsed ? t('film.timelineExpand') : t('film.timelineCollapse')"
        @click="collapsed = !collapsed"
      >{{ collapsed ? '▴' : '▾' }}</button>
      <span class="tl-title">🎞 {{ t('film.timelineTracks') }}</span>
      <span class="tl-total mono">{{ t('film.timelineTotal', { secs: fmtSecs(totalSecs) }) }}</span>
      <template v-if="!collapsed">
        <span class="tl-zoom">
          <span class="tl-zoom-label">{{ t('film.timelineZoom') }}</span>
          <input
            v-model.number="pxPerSec"
            class="tl-range"
            type="range"
            min="20"
            max="120"
            step="1"
            :aria-label="t('film.timelineZoom')"
          >
          <span class="tl-zoom-val mono">{{ pxPerSec }}px/s</span>
        </span>
        <button class="tl-btn" type="button" @click="fitWidth">
          ⤢ {{ t('film.timelineFit') }}
        </button>
      </template>
    </div>

    <div v-if="!collapsed" class="tl-body">
      <div v-if="!shots.length" class="tl-empty">{{ t('film.timelineEmpty') }}</div>
      <template v-else>
        <!-- 轨道头固定列（不随横向滚动）：图标+轨道名+就绪计数 x/y -->
        <div class="tl-heads">
          <div class="tl-ruler-head"></div>
          <div class="tl-head-cell tl-head-video">
            <span class="tl-head-name">🎞 {{ t('film.trackVideo') }}</span>
            <span class="tl-count mono">{{ videoReady }}/{{ shots.length }}</span>
          </div>
          <div class="tl-head-cell tl-head-voice">
            <span class="tl-head-name">🎙 {{ t('film.trackVoice') }}</span>
            <span class="tl-count mono">{{ voiceReady }}/{{ voiceDenom }}</span>
          </div>
          <div class="tl-head-cell tl-head-bgm">
            <span class="tl-head-name">🎵 {{ t('film.trackBgm') }}</span>
            <span class="tl-count mono">{{ bgmOn ? 1 : 0 }}/1</span>
          </div>
          <div class="tl-head-cell tl-head-sub">
            <span class="tl-head-name">💬 {{ t('film.trackSub') }}</span>
            <span class="tl-count mono">{{ subReady }}/{{ shots.length }}</span>
          </div>
        </div>

        <!-- 轨道滚动区：标尺 + 四轨（块 x=前序镜头 duration 累计 × pxPerSec） -->
        <div ref="scrollEl" class="tl-scroll">
          <div class="tl-content" :style="{ width: contentWidth + 'px' }">
            <!-- 时间标尺（秒刻度，步进 1/2/5/10s 自适应 + 总时长尾标）。
                 其上覆盖标尺命中条：点击=定位播放头（seek 引擎）、拖动连续 seek。 -->
            <div class="tl-ruler">
              <span
                v-for="s in rulerTicks"
                :key="s"
                class="tl-tick mono"
                :style="{ left: s * pxPerSec + 'px' }"
              >{{ s }}s</span>
              <span
                v-if="totalSecs > 0"
                class="tl-tail mono"
                :style="{ left: totalPx + 'px' }"
              >{{ fmtSecs(totalSecs) }}s</span>
            </div>

            <!-- ① 视频轨：就绪=实色边框 / 仅有图=半就绪色 / 缺图=斜纹 -->
            <div class="tl-track tl-track-video">
              <button
                v-for="b in blocks"
                :key="`v${b.shot}`"
                class="tl-block tl-vblock"
                :class="{
                  ready: b.hasVideo,
                  semi: !b.hasVideo && b.hasImg,
                  active: b.shot === selectedShot,
                  playing: b.shot === playingShot,
                }"
                :style="{ left: b.x + 'px', width: b.w + 'px' }"
                :data-shot="b.shot"
                :title="`#${b.shot} · ${fmtSecs(b.secs)}s`"
                type="button"
                @click="emit('select', b.shot)"
              >
                <span
                  class="tl-thumb"
                  :class="{ missing: !b.thumb }"
                  :style="b.thumb ? { backgroundImage: `url(${b.thumb})` } : undefined"
                  :title="b.thumb ? undefined : t('film.timelineNoImage')"
                ></span>
                <span class="tl-vlabel mono">#{{ b.shot }}</span>
              </button>
            </div>

            <!-- ② 配音轨：生成后实色（波形图标+时长）；无台词/未生成=虚线空槽 -->
            <div class="tl-track tl-track-voice">
              <button
                v-for="b in blocks"
                :key="`a${b.shot}`"
                class="tl-block tl-ablock"
                :class="{
                  ready: b.hasVoice,
                  active: b.shot === selectedShot,
                  playing: b.shot === playingShot,
                }"
                :style="{ left: b.x + 'px', width: b.w + 'px' }"
                :data-shot="b.shot"
                :title="b.hasVoice
                  ? `#${b.shot} · ${fmtSecs(b.secs)}s`
                  : t('film.timelineVoiceEmpty')"
                type="button"
                @click="emit('select', b.shot)"
              >
                <svg
                  v-if="b.hasVoice"
                  class="tl-wave"
                  viewBox="0 0 24 12"
                  width="20"
                  height="10"
                  fill="none"
                  stroke="currentColor"
                  stroke-width="1.5"
                  stroke-linecap="round"
                  aria-hidden="true"
                >
                  <line x1="2" y1="4" x2="2" y2="8" />
                  <line x1="6" y1="2" x2="6" y2="10" />
                  <line x1="10" y1="0.5" x2="10" y2="11.5" />
                  <line x1="14" y1="2" x2="14" y2="10" />
                  <line x1="18" y1="4" x2="18" y2="8" />
                  <line x1="22" y1="5" x2="22" y2="7" />
                </svg>
                <span v-if="b.hasVoice" class="tl-adur mono">{{ fmtSecs(b.secs) }}s</span>
                <span v-else class="tl-aslot">—</span>
              </button>
            </div>

            <!-- ③ BGM 轨：单块横贯标称时长；未生成=虚线槽 -->
            <div class="tl-track tl-track-bgm">
              <div
                class="tl-block tl-bgmblock"
                :class="{ ready: bgmOn }"
                :style="{ left: '0px', width: bgmSecs * pxPerSec + 'px' }"
                :title="bgmOn
                  ? `bgm.mp3 · ${t('film.timelineBgmNominal')}`
                  : t('film.timelineBgmNominal')"
              >
                <span class="tl-bgm-label">🎵 {{ t('film.trackBgm') }}</span>
              </div>
            </div>

            <!-- ④ 字幕轨：台词优先，无则 desc（CSS 截断） -->
            <div class="tl-track tl-track-sub">
              <button
                v-for="b in blocks"
                :key="`s${b.shot}`"
                class="tl-block tl-sblock"
                :class="{
                  ready: !!b.text,
                  active: b.shot === selectedShot,
                  playing: b.shot === playingShot,
                }"
                :style="{ left: b.x + 'px', width: b.w + 'px' }"
                :data-shot="b.shot"
                :title="b.text || t('film.timelineEmpty')"
                type="button"
                @click="emit('select', b.shot)"
              >
                <span v-if="b.text" class="tl-sub-text">{{ b.text }}</span>
                <span v-else class="tl-aslot">—</span>
              </button>
            </div>

            <!-- 播放头（v0.1.35）：引擎 playheadSec × pxPerSec；头把手可拖动 -->
            <div
              v-if="playheadVisible"
              class="tl-playhead"
              :class="{ dragging: draggingPh }"
              :style="{ left: playheadPx + 'px' }"
              :title="t('film.playheadLabel')"
            >
              <div class="tl-ph-line" aria-hidden="true"></div>
              <div
                class="tl-ph-head"
                :aria-label="t('film.playheadLabel')"
                @pointerdown="onPlayheadDown"
                @pointermove="onPlayheadMove"
                @pointerup="onPlayheadUp"
                @pointercancel="onPlayheadUp"
              ></div>
            </div>

            <!-- 标尺命中条（透明覆盖标尺行）：点击=定位播放头；拖动连续 seek -->
            <div
              v-if="playheadVisible"
              class="tl-ph-hit"
              :aria-label="t('film.playheadLabel')"
              @pointerdown="onPlayheadDown"
              @pointermove="onPlayheadMove"
              @pointerup="onPlayheadUp"
              @pointercancel="onPlayheadUp"
            ></div>
          </div>
        </div>
      </template>
    </div>
  </section>
</template>

<style scoped>
/* ===================== 条本体（展开 ~180px / 折叠 28px；零 vh） ===================== */
.tl-bar {
  display: flex;
  flex-direction: column;
  flex-shrink: 0;
  height: 182px;
  overflow: hidden;
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  background: var(--bg-card, #ffffff);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
  transition: height 0.15s ease;
}
.tl-bar.collapsed { height: 28px; }

/* —— 工具条（28px；折叠态=条本体） —— */
.tl-toolbar {
  display: flex;
  align-items: center;
  gap: 8px;
  height: 28px;
  padding: 0 8px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  flex-shrink: 0;
}
.tl-toggle {
  width: 20px;
  height: 20px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  border: 1px solid var(--border, #d1d5db);
  border-radius: 6px;
  background: var(--bg-card, #fff);
  color: var(--text-muted, #5E5C5F);
  font-size: 11px;
  line-height: 1;
  cursor: pointer;
  padding: 0;
  flex-shrink: 0;
}
.tl-toggle:hover { background: rgba(0, 0, 0, 0.04); }
.tl-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
}
.tl-total {
  font-size: 11px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
}
.tl-zoom {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-left: auto;
  min-width: 0;
}
.tl-zoom-label {
  font-size: 11px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
}
.tl-zoom-val {
  font-size: 10.5px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
}
.tl-range { width: 110px; height: 14px; accent-color: var(--accent, #E95420); }
.tl-btn {
  padding: 2px 8px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
  font-size: 11.5px;
  cursor: pointer;
  font-family: inherit;
  white-space: nowrap;
  flex-shrink: 0;
}
.tl-btn:hover { background: rgba(0, 0, 0, 0.04); }

/* —— 主体：轨道头固定列 + 滚动区（零 vh：定高行 + 横向滚动） —— */
.tl-body {
  flex: 1;
  min-height: 0;
  display: flex;
}
.tl-empty {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 12.5px;
  color: var(--text-muted, #5E5C5F);
}
.mono { font-family: var(--mono, monospace); }

/* 轨道头固定列（各 cell 行高与右侧轨道行严格一致，保证对齐） */
.tl-heads {
  width: 96px;
  flex-shrink: 0;
  border-right: 1px solid var(--border-soft, #EDEDED);
  display: flex;
  flex-direction: column;
}
.tl-ruler-head { height: 18px; flex-shrink: 0; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.tl-head-cell {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 4px;
  padding: 0 6px 0 8px;
  flex-shrink: 0;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
/* 各 cell 行高与右侧轨道行严格一致（保证对齐） */
.tl-head-video { height: 44px; }
.tl-head-voice { height: 28px; }
.tl-head-bgm { height: 26px; }
.tl-head-sub { height: 26px; border-bottom: none; }
.tl-head-name {
  font-size: 11px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.tl-count {
  font-size: 10px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
  flex-shrink: 0;
}

/* 滚动区 + 内容画布 */
.tl-scroll {
  flex: 1;
  min-width: 0;
  overflow-x: auto;
  overflow-y: hidden;
}
.tl-content { position: relative; min-width: 100%; }

/* 时间标尺 */
.tl-ruler {
  position: relative;
  height: 18px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  flex-shrink: 0;
  font-size: 9.5px;
  color: var(--text-muted, #5E5C5F);
}
.tl-tick {
  position: absolute;
  top: 3px;
  border-left: 1px solid var(--border, #d1d5db);
  padding-left: 3px;
  white-space: nowrap;
}
.tl-tail {
  position: absolute;
  top: 3px;
  border-left: 1px dashed var(--accent, #E95420);
  color: var(--accent, #E95420);
  padding-left: 3px;
  white-space: nowrap;
}

/* 轨道行（高度与轨道头 cell 一一对应） */
.tl-track {
  position: relative;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.tl-track:last-child { border-bottom: none; }
.tl-track-video { height: 44px; }
.tl-track-voice { height: 28px; }
.tl-track-bgm { height: 26px; }
.tl-track-sub { height: 26px; }

/* ===================== 块通用 ===================== */
.tl-block {
  position: absolute;
  top: 3px;
  bottom: 3px;
  border-radius: 5px;
  overflow: hidden;
  border: 1px solid var(--border, #d1d5db);
  background: var(--border-soft, #FAFAFA);
  cursor: pointer;
  font-family: inherit;
  text-align: left;
  padding: 0;
  display: flex;
  align-items: center;
  gap: 4px;
}
.tl-block.active {
  box-shadow: 0 0 0 2px var(--accent, #E95420);
  z-index: 1;
}
/* 播放中当前段块（红色描边——与选中镜头的橙色区分；两者同块叠加时红优先） */
.tl-block.playing {
  box-shadow: 0 0 0 2px #dc2626;
  z-index: 1;
}
.tl-block:hover { filter: brightness(0.97); }

/* ===================== 播放头（v0.1.35） ===================== */
/* 命中条：透明覆盖标尺行（指针交互层；视觉上仍是标尺刻度） */
.tl-ph-hit {
  position: absolute;
  top: 0;
  left: 0;
  right: 0;
  height: 18px;
  z-index: 4;
  cursor: ew-resize;
  touch-action: none;
}
/* 播放头本体：竖线 + 顶把手（指针事件只开把手，线不挡块点击） */
.tl-playhead {
  position: absolute;
  top: 0;
  bottom: 0;
  width: 0;
  z-index: 5;
  pointer-events: none;
}
.tl-ph-line {
  position: absolute;
  top: 0;
  bottom: 0;
  left: -0.75px;
  width: 1.5px;
  background: #dc2626;
}
.tl-playhead.dragging .tl-ph-line {
  box-shadow: 0 0 0 0.5px rgba(220, 38, 38, 0.45);
}
.tl-ph-head {
  position: absolute;
  top: 0;
  left: -5.5px;
  width: 11px;
  height: 14px;
  background: #dc2626;
  clip-path: polygon(0 0, 100% 0, 100% 55%, 50% 100%, 0 55%);
  pointer-events: auto;
  cursor: ew-resize;
  touch-action: none;
}

/* ① 视频块：分镜图背景（img 缩略）+ 镜头号角标；缺图=斜纹 */
.tl-vblock {
  flex-direction: column;
  align-items: stretch;
  justify-content: flex-end;
  gap: 0;
}
.tl-vblock.ready {
  border: 1.5px solid #16a34a;
  background: rgba(22, 163, 74, 0.08);
}
.tl-vblock.semi { border: 1.5px solid #d97706; }
.tl-thumb {
  position: absolute;
  inset: 0;
  background-size: cover;
  background-position: center;
  background-repeat: no-repeat;
}
.tl-thumb.missing {
  background-image: repeating-linear-gradient(
    45deg,
    rgba(0, 0, 0, 0.05) 0 5px,
    transparent 5px 10px
  );
}
.tl-vlabel {
  position: relative;
  align-self: flex-start;
  font-size: 10px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  background: rgba(255, 255, 255, 0.78);
  padding: 0 3px;
  border-radius: 3px;
  margin: 2px;
  white-space: nowrap;
}

/* ② 配音块：实色蓝 + 波形图标 + 时长；虚线空槽 */
.tl-ablock {
  padding: 0 5px;
}
.tl-ablock.ready {
  background: #eff6ff;
  border-color: #93c5fd;
  color: #1d4ed8;
}
.tl-wave { flex-shrink: 0; }
.tl-adur { font-size: 10px; white-space: nowrap; overflow: hidden; }
.tl-aslot {
  font-size: 10px;
  color: var(--text-muted, #5E5C5F);
  opacity: 0.6;
  padding: 0 4px;
  white-space: nowrap;
}

/* ③ BGM 块：单块横贯；未生成=虚线槽 */
.tl-bgmblock {
  padding: 0 6px;
  border-style: dashed;
  background: transparent;
  cursor: default;
}
.tl-bgmblock.ready {
  border-style: solid;
  background: #f5f3ff;
  border-color: #c4b5fd;
  color: #6d28d9;
}
.tl-bgm-label {
  font-size: 10.5px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* ④ 字幕块：台词/描述文本（截断） */
.tl-sblock {
  padding: 0 5px;
}
.tl-sblock.ready {
  background: #fffbeb;
  border-color: #fde68a;
}
.tl-sub-text {
  font-size: 10.5px;
  color: #92400e;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  min-width: 0;
}

/* 窄窗：轨道头收窄 */
@media (max-width: 880px) {
  .tl-heads { width: 76px; }
  .tl-head-name { font-size: 10px; }
}
</style>
