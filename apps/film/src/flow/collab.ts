// =============================================================================
// collab.ts —— 多人分工 v1（分区认领 + 定妆对象级认领 + 操作人 + 活动流）。
//
// 协作文件走通用 files 面（GET/PUT :id/files/ownership.json、activity.json）：
//   ownership.json —— {members, sections, casting_objects}
//     · members：项目成员名列表（前端自由维护）
//     · sections：五分区认领 {story: {owner, claimed_at}, ...}
//     · casting_objects：定妆对象级认领，键 = '<type>/<name>' 路径形态
//       （如 'characters/发光领航员'；六类通用）
//   activity.json —— 后端落流水 [{ts, author, action, target}] 环形 200 条
//     （写端点 body 带 author，缺省 anonymous）；前端只读展示。
//
// v1 软约束：他人对象卡不硬锁（显示负责人即可）；并发编辑以后保存为准，
// 真正的并发协作等 P1 git 仓化（分支/合并天然解决）。纯函数 + localStorage
// 操作人记忆——页面与 happy-dom 冒烟共用同一代码路径。
// =============================================================================

import { b64ToText, type FilmFileContent } from '../api'
import type { FilmStage } from './flowTypes'

/** 缺省操作人（后端同口径）。 */
export const ANONYMOUS_AUTHOR = 'anonymous'

/** localStorage key（操作人「我是」记忆；同浏览器全局一份）。 */
export const AUTHOR_STORAGE_KEY = 'nexos.film.author'

/** 认领条目（owner + 时间）。 */
export interface FilmClaim {
  owner: string
  claimed_at?: string
}

/** ownership.json 形态（宽松：sections 值兼容裸字符串旧形态）。 */
export interface FilmOwnership {
  members?: string[]
  sections?: Record<string, string | FilmClaim | undefined>
  casting_objects?: Record<string, FilmClaim | undefined>
}

/** 活动流条目（activity.json；宽松字段）。 */
export interface FilmActivityEntry {
  ts?: string | number
  author?: string
  action?: string
  target?: string
  [k: string]: unknown
}

/** UTF-8 文本 → b64（files PUT 写协作文件用；TextEncoder 走字节防中文乱码）。 */
export function textToB64(text: string): string {
  const bytes = new TextEncoder().encode(text)
  let bin = ''
  for (const b of bytes) bin += String.fromCharCode(b)
  return btoa(bin)
}

// —— 归一 helpers ——

function asClaim(v: string | FilmClaim | undefined | null): FilmClaim | null {
  if (!v) return null
  if (typeof v === 'string') return v.trim() ? { owner: v.trim() } : null
  const owner = typeof v.owner === 'string' ? v.owner.trim() : ''
  return owner ? { owner, claimed_at: v.claimed_at } : null
}

/** 分区负责人（'' = 未认领）。 */
export function sectionOwner(o: FilmOwnership | null, stage: FilmStage): string {
  return asClaim(o?.sections?.[stage])?.owner ?? ''
}

/** 定妆对象认领键（'<type>/<name>' 路径形态）。 */
export function castingObjectKey(type: string, name: string): string {
  return `${type}/${name}`
}

/** 定妆对象负责人（'' = 未认领）。 */
export function objectOwner(o: FilmOwnership | null, type: string, name: string): string {
  return asClaim(o?.casting_objects?.[castingObjectKey(type, name)])?.owner ?? ''
}

/** 成员列表去重保序。 */
export function normalizeMembers(names: string[]): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const n of names) {
    const t = (n ?? '').trim()
    if (!t || seen.has(t)) continue
    seen.add(t)
    out.push(t)
  }
  return out
}

/** 纯函数：认领分区（返回新 ownership；owner 空=释放）。 */
export function claimSection(o: FilmOwnership | null, stage: FilmStage, owner: string): FilmOwnership {
  const sections: Record<string, string | FilmClaim | undefined> = { ...(o?.sections ?? {}) }
  const t = owner.trim()
  if (t) sections[stage] = { owner: t, claimed_at: new Date().toISOString() }
  else delete sections[stage]
  return { ...(o ?? {}), sections }
}

