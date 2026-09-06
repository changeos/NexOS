// =============================================================================
// flowContext.ts —— 流程页共享上下文（provide/inject，FilmStudio provide）。
//
// 五个流程页 + 设置页 + 导航徽章共享的会话态：项目引用 / 模型源下拉 /
// 任务中心 / 能力可用性 / README 阶段 / 多人分工（ownership + 操作人 + 活动
// 流）。页面经 useFlow() 注入（缺上下文的宿主环境返回 null——组件内优雅
// 降级为空态，不崩）。
// =============================================================================

import { inject, type ComputedRef, type InjectionKey, type Ref } from 'vue'
import type { FilmModelRef, FilmProject, FilmTask } from '../api'
import type { FilmActivityEntry, FilmOwnership } from './collab'
import type { FilmStage, FlowView } from './flowTypes'

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
}

/** provide/inject 键。 */
export const FLOW_CONTEXT_KEY: InjectionKey<FlowContext> = Symbol('film-flow-context')

/** 便捷注入（缺上下文返回 null——页面降级为空态）。 */
export function useFlow(): FlowContext | null {
  return inject(FLOW_CONTEXT_KEY, null)
}
