<script setup lang="ts">
// =============================================================================
// CollabPage.vue —— PR/Issues 协作页（v0.1.10，FilmHub 完整协作层前端面）。
//
// 两 Tab：Issues（列表：状态徽章三色 open 蓝 / in-progress 琥珀 / closed 灰、
// labels、assignee、stage 环节归因标签紫、评论展开详情 + 评论流）+ PRs
// （列表：branch→base、状态、diff 查看 pre 展开截断、merge 确认弹窗）。真值在
// hub/collab/issues.json · prs.json（文件即真值——REST 便捷面，agent 亦可经
// files GET 直读双通道）。PR 的 branch 映射项目 git 仓（新建分支下拉 =
// GET :id/git/branches）。
//
// 任务中心 / 剧情页失败行的「提 Issue」按钮（from-task）成功后跳转本页
// （ctx.setView('collab')）——环节错误归因闭环的可视落点。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import {
  filmCommentIssue,
  filmCommentPr,
  filmCreateIssue,
  filmCreatePr,
  filmGetPrDiff,
  filmGitBranches,
  filmListIssues,
  filmListPrs,
  filmMergePr,
  filmUpdateIssue,
  filmUpdatePr,
  type FilmIssue,
  type FilmIssueState,
  type FilmPr,
  type FilmPrState,
} from '../api'
import { useFlow } from './flowContext'
import NxButton from '../nx/NxButton.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
import { fmtActivityTime } from './collab'
// v0.1.11 全局操作反馈：状态流转/评论/创建/合并成败 toast
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

// —— Tab 态（issues | prs）与过滤态 ——
type Tab = 'issues' | 'prs'
const tab = ref<Tab>('issues')
type IssueFilter = 'open' | 'closed' | 'all'
type PrFilter = 'open' | 'merged' | 'closed' | 'all'
const issueFilter = ref<IssueFilter>('open')
const prFilter = ref<PrFilter>('open')

const issues = ref<FilmIssue[]>([])
const issueCounts = ref<{ open?: number; in_progress?: number; closed?: number }>({})
const prs = ref<FilmPr[]>([])
const prCounts = ref<{ open?: number; merged?: number; closed?: number }>({})
const loading = ref(false)
const listError = ref('')

async function reload(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  loading.value = true
  listError.value = ''
  try {
    const [is, ps] = await Promise.all([
      filmListIssues(pid, issueFilter.value),
      filmListPrs(pid, prFilter.value),
    ])
    issues.value = is.issues ?? []
    issueCounts.value = is.counts ?? {}
    prs.value = ps.prs ?? []
    prCounts.value = ps.counts ?? {}
  } catch (e) {
    listError.value = ctx ? ctx.errMsg(e) : String(e)
  } finally {
    loading.value = false
  }
}

onMounted(() => void reload())
watch(
  () => [ctx?.refreshTick.value, issueFilter.value, prFilter.value],
  () => void reload(),
)

// —— Issue 状态徽章/动作（三态流转 open ⇄ in-progress ⇄ closed） ——

function issueStatePill(s: FilmIssueState): string {
  // open 蓝 / in-progress 琥珀 / closed 灰（三色徽章）
  if (s === 'closed') return 'fh-pill fh-pill-muted fh-pill-mini'
  if (s === 'in-progress') return 'fh-pill fh-pill-amber fh-pill-mini'
  return 'fh-pill fh-pill-blue fh-pill-mini'
}

function issueStateLabel(s: FilmIssueState): string {
  if (s === 'closed') return t('collab.stClosed')
  if (s === 'in-progress') return t('collab.stProgress')
  return t('collab.stOpen')
}

function prStatePill(s: FilmPrState): string {
  // open 蓝 / merged 绿 / closed 灰
  if (s === 'merged') return 'fh-pill fh-pill-ok fh-pill-mini'
  if (s === 'closed') return 'fh-pill fh-pill-muted fh-pill-mini'
  return 'fh-pill fh-pill-blue fh-pill-mini'
}

function prStateLabel(s: FilmPrState): string {
  if (s === 'merged') return t('collab.stMerged')
  if (s === 'closed') return t('collab.stClosed')
  return t('collab.stOpen')
}

