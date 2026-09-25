<script lang="ts">
// —— 导入编码兼容（v0.1.39；纯函数导出，冒烟脚本可直断言）——
//
// 中文网文 txt 几乎全是 GBK/GB2312/GB18030 编码，而 story/import 后端只收
// UTF-8（原路径直接 400「导入原文须为 UTF-8 文本」）。此处前端转码，浏览器
// 原生 TextDecoder/TextEncoder 零依赖：
//   1. fatal UTF-8 严格解码通过 = 真 UTF-8 → 原字节直通；
//   2. 失败 → GB18030（向下兼容 GBK/GB2312，覆盖中文 txt 全谱）解码 → 重编
//      码为 UTF-8 字节再上传（后端契约不变，收到即 UTF-8）；
//   3. 双双失败 = 二进制/未知编码 → 抛错，调用方维持原错误提示。
export interface DecodedSource {
  /** 待上传字节（UTF-8）。 */
  bytes: Uint8Array
  /** true = 经 GB18030 转码（成功后 toast 提示）。 */
  transcoded: boolean
}

/** 原文字节 → UTF-8 字节（UTF-8 直通 / GB18030 转码；均失败抛 TypeError）。 */
export function decodeSourceBytes(buf: ArrayBuffer): DecodedSource {
  try {
    new TextDecoder('utf-8', { fatal: true }).decode(buf)
    return { bytes: new Uint8Array(buf), transcoded: false }
  } catch {
    const text = new TextDecoder('gb18030', { fatal: true }).decode(buf)
    return { bytes: new TextEncoder().encode(text), transcoded: true }
  }
}

/** 字节 → 标准 b64（32KB 分块拼接，防大文件 String.fromCharCode 展开溢栈）。 */
export function bytesToB64(bytes: Uint8Array): string {
  let bin = ''
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000))
  }
  return btoa(bin)
}
</script>

<script setup lang="ts">
// =============================================================================
// StoryPage.vue —— 剧情页（流程 ①）。
//
// v0.1.7 UI 重设计第一批·全量重皮试点页二：视觉全量迁 design token 体系
// （nx-* 基座 + .nx-page 作用域覆写；样式唯一事实源 design/theme.css
// 「试点页二」区），功能面 v0.1.39/0.1.6 全保留：
//   · 左=原文区：导入（点击 + 拖拽；GBK/GB18030 前端转码）+ sources 列表 +
//     选中源内容 pre；管线操作条（v0.1.44 顺序引导：🧹清理 → 📖分章 →
//     🧠向量化 → 👥人物梳理[支线]——未分章时向量化禁用+tooltip[后端 400 同款
//     文案]，emb 位不可用同样禁用；并入件：已入库源[chapters/index.json 存在
//     且指向该源]清理/分章禁用+tooltip，源行加「已入库」绿实底徽章）；
//   · 右=结构区三 Tab：章节 / 人物档案（转定妆对象）/ 正稿 + 语义搜索；
//   · 管线进度重做（MaxKB 聚合徽章 + X/Y 计数思路）：
//     - 每源一行聚合状态徽章（pipelineStatus.aggregateSourcePipeline 纯函数
//       聚合任务中心 story.* 任务：失败 > 运行 > 排队 > 完成）；
//     - hover/点按徽章展开 NxPopover 任务明细：各任务「完成 X/Y」离散计数
//       （后端日志「块 3/17 完成」可解析则展示）+ NxProgress；解析不到分块
//       进度显示运行态 + 已耗时（mm:ss）——**不造假进度**；
//     - 失败行红底 + 行内重试（对失败阶段原参重发）；
//     - 6 秒静默轮询：有进行中任务时自动重载源列表（自动流转，无 loading
//       打扰）；耗时口径随同刻度刷新。
// =============================================================================
import { computed, onMounted, onUnmounted, reactive, ref, watch, type Ref } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  filmContentText,
  filmCreateCasting,
  filmGetFile,
  filmIssueFromTask,
  filmListFiles,
  filmStoryChapterize,
  filmStoryClean,
  filmStoryEmbed,
  filmStoryGenerate,
  filmStoryImport,
  filmStoryProfile,
  filmStorySearch,
  type FilmCharacterProfile,
  type FilmChapterEntry,
  type FilmChapterIndex,
  type FilmFileContent,
  type FilmFileEntry,
  type FilmStoryCleanMode,
  type FilmStorySearchHit,
  type FilmTask,
} from '../api'
import FlowPageHead from './FlowPageHead.vue'
import { PROJECT_DEFAULT_KEY, useFlow, filmTaskStageLabel } from './flowContext'
import {
  clampForDisplay,
  fileBasename,
  fmtBytes,
  hubFileEntries,
  parseFrontmatter,
  sourceBaseName,
  storySourceStatus,
  storySources,
  storyWordCount,
} from './flowFiles'
import {
  aggregateSourcePipeline,
  isPipelineTaskActive,
  parseChunkProgress,
  pipelineStageRank,
  type PipelineTaskSnapshot,
  type SourcePipelineAggregate,
  type StoryPipelineStage,
} from './pipelineStatus'
import NxButton from '../nx/NxButton.vue'
import NxCard from '../nx/NxCard.vue'
import NxBadge from '../nx/NxBadge.vue'
import NxPopover from '../nx/NxPopover.vue'
import NxProgress from '../nx/NxProgress.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
// v0.1.11 全局操作反馈：toast 单例（导入/转定妆/提 Issue/管线提交失败 409）
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

/** 客户端导入护栏（与服务端缺省一致：env NEXOS_FILM_SOURCE_MAX_MB=64MB）。 */
const IMPORT_LIMIT_MB = 64
/** 大文件慢路径提示阈值（b64 上传体感分界）。 */
const IMPORT_HINT_MB = 8
/** 管线静默轮询周期（有进行中任务时自动流转；不转圈不打扰）。 */
const PIPE_POLL_MS = 6000

// —— 原文素材（左栏）——
const tree = ref<FilmFileEntry[]>([])
const sources = ref<FilmFileEntry[]>([])
const sourcesError = ref('')
const importing = ref(false)
const importHint = ref('')
/** 转码成功提示（v0.1.39：GBK/GB18030 → UTF-8 自动转换后显示；下次导入清空）。 */
const importNotice = ref('')
const importInput = ref<HTMLInputElement | null>(null)
const dragOver = ref(false)
/** 选中查看的原文（path + 文本；超长截断展示）。 */
const selectedSource = ref('')
const sourceText = ref('')
const sourceTruncated = ref(false)
const sourceLoading = ref(false)

/** 章节清单（右栏·章 Tab 数据源 + 左栏「已分章」徽章/向量化前置派生输入）。 */
const chaptersIndex = ref<FilmChapterIndex | null>(null)
const chaptersError = ref('')
/** 展开的章节卡（no；null=全收起——点卡头切展，正文懒加载缓存）。 */
const expandedChapter = ref<number | null>(null)
/** 章节正文缓存（no → 文本；展开时懒加载，刷新章节清单时清空）。 */
const chapterTexts = reactive(new Map<number, string>())
/** 章节正文加载中（no 集合）。 */
const chapterLoadingNos = reactive(new Set<number>())

/** 人物档案（右栏·人物档案 Tab）。 */
const profiles = ref<FilmCharacterProfile[]>([])
const profilesError = ref('')
/** 转定妆状态（name → busy/done/error）。 */
const castingBusy = reactive(new Set<string>())
const castingDone = reactive(new Set<string>())
const castingError = reactive(new Map<string, string>())

// —— 管线操作条 ——
const cleanMode = ref<FilmStoryCleanMode>('rules')
const cleanBusy = ref(false)
const chapterizeBusy = ref(false)
const profileBusy = ref(false)
const embedBusy = ref(false)
const pipelineError = ref('')

