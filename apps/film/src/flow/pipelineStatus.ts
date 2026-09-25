// =============================================================================
// pipelineStatus.ts —— 剧情文档管线状态聚合（纯函数，v0.1.7 UI 重设计第一批）。
//
// 数据源（真实可得，不造假进度）：
//   任务中心轮询 GET /film/tasks/:id 的 log 环形日志尾。后端 story 管线
//   （film_hub.rs）分块执行时逐块记日志，格式已核实：
//     · 计划行：「原文 240000 字符 → 17 块 × ≤6K（重叠上下文 …）」
//               「LLM 深清：17 块 × ≤6K 字符（并发≤8…）」
//     · 进度行：「块 3/17 完成」/「块 3/17 完成（2 个章节断点）」
//     · 失败行：「LLM 深清块 3/17 失败：…」
//   → 「完成 X/Y」离散计数**可解析**；解析不到（如 AI 写剧情单段生成、或日志
//     尚未刷出计划行）时 counts=null，UI 显示运行态 + 已耗时，绝不编造数字。
//
// 聚合规则（每源一行徽章，按任务优先级）：
//   failed > running > queued > done > idle——运行/排队取管线序最靠前的任务
//   （清理 → 分章 → 人物梳理 → 向量化）；失败收集全部失败阶段供行内重试。
// =============================================================================

/** 剧情管线四阶段（管线序即聚合优先序）。 */
export type StoryPipelineStage =
  | 'story.clean'
  | 'story.chapterize'
  | 'story.profile'
  | 'story.embed'

/** 管线阶段序（清理 → 分章 → 人物梳理 → 向量化）。 */
export const STORY_PIPELINE_STAGES: readonly StoryPipelineStage[] = [
  'story.clean',
  'story.chapterize',
  'story.profile',
  'story.embed',
]

/** 任务中心条目快照（FilmStudio trackedTasks → FlowContext.storyTasks）。 */
export interface PipelineTaskSnapshot {
  id: string
  /** 任务阶段（story.clean 等；白名单外不参与聚合）。 */
  stage: string
  /** queued / running / completed / failed（前端统一映射后口径）。 */
  status: string
  /** 关联源文件路径（发起时由 StoryPage 登记的 source_file）。 */
  sourceFile: string
  /** 环形日志尾（分块进度解析输入）。 */
  log: string[]
  error?: string | null
  createdAt?: number | null
}

/** 源行聚合态（徽章词映射：idle 无徽章 / queued 排队 / running 运行中（细分
 *  阶段词：清理中/分章中/梳理中/向量化中）/ failed 失败 / done 成功）。 */
export interface SourcePipelineAggregate {
  key: 'idle' | 'queued' | 'running' | 'failed' | 'done'
  /** 聚焦阶段（running/queued=最靠前进行任务；failed=最近失败任务；done=最深完成）。 */
  stage: StoryPipelineStage | ''
  /** 当前任务离散计数（日志解析；null=不可解析——显示运行态+耗时）。 */
  counts: { done: number; total: number } | null
  /** 失败原因尾（failed 时）。 */
  error: string
  /** 全部失败阶段（行内重试目标；可能多个）。 */
  failedStages: StoryPipelineStage[]
  /** 关联任务数（popover 明细行数）。 */
  taskCount: number
}

/** 阶段的管线序（白名单外 -1——不参与聚合）。 */
export function pipelineStageRank(stage: string): number {
  return (STORY_PIPELINE_STAGES as readonly string[]).indexOf(stage)
}

/**
 * 日志尾 → 分块离散计数（完成 X/Y）。
 * 优先取**最后一条**「块 X/Y」行（失败行同格式——进度停在失败点）；
 * 无进度行时取计划行（「→ N 块 ×…」/「：N 块 ×…」）得 {done:0,total:N}；
 * 都没有返回 null（不可解析——UI 显示运行态+已耗时，不造假进度）。
 */
export function parseChunkProgress(log: readonly string[]): { done: number; total: number } | null {
  let lastProgress: { done: number; total: number } | null = null
  let planTotal = 0
  for (const line of log) {
    if (typeof line !== 'string' || !line) continue
    const prog = /块\s*(\d+)\s*\/\s*(\d+)/.exec(line)
    if (prog) {
      const done = Number(prog[1])
      const total = Number(prog[2])
      if (Number.isFinite(done) && Number.isFinite(total) && total > 0) {
        lastProgress = { done: Math.min(done, total), total }
      }
      continue
    }
    const plan = /(?:→|：)\s*(\d+)\s*块/.exec(line)
    if (plan) {
      const n = Number(plan[1])
      if (Number.isFinite(n) && n > 0) planTotal = n
    }
  }
  if (lastProgress) return lastProgress
  if (planTotal > 0) return { done: 0, total: planTotal }
  return null
}

/**
 * 单源聚合（按任务优先级：failed > running > queued > done > idle）。
 * 同级多任务取管线序最靠前者（清理先行）；失败聚合收集全部失败阶段。
 */
export function aggregateSourcePipeline(
  tasks: readonly PipelineTaskSnapshot[],
  sourceFile: string,
): SourcePipelineAggregate {
  const mine = tasks.filter(
    (t) => t.sourceFile === sourceFile && pipelineStageRank(t.stage) >= 0,
  )
  const base: SourcePipelineAggregate = {
    key: 'idle',
    stage: '',
    counts: null,
    error: '',
    failedStages: [],
    taskCount: mine.length,
  }
  if (!mine.length) return base

  const byRank = (a: PipelineTaskSnapshot, b: PipelineTaskSnapshot) =>
    pipelineStageRank(a.stage) - pipelineStageRank(b.stage)

  const failed = mine.filter((t) => t.status === 'failed').sort(byRank)
  if (failed.length) {
    const focus = failed[0]
    return {
      ...base,
      key: 'failed',
      stage: focus.stage as StoryPipelineStage,
      counts: parseChunkProgress(focus.log ?? []),
      error: (focus.error ?? '').trim(),
      failedStages: failed.map((t) => t.stage as StoryPipelineStage),
    }
  }

  const running = mine.filter((t) => t.status === 'running').sort(byRank)
  if (running.length) {
    const focus = running[0]
    return {
      ...base,
      key: 'running',
      stage: focus.stage as StoryPipelineStage,
      counts: parseChunkProgress(focus.log ?? []),
    }
  }

  const queued = mine.filter((t) => t.status === 'queued').sort(byRank)
  if (queued.length) {
    return { ...base, key: 'queued', stage: queued[0].stage as StoryPipelineStage, counts: null }
  }

  // 全部 completed → 成功（徽章在任务中心条目存续期内展示；产物徽章另由树派生）
  const doneSorted = [...mine].sort(byRank)
  const deepest = doneSorted[doneSorted.length - 1]
  return { ...base, key: 'done', stage: (deepest?.stage ?? '') as StoryPipelineStage }
}

/** 任务是否仍在进行（queued/running——6 秒静默轮询活跃判据）。 */
export function isPipelineTaskActive(status: string): boolean {
  return status === 'queued' || status === 'running'
}