// —— 展开态（issue / pr 详情 + 评论流；同一时刻每类至多展开一个） ——
const openIssue = ref<number | null>(null)
const openPr = ref<number | null>(null)

function toggleIssue(iid: number): void {
  openIssue.value = openIssue.value === iid ? null : iid
}

function togglePr(pid: number): void {
  openPr.value = openPr.value === pid ? null : pid
  if (openPr.value === pid) void loadDiff(pid)
}

// —— 状态流转（issue 三态按钮） ——

const stateBusy = ref<number | null>(null)

async function setIssueState(i: FilmIssue, state: FilmIssueState): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || stateBusy.value) return
  stateBusy.value = i.iid
  try {
    await filmUpdateIssue(pid, i.iid, { state, author: ctx?.author.value })
    await reload()
    // v0.1.11 保存类：Issue 状态流转成功全局反馈
    toast.success(t('toast.saved'))
  } catch (e) {
    listError.value = ctx ? ctx.errMsg(e) : String(e)
    toast.error(ctx ? ctx.errMsg(e) : String(e))
  } finally {
    stateBusy.value = null
  }
}

async function closePr(p: FilmPr): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || stateBusy.value) return
  if (!window.confirm(t('collab.prCloseConfirm', { n: p.pid }))) return
  stateBusy.value = p.pid
  try {
    await filmUpdatePr(pid, p.pid, { state: 'closed', author: ctx?.author.value })
    await reload()
    // v0.1.11 危险确认类：PR 关闭成败全局反馈
    toast.success(t('toast.prClosed', { n: p.pid }))
  } catch (e) {
    listError.value = ctx ? ctx.errMsg(e) : String(e)
    toast.error(ctx ? ctx.errMsg(e) : String(e))
  } finally {
    stateBusy.value = null
  }
}

// —— 评论（issue/PR 同语义：环形 200；展开区底部输入行） ——

const commentDraft = ref<Record<string, string>>({})

async function sendComment(kind: 'issue' | 'pr', n: number): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  const key = `${kind}-${n}`
  const body = (commentDraft.value[key] ?? '').trim()
  if (!body) return
  try {
    if (kind === 'issue') {
      await filmCommentIssue(pid, n, { author: ctx?.author.value, body })
    } else {
      await filmCommentPr(pid, n, { author: ctx?.author.value, body })
    }
    commentDraft.value[key] = ''
    await reload()
    if (kind === 'issue') openIssue.value = n
    else openPr.value = n
    // v0.1.11 保存类：评论发出全局反馈（展开区流即落点，toast 补确认）
    toast.success(t('toast.saved'))
  } catch (e) {
    listError.value = ctx ? ctx.errMsg(e) : String(e)
    toast.error(ctx ? ctx.errMsg(e) : String(e))
  }
}

// —— 新建 Issue（弹窗：title/body/labels 逗号分隔/assignee/stage） ——

const issueFormOpen = ref(false)
const issueForm = ref({ title: '', body: '', labels: '', assignee: '', stage: '' })
const createBusy = ref(false)
const createError = ref('')

function openIssueForm(): void {
  issueFormOpen.value = true
  createError.value = ''
  issueForm.value = { title: '', body: '', labels: '', assignee: '', stage: '' }
}

async function submitIssue(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || createBusy.value) return
  createError.value = ''
  const f = issueForm.value
  if (!f.title.trim()) {
    createError.value = t('collab.errTitle')
    return
  }
  createBusy.value = true
  try {
    const labels = f.labels.split(/[,，]/).map((s) => s.trim()).filter(Boolean)
    const created = await filmCreateIssue(pid, {
      title: f.title.trim(),
      ...(f.body.trim() ? { body: f.body.trim() } : {}),
      ...(labels.length ? { labels } : {}),
      ...(f.assignee.trim() ? { assignee: f.assignee.trim() } : {}),
      ...(f.stage.trim() ? { stage: f.stage.trim() } : {}),
      author: ctx?.author.value,
    })
    issueFormOpen.value = false
    issueFilter.value = 'open'
    await reload()
    openIssue.value = created.iid
    // v0.1.11 跳转/创建类：Issue 建成全局反馈（列表展开即落点，toast 补确认）
    toast.success(t('toast.issueCreated', { n: created.iid }))
  } catch (e) {
    createError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('collab.createFailed')
  } finally {
    createBusy.value = false
  }
}

