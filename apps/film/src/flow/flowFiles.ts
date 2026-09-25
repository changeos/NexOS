// =============================================================================
// flowFiles.ts —— 项目文件树派生与流程纯逻辑（FilmHub，v0.1.35）。
//
// 后端「文件即真值」：dist/ 成品版本、cache/ 半成品、sources/ 剧情原文、
// extraction.json 提取报告均从 GET :id/files 树派生；frontmatter / 字数 /
// 视图槽位 / BGM 表单校验为纯函数——页面与 happy-dom 冒烟共用同一代码路径。
// =============================================================================

import type { FilmCastType, FilmCastingObject, FilmFileEntry, FilmProject } from '../api'
import { isFlowView, type FilmStage, type FlowView } from './flowTypes'

// —— 路径 helpers ——

// —— files 树响应归一（v0.1.39.1）——
//
// 崩点根因：GET :id/files 实际返回信封 `{root, files:[{path,bytes,kind,…}]}`
// （后端 v0.1.37.1 起），而前端按裸数组消费 → `e.filter is not a function`
// （StoryPage 源列表崩，操作条/树卡连坐空）。归一纯函数三态防御：信封提取 /
// 裸数组直通 / undefined 空数组——条目再宽容字符串（路径）与对象（取 .path）
// 两种形态，页面与冒烟共用同一代码路径。

/** GET :id/files 树响应 → 条目数组（信封 {files} 提取 / 裸数组直通 / 其余空；
 *  字符串条目归一为 {path} 对象）。 */
export function hubFileEntries(treeResp: unknown): FilmFileEntry[] {
  const raw: unknown = Array.isArray(treeResp)
    ? treeResp
    : Array.isArray((treeResp as { files?: unknown } | null | undefined)?.files)
      ? (treeResp as { files: unknown[] }).files
      : []
  const out: FilmFileEntry[] = []
  for (const e of raw as unknown[]) {
    if (typeof e === 'string') out.push({ path: e })
    else if (e && typeof (e as FilmFileEntry).path === 'string') out.push(e as FilmFileEntry)
  }
  return out
}

/** GET :id/files 树响应 → 路径字符串数组（hubFilePaths；消费面统一入口，
 *  对象数组取 path / 字符串数组直通 / undefined → 空数组）。 */
export function hubFilePaths(treeResp: unknown): string[] {
  return hubFileEntries(treeResp).map((e) => e.path)
}

/** 路径是否位于某目录下（'dist/final-v1.mp4' under 'dist'；目录条目自身不算）。 */
export function isUnderDir(path: string, dir: string): boolean {
  return path.startsWith(`${dir}/`) && path.length > dir.length + 1
}

/** 文件基名（'dist/final-v2.mp4' → 'final-v2.mp4'）。 */
export function fileBasename(path: string): string {
  const i = path.lastIndexOf('/')
  return i >= 0 ? path.slice(i + 1) : path
}

/** 扩展名小写（无扩展返回 ''）。 */
export function fileExt(path: string): string {
  const b = fileBasename(path)
  const i = b.lastIndexOf('.')
  return i >= 0 ? b.slice(i + 1).toLowerCase() : ''
}

export function isImagePath(path: string): boolean {
  return ['png', 'jpg', 'jpeg', 'webp', 'gif'].includes(fileExt(path))
}
export function isVideoPath(path: string): boolean {
  return ['mp4', 'webm', 'mov'].includes(fileExt(path))
}
export function isAudioPath(path: string): boolean {
  return ['mp3', 'wav', 'ogg', 'm4a', 'aac', 'flac'].includes(fileExt(path))
}

/** 树条目修改时间（mtime > modified_at；空=''）。 */
export function entryMtime(e: FilmFileEntry): string {
  return e.mtime ?? e.modified_at ?? ''
}

/** 字节人性化（B/KB/MB/GB；缺省 '—'）。 */
export function fmtBytes(bytes?: number | null): string {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes) || bytes < 0) return '—'
  if (bytes < 1024) return `${bytes}B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)}KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)}MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)}GB`
}

// —— dist 成品版本 ——

/** dist 版本号（'dist/final-v3.mp4' → 3；非版本文件 -1）。 */
export function distVersionOf(path: string): number {
  const m = /final-v(\d+)\.mp4$/i.exec(path)
  return m ? Number(m[1]) : -1
}

/** dist 成品版本列表（final-v*.mp4；新版本在前）。 */
export function distVersions(tree: FilmFileEntry[]): FilmFileEntry[] {
  return tree
    .filter((e) => isUnderDir(e.path, 'dist') && distVersionOf(e.path) >= 0)
    .sort((a, b) => distVersionOf(b.path) - distVersionOf(a.path))
}

