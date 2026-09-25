// =============================================================================
// castParts.ts —— 定妆定制器部件模板与纯逻辑（FilmHub v0.1.39）。
//
// 六类对象统一进定制器：类型决定**主视图槽**（中央主体完整视图——人物=正面
// 全身 front / 宠物=全身立姿 body（植物对象=全株）/ 武器=完整形态 full /
// 场景=全景 pano / 排列=全员站位图 group / 动作=动作序列主帧 pose）与
// **部件槽集**（每槽 4-6 个常用预设 + 「自定义」自由输入）。
//
// 键集与后端 CASTING_PART_KEYS（crates/os-api/src/handlers/film_hub.rs）同一
// 契约——后端测试跨端读本文件断言一致；预设值纯前端（后端只按 `键：值` 拼
// 进组合提示词，不感知预设）。parts 存 card.md front-matter `parts:` 键。
//
// 纯函数（页面与冒烟共用同一代码路径）：
//   mainSlotOf / partSlotsFor / normalizeParts / partsEqual / partsDiff /
//   viewBase / viewVersion / mainViewVersions / auxViewSlots / composePromptPreview
// =============================================================================

import type { FilmCastType, FilmCastingObject } from '../api'
import { CAST_VIEW_SLOTS, type CastViewSlot } from './flowFiles'

/** 单个部件槽：键（中文短词）+ 预设选项（4-6 个常用预设；「自定义」由 UI 兜底）。 */
export interface CastPartSlot {
  key: string
  presets: string[]
}

/** 类型模板：主视图槽 + 部件槽集（pets 另有植物形态切换槽集 altSlots）。 */
export interface CastPartTemplate {
  mainSlot: string
  slots: CastPartSlot[]
  /** 形态=植物 时切换的槽集（v1 仅 pets：动物/植物二选一）。 */
  altSlots?: CastPartSlot[]
}

/** 六类部件模板（冻结）。预设=中文短词（本文件为唯一真值，后端不 hardcode）。 */
export const PART_TEMPLATES: Record<FilmCastType, CastPartTemplate> = {
  characters: {
    mainSlot: 'front',
    slots: [
      { key: '脸型', presets: ['瓜子脸', '圆脸', '鹅蛋脸', '方脸', '尖脸'] },
      { key: '发型', presets: ['长发', '短发', '卷发', '马尾', '辫子', '寸头'] },
      { key: '服装', presets: ['长袍', '西装', '战甲', '便装', '风衣', '制服'] },
      { key: '配饰', presets: ['红围巾', '眼镜', '面纱', '耳环', '帽子', '无'] },
      { key: '体型', presets: ['高瘦', '匀称', '壮硕', '纤细', '矮壮'] },
      { key: '气质', presets: ['英气', '温柔', '冷峻', '活泼', '沧桑', '神秘'] },
    ],
  },
  pets: {
    mainSlot: 'body',
    slots: [
      { key: '形态', presets: ['动物', '植物'] },
      { key: '体型', presets: ['小型', '中型', '大型', '修长', '圆润'] },
      { key: '毛色', presets: ['黑色', '白色', '棕色', '灰色', '金色', '花斑'] },
      { key: '花纹', presets: ['纯色', '条纹', '斑点', '渐变', '虎斑'] },
      { key: '耳型', presets: ['立耳', '垂耳', '折耳', '尖耳', '圆耳'] },
      { key: '尾型', presets: ['长尾', '短尾', '卷尾', '蓬松', '无尾'] },
      { key: '表情', presets: ['机警', '温顺', '凶猛', '呆萌', '慵懒'] },
    ],
    altSlots: [
      { key: '形态', presets: ['动物', '植物'] },
      { key: '株型', presets: ['乔木', '灌木', '藤蔓', '草本', '多肉'] },
      { key: '叶形', presets: ['针叶', '阔叶', '羽状', '掌状', '线形'] },
      { key: '花色', presets: ['白花', '红花', '黄花', '蓝紫花', '无花'] },
      { key: '果实', presets: ['红果', '浆果', '荚果', '坚果', '无果'] },
      { key: '姿态', presets: ['挺拔', '舒展', '垂坠', '攀附', '丛生'] },
    ],
  },
  props: {
    mainSlot: 'full',
    slots: [
      { key: '形制', presets: ['长剑', '短刃', '巨斧', '法杖', '弓箭'] },
      { key: '材质', presets: ['玄铁', '白银', '青铜', '木质', '水晶'] },
      { key: '纹饰', presets: ['素面', '云纹', '龙纹', '符文', '镶嵌'] },
      { key: '尺寸感', presets: ['轻巧', '常规', '沉重', '巨大', '微型'] },
    ],
  },
  scenes: {
    mainSlot: 'pano',
    slots: [
      { key: '地形', presets: ['平原', '山地', '沙漠', '水乡', '雪原'] },
      { key: '建筑', presets: ['古城', '都市', '废墟', '村落', '殿堂'] },
      { key: '植被', presets: ['密林', '荒原', '竹海', '花田', '苔原'] },
      { key: '光照氛围', presets: ['晨曦', '正午', '黄昏', '夜幕', '雨雾'] },
    ],
  },
  formations: {
    mainSlot: 'group',
    slots: [
      { key: '站位构图', presets: ['一字横列', '两翼包抄', '三角阵', '圆阵', '散点'] },
      { key: '人数', presets: ['二人', '三人', '五人', '十人', '千军'] },
      { key: '前景背景', presets: ['前实后虚', '前虚后实', '全景深', '逆光剪影', '对称构图'] },
    ],
  },
  actions: {
    mainSlot: 'pose',
    slots: [
      { key: '姿态', presets: ['站立', '奔跑', '腾跃', '挥击', '施法'] },
      { key: '力度', presets: ['轻盈', '常规', '爆发', '持重', '悬停'] },
      { key: '方向', presets: ['向左', '向右', '正面', '背身', '俯冲'] },
      { key: '特效', presets: ['无特效', '风尘', '火焰', '冰晶', '雷光'] },
    ],
  },
}