// —— 新建 PR（弹窗：title/branch 下拉=git 分支/base/issue_refs/body） ——

const prFormOpen = ref(false)
const prForm = ref({ title: '', branch: '', base: 'main', refs: '', body: '' })
const branches = ref<{ name: string; hash?: string }[]>([])
const currentBranch = ref('')

async function openPrForm(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid) return
  prFormOpen.value = true
  createError.value = ''
  prForm.value = { title: '', branch: '', base: 'main', refs: '', body: '' }
  try {
    const res = await filmGitBranches(pid)
    branches.value = res.branches ?? []
    currentBranch.value = res.current ?? ''
    // 缺省选当前分支之外的首个非 base 分支（无则空=提示先建分支）
    const first = branches.value.find((b) => b.name !== 'main' && b.name !== currentBranch.value)
    prForm.value.branch = first?.name ?? ''
  } catch (e) {
    branches.value = []
    createError.value = ctx ? ctx.errMsg(e) : String(e)
  }
}

/** PR 分支下拉候选（排除 base 自身——branch==base 后端 400）。 */
const branchOptions = computed(() =>
  branches.value.filter((b) => b.name !== prForm.value.base),
)

async function submitPr(): Promise<void> {
  const pid = ctx?.project.value?.id
  if (!pid || createBusy.value) return
  createError.value = ''
  const f = prForm.value
  if (!f.title.trim()) {
    createError.value = t('collab.errTitle')
    return
  }
  if (!f.branch) {
    createError.value = t('collab.errBranch')
    return
  }
  createBusy.value = true
  try {
    const refs = f.refs.split(/[,，\s]+/).map((s) => parseInt(s, 10)).filter((n) => Number.isInteger(n) && n > 0)
    const created = await filmCreatePr(pid, {
      title: f.title.trim(),
      branch: f.branch,
      base: f.base.trim() || 'main',
      ...(refs.length ? { issue_refs: refs } : {}),
      ...(f.body.trim() ? { body: f.body.trim() } : {}),
      author: ctx?.author.value,
    })
    prFormOpen.value = false
    prFilter.value = 'open'
    await reload()
    openPr.value = created.pid
    // v0.1.11 创建类：PR 建成全局反馈
    toast.success(t('toast.prCreated', { n: created.pid }))
  } catch (e) {
    createError.value = (ctx ? ctx.errMsg(e) : String(e)) || t('collab.createFailed')
  } finally {
    createBusy.value = false
  }
}

// —— diff 查看（PR 展开区：pre 截断 + 展开全部） ——

const diffCache = ref<Record<number, { text: string; truncated: boolean }>>({})
const diffBusy = ref<number | null>(null)
const diffFull = ref<Record<number, boolean>>({})

/** 展示用 diff（默认截断 4000 字符 + 截断标注；展开全部按钮翻全量）。 */
const DIFF_PREVIEW_CHARS = 4000

function diffPreview(pid: number): string {
  const d = diffCache.value[pid]
  if (!d) return ''
  if (diffFull.value[pid] || d.text.length <= DIFF_PREVIEW_CHARS) return d.text
  return d.text.slice(0, DIFF_PREVIEW_CHARS) + '\n…' + t('collab.diffTruncated', { n: d.text.length })
}

async function loadDiff(pid: number): Promise<void> {
  const p = ctx?.project.value?.id
  if (!p || diffCache.value[pid] || diffBusy.value) return
  diffBusy.value = pid
  try {
    const res = await filmGetPrDiff(p, pid)
    diffCache.value[pid] = { text: res.diff ?? '', truncated: !!res.truncated }
  } catch (e) {
    diffCache.value[pid] = {
      text: ctx ? ctx.errMsg(e) : String(e),
      truncated: false,
    }
  } finally {
    diffBusy.value = null
  }
}

// —— merge 确认弹窗（nx 体系：fh-modal；确认后 merge + 自动关 issue 提示） ——

const merging = ref<FilmPr | null>(null)
const mergeBusy = ref(false)
const mergeNotice = ref('')

function askMerge(p: FilmPr): void {
  merging.value = p
  mergeNotice.value = ''
}