// —— cache 半成品 ——

/** 目录自文档占位文件名（后端 hub_placeholder_files 的 _about.md——撑目录
 *  层级用，不是产物；cache/dist 空目录靠它可见，派生面须排除）。 */
const ABOUT_PLACEHOLDER = '_about.md'

/** cache 半成品列表（cache/ 下全部文件；排除目录条目与 _about.md 占位说明
 *  文件——cache 空目录只剩占位时派生为空 → 合成页空态友好，按路径排序）。 */
export function cacheEntries(tree: FilmFileEntry[]): FilmFileEntry[] {
  return tree
    .filter(
      (e) =>
        isUnderDir(e.path, 'cache') &&
        (e.kind ?? 'file') !== 'dir' &&
        fileBasename(e.path) !== ABOUT_PLACEHOLDER,
    )
    .sort((a, b) => a.path.localeCompare(b.path))
}

// —— 剧情原文素材 ——

/** 剧情原文候选目录（后端目录约定宽松兼容：sources/ 主口径）。 */
const SOURCE_DIRS = ['sources/', 'story/sources/', 'source/'] as const

/** 路径是否剧情原文素材（sources/ 下 txt/md；根下 source-*.txt；或 hub 树
 *  story/source-*.txt——v0.1.39.1 真机复现补充：story/import 落点即
 *  story/source-<slug>.txt，此前源列表派生漏认 → 列表空）。 */
export function isStorySourcePath(path: string): string | false {
  for (const d of SOURCE_DIRS) {
    if (path.startsWith(d) && ['txt', 'md'].includes(fileExt(path))) return d
  }
  if (/^source-[^/]+\.(txt|md)$/i.test(path)) return ''
  if (/^story\/source-[^/]+\.(txt|md)$/i.test(path)) return 'story/'
  return false
}

/** 剧情原文素材列表（树派生；按文件名排序）。 */
export function storySources(tree: FilmFileEntry[]): FilmFileEntry[] {
  return tree
    .filter((e) => isStorySourcePath(e.path) !== false)
    .sort((a, b) => fileBasename(a.path).localeCompare(fileBasename(b.path)))
}

// —— 剧情文档处理管线派生（v0.1.37；与后端 source_base_slug 同口径镜像） ——

/** 源路径 → 基名（'story/source-小说.txt' / 'story/cleaned-小说.txt' / '小说.md'
 *  → '小说'；非字母数字折叠 '-'、≤64 字符——后端 slugify 的前端近似）。 */
export function sourceBaseName(path: string): string {
  const base = fileBasename(path).replace(/\.[^.]+$/, '')
  const stripped = base.replace(/^(source-|cleaned-)/, '')
  const slug = Array.from(stripped)
    .map((c) => (/[\p{L}\p{N}]/u.test(c) ? c : '-'))
    .join('')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '')
    .slice(0, 64)
  return slug || 'source'
}

/** 源文件的管线状态徽章（树派生：cleaned-<base>.txt 存在 → 已清理；
 *  chaptersIndex.source 同基名且 index 在树 → 已分章；
 *  vectors-<base>.json 存在 → 已向量化（v0.1.39.1 语义检索缓存）。
 *  v0.1.44 并入件·ingested=已入库（chapters/index.json 存在且指向该源——
 *  index 无 source 字段存在即算[后端 clean/chapterize 409 锁同口径镜像]，
 *  清理/分章锁定只可向量化；徽章显示优先级最高档）。 */
export function storySourceStatus(
  tree: FilmFileEntry[],
  sourcePath: string,
  chaptersSource?: string | null,
): { cleaned: boolean; chapterized: boolean; embedded: boolean; ingested: boolean } {
  const base = sourceBaseName(sourcePath)
  const cleaned = tree.some((e) => e.path === `story/cleaned-${base}.txt`)
  const hasIndex = tree.some((e) => e.path === 'story/chapters/index.json')
  const src = (chaptersSource ?? '').trim()
  const chapterized = hasIndex && !!src && sourceBaseName(src) === base
  const ingested = hasIndex && (!src || sourceBaseName(src) === base)
  const embedded = tree.some((e) => e.path === `story/vectors-${base}.json`)
  return { cleaned, chapterized, embedded, ingested }
}