/** 某类主视图槽名（与后端 casting_main_slot 同口径）。 */
export function mainSlotOf(type: FilmCastType): string {
  return PART_TEMPLATES[type].mainSlot
}

/** 当前生效部件槽集（pets 按 形态=植物 切换 altSlots；其余恒 slots）。 */
export function partSlotsFor(type: FilmCastType, parts: Record<string, string>): CastPartSlot[] {
  const tpl = PART_TEMPLATES[type]
  if (type === 'pets' && parts['形态'] === '植物' && tpl.altSlots) return tpl.altSlots
  return tpl.slots
}

/** API/存储侧 parts 宽容归一（非字符串值/空白剔除；非对象 → 空表）。 */
export function normalizeParts(raw: unknown): Record<string, string> {
  const out: Record<string, string> = {}
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return out
  for (const [k, v] of Object.entries(raw as Record<string, unknown>)) {
    if (typeof v !== 'string') continue
    const key = k.trim()
    const val = v.trim()
    if (key && val) out[key] = val
  }
  return out
}

/** 两 parts 表是否全等（dirty 判定口径）。 */
export function partsEqual(a: Record<string, string>, b: Record<string, string>): boolean {
  const ka = Object.keys(a)
  const kb = Object.keys(b)
  return (
    ka.length === kb.length && ka.every((k) => a[k] === b[k])
  )
}

/** 两 parts 表差异键列表（冒烟断言：替换部件后仅对应键变化）。 */
export function partsDiff(a: Record<string, string>, b: Record<string, string>): string[] {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)])
  return [...keys].filter((k) => (a[k] ?? '') !== (b[k] ?? ''))
}

// —— 主槽视图版本派生（views 里主槽多版本 front/front-v2… 保留） ——

/** 视图名主槽基名（"front-v2" → "front"；非版本后缀原样）。 */
export function viewBase(view: string): string {
  const m = /^(.*)-v(\d+)$/.exec(view ?? '')
  return m && m[1] ? m[1] : view ?? ''
}

/** 视图名版本号（基名=1；front-v3 → 3）。 */
export function viewVersion(view: string): number {
  const m = /^(.*)-v(\d+)$/.exec(view ?? '')
  return m && m[1] ? Number(m[2]) : 1
}

/** 对象视图条目（FilmCastingObject.views 元素的宽松形态）。 */
export interface CastViewEntry {
  view?: string | null
  path?: string | null
  url?: string | null
  bytes?: number | null
}

/** 主槽全部版本（基名匹配 + 版本新→旧；空=未生成）。 */
export function mainViewVersions(
  obj: Pick<FilmCastingObject, 'views'> | null,
  type: FilmCastType,
): CastViewEntry[] {
  const main = mainSlotOf(type)
  return (obj?.views ?? [])
    .filter((v) => (v?.view ?? '') !== '' && viewBase(v.view ?? '') === main)
    .sort((a, b) => viewVersion(b.view ?? '') - viewVersion(a.view ?? ''))
}

/** 最新主槽视图（中央大图数据源；null=虚线占位）。 */
export function latestMainView(
  obj: Pick<FilmCastingObject, 'views'> | null,
  type: FilmCastType,
): CastViewEntry | null {
  return mainViewVersions(obj, type)[0] ?? null
}