async function doMerge(): Promise<void> {
  const pid = ctx?.project.value?.id
  const p = merging.value
  if (!pid || !p || mergeBusy.value) return
  mergeBusy.value = true
  try {
    const res = await filmMergePr(pid, p.pid, { author: ctx?.author.value })
    mergeNotice.value = t('collab.mergeDone', {
      n: p.pid,
      c: (res.merge_commit ?? '').slice(0, 8),
      issues: (res.closed_issues ?? []).join('、') || '—',
    })
    merging.value = null
    await reload()
    await ctx?.refreshCollab()
    // v0.1.11 提交类：merge 成败全局反馈（页内 ✓ 行保留——toast 全页可见）
    toast.success(mergeNotice.value)
  } catch (e) {
    mergeNotice.value = ''
    createError.value = ctx ? ctx.errMsg(e) : String(e)
    toast.error(ctx ? ctx.errMsg(e) : String(e))
  } finally {
    mergeBusy.value = false
  }
}

/** refs 展示（#1 #2 …）。 */
function refsLabel(p: FilmPr): string {
  return (p.issue_refs ?? []).map((n) => `#${n}`).join(' ') || ''
}
</script>

<template>
  <div class="fh-page nx-page">
    <!-- 页头：🧭 协作 + json 真值路径 pill + 刷新 -->
    <div class="fh-head">
      <span class="fh-head-title">🧭 {{ t('collab.title') }}</span>
      <div class="fh-head-actions">
        <NxThemeToggle />
        <span class="fh-pill fh-pill-muted fh-pill-mini mono">hub/collab/*.json</span>
        <button
          class="fh-btn fh-btn-small"
          type="button"
          :disabled="loading || !ctx?.project.value"
          :title="t('film.refresh')"
          @click="reload()"
        >↻</button>
      </div>
    </div>
    <div v-if="mergeNotice" class="fh-collab-tip collab-notice">✓ {{ mergeNotice }}</div>
    <div v-if="createError" class="fh-error-box">{{ createError }}</div>
    <div v-if="listError" class="fh-error-box">{{ listError }}</div>

    <!-- Tab 切换：Issues / PRs -->
    <div class="fh-tabs cp-tabs">
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'issues' }"
        type="button"
        @click="tab = 'issues'"
      >🐛 {{ t('collab.tabIssues') }}（{{ issueCounts.open ?? 0 }}）</button>
      <button
        class="fh-tab"
        :class="{ 'is-active': tab === 'prs' }"
        type="button"
        @click="tab = 'prs'"
      >🔀 {{ t('collab.tabPrs') }}（{{ prCounts.open ?? 0 }}）</button>
    </div>

    <div class="fh-page-scroll">
      <!-- ================= Issues Tab ================= -->
      <section v-if="tab === 'issues'" class="fh-card">
        <div class="fh-card-head">
          <div class="fh-tabs">
            <button
              v-for="f in ['open', 'closed', 'all'] as IssueFilter[]"
              :key="f"
              class="fh-tab"
              :class="{ 'is-active': issueFilter === f }"
              type="button"
              @click="issueFilter = f"
            >{{ f === 'open' ? t('collab.fOpen', { n: issueCounts.open ?? 0 })
              : f === 'closed' ? t('collab.fClosed', { n: issueCounts.closed ?? 0 })
              : t('collab.fAll') }}</button>
          </div>
          <button
            class="fh-btn fh-btn-small fh-btn-primary"
            type="button"
            :disabled="!ctx?.project.value"
            @click="openIssueForm()"
          >＋ {{ t('collab.newIssue') }}</button>
        </div>
        <div class="fh-card-body">
          <div v-if="!issues.length && !listError" class="fh-empty">
            {{ loading ? t('film.loading') : t('collab.issuesEmpty') }}
          </div>
          <div v-for="i in issues" :key="i.iid" class="cp-item">
            <!-- 概要行：状态徽章 + #iid + 标题 + stage 归因 + labels + assignee -->
            <div
              class="fh-row cp-row"
              role="button"
              :tabindex="0"
              @click="toggleIssue(i.iid)"
              @keydown.enter="toggleIssue(i.iid)"
            >
              <span :class="issueStatePill(i.state)">{{ issueStateLabel(i.state) }}</span>
              <span class="fh-mono cp-iid">#{{ i.iid }}</span>
              <span class="cp-title fh-ellipsis">{{ i.title }}</span>
              <span
                v-if="i.stage"
                class="fh-pill fh-pill-violet fh-pill-mini"
                :title="t('collab.stageTip')"
              >§ {{ i.stage }}</span>
              <span
                v-for="l in i.labels ?? []"
                :key="l"
                class="fh-pill fh-pill-muted fh-pill-mini"
              >{{ l }}</span>
              <span class="fh-muted fh-small cp-meta">
                {{ i.author }}<template v-if="i.assignee"> → {{ i.assignee }}</template>
                · {{ fmtActivityTime(i.created_at) }}
                <template v-if="(i.comments ?? []).length"> · 💬 {{ (i.comments ?? []).length }}</template>
              </span>
              <span class="cp-caret" aria-hidden="true">{{ openIssue === i.iid ? '▾' : '▸' }}</span>
            </div>
            <!-- 展开详情：body + 状态流转按钮 + 评论流 + 评论输入 -->
            <div v-if="openIssue === i.iid" class="cp-detail">
              <pre v-if="i.body" class="fh-pre cp-body">{{ i.body }}</pre>
              <div v-if="i.closed_at" class="fh-muted fh-small">
                {{ t('collab.closedAt') }}{{ fmtActivityTime(i.closed_at) }}
              </div>
              <div class="cp-actions">
                <NxButton
                  v-if="i.state === 'open'"
                  size="sm"
                  :disabled="stateBusy !== null"
                  @click="setIssueState(i, 'in-progress')"
                >{{ t('collab.btnStart') }}</NxButton>
                <NxButton
                  v-if="i.state !== 'closed'"
                  variant="destructive"
                  size="sm"
                  :disabled="stateBusy !== null"
                  @click="setIssueState(i, 'closed')"
                >{{ t('collab.btnClose') }}</NxButton>
                <NxButton
                  v-if="i.state === 'closed'"
                  size="sm"
                  :disabled="stateBusy !== null"
                  @click="setIssueState(i, 'open')"
                >{{ t('collab.btnReopen') }}</NxButton>
              </div>
              <div v-if="(i.comments ?? []).length" class="cp-comments">
                <div v-for="(c, idx) in i.comments ?? []" :key="idx" class="cp-comment">
                  <span class="cp-comment-head">
                    <b>{{ c.author }}</b>
                    <span class="fh-muted fh-small">{{ fmtActivityTime(c.created_at) }}</span>
                  </span>
                  <div class="cp-comment-body">{{ c.body }}</div>
                </div>
              </div>
              <div class="cp-comment-input">
                <input
                  v-model="commentDraft[`issue-${i.iid}`]"
                  class="fh-input"
                  type="text"
                  :placeholder="t('collab.commentPh')"
                  @keydown.enter="sendComment('issue', i.iid)"
                >
                <NxButton
                  size="sm"
                  :disabled="!(commentDraft[`issue-${i.iid}`] ?? '').trim()"
                  @click="sendComment('issue', i.iid)"
                >{{ t('collab.commentSend') }}</NxButton>
              </div>
            </div>
          </div>
        </div>
      </section>

      <!-- ================= PRs Tab ================= -->
      <section v-else class="fh-card">
        <div class="fh-card-head">
          <div class="fh-tabs">
            <button
              v-for="f in ['open', 'merged', 'closed', 'all'] as PrFilter[]"
              :key="f"
              class="fh-tab"
              :class="{ 'is-active': prFilter === f }"
              type="button"
              @click="prFilter = f"
            >{{ f === 'open' ? t('collab.fOpen', { n: prCounts.open ?? 0 })
              : f === 'merged' ? t('collab.fMerged', { n: prCounts.merged ?? 0 })
              : f === 'closed' ? t('collab.fClosed', { n: prCounts.closed ?? 0 })
              : t('collab.fAll') }}</button>
          </div>
          <button
            class="fh-btn fh-btn-small fh-btn-primary"
            type="button"
            :disabled="!ctx?.project.value"
            @click="openPrForm()"
          >＋ {{ t('collab.newPr') }}</button>
        </div>
        <div class="fh-card-body">
          <div v-if="!prs.length && !listError" class="fh-empty">
            {{ loading ? t('film.loading') : t('collab.prsEmpty') }}
          </div>
          <div v-for="p in prs" :key="p.pid" class="cp-item">
            <!-- 概要行：状态徽章 + !pid + 标题 + branch→base + refs -->
            <div
              class="fh-row cp-row"
              role="button"
              :tabindex="0"
              @click="togglePr(p.pid)"
              @keydown.enter="togglePr(p.pid)"
            >
              <span :class="prStatePill(p.state)">{{ prStateLabel(p.state) }}</span>
              <span class="fh-mono cp-iid">!{{ p.pid }}</span>
              <span class="cp-title fh-ellipsis">{{ p.title }}</span>
              <span class="fh-pill fh-pill-blue fh-pill-mini fh-mono">{{ p.branch }} → {{ p.base }}</span>
              <span
                v-if="refsLabel(p)"
                class="fh-pill fh-pill-violet fh-pill-mini"
                :title="t('collab.refsTip')"
              >{{ refsLabel(p) }}</span>
              <span class="fh-muted fh-small cp-meta">
                {{ p.author }} · {{ fmtActivityTime(p.created_at) }}
                <template v-if="(p.comments ?? []).length"> · 💬 {{ (p.comments ?? []).length }}</template>
              </span>
              <span class="cp-caret" aria-hidden="true">{{ openPr === p.pid ? '▾' : '▸' }}</span>
            </div>
            <!-- 展开详情：body + diff（截断/展开全部）+ merge/关闭 + 评论流 -->
            <div v-if="openPr === p.pid" class="cp-detail">
              <pre v-if="p.body" class="fh-pre cp-body">{{ p.body }}</pre>
              <div v-if="p.state === 'merged' && p.merge_commit" class="fh-muted fh-small">
                {{ t('collab.mergedInfo', {
                  by: p.merged_by ?? '—',
                  at: fmtActivityTime(p.merged_at ?? undefined),
                  c: (p.merge_commit ?? '').slice(0, 8),
                }) }}
              </div>
              <div class="cp-diff-box">
                <div class="cp-diff-head">
                  <span class="fh-muted fh-small fh-mono">git diff {{ p.base }}...{{ p.branch }}</span>
                  <NxButton
                    v-if="diffCache[p.pid] && diffCache[p.pid].text.length > DIFF_PREVIEW_CHARS && !diffFull[p.pid]"
                    size="sm"
                    variant="ghost"
                    @click="diffFull[p.pid] = true"
                  >{{ t('collab.diffExpand') }}</NxButton>
                </div>
                <pre v-if="diffBusy === p.pid" class="fh-pre fh-muted">{{ t('film.loading') }}</pre>
                <pre v-else class="fh-pre cp-diff">{{ diffPreview(p.pid) || '—' }}</pre>
              </div>
              <div class="cp-actions">
                <NxButton
                  v-if="p.state === 'open'"
                  variant="primary"
                  size="sm"
                  @click="askMerge(p)"
                >🔀 {{ t('collab.btnMerge') }}</NxButton>
                <NxButton
                  v-if="p.state === 'open'"
                  variant="destructive"
                  size="sm"
                  :disabled="stateBusy !== null"
                  @click="closePr(p)"
                >{{ t('collab.btnPrClose') }}</NxButton>
              </div>
              <div v-if="(p.comments ?? []).length" class="cp-comments">
                <div v-for="(c, idx) in p.comments ?? []" :key="idx" class="cp-comment">
                  <span class="cp-comment-head">
                    <b>{{ c.author }}</b>
                    <span class="fh-muted fh-small">{{ fmtActivityTime(c.created_at) }}</span>
                  </span>
                  <div class="cp-comment-body">{{ c.body }}</div>
                </div>
              </div>
              <div class="cp-comment-input">
                <input
                  v-model="commentDraft[`pr-${p.pid}`]"
                  class="fh-input"
                  type="text"
                  :placeholder="t('collab.commentPh')"
                  @keydown.enter="sendComment('pr', p.pid)"
                >
                <NxButton
                  size="sm"
                  :disabled="!(commentDraft[`pr-${p.pid}`] ?? '').trim()"
                  @click="sendComment('pr', p.pid)"
                >{{ t('collab.commentSend') }}</NxButton>
              </div>
            </div>
          </div>
        </div>
      </section>
    </div>

    <!-- 新建 Issue 弹窗 -->
    <div v-if="issueFormOpen" class="fh-modal-backdrop" @click.self="issueFormOpen = false">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-collab-issue-title">
        <div class="fh-modal-head">
          <h3 id="film-collab-issue-title">🐛 {{ t('collab.newIssue') }}</h3>
          <button class="fh-modal-close" type="button" @click="issueFormOpen = false">×</button>
        </div>
        <div class="fh-modal-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fTitle') }}</span>
            <input v-model="issueForm.title" type="text" class="fh-input" :placeholder="t('collab.titlePh')">
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fBody') }}</span>
            <textarea v-model="issueForm.body" rows="4" class="fh-input" :placeholder="t('collab.bodyPh')" />
          </label>
          <div class="fh-field-row">
            <label class="fh-field" style="flex: 1 1 160px">
              <span class="fh-field-label">{{ t('collab.fLabels') }}</span>
              <input v-model="issueForm.labels" type="text" class="fh-input" :placeholder="t('collab.labelsPh')">
            </label>
            <label class="fh-field" style="flex: 1 1 140px">
              <span class="fh-field-label">{{ t('collab.fAssignee') }}</span>
              <input v-model="issueForm.assignee" type="text" class="fh-input" :placeholder="t('collab.assigneePh')">
            </label>
          </div>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fStage') }}</span>
            <input v-model="issueForm.stage" type="text" class="fh-input" :placeholder="t('collab.stagePh')">
            <span class="fh-muted fh-small">{{ t('collab.stageHint') }}</span>
          </label>
          <div v-if="createError" class="fh-error-box">{{ createError }}</div>
        </div>
        <div class="fh-form-actions">
          <button class="fh-btn" type="button" :disabled="createBusy" @click="issueFormOpen = false">
            {{ t('film.cancel') }}
          </button>
          <button class="fh-btn fh-btn-primary" type="button" :disabled="createBusy" @click="submitIssue()">
            {{ createBusy ? '…' : t('collab.create') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 新建 PR 弹窗（分支下拉 = git branches） -->
    <div v-if="prFormOpen" class="fh-modal-backdrop" @click.self="prFormOpen = false">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-collab-pr-title">
        <div class="fh-modal-head">
          <h3 id="film-collab-pr-title">🔀 {{ t('collab.newPr') }}</h3>
          <button class="fh-modal-close" type="button" @click="prFormOpen = false">×</button>
        </div>
        <div class="fh-modal-body">
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fTitle') }}</span>
            <input v-model="prForm.title" type="text" class="fh-input" :placeholder="t('collab.prTitlePh')">
          </label>
          <div class="fh-field-row">
            <label class="fh-field" style="flex: 1 1 180px">
              <span class="fh-field-label">{{ t('collab.fBranch') }}</span>
              <select v-model="prForm.branch" class="fh-select">
                <option v-if="!branchOptions.length" value="" disabled>
                  {{ t('collab.noBranches') }}
                </option>
                <option v-for="b in branchOptions" :key="b.name" :value="b.name">
                  {{ b.name }}{{ b.name === currentBranch ? ' ⬅' : '' }}
                </option>
              </select>
            </label>
            <label class="fh-field" style="flex: 0 1 150px">
              <span class="fh-field-label">{{ t('collab.fBase') }}</span>
              <input v-model="prForm.base" type="text" class="fh-input fh-mono" placeholder="main">
            </label>
          </div>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fRefs') }}</span>
            <input v-model="prForm.refs" type="text" class="fh-input" :placeholder="t('collab.refsPh')">
            <span class="fh-muted fh-small">{{ t('collab.refsHint') }}</span>
          </label>
          <label class="fh-field">
            <span class="fh-field-label">{{ t('collab.fBody') }}</span>
            <textarea v-model="prForm.body" rows="3" class="fh-input" :placeholder="t('collab.bodyPh')" />
          </label>
          <div v-if="createError" class="fh-error-box">{{ createError }}</div>
        </div>
        <div class="fh-form-actions">
          <button class="fh-btn" type="button" :disabled="createBusy" @click="prFormOpen = false">
            {{ t('film.cancel') }}
          </button>
          <button class="fh-btn fh-btn-primary" type="button" :disabled="createBusy" @click="submitPr()">
            {{ createBusy ? '…' : t('collab.create') }}
          </button>
        </div>
      </div>
    </div>

    <!-- merge 确认弹窗（nx 体系；确认 → merge --no-ff + 关联 issue 自动关） -->
    <div v-if="merging" class="fh-modal-backdrop" @click.self="merging = null">
      <div class="fh-modal" role="dialog" aria-modal="true" aria-labelledby="film-collab-merge-title">
        <div class="fh-modal-head">
          <h3 id="film-collab-merge-title">🔀 {{ t('collab.mergeTitle', { n: merging.pid }) }}</h3>
          <button class="fh-modal-close" type="button" @click="merging = null">×</button>
        </div>
        <div class="fh-modal-body">
          <div class="cp-merge-line">
            <span class="fh-pill fh-pill-blue fh-pill-mini fh-mono">{{ merging.branch }}</span>
            →
            <span class="fh-pill fh-pill-ok fh-pill-mini fh-mono">{{ merging.base }}</span>
          </div>
          <div class="fh-muted">{{ merging.title }}</div>
          <div v-if="refsLabel(merging)" class="fh-muted fh-small">
            {{ t('collab.mergeRefsHint') }}{{ refsLabel(merging) }}
          </div>
          <div class="fh-error-box cp-warn">{{ t('collab.mergeConfirmText') }}</div>
        </div>
        <div class="fh-form-actions">
          <button class="fh-btn" type="button" :disabled="mergeBusy" @click="merging = null">
            {{ t('film.cancel') }}
          </button>
          <button class="fh-btn fh-btn-primary" type="button" :disabled="mergeBusy" @click="doMerge()">
            {{ mergeBusy ? '…' : t('collab.btnMerge') }}
          </button>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* v0.1.10：视觉刻面全走 theme.css（.nx-page .fh-card / .fh-tab / .fh-pill /
   .fh-pre / .fh-modal 系列）；此处仅本页布局。 */
