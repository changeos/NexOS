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

/** 工作室视图：五阶段页 + workbench 工作台 + hub 项目文件树浏览 + settings 设置/成员。 */
export type FlowView = FilmStage | 'workbench' | 'hub' | 'settings'

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

// =============================================================================
// 分辨率预设（v0.1.37 新建影片六档）——与后端 film.rs 的 COMPOSE_DIMS 预设表
// 一表同源：key 仅前端展示概念（i18n 键尾），ratio 为接口/落库字符串；
// width/height 为 compose 合成分辨率（监视器画面比例、时间轴/合成页输出小字、
// 新建弹窗预设卡共用本表，勿在组件内另抄硬编码）。
// =============================================================================

/** 分辨率预设（六档）。 */
export interface RatioPreset {
  /** 预设 key（i18n `film.preset<Key>` / `film.preset<Key>Desc` 键尾）。 */
  key: 'landscape' | 'portrait' | 'cinemascope' | 'classic' | 'square' | 'tv43'
  /** 比例字符串（后端白名单与落库值；如 `2.39:1`）。 */
  ratio: string
  /** 合成分辨率宽（px）。 */
  width: number
  /** 合成分辨率高（px）。 */
  height: number
}

/** 六档预设（顺序即新建弹窗卡片顺序：横/竖/影院/传统/方/电视）。 */
export const RATIO_PRESETS: readonly RatioPreset[] = [
  { key: 'landscape', ratio: '16:9', width: 1920, height: 1080 },
  { key: 'portrait', ratio: '9:16', width: 1080, height: 1920 },
  { key: 'cinemascope', ratio: '2.39:1', width: 2048, height: 858 },
  { key: 'classic', ratio: '1.85:1', width: 1998, height: 1080 },
  { key: 'square', ratio: '1:1', width: 1080, height: 1080 },
  { key: 'tv43', ratio: '4:3', width: 1440, height: 1080 },
]

/** 比例字符串 → 预设（未知档 undefined——调用方按原始字符串回退展示）。 */
export function ratioPresetOf(ratio: string): RatioPreset | undefined {
  return RATIO_PRESETS.find((p) => p.ratio === ratio)
}

/**
 * 比例字符串 → 数值宽高比（`"2.39:1"` → 2.39；`"9:16"` → 0.5625）。
 * 泛化解析任意 `a:b` 形态（两侧可为小数），非法/除零返回 null——监视器
 * 画面区 aspect-ratio、预设卡比例条共用（不再枚举 16:9/9:16/1:1 三态）。
 */
export function ratioValueOf(ratio: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*:\s*(\d+(?:\.\d+)?)\s*$/.exec(ratio)
  if (!m) return null
  const w = Number(m[1])
  const h = Number(m[2])
  if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return null
  return w / h
}

/** 预设 key → i18n 键尾首字母大写（landscape → Landscape；tv43 → Tv43）。 */
export function presetKeyCap(key: RatioPreset['key']): string {
  return key.charAt(0).toUpperCase() + key.slice(1)
}