// ============================================================================
// 管线进度（v0.1.7）：任务中心 story.* 任务 → 源行聚合徽章 + X/Y 计数
// ============================================================================

/** 任务中心管线任务快照（FilmStudio 轮询维护；mock 环境缺省空）。 */
const storyTasks = computed<PipelineTaskSnapshot[]>(() => ctx?.storyTasks?.value ?? [])

/** 源行聚合态（徽章词/变色/重试目标）。 */
function pipeOf(path: string): SourcePipelineAggregate {
  return aggregateSourcePipeline(storyTasks.value, path)
}

/** 源关联任务（popover 明细行；按管线序排）。 */
function tasksFor(path: string): PipelineTaskSnapshot[] {
  return storyTasks.value
    .filter((x) => x.sourceFile === path && pipelineStageRank(x.stage) >= 0)
    .sort((a, b) => pipelineStageRank(a.stage) - pipelineStageRank(b.stage))
}

/** 聚合徽章词（运行态按阶段细分：清理中/分章中/梳理中/向量化中）。 */
function pipeWord(agg: SourcePipelineAggregate): string {
  switch (agg.key) {
    case 'queued':
      return t('film.pipeQueue')
    case 'running':
      switch (agg.stage) {
        case 'story.clean':
          return t('film.pipeClean')
        case 'story.chapterize':
          return t('film.pipeChapter')
        case 'story.profile':
          return t('film.pipeProfile')
        case 'story.embed':
          return t('film.pipeEmbed')
        default:
          return t('film.taskRunning')
      }
    case 'failed':
      return t('film.pipeFailed')
    case 'done':
      return t('film.pipeDone')
    default:
      return ''
  }
}

/** 聚合徽章色（排队中性 / 运行蓝 / 失败红 / 成功绿）。 */
function pipeVariant(agg: SourcePipelineAggregate): 'neutral' | 'info' | 'danger' | 'success' {
  switch (agg.key) {
    case 'running':
      return 'info'
    case 'failed':
      return 'danger'
    case 'done':
      return 'success'
    default:
      return 'neutral'
  }
}

/** 任务状态徽章色。 */
function taskVariant(status: string): 'neutral' | 'info' | 'danger' | 'success' {
  if (status === 'running') return 'info'
  if (status === 'failed') return 'danger'
  if (status === 'completed') return 'success'
  return 'neutral'
}

/** 任务状态词（任务中心同口径 i18n）。 */
function taskWord(status: string): string {
  if (status === 'running') return t('film.taskRunning')
  if (status === 'failed') return t('film.taskFailed')
  if (status === 'completed') return t('film.taskDone')
  return t('film.taskQueued')
}

/** 任务阶段词（filmTaskStageLabel：清理/分章/人物梳理/向量化）。 */
function taskStageWord(stage: string): string {
  return filmTaskStageLabel(stage, t)
}

/** 源行徽章的 X/Y 附注（解析不到不显示——不造假进度）。 */
function pipeXy(agg: SourcePipelineAggregate): string {
  return agg.counts ? `${agg.counts.done}/${agg.counts.total}` : ''
}

