// =============================================================================
// flowTypes.ts —— 流程阶段与视图类型（FilmHub 流程化，v0.1.35）。
//
// 五阶段与 README.md frontmatter 的 stage 字段一一对应（后端随流程推进写）：
//   story 剧情 → storyboard 分镜 → casting 定妆 → audio 音频 → compose 合成
// 视图 = 五阶段页 + 工作台（原五区：镜头面板/监视器/时间轴）+ 设置/成员。
// 纯类型与纯函数（零依赖）——左侧导航、页面、冒烟共用。
// =============================================================================

/** 流程阶段（README frontmatter stage 字段口径）。 */
export type FilmStage = 'story' | 'storyboard' | 'casting' | 'audio' | 'compose'

/** 工作室视图：五阶段页 + workbench 工作台 + settings 设置/成员。 */
export type FlowView = FilmStage | 'workbench' | 'settings'

/** 阶段序（导航顺序；index 即进度）。 */
export const FLOW_STAGES: readonly FilmStage[] = [
  'story',
  'storyboard',
  'casting',
  'audio',
  'compose',
]

/** 值是否为合法阶段。 */
export function isFilmStage(v: string): v is FilmStage {
  return (FLOW_STAGES as readonly string[]).includes(v)
}

/** 阶段序号（0 起；非法值 -1）。 */
export function stageIndex(s: FilmStage): number {
  return FLOW_STAGES.indexOf(s)
}

/** 阶段进度比较：a 是否不晚于 b（含相等）。 */
export function stageAtOrBefore(a: FilmStage, b: FilmStage): boolean {
  return stageIndex(a) <= stageIndex(b)
}

/**
 * 从 markdown（README.md / project.md）解析 frontmatter 的 stage 字段。
 * 形态：文件以 `---` 围栏开头，内含 `stage: casting` 行；宽容容错（大小写、
 * 引号、行内空白）。解析不出返回 ''（未知——导航退化为仅高亮当前视图）。
 */
export function parseStageFromMarkdown(md: string): FilmStage | '' {
  if (!md) return ''
  const m = /^---\r?\n([\s\S]*?)\r?\n---/.exec(md)
  const fm = m ? m[1] : md.slice(0, 2000)
  const line = fm
    .split(/\r?\n/)
    .map((l) => l.trim())
    .find((l) => /^stage\s*:/i.test(l))
  if (!line) return ''
  const val = line.replace(/^stage\s*:\s*/i, '').trim().replace(/^['"]|['"]$/g, '')
  return isFilmStage(val) ? val : ''
}

/** 阶段徽章序号文案（①②③④⑤；导航项左侧圆点）。 */
export function stageBadge(n: number): string {
  return ['①', '②', '③', '④', '⑤'][n] ?? `${n + 1}`
}
