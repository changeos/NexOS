// =============================================================================
// flowContext.ts —— 流程页共享上下文（provide/inject，FilmStudio provide）。
//
// 五个流程页 + 设置页 + 导航徽章共享的会话态：项目引用 / 模型源下拉 /
// 任务中心 / 能力可用性 / README 阶段 / 多人分工（ownership + 操作人 + 活动
// 流）。页面经 useFlow() 注入（缺上下文的宿主环境返回 null——组件内优雅
// 降级为空态，不崩）。
// =============================================================================

import { inject, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import type {
  FilmCastType,
  FilmModelRef,
  FilmProject,
  FilmProjectModels,
  FilmTask,
} from '../api'
import type { FilmActivityEntry, FilmOwnership } from './collab'
import type { FilmStage, FlowView } from './flowTypes'
import type { PipelineTaskSnapshot } from './pipelineStatus'

/** 模型源下拉选项（单组内；relay=🌐 联邦中继渠道）。 */
export interface FlowModelOption {
  key: string
  label: string
  relay: boolean
}

/** 模型源下拉分组（optgroup）。 */
export interface FlowModelGroup {
  label: string
  options: FlowModelOption[]
}

/** 能力面（与 FilmModelRef.capability 一致）。 */
export type FlowCap = FilmModelRef['capability']

/** 流程页共享上下文（FilmStudio 构造并 provide）。 */
export interface FlowContext {
  /** 当前项目（响应式；reloadProject 刷新）。 */
  project: Ref<FilmProject | null>
  /** 模型源下拉分组（复用 FilmStudio 的本地实例+网关渠道加载）。 */
  optionsFor(cap: FlowCap): FlowModelGroup[]
  /** 某能力面是否有可选项。 */
  hasOptionsFor(cap: FlowCap): boolean
  /** 各能力面选中的下拉 key（与工作台共享同一份选择）。 */
  modelSel: Record<FlowCap, string>
  /** 下拉 key → 契约 model_ref（无效选择 null）。 */
  modelRefFor(cap: FlowCap): FilmModelRef | null
  /** 任务进任务中心（2s 轮询；终态刷新项目+流程页）。 */
  addTracked(task: FilmTask): void
  /** 任务以 id+stage 显式进任务中心（管线按钮 202 后调用——响应缺 stage
   *  也不失真；同 id 去重防重复登记；轮询/终态联动与 addTracked 同路）。
   *  v0.1.7 meta 扩展：sourceFile 关联源文件（剧情页源行聚合徽章）、createdAt
   *  任务创建时刻（已耗时口径）——旧调用两参照常。 */
  trackFilmTask(
    id: string,
    stage: string,
    meta?: { sourceFile?: string; createdAt?: number | null },
  ): void
  /** 剧情管线任务快照（v0.1.7 源列表聚合徽章 / X-Y 计数数据源；含源关联、
   *  日志尾与状态——pipelineStatus.aggregateSourcePipeline 消费）。 */
  storyTasks: ComputedRef<PipelineTaskSnapshot[]>
  /** 统一错误文案（404/405 给「后端可能尚未就绪」口径）。 */
  errMsg(e: unknown): string
  /** 数据刷新版本号（任务终态/项目重载时 ++；页面 watch 后重载自己的数据）。 */
  refreshTick: Ref<number>
  /** 项目详情重载（保守合并）。 */
  reloadProject(): Promise<void>
  // —— 能力可用性（生成按钮置灰口径与工作台一致）——
  chatAvailable: ComputedRef<boolean>
  channelAvailable: ComputedRef<boolean>
  composeAvailable: ComputedRef<boolean>
  isOffline: ComputedRef<boolean>
  // —— 流程阶段（README frontmatter stage；'' = 未知）——
  stage: Ref<FilmStage | ''>
  /** 当前视图（导航高亮源）。 */
  view: Ref<FlowView>
  /** 切视图（「去工作台细调」/「预览成片」等页间跳转）。 */
  setView(v: FlowView): void
  // —— 多人分工 v1 ——
  /** ownership.json（成员 + 分区认领 + 定妆对象认领；null=未加载/无文件）。 */
  ownership: Ref<FilmOwnership | null>
  /** 当前操作人（「我是」；缺省 anonymous；写操作 author 字段）。 */
  author: Ref<string>
  /** 活动流（activity.json 最近条目，新→旧）。 */
  activity: Ref<FilmActivityEntry[]>
  /** 改操作人（localStorage 记忆）。 */
  setAuthor(name: string): void
  /** 保存 ownership（PUT files/ownership.json + 本地态更新；成功 true）。 */
  saveOwnership(next: FilmOwnership): Promise<boolean>
  /** 刷新协作态（ownership + activity；写操作后调用）。 */
  refreshCollab(): Promise<void>
  // —— Hub 浏览页互跳（v0.1.1）——
  /** 待选中的定妆对象（Hub 浏览「在工作台打开」casting/* 路径时置位；定妆页
   *  挂载/更新时消费并清空——切 Tab + 选中对象）。 */
  pendingCastSelect: Ref<{ type: FilmCastType; name: string } | null>
  /** 待打开的 Hub 文件路径（v0.1.37.1 SideNav 树卡点击文件置位；HubBrowse
   *  挂载/更新时消费并清空——切到 hub 视图 + 选中该文件并加载内容）。 */
  pendingHubFile: Ref<string | null>
  /** 待预填的分镜章节范围（v0.1.44 剧情页章节卡「从此章生成分镜」置位；
   *  StoryboardGenPanel 挂载/更新时消费并清空——切分镜页 + 下拉选中该章
   *  + 展开面板）。 */
  pendingStoryboardChapter: Ref<string | null>
  // —— 项目级模型设置（v0.1.38：models.json 八能力位默认源）——
  /** models 配置快照（GET :id/models；null=未加载/后端未就绪）。 */
  projectModels: Ref<FilmProjectModels | null>
  /** 刷新 models 配置快照（模型设置页/PUT 后调用）。 */
  reloadProjectModels(): Promise<void>
  /** 某能力位项目默认配置摘要（「项目默认」选项小字与 ModelsPage 行；''=未设置）。 */
  defaultModelSummary(cap: string): string
  /** 某能力位下拉是否就绪（有可选项或选中「项目默认」——生成按钮置灰口径）。 */
  modelSelReady(cap: FlowCap): boolean
  /** 选中「项目默认」与否（true=生成时不传 model_ref，走 models.json 缺省链）。 */
  isProjectDefaultSel(cap: FlowCap): boolean
}

/** 「项目默认」下拉项 key（选中=不传 model_ref——后端读 models.json 缺省链）。 */
export const PROJECT_DEFAULT_KEY = 'project'

// —— 任务中心 stage 标签（纯函数导出，冒烟脚本可直断言） ——

/**
 * 任务中心已知 stage 白名单（taskLabel 本地化口径）。v0.1.6 起管线任务统一
 * 进任务中心，新增 story 系（story=AI 写剧情 / story.clean=清理 /
 * story.chapterize=分章 / story.profile=人物梳理 / story.embed=向量化）
 * 与 casting（定妆提取）；storyboard 原有。白名单外 stage 原样展示不崩。
 */
export const FILM_TASK_STAGES: readonly string[] = [
  'script',
  'storyboard',
  'image',
  'video',
  'tts',
  'music',
  'compose',
  'portrait',
  'story',
  'story.clean',
  'story.chapterize',
  'story.profile',
  'story.embed',
  'casting',
]

/** stage → film.kXxx i18n 键（点分驼峰：story.clean → film.kStoryClean）。 */
export function filmTaskStageKey(stage: string): string {
  return (
    'film.k' +
    stage
      .split('.')
      .map((p) => p.charAt(0).toUpperCase() + p.slice(1))
      .join('')
  )
}

/** 白名单内 stage → 本地化标签；白名单外原样返回（未知 stage 防崩）。 */
export function filmTaskStageLabel(stage: string, t: (key: string) => string): string {
  return FILM_TASK_STAGES.includes(stage) ? t(filmTaskStageKey(stage)) : stage
}

/** provide/inject 键。 */
export const FLOW_CONTEXT_KEY: InjectionKey<FlowContext> = Symbol('film-flow-context')

/** 便捷注入（缺上下文返回 null——页面降级为空态）。 */
export function useFlow(): FlowContext | null {
  return inject(FLOW_CONTEXT_KEY, null)
}