/** 已耗时（mm:ss / h:mm:ss，mono 呈现；无 createdAt 返回 ''）。 */
function fmtElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000))
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  const pad = (n: number): string => String(n).padStart(2, '0')
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`
}

/** 任务明细行右侧 meta：X/Y > 运行+已耗时 > 排队 —。 */
function taskMeta(task: PipelineTaskSnapshot, now: number): string {
  const counts = parseChunkProgress(task.log ?? [])
  if (counts) return t('film.pipeXOfY', { done: counts.done, total: counts.total })
  if (task.status === 'running') {
    if (typeof task.createdAt === 'number') {
      return `${t('film.pipeElapsed')} ${fmtElapsed(now - task.createdAt)}`
    }
    return t('film.pipeNoProgress')
  }
  if (task.status === 'completed') return '✓'
  return '—'
}

// —— 6 秒静默轮询（自动流转）：有进行中管线任务时重载源列表（产物徽章落树）
//    + 刷新耗时口径；无任务时空转不发包。 ——
const nowMs = ref(Date.now())
let pipeTimer: ReturnType<typeof setInterval> | null = null

const hasActivePipeTask = computed(() =>
  storyTasks.value.some((x) => isPipelineTaskActive(x.status)),
)

function startPipePolling(): void {
  if (pipeTimer !== null) return
  pipeTimer = setInterval(() => {
    nowMs.value = Date.now()
    if (hasActivePipeTask.value) void loadSources().catch(() => undefined)
  }, PIPE_POLL_MS)
}

function stopPipePolling(): void {
  if (pipeTimer !== null) {
    clearInterval(pipeTimer)
    pipeTimer = null
  }
}

onMounted(() => startPipePolling())
onUnmounted(stopPipePolling)

// —— 语义检索（v0.1.39.1：emb 能力位消费；hub 树 vectors-*.json 即缓存） ——

/** emb 能力位快照（models.json 文件即真值；null=未加载/未设置）。 */
const embSlot = computed(() => {
  const caps = ctx?.projectModels.value?.capabilities
  return caps?.find((c) => c.capability === 'emb') ?? null
})
/** emb 位就绪（已设置 + 探测 ok——本地须 --task embed 实例，渠道须 embeddings API）。 */
const embReady = computed(
  () => !!embSlot.value?.source && embSlot.value.available?.ok !== false,
)
/** 向量化钮禁用 tooltip（''=可用）：探测详情 + 一句人话指引。 */
const embTip = computed(() => {
  const s = embSlot.value
  if (!s?.source) return `${t('film.storyEmbNotSet')}\n${t('film.storyEmbHint')}`
  if (s.available?.ok === false) {
    return (
      t('film.storyEmbUnavailable') +
      (s.available.detail ? `：${s.available.detail}` : '') +
      `\n${t('film.storyEmbHint')}`
    )
  }
  return ''
})
/** 提示行文案（一行：探测详情/未设置 + 指引；与 tooltip 同源）。 */
const embHintText = computed(() => embTip.value.replace(/\n/g, ' '))
/** 提示行关闭态（本会话内不再显示；emb 位重新配置并重进本页即重估）。 */
const embHintClosed = ref(false)

/** 语义搜索态（右栏「章节」Tab 上方搜索框 → POST story/search → 结果块卡片）。 */
const searchQuery = ref('')
const searchBusy = ref(false)
const searchError = ref('')
const searchResults = ref<FilmStorySearchHit[]>([])
/** 最近一次检索命中的源（结果卡「定位原文」跳左侧源列表）。 */
const searchHitSource = ref('')

// —— 右栏 Tab ——
const rightTab = ref<'chapters' | 'profile' | 'draft'>('draft')

// —— 剧情正稿 story.md（正稿 Tab，v0.1.35 功能保留）——
const storyMd = ref('')
const storyLoading = ref(false)
/** story.md 缺失（404 等）→ 空态提示而非错误。 */
const storyMissing = ref(false)
const storyError = ref('')
const genPrompt = ref('')
const genSourceFile = ref('')
const genBusy = ref(false)
const genError = ref('')

/** front-matter 信息条（来源/字数）。 */
const storyFm = computed(() => parseFrontmatter(storyMd.value))
const storyWords = computed(() => storyWordCount(storyMd.value))

/** 源文件管线状态徽章（树 + chaptersIndex.source 派生；embedded=已有向量缓存；
 *  ingested=已入库[v0.1.44 并入件，后端 409 锁同口径镜像]）。 */
function sourceStatus(path: string): {
  cleaned: boolean
  chapterized: boolean
  embedded: boolean
  ingested: boolean
} {
  return storySourceStatus(tree.value, path, chaptersIndex.value?.source ?? null)
}

/**
 * 选中源是否已分章（v0.1.44 向量化前置门——镜像后端 story/embed 400 语义）：
 * 章节清单存在且非空；index 有 source 字段时须与选中源同基名（该源无对应
 * 章节即未分章）；index 无 source 字段（早期产物）存在即过。
 */
const chaptersReadyForSelected = computed(() => {
  const idx = chaptersIndex.value
  if (!idx || !(idx.chapters ?? []).length) return false
  const src = (idx.source ?? '').trim()
  if (!src) return true // index 无 source 字段：存在即过（宽松兼容）
  return (
    !!selectedSource.value && sourceBaseName(src) === sourceBaseName(selectedSource.value)
  )
})

/** 向量化钮禁用 tooltip（未分章引导——与后端 400 冻结文案同款；''=不拦）。 */
const needChaptersTip = computed(() =>
  chaptersReadyForSelected.value ? '' : t('film.storyNeedChaptersTip'),
)

/** 源是否已入库（v0.1.44 并入件——镜像后端 clean/chapterize 409 锁口径：
 *  chapters/index.json 存在且指向该源；无 source 字段存在即算）。 */
function isSourceIngested(path: string): boolean {
  return storySourceStatus(tree.value, path, chaptersIndex.value?.source ?? null).ingested
}

/** 选中源已入库 → 清理/分章禁用 tooltip（后端 409 文案精简版；''=不拦）。
 *  向量化/人物梳理不受锁（向量化正是入库后的操作；梳理支线只读）。 */
const ingestedTip = computed(() =>
  selectedSource.value && isSourceIngested(selectedSource.value)
    ? t('film.storyIngestedTip')
    : '',
)

async function loadSources(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  try {
    // v0.1.39.1 归一：GET :id/files 是 {root, files} 信封——统一走 hubFileEntries
    // 三态防御（信封 / 裸数组 / undefined）。
    const files = hubFileEntries(await filmListFiles(pid))
    tree.value = files
    sources.value = storySources(files)
    sourcesError.value = ''
  } catch (e) {
    tree.value = []
    sources.value = []
    sourcesError.value = ctx ? ctx.errMsg(e) : String(e)
  }
}

/** 读 hub 树内文本文件（content / b64 双契约归一）。 */
async function fetchText(path: string): Promise<string> {
  const pid = ctx?.project.value?.id
  if (!pid) return ''
  const env: FilmFileContent = await filmGetFile(pid, path)
  return filmContentText(env)
}

async function loadChapters(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx) return
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/chapters/index.json')
    const idx = JSON.parse(filmContentText(env)) as FilmChapterIndex
    chaptersIndex.value = Array.isArray(idx.chapters) ? idx : { chapters: [] }
    chaptersError.value = ''
    // 展开态与正文缓存失效（重新分章后章号可能错位——全收起重拉）
    expandedChapter.value = null
    chapterTexts.clear()
  } catch (e) {
    // 无 index = 未分章（空态），其余如实展示
    chaptersIndex.value = null
    chapterTexts.clear()
    const m = ctx.errMsg(e)
    chaptersError.value = /404|not found/i.test(m) ? '' : t('film.storyChapterLoadFailed') + m
  }
}

async function loadProfiles(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx) return
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/characters-profile.json')
    const arr = JSON.parse(filmContentText(env)) as FilmCharacterProfile[]
    profiles.value = Array.isArray(arr) ? arr : []
    profilesError.value = ''
  } catch (e) {
    profiles.value = []
    const m = ctx.errMsg(e)
    profilesError.value = /404|not found/i.test(m) ? '' : m
  }
}

async function viewSource(path: string): Promise<void> {
  selectedSource.value = path
  sourceText.value = ''
  sourceTruncated.value = false
  sourceLoading.value = true
  try {
    const text = await fetchText(path)
    const clamped = clampForDisplay(text, 50_000)
    sourceText.value = clamped.text
    sourceTruncated.value = clamped.truncated
  } catch (e) {
    sourceText.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    sourceLoading.value = false
  }
}

/** 章节卡展开/收起（正文懒加载：首次展开读 ch-<NN>.md 缓存到 Map）。 */
async function toggleChapter(ch: FilmChapterEntry): Promise<void> {
  if (expandedChapter.value === ch.no) {
    expandedChapter.value = null
    return
  }
  expandedChapter.value = ch.no
  if (!chapterTexts.has(ch.no)) {
    chapterLoadingNos.add(ch.no)
    try {
      const raw = await fetchText(
        ch.file || `story/chapters/ch-${String(ch.no).padStart(2, '0')}.md`,
      )
      // 去头部展示（front-matter 元数据已在卡头呈现，正文只留 body）
      const m = /^---\r?\n[\s\S]*?\r?\n---\r?\n?/.exec(raw)
      chapterTexts.set(ch.no, m ? raw.slice(m[0].length) : raw)
    } catch (e) {
      chapterTexts.set(ch.no, ctx ? ctx.errMsg(e) : String(e))
    } finally {
      chapterLoadingNos.delete(ch.no)
    }
  }
}

/** 章节卡「从此章生成分镜」：预填分镜面板章节范围（pending 注册表式）+
 *  跳分镜页（StoryboardGenPanel 挂载时消费——下拉选中该章+展开面板）。 */
function sbFromChapter(ch: FilmChapterEntry): void {
  if (!ctx) return
  ctx.pendingStoryboardChapter.value = String(ch.no)
  ctx.setView('storyboard')
}

async function importFile(file: File | undefined | null): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!file || !pid || !ctx) return
  if (file.size > IMPORT_LIMIT_MB * 1024 * 1024) {
    sourcesError.value = t('film.storyImportMax', { mb: IMPORT_LIMIT_MB })
    return
  }
  importing.value = true
  importHint.value =
    file.size > IMPORT_HINT_MB * 1024 * 1024
      ? t('film.storyImportBig', { mb: (file.size / 1024 / 1024).toFixed(0) })
      : ''
  sourcesError.value = ''
  importNotice.value = ''
  try {
    // v0.1.39 编码兼容：UTF-8 直通 / GB18030 转码后再 b64 上传（后端只收
    // UTF-8；GBK/GB2312/GB18030 中文 txt 不再 400）。
    const buf = await file.arrayBuffer()
    let decoded: DecodedSource
    try {
      decoded = decodeSourceBytes(buf)
    } catch {
      throw new Error(t('film.storyImportNotText'))
    }
    await filmStoryImport(pid, {
      filename: file.name,
      content_b64: bytesToB64(decoded.bytes),
      author: ctx.author.value,
    })
    if (decoded.transcoded) importNotice.value = t('film.storyImportTranscoded')
    await loadSources()
    await ctx.refreshCollab()
    // v0.1.11 提交类：导入成功全局反馈（大文件 b64 上传耗时——成败都该可见）
    toast.success(t('toast.imported', { name: file.name }))
  } catch (err) {
    const msg = `${t('film.storyImportFailed')}${ctx.errMsg(err)}`
    sourcesError.value = msg
    toast.error(msg)
  } finally {
    importing.value = false
    importHint.value = ''
  }
}

async function onImportFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement
  const file = input.files?.[0]
  input.value = ''
  await importFile(file)
}

async function onDrop(e: DragEvent): Promise<void> {
  dragOver.value = false
  const file = e.dataTransfer?.files?.[0]
  if (file) await importFile(file)
}

// —— 管线四步（202 任务进任务中心；终态经 refreshTick 重载；
//    v0.1.38「项目默认」= 不传 model_ref，走 models.json chat 缺省链；
//    v0.1.7 launchStage 统一入口——按钮与失败行内重试同一条路） ——

function needModel(): boolean {
  if (!ctx) return false
  if (ctx.modelRefFor('chat') || ctx.isProjectDefaultSel('chat')) return true
  pipelineError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.storyNoModel')
  return false
}

/** 管线任务统一进任务中心（v0.1.6）+ v0.1.7 源关联（聚合徽章/耗时数据源）。
 *  v0.1.11：提交失败全局 toast——后端 409（同源互斥兜底）走 warning 透传
 *  后端文案，其余走 error。 */
async function trackTask(stage: string, task: FilmTask | Promise<FilmTask>): Promise<boolean> {
  try {
    const tk = await task
    ctx?.trackFilmTask(tk.id, stage, {
      sourceFile: selectedSource.value,
      createdAt: typeof tk.created_at === 'number' ? tk.created_at : Date.parse(String(tk.created_at ?? '')) || null,
    })
    return true
  } catch (e) {
    const msg = ctx ? ctx.errMsg(e) : String(e)
    pipelineError.value = msg
    if (/409|conflict/i.test(msg)) toast.warning(msg)
    else toast.error(`${t('film.actFailed')}${msg}`)
    return false
  }
}

// —— 源级互斥锁（v0.1.11 前端侧）：同一源文件任一管线任务进行中 → 该源全部
//    管线按钮禁用 + tooltip 说明（「该文件正在执行〈清理中 3/17〉」）；后端
//    另有 409 兜底（trackTask catch → toast.warning）。单按钮 busy（提交在途
//    未登记）保留，两者叠加判定。 ——
/** 选中源的进行中管线任务（互斥依据；null=空闲）。 */
const activeSourceTask = computed<PipelineTaskSnapshot | null>(() => {
  const path = selectedSource.value
  if (!path) return null
  return (
    storyTasks.value.find(
      (x) => x.sourceFile === path && isPipelineTaskActive(x.status),
    ) ?? null
  )
})

/** 运行中任务的阶段词（清理中/分章中/梳理中/向量化中——与聚合徽章同口径）。 */
function runningStageWord(stage: string): string {
  switch (stage) {
    case 'story.clean':
      return t('film.pipeClean')
    case 'story.chapterize':
      return t('film.pipeChapter')
    case 'story.profile':
      return t('film.pipeProfile')
    case 'story.embed':
      return t('film.pipeEmbed')
    default:
      return t('film.taskRunning')
  }
}

/** 互斥 tooltip：〈清理中 3/17〉（有分块进度附 X/Y）。 */
const activeSourceTip = computed(() => {
  const tk = activeSourceTask.value
  if (!tk) return ''
  const counts = parseChunkProgress(tk.log ?? [])
  const state = counts
    ? `${runningStageWord(tk.stage)} ${counts.done}/${counts.total}`
    : runningStageWord(tk.stage)
  return t('film.storySourceBusyTip', { state })
})

/** 各阶段 busy 开关（按钮禁用与 spinner 共口径）。 */
function busyOf(stage: StoryPipelineStage): Ref<boolean> {
  switch (stage) {
    case 'story.clean':
      return cleanBusy
    case 'story.chapterize':
      return chapterizeBusy
    case 'story.profile':
      return profileBusy
    case 'story.embed':
      return embedBusy
  }
}

/** 统一发射：按阶段校验（模型/emb）→ POST → 任务登记（带源关联）。
 *  v0.1.11：源级互斥前置守卫（按钮禁用之外的兜底——失败行重试/竞态点击）。 */
async function launchStage(stage: StoryPipelineStage, sourceFile: string): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!ctx || !pid || !sourceFile) return
  const busy = busyOf(stage)
  if (busy.value) return
  const active = storyTasks.value.find(
    (x) => x.sourceFile === sourceFile && isPipelineTaskActive(x.status),
  )
  if (active) {
    pipelineError.value = t('film.storySourceBusyTip', { state: runningStageWord(active.stage) })
    toast.warning(pipelineError.value)
    return
  }
  if (stage === 'story.clean' && cleanMode.value === 'llm') {
    const useDefault = ctx.isProjectDefaultSel('chat')
    const ref = ctx.modelRefFor('chat')
    if (!ref && !useDefault) {
      pipelineError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.storyNoModel')
      return
    }
  }
  if ((stage === 'story.chapterize' || stage === 'story.profile') && !needModel()) return
  if (stage === 'story.embed' && !embReady.value) {
    pipelineError.value = embTip.value || t('film.storyEmbNotSet')
    return
  }
  // v0.1.44 顺序化前置门（按钮禁用之外的兜底——失败行重试/竞态点击）：
  // 未分章不放行向量化（后端另有 400 硬拦截，文案同款）
  if (stage === 'story.embed' && !chaptersReadyForSelected.value) {
    pipelineError.value = t('film.storyNeedChaptersTip')
    toast.warning(pipelineError.value)
    return
  }
  // v0.1.44 并入件·源入库锁定前置守卫（同上兜底）：已入库源不放行清理/分章
  //（后端另有 409 硬拦截，文案同源）；向量化/人物梳理不在此列
  if ((stage === 'story.clean' || stage === 'story.chapterize') && isSourceIngested(sourceFile)) {
    pipelineError.value = t('film.storyIngestedTip')
    toast.warning(pipelineError.value)
    return
  }
  busy.value = true
  pipelineError.value = ''
  const prevSel = selectedSource.value
  selectedSource.value = sourceFile
  try {
    if (stage === 'story.clean') {
      const ref = cleanMode.value === 'llm' ? ctx.modelRefFor('chat') : null
      await trackTask(
        stage,
        filmStoryClean(pid, {
          source_file: sourceFile,
          mode: cleanMode.value,
          ...(ref ? { model_ref: ref } : {}),
          author: ctx.author.value,
        }),
      )
    } else if (stage === 'story.chapterize') {
      const mr = ctx.modelRefFor('chat')
      await trackTask(
        stage,
        filmStoryChapterize(pid, {
          ...(mr ? { model_ref: mr } : {}),
          source_file: sourceFile,
          author: ctx.author.value,
        }),
      )
    } else if (stage === 'story.profile') {
      const mr = ctx.modelRefFor('chat')
      await trackTask(
        stage,
        filmStoryProfile(pid, {
          ...(mr ? { model_ref: mr } : {}),
          source_file: sourceFile,
          author: ctx.author.value,
        }),
      )
    } else {
      await trackTask(
        stage,
        filmStoryEmbed(pid, {
          source_file: sourceFile,
          author: ctx.author.value,
        }),
      )
    }
  } finally {
    busy.value = false
    selectedSource.value = prevSel || sourceFile
  }
}

/** 按钮路径（操作条）：选中源校验后发射。 */
function runStage(stage: StoryPipelineStage): void {
  if (!selectedSource.value) {
    pipelineError.value = t('film.storyPipelineNeedsSource')
    return
  }
  void launchStage(stage, selectedSource.value)
}

/** 失败行内重试：对失败阶段逐个原参重发（通常单阶段失败）。 */
async function retryFailed(path: string, agg: SourcePipelineAggregate): Promise<void> {
  for (const stage of agg.failedStages) {
    await launchStage(stage, path)
  }
}

// —— 失败行一键提 Issue（v0.1.10：from-task 自动成文 → 协作页）——
/** 提交中标记（源 path 集合；防重复点击）。 */
const issueBusyPaths = ref(new Set<string>())
/** 提交失败文案（源 path → 错误；成功即跳协作页无需提示位）。 */
const issueErrors = ref(new Map<string, string>())

/** 该源最近失败任务的 id（from-task 的 task_id 取数源）。 */
function lastFailedTaskId(path: string): string {
  const mine = tasksFor(path).find((x) => x.status === 'failed' && x.id)
  return mine?.id ?? ''
}

/** 失败源行「提 Issue」：读最近失败任务现场（stage/error/日志尾/源文件）自动
 *  成文（title=[环节] <stage> 失败 + labels=[pipeline-error] + stage 归因）；
 *  成功跳协作页（CollabPage 即落点）。 */
async function fileIssueFromFailedSource(path: string): Promise<void> {
  const pid = ctx?.project.value?.id
  const taskId = lastFailedTaskId(path)
  if (!ctx || !pid || issueBusyPaths.value.has(path)) return
  if (!taskId) {
    issueErrors.value.set(path, t('collab.fromTaskNoTask'))
    return
  }
  issueBusyPaths.value.add(path)
  issueErrors.value.delete(path)
  try {
    await filmIssueFromTask(pid, {
      task_id: taskId,
      author: ctx.author.value,
    }).then((res) => {
      // v0.1.11 跳转类：成功 toast + 跳转（此前「落点即反馈」无过程提示）
      toast.success(t('collab.fromTaskDone', { n: (res as { issue?: { iid?: number } }).issue?.iid ?? 0 }))
    })
    // 成功 → 跳协作页（Issue 展开即见自动成文正文）
    ctx.setView('collab')
  } catch (e) {
    const msg = ctx.errMsg(e)
    issueErrors.value.set(path, msg)
    toast.error(t('collab.fromTaskFailed') + msg)
  } finally {
    issueBusyPaths.value.delete(path)
  }
}

/** 语义检索（同步 200：query 向量化 → 向量缓存余弦扫；选中源优先，否则树内
 *  最近向量缓存）。结果卡点击「定位原文」= 左栏加载该源。 */
async function runSearch(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!ctx || !pid || searchBusy.value) return
  const q = searchQuery.value.trim()
  if (!q) {
    searchError.value = t('film.storySearchNeedQuery')
    return
  }
  searchBusy.value = true
  searchError.value = ''
  try {
    const res = await filmStorySearch(pid, {
      query: q,
      // 选中源有向量缓存则限定它；未选中/未向量化走后端「树内最近缓存」缺省
      ...(selectedSource.value && sourceStatus(selectedSource.value).embedded
        ? { source_file: selectedSource.value }
        : {}),
    })
    searchResults.value = res.results ?? []
    searchHitSource.value = res.source ?? ''
    if (!searchResults.value.length) searchError.value = t('film.storySearchEmpty')
  } catch (e) {
    searchResults.value = []
    searchHitSource.value = ''
    searchError.value = t('film.storySearchFailed') + ctx.errMsg(e)
  } finally {
    searchBusy.value = false
  }
}

/** 人物档案 → 定妆对象（desc = 外貌+性格拼接；作者自动认领）。 */
async function castToCasting(p: FilmCharacterProfile): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || !ctx || castingBusy.has(p.name) || castingDone.has(p.name)) return
  castingBusy.add(p.name)
  castingError.delete(p.name)
  try {
    const desc = [p.appearance, p.personality].filter((s) => (s ?? '').trim()).join('；')
    await filmCreateCasting(pid, 'characters', {
      name: p.name,
      desc: desc || t('film.storyToCasting'),
      author: ctx.author.value,
    })
    castingDone.add(p.name)
    await ctx.refreshCollab()
    // v0.1.11 跳转类：转定妆成功 toast（按钮 ✓ 态之外的全局确认）
    toast.success(t('film.storyToCastingDone'))
  } catch (e) {
    const msg = ctx.errMsg(e)
    castingError.set(p.name, msg)
    toast.error(t('film.storyToCastingFailed') + msg)
  } finally {
    castingBusy.delete(p.name)
  }
}

// —— 剧情正稿（正稿 Tab；AI 生成走 chat model_ref）——

async function loadStory(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  storyLoading.value = true
  try {
    const env: FilmFileContent = await filmGetFile(pid, 'story/story.md')
    storyMd.value = filmContentText(env)
    storyMissing.value = false
    storyError.value = ''
  } catch (e) {
    storyMd.value = ''
    const m = ctx ? ctx.errMsg(e) : String(e)
    // 无 story.md = 尚未生成（空态），其余如实展示
    if (/404|not found/i.test(m)) storyMissing.value = true
    else storyError.value = t('film.storyLoadFailed') + m
  } finally {
    storyLoading.value = false
  }
}

/** AI 写剧情（model_ref 复用 chat 面 + prompt + 可选参考原文；「项目默认」=
 * 不传 model_ref 走 models.json chat 缺省链）。 */
async function generateStory(): Promise<void> {
  if (!ctx || genBusy.value) return
  const useDefault = ctx.isProjectDefaultSel('chat')
  const ref = ctx.modelRefFor('chat')
  if (!useDefault && !ref) {
    genError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  genBusy.value = true
  genError.value = ''
  try {
    const task = await filmStoryGenerate(ctx.project.value!.id, {
      ...(ref ? { model_ref: ref } : {}),
      ...(genPrompt.value.trim() ? { prompt: genPrompt.value.trim() } : {}),
      ...(genSourceFile.value ? { source_file: genSourceFile.value } : {}),
      author: ctx.author.value,
    })
    // 任务中心登记（v0.1.6：stage 显式传 story，与管线按钮同路）
    ctx.trackFilmTask(task.id, 'story')
  } catch (e) {
    const msg = ctx.errMsg(e)
    genError.value = t('film.storyGenFailed') + msg
    // v0.1.11：提交失败全局可见（409 冲突走 warning）
    if (/409|conflict/i.test(msg)) toast.warning(msg)
    else toast.error(t('film.storyGenFailed') + msg)
  } finally {
    genBusy.value = false
  }
}

function loadAll(): void {
  void loadSources()
  void loadChapters()
  void loadProfiles()
  void loadStory()
}

onMounted(loadAll)

// 任务终态 / 项目刷新 → 重载本页数据
watch(
  () => ctx?.refreshTick.value,
  () => loadAll(),
)
</script>

<template>
  <div class="fh-page nx-page">
    <FlowPageHead stage="story" :title="t('film.storyTitle')">
      <template #actions>
        <NxThemeToggle />
        <select v-model="genSourceFile" class="fh-select nx-select" :title="t('film.storySourceFile')">
          <option value="">{{ t('film.storySourceNone') }}</option>
          <option v-for="s in sources" :key="s.path" :value="s.path">
            {{ fileBasename(s.path) }}
          </option>
        </select>
        <select
          v-if="ctx"
          v-model="ctx.modelSel.chat"
          class="fh-select nx-select"
          :title="t('film.model')"
        >
          <option :value="PROJECT_DEFAULT_KEY">
            🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('chat') ? ' · ' + ctx.defaultModelSummary('chat') : '' }}
          </option>
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>
            {{ t('film.noRunningLlm') }}
          </option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">
              {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
            </option>
          </optgroup>
        </select>
        <NxButton
          variant="primary"
          size="sm"
          :loading="genBusy"
          :disabled="!ctx?.project.value || !ctx?.modelSelReady('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="generateStory"
        >{{ genBusy ? t('film.btnBusy') : t('film.storyGenerate') }}</NxButton>
      </template>
    </FlowPageHead>

    <div class="fh-two-col fh-page-scroll">
      <!-- 左：原文素材 + 管线操作条 -->
      <div class="fh-col">
        <NxCard
          class="fh-card sp-srccard"
          :class="{ 'is-dragover': dragOver }"
          @dragover.prevent="dragOver = true"
          @dragleave="dragOver = false"
          @drop.prevent="onDrop"
        >
          <div class="nx-card__head fh-card-head">
            <span>{{ t('film.storySources') }}</span>
            <span class="nx-badge nx-badge--neutral nx-badge--sm">{{ sources.length }}</span>
            <div class="fh-head-actions">
              <NxButton
                size="sm"
                :loading="importing"
                :disabled="!ctx?.project.value"
                @click="importInput?.click()"
              >{{ importing ? t('film.btnBusy') : t('film.storyImport') }}</NxButton>
              <input
                ref="importInput"
                type="file"
                accept=".txt,.md,text/plain,text/markdown"
                class="fh-hidden-input"
                @change="onImportFile"
              />
            </div>
          </div>
          <div class="nx-card__body fh-card-body">
            <div v-if="importHint" class="fh-muted fh-small">{{ importHint }}</div>
            <div v-if="importNotice" class="nx-alert nx-alert--success">✓ {{ importNotice }}</div>
            <div v-if="sourcesError" class="nx-alert nx-alert--danger" role="alert">{{ sourcesError }}</div>
            <div v-if="!sources.length && !sourcesError && !importHint" class="nx-empty">
              {{ t('film.storySourceEmpty') }}
            </div>
            <div
              v-for="s in sources"
              :key="s.path"
              class="fh-row nx-row is-clickable"
              :class="{
                'is-active': selectedSource === s.path,
                'is-failed': pipeOf(s.path).key === 'failed',
              }"
              :title="s.path"
              tabindex="0"
              role="button"
              :aria-label="fileBasename(s.path)"
              @click="viewSource(s.path)"
              @keydown.enter="viewSource(s.path)"
            >
              <svg class="story-src-fileicon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z" />
                <path d="M14 2v6h6" />
              </svg>
              <span class="story-src-name fh-ellipsis">{{ fileBasename(s.path) }}</span>
              <!-- 聚合状态徽章（排队/清理中/分章中/梳理中/向量化中/成功/失败 +
                   可解析时的 X/Y）；hover/点按展开任务明细 popover -->
              <NxPopover
                v-if="pipeOf(s.path).key !== 'idle'"
                trigger="hover"
                placement="bottom-end"
                :title="`${fileBasename(s.path)} · ${t('film.storyPipeline')}`"
              >
                <template #trigger>
                  <NxBadge
                    :variant="pipeVariant(pipeOf(s.path))"
                    dot
                    :pulse="pipeOf(s.path).key === 'running'"
                  >{{ pipeWord(pipeOf(s.path)) }}<span
                      v-if="pipeXy(pipeOf(s.path))"
                      class="story-src-xy"
                    >{{ pipeXy(pipeOf(s.path)) }}</span></NxBadge>
                </template>
                <div
                  v-for="task in tasksFor(s.path)"
                  :key="task.id"
                  class="story-pipe-task"
                >
                  <div class="story-pipe-task__top">
                    <span class="story-pipe-task__stage">{{ taskStageWord(task.stage) }}</span>
                    <NxBadge size="sm" :variant="taskVariant(task.status)" dot :pulse="task.status === 'running'">
                      {{ taskWord(task.status) }}
                    </NxBadge>
                    <span class="story-pipe-task__meta">{{ taskMeta(task, nowMs) }}</span>
                  </div>
                  <NxProgress
                    v-if="parseChunkProgress(task.log ?? [])"
                    size="sm"
                    :done="parseChunkProgress(task.log ?? [])!.done"
                    :total="parseChunkProgress(task.log ?? [])!.total"
                    :state="task.status === 'failed' ? 'danger' : task.status === 'completed' ? 'success' : 'accent'"
                  />
                  <div v-if="task.status === 'failed' && task.error" class="story-pipe-task__err">
                    {{ task.error }}
                  </div>
                </div>
              </NxPopover>
              <!-- 产物徽章（树派生的完成态标记）；已入库=最高显示档（绿实底，
                   分章定稿冻结——清理/分章锁定，tooltip 指重处理出口） -->
              <span
                v-if="sourceStatus(s.path).cleaned"
                class="fh-pill fh-pill-ok fh-pill-mini"
                >{{ t('film.storyStatusCleaned') }}</span>
              <span
                v-if="sourceStatus(s.path).ingested"
                class="fh-pill fh-pill-ingested fh-pill-mini"
                :title="t('film.storyIngestedTip')"
                >{{ t('film.storyIngested') }}</span>
              <span
                v-else-if="sourceStatus(s.path).chapterized"
                class="fh-pill fh-pill-violet fh-pill-mini"
                >{{ t('film.storyStatusChapterized') }}</span>
              <span
                v-if="sourceStatus(s.path).embedded"
                class="fh-pill fh-pill-blue fh-pill-mini"
                :title="t('film.storySearchPh')"
              >🧠 {{ t('film.storyEmbedded') }}</span>
              <span class="story-src-bytes fh-mono">{{ fmtBytes(s.bytes) }}</span>
              <!-- 失败行内重试（对失败阶段原参重发） + 一键提 Issue（from-task） -->
              <NxButton
                v-if="pipeOf(s.path).key === 'failed'"
                variant="destructive"
                size="sm"
                class="story-retry"
                @click.stop="retryFailed(s.path, pipeOf(s.path))"
              >↻ {{ t('film.retry') }}</NxButton>
              <NxButton
                v-if="pipeOf(s.path).key === 'failed'"
                variant="secondary"
                size="sm"
                class="story-issue"
                :title="t('collab.fromTaskTip')"
                :disabled="issueBusyPaths.has(s.path)"
                @click.stop="fileIssueFromFailedSource(s.path)"
              >🐞 {{ issueBusyPaths.has(s.path) ? '…' : t('collab.fromTask') }}</NxButton>
              <span
                v-if="issueErrors.get(s.path)"
                class="nx-alert nx-alert--danger story-issue-err"
                role="alert"
              >{{ issueErrors.get(s.path) }}</span>
            </div>
            <!-- 选中原文内容（pre 等宽；超长截断） -->
            <div v-if="selectedSource" class="fh-pre-box sp-prebox">
              <div class="fh-muted fh-small fh-ellipsis sp-path-row">
                {{ selectedSource }}
                <span v-if="sourceTruncated" class="fh-pill fh-pill-amber fh-pill-mini sp-ml-1-5">
                  {{ t('film.storyTruncated', { n: 50000 }) }}
                </span>
              </div>
              <pre v-if="sourceLoading" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
              <pre v-else class="fh-pre">{{ sourceText || '—' }}</pre>
            </div>
          </div>
        </NxCard>

        <!-- emb 不可用提示行（管线操作条上方一行小字；可关闭） -->
        <div v-if="selectedSource && !embReady && !embHintClosed" class="story-emb-hint" role="note">
          <span class="story-emb-hint-text">🧠 {{ embHintText }}</span>
          <NxButton
            variant="ghost"
            size="sm"
            :title="t('film.cancel')"
            :aria-label="t('film.cancel')"
            @click="embHintClosed = true"
          >×</NxButton>
        </div>

        <!-- 管线操作条（选中源后显示） -->
        <NxCard v-if="selectedSource" class="fh-card">
          <div class="nx-card__head fh-card-head">
            <span>{{ t('film.storyPipeline') }}</span>
            <span class="fh-muted fh-small fh-ellipsis" :title="t('film.storyPipelineHint')">
              {{ t('film.storyPipelineHint') }}
            </span>
          </div>
          <div class="nx-card__body nx-card__body--row fh-card-body">
            <!-- v0.1.44 顺序引导：按钮按管线序 导入(上方源卡)→清理→分章→向量化
                 排布（人物梳理为可选支线排后）；未分章时向量化禁用+tooltip
                 （文案与后端 400 同款）；该源任一管线任务进行中全禁（互斥）；
                 并入件：已入库源清理/分章禁用+tooltip（后端 409 精简同源），
                 向量化/人物梳理照常 -->
            <select
              v-model="cleanMode"
              class="fh-select nx-select sp-auto"
              :title="t('film.storyCleanMode')"
              :disabled="!!activeSourceTask || !!ingestedTip"
            >
              <option value="rules">{{ t('film.storyCleanModeRules') }}</option>
              <option value="llm">{{ t('film.storyCleanModeLlm') }}</option>
            </select>
            <NxButton
              size="sm"
              :loading="cleanBusy"
              :disabled="!ctx?.project.value || !!activeSourceTask || !!ingestedTip"
              :title="activeSourceTip || ingestedTip || undefined"
              @click="runStage('story.clean')"
              >{{ cleanBusy ? t('film.btnBusy') : t('film.storyClean') }}</NxButton>
            <NxButton
              size="sm"
              :loading="chapterizeBusy"
              :disabled="!ctx?.project.value || !!activeSourceTask || !!ingestedTip"
              :title="activeSourceTip || ingestedTip || undefined"
              @click="runStage('story.chapterize')"
              >{{ chapterizeBusy ? t('film.btnBusy') : t('film.storyChapterize') }}</NxButton>
            <!-- 🧠 向量化（v0.1.44 前置分章：未分章禁用+「请先分章」tooltip
                 [后端 400 同款文案]；emb 位未配/不可用同样禁用；源互斥叠加） -->
            <NxButton
              size="sm"
              :loading="embedBusy"
              :disabled="!ctx?.project.value || !embReady || !chaptersReadyForSelected || !!activeSourceTask"
              :title="activeSourceTip || needChaptersTip || (embReady ? '' : embTip)"
              @click="runStage('story.embed')"
            >{{ embedBusy ? t('film.btnBusy') : t('film.storyEmbed') }}</NxButton>
            <NxButton
              size="sm"
              :loading="profileBusy"
              :disabled="!ctx?.project.value || !!activeSourceTask"
              :title="activeSourceTip || undefined"
              @click="runStage('story.profile')"
            >{{ profileBusy ? t('film.btnBusy') : t('film.storyProfile') }}</NxButton>
            <span v-if="pipelineError" class="nx-alert nx-alert--danger sp-full">{{ pipelineError }}</span>
          </div>
        </NxCard>
      </div>

      <!-- 右：结构区三 Tab（章节 / 人物档案 / 正稿）；顶部语义搜索 -->
      <NxCard class="fh-card fh-col">
        <!-- 语义搜索框（「章节」Tab 上方：query 向量化 → 向量缓存余弦扫 → 结果块卡）；
             布局走语义工具类（间距刻度 4px 系：gap-2/px-4/pt-3） -->
        <div class="flex items-center gap-2 px-4 pt-3">
          <input
            v-model="searchQuery"
            class="fh-input nx-input sp-grow"
            :placeholder="t('film.storySearchPh')"
            :disabled="searchBusy"
            @keyup.enter="runSearch"
          />
          <NxButton
            size="sm"
            :loading="searchBusy"
            :disabled="!ctx?.project.value"
            @click="runSearch"
          >{{ searchBusy ? t('film.btnBusy') : '🔍 ' + t('film.storySearchBtn') }}</NxButton>
        </div>
        <div v-if="searchError" class="nx-alert nx-alert--danger sp-alert-row">{{ searchError }}</div>
        <div v-if="searchResults.length" class="story-search-results">
          <div
            v-for="h in searchResults"
            :key="h.i"
            class="story-search-hit"
            tabindex="0"
            role="button"
            :title="t('film.storySearchLocate')"
            @click="searchHitSource && viewSource(searchHitSource)"
            @keydown.enter="searchHitSource && viewSource(searchHitSource)"
          >
            <div class="fh-muted fh-small fh-mono sp-hit-meta">
              L{{ h.start_line ?? 0 }} <span class="fh-muted">· {{ h.score.toFixed(3) }}</span>
            </div>
            <div class="story-search-text">{{ h.text }}</div>
          </div>
        </div>
        <div class="fh-tabs sp-tabs">
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'chapters' }"
            type="button"
            @click="rightTab = 'chapters'"
          >📖 {{ t('film.storyTabChapters') }}
            <span v-if="chaptersIndex?.chapters?.length" class="fh-pill fh-pill-muted fh-pill-mini">
              {{ t('film.storyChaptersCount', { n: chaptersIndex.chapters.length }) }}
            </span>
          </button>
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'profile' }"
            type="button"
            @click="rightTab = 'profile'"
          >👥 {{ t('film.storyTabProfile') }}
            <span v-if="profiles.length" class="fh-pill fh-pill-muted fh-pill-mini">
              {{ t('film.storyProfileCount', { n: profiles.length }) }}
            </span>
          </button>
          <button
            class="fh-tab"
            :class="{ 'is-active': rightTab === 'draft' }"
            type="button"
            @click="rightTab = 'draft'"
          >✍ {{ t('film.storyTabDraft') }}</button>
        </div>

        <div class="nx-card__body fh-card-body sp-grow-col">
          <!-- Tab 1：章节（v0.1.44 卡片化：每章一卡——章号徽章+标题+字数+auto
               徽章[自动分段]+点开正文折叠展开+「从此章生成分镜」快捷钮） -->
          <template v-if="rightTab === 'chapters'">
            <div class="fh-card-head sp-subhead">
              <span v-if="chaptersIndex?.auto" class="fh-pill fh-pill-amber fh-pill-mini">
                {{ t('film.storyAutoSeg') }}
              </span>
              <div class="fh-head-actions">
                <NxButton variant="ghost" size="sm" @click="loadChapters">↻</NxButton>
              </div>
            </div>
            <div v-if="chaptersError" class="nx-alert nx-alert--danger">{{ chaptersError }}</div>
            <div v-if="!chaptersIndex?.chapters?.length && !chaptersError" class="nx-empty">
              {{ t('film.storyChaptersEmpty') }}
            </div>
            <div
              v-for="ch in chaptersIndex?.chapters ?? []"
              :key="ch.no"
              class="story-ch-card"
              :class="{ 'is-open': expandedChapter === ch.no }"
            >
              <div
                class="story-ch-head nx-row is-clickable"
                role="button"
                tabindex="0"
                :aria-expanded="expandedChapter === ch.no"
                :aria-label="ch.title"
                @click="toggleChapter(ch)"
                @keydown.enter="toggleChapter(ch)"
              >
                <span class="story-ch-no fh-mono">{{ ch.no }}</span>
                <span class="story-ch-title fh-ellipsis" :title="ch.title">{{ ch.title }}</span>
                <span v-if="ch.auto" class="fh-pill fh-pill-amber fh-pill-mini">{{ t('film.storyAutoSeg') }}</span>
                <span class="fh-muted fh-small">{{ t('film.storyChapterWords', { n: ch.words ?? 0 }) }}</span>
                <span class="story-ch-caret fh-muted" aria-hidden="true">{{ expandedChapter === ch.no ? '▾' : '▸' }}</span>
              </div>
              <!-- 卡体：展开态 = 正文 pre（懒加载）+「从此章生成分镜」快捷钮 -->
              <div v-if="expandedChapter === ch.no" class="story-ch-body">
                <pre v-if="chapterLoadingNos.has(ch.no)" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
                <pre v-else class="fh-pre">{{ chapterTexts.get(ch.no) || '—' }}</pre>
                <div class="story-ch-actions">
                  <NxButton
                    variant="secondary"
                    size="sm"
                    :title="t('film.storySbFromChapterTip', { n: ch.no })"
                    @click="sbFromChapter(ch)"
                  >🎬 {{ t('film.storySbFromChapter') }}</NxButton>
                </div>
              </div>
            </div>
          </template>

          <!-- Tab 2：人物档案 -->
          <template v-else-if="rightTab === 'profile'">
            <div class="fh-card-head sp-subhead">
              <div class="fh-head-actions">
                <NxButton variant="ghost" size="sm" @click="loadProfiles">↻</NxButton>
              </div>
            </div>
            <div v-if="profilesError" class="nx-alert nx-alert--danger">{{ profilesError }}</div>
            <div v-if="!profiles.length && !profilesError" class="nx-empty">
              {{ t('film.storyProfileEmpty') }}
            </div>
            <div v-for="p in profiles" :key="p.name" class="story-profile-card">
              <div class="story-profile-head">
                <span class="story-profile-name">{{ p.name }}</span>
                <span v-if="p.gender || p.age" class="fh-muted fh-small">{{ [p.gender, p.age].filter(Boolean).join(' · ') }}</span>
                <span
                  v-if="p.first_chapter"
                  class="fh-pill fh-pill-blue fh-pill-mini"
                >{{ t('film.storyProfileFirst', { n: p.first_chapter }) }}</span>
                <NxButton
                  :variant="castingDone.has(p.name) ? 'secondary' : 'primary'"
                  size="sm"
                  class="sp-mlauto"
                  :disabled="castingBusy.has(p.name) || castingDone.has(p.name)"
                  @click="castToCasting(p)"
                >{{ castingDone.has(p.name) ? '✓ ' + t('film.storyToCastingDone') : t('film.storyToCasting') }}</NxButton>
              </div>
              <div v-if="castingError.get(p.name)" class="nx-alert nx-alert--danger">
                {{ t('film.storyToCastingFailed') }}{{ castingError.get(p.name) }}
              </div>
              <div v-if="p.aliases?.length" class="story-profile-line">
                <span class="fh-muted fh-small">{{ t('film.storyProfileAliases') }}：</span>
                <span
                  v-for="a in p.aliases"
                  :key="a"
                  class="fh-pill fh-pill-muted fh-pill-mini"
                >{{ a }}</span>
              </div>
              <div v-if="p.appearance" class="story-profile-line fh-small">🎭 {{ p.appearance }}</div>
              <div v-if="p.personality" class="story-profile-line fh-small">🧠 {{ p.personality }}</div>
              <div v-if="p.relations?.length" class="story-profile-line fh-small">
                <span class="fh-muted">{{ t('film.storyProfileRelations') }}：</span>
                <span v-for="(r, i) in p.relations" :key="i" class="fh-pill fh-pill-blue fh-pill-mini sp-mr-1">
                  {{ r.name }}{{ r.relation ? `·${r.relation}` : '' }}
                </span>
              </div>
              <div v-if="p.arc" class="story-profile-line fh-small">
                <span class="fh-muted">{{ t('film.storyProfileArc') }}：</span>{{ p.arc }}
              </div>
            </div>
          </template>

          <!-- Tab 3：正稿 story.md（v0.1.35 功能保留） -->
          <template v-else>
            <div class="fh-card-head sp-subhead">
              <span v-if="storyMd" class="fh-pill fh-pill-muted">
                {{ t('film.storyWords', { n: storyWords }) }}
              </span>
              <span
                v-if="storyFm.source"
                class="fh-pill fh-pill-blue fh-ellipsis sp-maxw"
                :title="storyFm.source"
              >{{ t('film.storySourceLabel', { source: storyFm.source }) }}</span>
              <div class="fh-head-actions">
                <NxButton
                  v-if="storyMd || storyMissing"
                  variant="ghost"
                  size="sm"
                  :disabled="storyLoading"
                  @click="loadStory"
                >↻</NxButton>
              </div>
            </div>
            <label class="fh-field">
              <span class="fh-field-label">{{ t('film.storyPrompt') }}</span>
              <textarea v-model="genPrompt" rows="2" :placeholder="t('film.storyPromptPh')" />
            </label>
            <div v-if="genError" class="nx-alert nx-alert--danger">{{ genError }}</div>
            <div v-if="storyError" class="nx-alert nx-alert--danger">{{ storyError }}</div>
            <div v-if="storyLoading" class="nx-empty">{{ t('film.loading') }}</div>
            <div v-else-if="!storyMd" class="nx-empty">
              {{ storyMissing || !storyError ? t('film.storyDraftEmpty') : '—' }}
            </div>
            <div v-else class="fh-pre-box sp-prebox">
              <pre class="fh-pre">{{ storyMd }}</pre>
            </div>
          </template>
        </div>
      </NxCard>
    </div>
  </div>
</template>

<style scoped>
/* —— 组件局部布局类（token 间距刻度：4px 系；视觉规则在 design/theme.css） —— */
/* 左栏源卡占位（上下两卡 50/50 弹性） */
.sp-srccard {
  flex: 1 1 50%;
  min-height: 0;
}
/* 卡内子头（Tab 顶部行：无底线紧凑态） */
.sp-subhead {
  padding: 0 0 8px;
  border-bottom: none;
}
/* pre 展示盒（选中原文/章节正文：撑满剩余） */
.sp-prebox {
  flex: 1;
  min-height: 120px;
}
.sp-path-row {
  margin-bottom: 6px;
}
.sp-ml-1-5 {
  margin-left: 6px;
}
.sp-auto {
  width: auto;
}
.sp-full {
  flex-basis: 100%;
}
.sp-grow {
  flex: 1;
  min-width: 0;
}
.sp-grow-col {
  flex: 1;
  min-height: 0;
}
.sp-alert-row {
  margin: 8px 16px 0;
}
.sp-hit-meta {
  margin-bottom: 2px;
}
.sp-tabs {
  padding: 0 16px;
}
.sp-mlauto {
  margin-left: auto;
}
.sp-mr-1 {
  margin-right: 4px;
}
.sp-maxw {
  max-width: 200px;
}

/* 源行 X/Y 附注（mono 小字，不抢徽章主词） */
.story-src-xy {
  font-family: var(--nx-font-mono, monospace);
  font-size: 11px;
  opacity: 0.85;
}

/* —— 章节卡（v0.1.44 卡片化：每章一卡，点卡头折叠展开正文） —— */
.story-ch-card {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  background: var(--nx-bg-card);
  flex-shrink: 0;
  overflow: hidden;
}
.story-ch-card.is-open {
  border-color: var(--nx-line-strong);
  background: var(--nx-bg-subtle);
}
.story-ch-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 12px;
  cursor: pointer;
}
.story-ch-head:hover {
  background: var(--nx-bg-hover);
}
.story-ch-head:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: -2px;
}
/* 章号徽章（mono 方块） */
.story-ch-no {
  flex-shrink: 0;
  min-width: 26px;
  padding: 1px 6px;
  border-radius: var(--nx-radius-badge);
  border: 1px solid var(--nx-info-border);
  background: var(--nx-info-bg);
  color: var(--nx-info);
  text-align: center;
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-semibold);
}
.story-ch-title {
  flex: 1;
  min-width: 0;
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
}
.story-ch-caret {
  flex-shrink: 0;
  font-size: var(--nx-font-size-xs);
}
/* 卡体：正文 pre（等宽；撑满卡宽）+ 快捷钮行 */
.story-ch-body {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 0 12px 10px;
  border-top: 0.5px solid var(--nx-line-divider);
}
.story-ch-body .fh-pre {
  max-height: 240px;
  overflow: auto;
  margin: 8px 0 0;
}
.story-ch-actions {
  display: flex;
  justify-content: flex-end;
}

/* 失败源行「提 Issue」（v0.1.10：from-task 一键成文；与重试钮并排） */
.story-issue {
  flex-shrink: 0;
}
.story-issue-err {
  flex-basis: 100%;
  font-size: var(--nx-font-size-xs, 12px);
}
</style>