/** 「更多视图」折叠区的辅助槽（现有五槽位机制；排除已被中央主槽占用的槽）。 */
export function auxViewSlots(type: FilmCastType): CastViewSlot[] {
  const main = mainSlotOf(type)
  return CAST_VIEW_SLOTS.filter((s) => s !== main)
}

/**
 * 组合提示词预览（与后端 composed_view_prompt 同模板——主槽 + parts 非空时：
 * `<desc>，<键：值>…（与其它镜头严格同一定妆对象），<主视图描述>`；否则回落
 * 后端缺省模板。仅 UI 预览/冒烟断言用，生成真值仍由后端组装）。
 */
export function composePromptPreview(
  type: FilmCastType,
  desc: string,
  view: string,
  parts: Record<string, string>,
): string {
  const main = mainSlotOf(type)
  if (viewBase(view) !== main || Object.keys(parts).length === 0) {
    return type === 'characters'
      ? `同一定妆对象的多视图：${desc}，${view} 视图，严格一致外形`
      : `定妆对象（${type}）的 ${view} 视图定妆图：${desc}，严格一致外形`
  }
  // 按类型模板槽序拼接（与后端 casting_part_keys 序一致；面板序=提示词序）
  const joined = partSlotsFor(type, parts)
    .filter((s) => parts[s.key])
    .map((s) => `${s.key}：${parts[s.key]}`)
    .join('，')
  const descPart = type === 'pets' && parts['形态'] === '植物' ? '全株' : MAIN_VIEW_DESC[type]
  return `${desc}${joined ? '，' + joined : ''}（与其它镜头严格同一定妆对象），${descPart}`
}

/** 类型主视图描述（后端 casting_main_view_desc 同口径；pets 植物在预览内特判）。 */
export const MAIN_VIEW_DESC: Record<FilmCastType, string> = {
  characters: '正面全身',
  pets: '全身立姿',
  props: '完整形态',
  scenes: '全景',
  formations: '全员站位图',
  actions: '动作序列主帧',
}

// —— v0.1.41 P0 动作/运镜子选项（仅 actions 类；card.md motion:/camera: 键） ——

/**
 * 运镜预设（8 项）——与后端 CAMERA_PRESETS（film_hub.rs）同一契约，后端测试
 * 跨端读本文件断言一致。业界参数化运镜最大公约数方向集：Kling v1
 * camera_control / PixVerse 8 枚举可互译；无参数渠道走 prompt 描述词兜底。
 */
export const CAMERA_PRESETS: readonly string[] = [
  '推近',
  '拉远',
  '左摇',
  '右摇',
  '上摇',
  '下摇',
  '跟随',
  '环绕',
]

/** 动作参考子选项（card.md `motion:`；kind=video 走 mp4 / skeleton 走骨架 png）。 */
export interface MotionCfg {
  kind: 'video' | 'skeleton'
  /** 相对对象目录（如 `views/motion.mp4` / `views/motion-skeleton.png`）。 */
  asset: string
  /** 自由备注（生成链 local 档 prompt 注入：对象名 + note）。 */
  note?: string
}

/** 运镜子选项（card.md `camera:`；enum=8 预设 / describe=自由描述）。 */
export interface CameraCfg {
  kind: 'enum' | 'describe'
  preset?: string
  describe?: string
}

/** 动作参考两槽的固定视图名（导入落 views/<view>.<ext>，与槽一一对应）。 */
export const MOTION_VIEW_NAMES: Record<'video' | 'skeleton', string> = {
  video: 'motion',
  skeleton: 'motion-skeleton',
}

/** 宽容归一 motion（旧后端/损坏卡 → null）。 */
export function normalizeMotion(raw: unknown): MotionCfg | null {
  if (!raw || typeof raw !== 'object') return null
  const m = raw as { kind?: unknown; asset?: unknown; note?: unknown }
  if (m.kind !== 'video' && m.kind !== 'skeleton') return null
  const asset = typeof m.asset === 'string' ? m.asset.trim() : ''
  if (!asset) return null
  return { kind: m.kind, asset, note: typeof m.note === 'string' ? m.note : '' }
}

/** 宽容归一 camera（enum 档校验 preset 合法性；损坏 → null）。 */
export function normalizeCamera(raw: unknown): CameraCfg | null {
  if (!raw || typeof raw !== 'object') return null
  const c = raw as { kind?: unknown; preset?: unknown; describe?: unknown }
  if (c.kind === 'enum') {
    const preset = typeof c.preset === 'string' ? c.preset : ''
    if (!CAMERA_PRESETS.includes(preset)) return null
    return { kind: 'enum', preset }
  }
  if (c.kind === 'describe') {
    const describe = typeof c.describe === 'string' ? c.describe.trim() : ''
    if (!describe) return null
    return { kind: 'describe', describe }
  }
  return null
}