/** 纯函数：认领定妆对象（owner 空=释放）。 */
export function claimCastingObject(
  o: FilmOwnership | null,
  type: string,
  name: string,
  owner: string,
): FilmOwnership {
  const castingObjects: Record<string, FilmClaim | undefined> = { ...(o?.casting_objects ?? {}) }
  const key = castingObjectKey(type, name)
  const t = owner.trim()
  if (t) castingObjects[key] = { owner: t, claimed_at: new Date().toISOString() }
  else delete castingObjects[key]
  return { ...(o ?? {}), casting_objects: castingObjects }
}

// —— 序列化 / 解析（files 信封）——

/** ownership.json 文本序列化（两空格缩进，成员去重保序）。 */
export function serializeOwnership(o: FilmOwnership): string {
  const normalized: FilmOwnership = {
    members: normalizeMembers(o.members ?? []),
    sections: o.sections ?? {},
    casting_objects: o.casting_objects ?? {},
  }
  return `${JSON.stringify(normalized, null, 2)}\n`
}

/** files 信封 → ownership（b64 解码 → JSON 宽容解析；坏文件返回 null）。 */
export function parseOwnership(env: FilmFileContent | null | undefined): FilmOwnership | null {
  if (!env?.content_b64) return null
  try {
    const obj = JSON.parse(b64ToText(env.content_b64)) as FilmOwnership
    if (!obj || typeof obj !== 'object' || Array.isArray(obj)) return null
    return {
      members: Array.isArray(obj.members) ? normalizeMembers(obj.members) : [],
      sections: obj.sections && typeof obj.sections === 'object' ? obj.sections : {},
      casting_objects:
        obj.casting_objects && typeof obj.casting_objects === 'object' ? obj.casting_objects : {},
    }
  } catch {
    return null
  }
}

/** files 信封 → 活动流（b64 → JSON 数组宽容；ts 新→旧排序，环形容错）。 */
export function parseActivity(env: FilmFileContent | null | undefined): FilmActivityEntry[] {
  if (!env?.content_b64) return []
  try {
    const raw = JSON.parse(b64ToText(env.content_b64))
    if (!Array.isArray(raw)) return []
    const list = raw.slice(-200).map((x) => (x && typeof x === 'object' ? (x as FilmActivityEntry) : {}))
    return list.sort((a, b) => activityTs(b) - activityTs(a))
  } catch {
    return []
  }
}

/** 活动条目时间戳数值（ms；缺省 0）。 */
function activityTs(e: FilmActivityEntry): number {
  const ts = e.ts
  if (typeof ts === 'number') return ts > 1e12 ? ts : ts * 1000
  if (typeof ts === 'string') {
    const d = new Date(ts).getTime()
    return Number.isNaN(d) ? 0 : d
  }
  return 0
}

/** 活动时间展示（ISO/秒级数值 → 本地串；坏值原样）。 */
export function fmtActivityTime(ts: string | number | undefined): string {
  if (ts === undefined || ts === '') return '—'
  if (typeof ts === 'number') {
    const d = new Date(ts > 1e12 ? ts : ts * 1000)
    return Number.isNaN(d.getTime()) ? String(ts) : d.toLocaleString()
  }
  const d = new Date(ts)
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString()
}

// —— 操作人（「我是」；localStorage 记忆）——

/** 记忆的操作人（trim 后空 → anonymous）。 */
export function loadAuthor(): string {
  try {
    const v = localStorage.getItem(AUTHOR_STORAGE_KEY) ?? ''
    return v.trim() || ANONYMOUS_AUTHOR
  } catch {
    return ANONYMOUS_AUTHOR
  }
}

/** 记忆操作人（空串清回 anonymous）。 */
export function saveAuthor(name: string): string {
  const t = (name ?? '').trim()
  try {
    if (t) localStorage.setItem(AUTHOR_STORAGE_KEY, t)
    else localStorage.removeItem(AUTHOR_STORAGE_KEY)
  } catch {
    /* 隐私模式等：忽略，仅内存生效 */
  }
  return t || ANONYMOUS_AUTHOR
}
