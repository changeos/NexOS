// =============================================================================
// taskToast.ts —— 任务 ↔ toast 桥（v0.1.11 全局操作反馈系统）。
//
// 三件事（全部纯逻辑 / 工厂闭包，冒烟可直断言——见 scripts/toast-smoke.mjs）：
//   1. taskOutcomeSummary：任务终态「关键产出摘要」——从 output / 环形日志尾
//      解析（分镜 →「12 镜头」/ 清理 →「删 1234 行」/ 分章 →「42 章」…），
//      解析不到返回 ''（上层退化为纯标签通知，不造假）；
//   2. createTaskToastNotifier：提交 loading（「已提交，完成后通知」）→
//      轮询进度文案刷新（「向量化中 3/17」——embed 等分钟级重任务的实时面）→
//      终态 success/error（一次操作的完整闭环；error 带「查看任务」动作钮
//      跳任务中心）。FilmStudio 任务轮询（pollTasks 2s）驱动；
//   3. firstErrorLine：失败原因首行（error 多行日志只取第一行进 toast，
//      完整错误在任务中心看）。
//
// i18n 键：toast.submitted / toast.running / toast.done / toast.donePlain /
// toast.failed / toast.failedPlain / toast.viewTasks（×4 语言）。
// =============================================================================
import { toast } from '../nx/toast'
import { filmTaskStageLabel } from './flowContext'
import { parseChunkProgress } from './pipelineStatus'

/** 翻译面（vue-i18n t 或任意 (key, params?) => string）。 */
export type Translate = (key: string, params?: Record<string, unknown>) => string

/** 任务终态摘要的模式表：正则 → 摘要拼装（顺序即优先级）。 */
const SUMMARY_PATTERNS: { re: RegExp; build: (m: RegExpMatchArray) => string }[] = [
  // 清理：删除/清理/过滤 N 行（广告行/空行等）
  { re: /(?:删除|清理|去掉|过滤|移除)\D{0,12}?(\d+)\s*行/, build: (m) => `删 ${m[1]} 行` },
  // 分镜 / 剧本：N 个镜头
  { re: /(\d+)\s*个?镜头/, build: (m) => `${m[1]} 镜头` },
  // 分章：N 章（「文章」等构词不误伤——数字与「章」间仅容空格/个）
  { re: /(\d+)\s*个?章/, build: (m) => `${m[1]} 章` },
  // 人物梳理：N 位/个人物
  { re: /(\d+)\s*(?:位|个)?人物/, build: (m) => `${m[1]} 位人物` },
  // 分块任务（清理 LLM / 分章 / 梳理 / 向量化）：N 块
  { re: /(\d+)\s*块/, build: (m) => `${m[1]} 块` },
  // 字数产出（正稿等）：N 字
  { re: /(\d+)\s*字/, build: (m) => `${m[1]} 字` },
]

/**
 * 任务终态摘要（从日志尾 **新→旧** 扫描模式表，先命中先赢；再退 output 产物
 * 路径 basename；均无返回 ''——上层纯标签通知）。纯函数，冒烟直断言。
 */
export function taskOutcomeSummary(
  stage: string,
  logTail: string[],
  output?: string | null,
): string {
  void stage // 阶段无关统一模式表（后端日志口径集中演进时再分 stage 特化）
  const lines = [...(logTail ?? [])].reverse()
  for (const line of lines) {
    if (!line) continue
    for (const p of SUMMARY_PATTERNS) {
      const m = line.match(p.re)
      if (m) return p.build(m)
    }
  }
  if (output && typeof output === 'string') {
    const base = output.split('/').pop() ?? ''
    if (base) return base
  }
  return ''
}

/** 失败原因首行（多行 error 只取第一行；截 160 字防刷屏；空 error → ''）。 */
export function firstErrorLine(error?: string | null): string {
  const line = (error ?? '').split('\n').map((s) => s.trim()).find((s) => s.length > 0) ?? ''
  return line.length > 160 ? `${line.slice(0, 160)}…` : line
}

/** 任务标签（stage 白名单本地化 + 关联镜头；与 FilmStudio.taskLabel 同口径）。 */
export function taskNotifyLabel(stage: string, shot: number | null, t: Translate): string {
  const kindText = filmTaskStageLabel(stage || '', t as (key: string) => string)
  return shot ? `${kindText}${t('film.taskShot', { n: shot })}` : kindText
}

/** notifier 消费的任务快照（FilmStudio TrackedTask 投影）。 */
export interface TaskNotifySnapshot {
  id: string
  stage: string
  shot?: number | null
  status: string
  logTail: string[]
  output?: string | null
  error?: string | null
}

export interface TaskToastNotifier {
  /** 提交时：loading toast（「已提交，完成后通知」；返回 id 由调用方持有）。 */
  submitted(stage: string, shot?: number | null): number
  /** 轮询中：日志带「块 X/Y」时刷新进度文案（无进度静默不动）。 */
  progress(toastId: number, stage: string, shot: number | null, logTail: string[]): void
  /** 终态：loading 翻 success（含产出摘要）/ error（首行原因 + 查看任务钮）。 */
  terminal(toastId: number, task: TaskNotifySnapshot): void
}

/**
 * 建任务通知器（FilmStudio 轮询驱动；onViewTasks = 跳工作台并展开任务中心）。
 * toastId 失效（被挤出/手关）时 update 自动重落一条——终态不丢。
 */
export function createTaskToastNotifier(t: Translate, onViewTasks: () => void): TaskToastNotifier {
  return {
    submitted(stage, shot = null) {
      const label = taskNotifyLabel(stage, shot, t)
      return toast.loading(t('toast.submitted'), label)
    },
    progress(toastId, stage, shot, logTail) {
      const counts = parseChunkProgress(logTail ?? [])
      if (!counts) return
      const label = taskNotifyLabel(stage, shot, t)
      toast.loading(toastId, t('toast.running', { label, done: counts.done, total: counts.total }))
    },
    terminal(toastId, task) {
      const label = taskNotifyLabel(task.stage, task.shot ?? null, t)
      if (task.status === 'completed') {
        const summary = taskOutcomeSummary(task.stage, task.logTail, task.output)
        toast.update(
          toastId,
          'success',
          summary ? t('toast.done', { summary }) : t('toast.donePlain'),
          { title: label },
        )
      } else {
        const reason = firstErrorLine(task.error)
        toast.update(toastId, 'error', reason ? t('toast.failed', { reason }) : t('toast.failedPlain'), {
          title: label,
          action: { label: t('toast.viewTasks'), onClick: onViewTasks },
        })
      }
    },
  }
}