.cp-tabs { padding: 0 2px; }
.cp-item { display: flex; flex-direction: column; gap: 6px; }
.cp-row { cursor: pointer; }
.cp-iid { flex-shrink: 0; font-size: var(--nx-font-size-xs, 12px); }
.cp-title {
  flex: 1 1 200px;
  min-width: 120px;
  font-weight: 500;
}
.cp-meta { flex-shrink: 0; margin-left: auto; }
.cp-caret { flex-shrink: 0; color: var(--nx-text-tertiary, #888); }
.cp-detail {
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 10px 14px 12px;
  border-top: 1px dashed var(--nx-border-color, rgba(128, 128, 128, 0.25));
}
.cp-body { margin: 0; white-space: pre-wrap; }
.cp-actions { display: flex; gap: 8px; flex-wrap: wrap; }
.cp-comments {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-height: 260px;
  overflow-y: auto;
}
.cp-comment {
  padding: 8px 10px;
  border-left: 3px solid var(--nx-accent, #4c8dff);
  background: var(--nx-surface-2, rgba(128, 128, 128, 0.08));
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.cp-comment-head { display: flex; gap: 8px; align-items: baseline; }
.cp-comment-body { white-space: pre-wrap; word-break: break-word; }
.cp-comment-input { display: flex; gap: 8px; align-items: center; }
.cp-comment-input .fh-input { flex: 1 1 auto; }
.cp-diff-box { display: flex; flex-direction: column; gap: 6px; }
.cp-diff-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.cp-diff {
  margin: 0;
  max-height: 340px;
  overflow: auto;
  white-space: pre;
}
.cp-merge-line { display: flex; align-items: center; gap: 10px; }
.cp-warn { font-size: var(--nx-font-size-xs, 12px); }
.collab-notice { color: var(--nx-success, #2e9e5b); }
@media (max-width: 720px) {
  .cp-title { flex-basis: 100%; }
  .cp-meta { margin-left: 0; flex-basis: 100%; }
}
</style>