/** 超长文本展示截断（原文 pre 与章节正文共用；码点口径）。 */
export function clampForDisplay(
  text: string,
  maxChars = 50_000,
): { text: string; truncated: boolean } {
  const arr = Array.from(text)
  if (arr.length <= maxChars) return { text, truncated: false }
  return { text: `${arr.slice(0, maxChars).join('')}\n…`, truncated: true }
}

// —— frontmatter / 字数 ——

/** 解析 markdown 头部 frontmatter（--- 围栏内的扁平 key: value；宽容容错）。 */
export function parseFrontmatter(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  if (!text) return out
  const m = /^---\r?\n([\s\S]*?)\r?\n---/.exec(text)
  if (!m) return out
  for (const raw of m[1].split(/\r?\n/)) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const kv = /^([\w-]+)\s*:\s*(.*)$/.exec(line)
    if (kv) out[kv[1]] = kv[2].trim().replace(/^['"]|['"]$/g, '')
  }
  return out
}

/** 剧情字数（去 frontmatter / markdown 符号 / 空白后的码点数；中文口径）。 */
export function storyWordCount(text: string): number {
  if (!text) return 0
  const body = text.replace(/^---\r?\n[\s\S]*?\r?\n---/, '')
  const stripped = body.replace(/[\s#>*`\-\[\]()!_|~]/g, '')
  return Array.from(stripped).length
}

// —— 定妆视图槽位 ——

/** 对象多视图标准槽位（定妆页详情网格；custom 为自定义名兜底槽）。 */
export const CAST_VIEW_SLOTS = ['front', 'side', 'back', 'action', 'custom'] as const
export type CastViewSlot = (typeof CAST_VIEW_SLOTS)[number]

/** 对象已有视图中匹配槽位的条目（view 字段相等；custom 槽可匹配任意非标准视图名）。 */
export function matchCastView(
  obj: FilmCastingObject | null,
  slot: CastViewSlot,
): { view: string; path?: string | null; url?: string | null } | null {
  const views = obj?.views ?? []
  const found =
    slot === 'custom'
      ? views.find(
          (v) =>
            (v.view ?? '') !== '' &&
            !(CAST_VIEW_SLOTS as readonly string[]).slice(0, 4).includes(v.view ?? ''),
        )
      : views.find((v) => (v.view ?? '') === slot)
  return found ? { view: found.view ?? '', path: found.path, url: found.url } : null
}

/** 对象已就绪视图数（有 path/url 可显示的）。 */
export function readyViewCount(obj: FilmCastingObject | null): number {
  return (obj?.views ?? []).filter((v) => v.path || v.url).length
}

// —— BGM 表单校验（纯函数：返回错误 code；'' = 通过） ——

export type BgmFormError = '' | 'trigger' | 'file'

/**
 * BGM 导入/新建表单校验：trigger 必填（global 或场景名）；requireFile=true 时
 * （「导入并上传」口径）音频文件必选，false（仅建条目供 AI 生成）文件可空。
 */
export function validateBgmForm(
  trigger: string,
  requireFile: boolean,
  hasFile: boolean,
): BgmFormError {
  if (!trigger.trim()) return 'trigger'
  if (requireFile && !hasFile) return 'file'
  return ''
}

// —— FilmHub 大厅 / Hub 浏览（v0.1.1 显性大厅；纯函数——页面与冒烟共用） ——

/** Hub 文件树节点（buildHubTree 产物；目录递归 children）。 */
export interface HubTreeNode {
  name: string
  path: string
  isDir: boolean
  bytes?: number
  mtime?: string
  children: HubTreeNode[]
}

/**
 * 平铺文件清单 → 嵌套树（目录在前、文件在后，各按名排序；kind='dir' 条目与
 * 中间路径段都归一为目录节点）。Hub 浏览页左栏数据源。
 */
export function buildHubTree(entries: FilmFileEntry[]): HubTreeNode[] {
  const root: HubTreeNode = { name: '', path: '', isDir: true, children: [] }
  const dirIndex = new Map<string, HubTreeNode>()
  const ensureDir = (path: string): HubTreeNode => {
    const hit = dirIndex.get(path)
    if (hit) return hit
    const i = path.lastIndexOf('/')
    const node: HubTreeNode = {
      name: i >= 0 ? path.slice(i + 1) : path,
      path,
      isDir: true,
      children: [],
    }
    dirIndex.set(path, node)
    const parent = i >= 0 ? ensureDir(path.slice(0, i)) : root
    parent.children.push(node)
    return node
  }
  for (const e of entries) {
    if ((e.kind ?? 'file') === 'dir') {
      ensureDir(e.path)
      continue
    }
    const i = e.path.lastIndexOf('/')
    const parent = i >= 0 ? ensureDir(e.path.slice(0, i)) : root
    parent.children.push({
      name: e.path.slice(i + 1),
      path: e.path,
      isDir: false,
      bytes: e.bytes,
      mtime: entryMtime(e),
      children: [],
    })
  }
  const sortRec = (n: HubTreeNode): void => {
    n.children.sort((a, b) =>
      a.isDir !== b.isDir ? (a.isDir ? -1 : 1) : a.name.localeCompare(b.name),
    )
    for (const c of n.children) sortRec(c)
  }
  sortRec(root)
  return root.children
}

/** 文件类型图标（md📝 / json🧾 / 图🖼 / 音🎵 / 视频▶ / 其余📄）。 */
export function hubFileIcon(path: string): string {
  const ext = fileExt(path)
  if (['md', 'markdown'].includes(ext)) return '📝'
  if (ext === 'json') return '🧾'
  if (isImagePath(path)) return '🖼'
  if (isAudioPath(path)) return '🎵'
  if (isVideoPath(path)) return '▶'
  return '📄'
}

/** 文本类扩展（内容区 pre 等宽展示口径；mime 缺省时的前端判定）。 */
const HUB_TEXT_EXTS = ['md', 'markdown', 'json', 'txt', 'csv', 'yml', 'yaml', 'xml', 'log'] as const

/** 是否文本类文件（扩展名口径——files 信封 mime 优先，此为兜底）。 */
export function isHubTextPath(path: string): boolean {
  return (HUB_TEXT_EXTS as readonly string[]).includes(fileExt(path))
}

/** Hub 文件预览形态（HubBrowse 内容区与树卡迷你预览共用判定）。 */
export type HubPreviewKind = 'image' | 'audio' | 'video' | 'text' | 'binary'

/**
 * 路径 + mime → 预览形态（mime 优先 + 扩展名兜底；无 mime 的未知扩展按
 * 文本宽容——服务端 files 读白名单里 md/txt/json/srt 均为文本）。
 */
export function hubPreviewKind(path: string, mime: string): HubPreviewKind {
  const m = (mime || '').toLowerCase()
  if (m.startsWith('image/') || (!m && isImagePath(path))) return 'image'
  if (m.startsWith('audio/') || (!m && isAudioPath(path))) return 'audio'
  if (m.startsWith('video/') || (!m && isVideoPath(path))) return 'video'
  const textual =
    m.startsWith('text/') ||
    /json|yaml|markdown|xml/.test(m) ||
    (!m && (isHubTextPath(path) || fileExt(path) === ''))
  return textual ? 'text' : 'binary'
}

/** 文本前 N 行（迷你预览截断口径；truncated=是否截断）。 */
export function textHeadLines(text: string, maxLines = 50): { text: string; truncated: boolean } {
  const lines = text.split(/\r?\n/)
  if (lines.length <= maxLines) return { text, truncated: false }
  return { text: lines.slice(0, maxLines).join('\n'), truncated: true }
}

/**
 * 文件路径 → 流程视图（「在工作台打开」跳转映射）：
 * story.md / story/ / sources/ → 剧情；storyboard.json → 分镜；casting/ 与
 * extraction.json → 定妆；audio/ → 音频；dist/ cache/ final*.mp4 → 合成；
 * 其余（镜头产物等）→ 工作台。
 */
export function hubTargetView(path: string): FlowView {
  const p = path.toLowerCase()
  if (p === 'story.md' || p.startsWith('story/') || p.startsWith('sources/')) return 'story'
  if (p === 'storyboard.json' || p.startsWith('storyboard/')) return 'storyboard'
  if (p.startsWith('casting/') || p === 'extraction.json') return 'casting'
  if (p.startsWith('audio/')) return 'audio'
  if (p.startsWith('dist/') || p.startsWith('cache/') || /^final(-v\d+)?\.mp4$/.test(p)) {
    return 'compose'
  }
  return 'workbench'
}

/** casting 六类（hubCastSelect 与深链 cast 参数共用的合法域）。 */
const HUB_CAST_TYPES: readonly string[] = [
  'characters',
  'props',
  'pets',
  'formations',
  'actions',
  'scenes',
]

/** casting 路径 → 定妆对象定位（casting/<type>/<name>/…；name 取第三段，
 *  其后为视图文件段；非 casting 路径 null）。 */
export function hubCastSelect(path: string): { type: FilmCastType; name: string } | null {
  const segs = path.split('/').filter(Boolean)
  if (segs[0] !== 'casting' || segs.length < 3) return null
  const type = segs[1] as FilmCastType
  if (!HUB_CAST_TYPES.includes(type)) {
    return null
  }
  return { type, name: segs[2] }
}

// —— 视图深链（v0.1.5）——
// URL 约定：standalone.html?p=<projectId>&view=<viewKey>（viewKey ∈
// story|storyboard|casting|audio|compose|workbench|hub|models|settings）；
// casting 可加 &cast=<type>/<name>（定妆页挂载消费定位对象）、hub 可加
// &file=<path>（HubBrowse 挂载消费打开该文件）。纯函数：SideNav 外链拼装与
// FilmStudio 启动解析共用同一代码路径（冒烟断言同源）。
// v0.1.45：view=shotgen 应用级首屏深链（短片生成页）——非工作室视图（不进
// FLOW_VIEWS 合法域，SideNav 不出现），parseStandaloneQuery 以独立布尔位
// 返回；与 p 并存时项目深链优先（FilmStudio 消费口径）。

/** 独立页文件名（相对形态；嵌入桌面模式加 /apps-assets/film/ 前缀打开）。 */
export const STANDALONE_PAGE = 'standalone.html'

/** 深链解析结果（projectId 空 = 常规大厅；view 空 = 进项目走缺省/阶段推导）。 */
export interface StandaloneDeepLink {
  projectId: string
  view: FlowView | ''
  /** view=shotgen（应用级短片生成首屏；p 并存时被项目深链压过）。 */
  shotgen: boolean
  /** 定妆对象定位（&cast=<type>/<name>；非法宽容忽略）。 */
  cast: { type: FilmCastType; name: string } | null
  /** Hub 文件定位（&file=<path>；配合 view=hub 由 HubBrowse 消费）。 */
  file: string
}

/**
 * 解析 location.search 形态的深链查询串（'?p=a&view=casting&cast=…&file=…'）。
 * URLSearchParams 负责百分号解码（中文项目/路径安全）；view 非法、cast 形态
 * 或类型非法时宽容忽略（该维度退化为空，页面照常打开）。
 */
export function parseStandaloneQuery(search: string): StandaloneDeepLink {
  const q = new URLSearchParams(search.startsWith('?') ? search.slice(1) : search)
  const viewRaw = q.get('view') ?? ''
  const castRaw = q.get('cast') ?? ''
  let cast: StandaloneDeepLink['cast'] = null
  const i = castRaw.indexOf('/')
  if (i > 0) {
    const type = castRaw.slice(0, i)
    const name = castRaw.slice(i + 1)
    if (name && HUB_CAST_TYPES.includes(type)) cast = { type: type as FilmCastType, name }
  }
  return {
    projectId: (q.get('p') ?? '').trim(),
    view: isFlowView(viewRaw) ? viewRaw : '',
    shotgen: viewRaw === 'shotgen',
    cast,
    file: q.get('file') ?? '',
  }
}

/**
 * 拼装视图深链 URL（相对形态 'standalone.html?…'；query 经 URLSearchParams
 * 编码——中文对象名/文件路径、空格、+ 号均安全 round-trip）。
 */
export function buildStandaloneUrl(
  projectId: string,
  view?: FlowView | '',
  opts?: { cast?: { type: FilmCastType; name: string } | null; file?: string },
): string {
  const q = new URLSearchParams()
  q.set('p', projectId)
  if (view) q.set('view', view)
  if (opts?.cast) q.set('cast', `${opts.cast.type}/${opts.cast.name}`)
  if (opts?.file) q.set('file', opts.file)
  return `${STANDALONE_PAGE}?${q.toString()}`
}

/**
 * 项目列表数据 → 阶段推导（大厅卡进度条口径；启发式——列表无 README stage，
 * 进入项目后以 README 为准）：final* → compose；bgm/line-* → audio；
 * shot-*.png / casting/* → casting；有分镜 → storyboard；否则 story。
 */
export function deriveStageFromProject(
  p: Pick<FilmProject, 'script' | 'artifacts'>,
): FilmStage {
  const names = (p.artifacts ?? []).map((a) => a.name)
  const has = (re: RegExp) => names.some((n) => re.test(n))
  if (has(/^(dist\/)?final(-v\d+)?\.mp4$/)) return 'compose'
  if (has(/^bgm\.mp3$/) || has(/^line-\d+\.mp3$/)) return 'audio'
  if (has(/^shot-\d+\.(png|webp|jpg)$/) || names.some((n) => n.startsWith('casting/'))) {
    return 'casting'
  }
  if ((p.script ?? []).length > 0) return 'storyboard'
  return 'story'
}
