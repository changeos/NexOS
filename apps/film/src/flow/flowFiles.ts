// =============================================================================
// flowFiles.ts —— 项目文件树派生与流程纯逻辑（FilmHub，v0.1.35）。
//
// 后端「文件即真值」：dist/ 成品版本、cache/ 半成品、sources/ 剧情原文、
// extraction.json 提取报告均从 GET :id/files 树派生；frontmatter / 字数 /
// 视图槽位 / BGM 表单校验为纯函数——页面与 happy-dom 冒烟共用同一代码路径。
// =============================================================================

import type { FilmCastingObject, FilmFileEntry } from '../api'

// —— 路径 helpers ——

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

/** cache 半成品列表（cache/ 下全部文件；排除目录条目，按路径排序）。 */
export function cacheEntries(tree: FilmFileEntry[]): FilmFileEntry[] {
  return tree
    .filter((e) => isUnderDir(e.path, 'cache') && (e.kind ?? 'file') !== 'dir')
    .sort((a, b) => a.path.localeCompare(b.path))
}

// —— 剧情原文素材 ——

/** 剧情原文候选目录（后端目录约定宽松兼容：sources/ 主口径）。 */
const SOURCE_DIRS = ['sources/', 'story/sources/', 'source/'] as const

/** 路径是否剧情原文素材（sources/ 下 txt/md；或根下 source-*.txt）。 */
export function isStorySourcePath(path: string): string | false {
  for (const d of SOURCE_DIRS) {
    if (path.startsWith(d) && ['txt', 'md'].includes(fileExt(path))) return d
  }
  if (/^source-[^/]+\.(txt|md)$/i.test(path)) return ''
  return false
}

/** 剧情原文素材列表（树派生；按文件名排序）。 */
export function storySources(tree: FilmFileEntry[]): FilmFileEntry[] {
  return tree
    .filter((e) => isStorySourcePath(e.path) !== false)
    .sort((a, b) => fileBasename(a.path).localeCompare(fileBasename(b.path)))
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
