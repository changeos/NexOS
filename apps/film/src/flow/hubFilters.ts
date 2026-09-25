// =============================================================================
// hubFilters.ts —— FilmHub 大厅列表筛选（纯函数，v0.1.7 UI 重设计第一批）。
//
// 「表头内嵌筛选」的数据面：搜索词（标题/idea）× 阶段（产物启发式推导）×
// 类别（分辨率档）。冒烟直断言同一代码路径。
// =============================================================================
import type { FilmProject } from '../api'
import { deriveStageFromProject } from './flowFiles'
import type { FilmStage } from './flowTypes'

/** 筛选态（'' / 全部 = 不筛）。 */
export interface HubFilterState {
  /** 搜索词（标题/idea，大小写不敏感）。 */
  q: string
  /** 阶段（''=全部）。 */
  stage: FilmStage | ''
  /** 分辨率档 ratio 值（''=全部）。 */
  ratio: string
}

/** 大厅筛选主函数：搜索 × 阶段 × 类别三联。 */
export function filterHubProjects(
  projects: readonly FilmProject[],
  f: HubFilterState,
): FilmProject[] {
  const kw = f.q.trim().toLowerCase()
  return projects.filter((p) => {
    if (kw) {
      const hit =
        (p.title ?? '').toLowerCase().includes(kw) ||
        (p.idea ?? '').toLowerCase().includes(kw)
      if (!hit) return false
    }
    if (f.stage && deriveStageFromProject(p) !== f.stage) return false
    if (f.ratio && (p.ratio ?? '') !== f.ratio) return false
    return true
  })
}

/** 列表内出现的分辨率档（去重保序——「类别」筛选选项数据源）。 */
export function hubRatioOptions(projects: readonly FilmProject[]): string[] {
  const out: string[] = []
  for (const p of projects) {
    const r = p.ratio ?? ''
    if (r && !out.includes(r)) out.push(r)
  }
  return out
}
