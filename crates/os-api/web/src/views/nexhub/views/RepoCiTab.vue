<script setup lang="ts">
// =============================================================================
// RepoCiTab —— 仓库详情页「CI」Tab（v0.1.33，内置 CI 前端）。
//
// runs 列表（最新 20：状态 / 触发 / 流水线 / 耗时 / 时间 / 删记录）+ 点开看
// 日志（<pre> 自动滚底 + 手动刷新 + 运行中 1.5s 轮询）+ 顶部「运行 CI」
// （admin 动作，后端强制；站内确认弹窗）。数据源端点见 client.ts
// codeRepoCiRuns / codeRepoCiRun / codeRepoCiTrigger / codeRepoCiDeleteRun。
// =============================================================================

import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints, type CiRun } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatDate, formatRelative } from '@/views/nexhub/model';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';

const props = defineProps<{
  /** 仓库名。 */
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

// —— runs 列表 ——
const runs = ref<CiRun[]>([]);
const loading = ref(false);

async function loadRuns(): Promise<void> {
  loading.value = true;
  try {
    const r = await endpoints.codeRepoCiRuns(props.repoName);
    runs.value = Array.isArray(r?.runs) ? r.runs : [];
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.ci.loadFailed')}: ${errMsg(e)}`);
    runs.value = [];
  } finally {
    loading.value = false;
  }
}

// 仓库切换 / 首次显示 → 拉列表
watch(
  () => props.repoName,
  () => {
    closeLog();
    void loadRuns();
  },
  { immediate: true },
);

// —— 日志详情（点开一条 run）——
const openRunId = ref('');
const openRun = ref<CiRun | null>(null);
const logText = ref('');
const logLoading = ref(false);
const logBox = ref<HTMLPreElement | null>(null);
/** 运行中轮询句柄（切仓 / 关日志清理）。 */
let pollTimer: ReturnType<typeof setTimeout> | undefined;

/** 日志是否自动滚底（用户上滚查看历史时不打扰）。 */
const autoScroll = ref(true);

function onLogScroll(): void {
  const el = logBox.value;
  if (!el) return;
  autoScroll.value = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
}

function scrollToBottom(): void {
  if (!autoScroll.value) return;
  void nextTick(() => {
    const el = logBox.value;
    if (el) el.scrollTop = el.scrollHeight;
  });
}

/** 运行中（或排队）→ 1.5s 轮询详情直到终态。 */
function schedulePoll(): void {
  stopPoll();
  const status = openRun.value?.status;
  if (status !== 'running' && status !== 'queued') return;
  pollTimer = setTimeout(async () => {
    await openLog(openRunId.value, true);
    schedulePoll();
  }, 1500);
}

function stopPoll(): void {
  if (pollTimer) {
    clearTimeout(pollTimer);
    pollTimer = undefined;
  }
}

async function openLog(runId: string, silent = false): Promise<void> {
  openRunId.value = runId;
  if (!silent) {
    logLoading.value = true;
    logText.value = '';
    openRun.value = null;
  }
  try {
    const r = await endpoints.codeRepoCiRun(props.repoName, runId);
    openRun.value = r.run ?? null;
    logText.value = r.run?.log ?? '';
    // 详情回到列表同步状态（轮询后列表徽章不滞后）
    const idx = runs.value.findIndex((x) => x.id === runId);
    if (idx >= 0 && r.run) runs.value[idx] = { ...runs.value[idx], ...r.run, log: undefined };
    scrollToBottom();
  } catch (e) {
    if (!silent) ctx.showMsg('error', `${t('nexhub.ci.logLoadFailed')}: ${errMsg(e)}`);
  } finally {
    logLoading.value = false;
  }
  // 运行中 / 排队 → 继续轮询至终态
  schedulePoll();
}

function closeLog(): void {
  stopPoll();
  openRunId.value = '';
  openRun.value = null;
  logText.value = '';
}

onBeforeUnmount(stopPoll);

// —— 运行 CI（admin 动作；后端强制，前端确认弹窗防手滑）——
const showTrigger = ref(false);
const triggering = ref(false);

async function doTrigger(): Promise<void> {
  triggering.value = true;
  ctx.clearMsg();
  try {
    const r = await endpoints.codeRepoCiTrigger(props.repoName);
    ctx.showMsg('ok', t('nexhub.ci.triggerQueued', { id: r.run?.id ?? '' }));
    showTrigger.value = false;
    await loadRuns();
    if (r.run?.id) await openLog(r.run.id);
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.ci.triggerFailed')}: ${errMsg(e)}`);
  } finally {
    triggering.value = false;
  }
}

// —— 删除记录（admin；终态可删，进行中 409 后端拒绝）——
const deleteRunId = ref('');
const showDeleteRun = ref(false);

function askDelete(run: CiRun): void {
  deleteRunId.value = run.id;
  showDeleteRun.value = true;
}

async function doDeleteRun(): Promise<void> {
  try {
    await endpoints.codeRepoCiDeleteRun(props.repoName, deleteRunId.value);
    ctx.showMsg('ok', t('nexhub.ci.runDeleted'));
    if (openRunId.value === deleteRunId.value) closeLog();
    await loadRuns();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.ci.deleteFailed')}: ${errMsg(e)}`);
  }
}

// —— 展示辅助 ——

/** 状态 → 徽章 class。 */
function statusClass(s?: string): string {
  return `st-${s || 'none'}`;
}

function statusLabel(s?: string): string {
  switch (s) {
    case 'queued':
      return t('nexhub.ci.statusQueued');
    case 'running':
      return t('nexhub.ci.statusRunning');
    case 'passed':
      return t('nexhub.ci.statusPassed');
    case 'failed':
      return t('nexhub.ci.statusFailed');
    case 'skipped':
      return t('nexhub.ci.statusSkipped');
    default:
      return s || '—';
  }
}

function triggerLabel(s?: string): string {
  return s === 'push' ? t('nexhub.ci.triggerPush') : t('nexhub.ci.triggerManual');
}

/** 耗时：运行中显示 live 计时基线（无 finished 取至今）。 */
function durationText(run: CiRun): string {
  if (run.status === 'running' && run.started_at) {
    const ms = Date.now() - new Date(run.started_at).getTime();
    return ms > 0 ? `${Math.round(ms / 1000)}s…` : '…';
  }
  const ms = run.duration_ms;
  if (!ms || ms < 0) return '—';
  const s = ms / 1000;
  if (s < 60) return `${s < 10 ? s.toFixed(1) : Math.round(s)}s`;
  const m = Math.floor(s / 60);
  return `${m}m${String(Math.round(s % 60)).padStart(2, '0')}s`;
}

/** 列表时间（相对；悬浮原值）。 */
function timeText(run: CiRun): string {
  return run.finished_at || run.started_at
    ? formatRelative(run.finished_at ?? run.started_at)
    : formatDate(run.created_at);
}

/** 悬浮完整时间（避 null 进 title）。 */
function timeTitle(run: CiRun): string | undefined {
  return (run.finished_at ?? run.started_at ?? run.created_at) ?? undefined;
}

/** 是否有进行中的 run（列表头「自动刷新」提示态）。 */
const hasActive = computed(() => runs.value.some((r) => r.status === 'running' || r.status === 'queued'));
</script>

<template>
  <section class="ci-tab">
    <!-- 工具条：运行 CI + 刷新 + 运行中提示 -->
    <div class="ci-toolbar">
      <button class="btn btn-small btn-primary" type="button" :disabled="triggering" @click="showTrigger = true">
        {{ triggering ? t('nexhub.ci.triggering') : t('nexhub.ci.runNow') }}
      </button>
      <button class="btn btn-small btn-ghost" type="button" :disabled="loading" @click="loadRuns">
        {{ t('nexhub.common.refresh') }}
      </button>
      <span v-if="hasActive" class="active-hint muted small">{{ t('nexhub.ci.activeHint') }}</span>
      <span class="spacer" />
      <span class="muted small">{{ t('nexhub.ci.listHint', { n: runs.length }) }}</span>
    </div>

    <!-- 空态 -->
    <div v-if="!loading && runs.length === 0" class="card empty-card">
      {{ t('nexhub.ci.noRuns') }}
    </div>

    <!-- runs 列表 -->
    <div v-else class="card runs-card">
      <table class="runs-table">
        <thead>
          <tr>
            <th>{{ t('nexhub.ci.colRun') }}</th>
            <th>{{ t('nexhub.ci.colStatus') }}</th>
            <th>{{ t('nexhub.ci.colTrigger') }}</th>
            <th class="col-pipeline">{{ t('nexhub.ci.colPipeline') }}</th>
            <th>{{ t('nexhub.ci.colDuration') }}</th>
            <th>{{ t('nexhub.ci.colTime') }}</th>
            <th class="col-actions"></th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="r in runs"
            :key="r.id"
            :class="{ selected: r.id === openRunId }"
            @click="openLog(r.id)"
          >
            <td><code class="run-id">{{ r.id }}</code></td>
            <td>
              <span class="status-pill" :class="statusClass(r.status)">
                <span v-if="r.status === 'running'" class="spin" aria-hidden="true">↻</span>
                {{ statusLabel(r.status) }}
              </span>
            </td>
            <td class="muted small">{{ triggerLabel(r.trigger) }}</td>
            <td class="col-pipeline"><code class="pipeline" :title="r.pipeline ?? undefined">{{ r.pipeline || '—' }}</code></td>
            <td class="small">{{ durationText(r) }}</td>
            <td class="small muted" :title="timeTitle(r)">{{ timeText(r) }}</td>
            <td class="col-actions">
              <button
                v-if="r.status !== 'running' && r.status !== 'queued'"
                class="btn btn-tiny btn-ghost"
                type="button"
                :title="t('nexhub.ci.deleteRun')"
                @click.stop="askDelete(r)"
              >✕</button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <!-- 日志详情（点开一条 run）-->
    <div v-if="openRunId" class="card log-card">
      <div class="log-head">
        <span class="status-pill" :class="statusClass(openRun?.status)">
          <span v-if="openRun?.status === 'running'" class="spin" aria-hidden="true">↻</span>
          {{ statusLabel(openRun?.status) }}
        </span>
        <code class="run-id">{{ openRunId }}</code>
        <code v-if="openRun?.pipeline" class="pipeline">{{ openRun.pipeline }}</code>
        <span class="spacer" />
        <label class="muted small autoscroll-label">
          <input v-model="autoScroll" type="checkbox" />
          {{ t('nexhub.ci.autoScroll') }}
        </label>
        <button class="btn btn-tiny btn-ghost" type="button" @click="closeLog">✕</button>
      </div>
      <pre ref="logBox" class="log-box" @scroll="onLogScroll">{{ logLoading ? t('common.loading') : logText || t('nexhub.ci.emptyLog') }}</pre>
    </div>

    <!-- 运行 CI 确认（站内弹窗）-->
    <NexhubConfirm
      v-model:open="showTrigger"
      :title="t('nexhub.ci.runConfirmTitle', { name: repoName })"
      :body="t('nexhub.ci.runConfirmBody')"
      :confirm-text="t('nexhub.ci.runNow')"
      @confirm="doTrigger"
    />

    <!-- 删除记录确认 -->
    <NexhubConfirm
      v-model:open="showDeleteRun"
      :title="t('nexhub.ci.deleteConfirmTitle')"
      :body="t('nexhub.ci.deleteConfirmBody', { id: deleteRunId })"
      :danger="true"
      :confirm-text="t('nexhub.ci.deleteRun')"
      @confirm="doDeleteRun"
    />
  </section>
</template>

<style scoped>
.ci-tab { display: flex; flex-direction: column; gap: 12px; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.spacer { flex: 1; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
}

.ci-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.active-hint { display: inline-flex; align-items: center; gap: 4px; }

.empty-card { padding: 24px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 13.5px; }

.runs-card { overflow-x: auto; }
.runs-table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
.runs-table th {
  text-align: left; padding: 9px 12px; font-size: 11.5px; font-weight: 600;
  color: var(--text-muted, #5E5C5F); border-bottom: 1px solid var(--border-soft, #EDEDED);
  white-space: nowrap;
}
.runs-table td { padding: 8px 12px; border-bottom: 1px solid var(--border-soft, #F3F4F6); }
.runs-table tbody tr { cursor: pointer; transition: background 0.12s ease; }
.runs-table tbody tr:hover { background: var(--border-soft, #F9FAFB); }
.runs-table tbody tr.selected { background: rgba(233, 84, 32, 0.06); }
.runs-table tbody tr:last-child td { border-bottom: none; }
.col-pipeline { max-width: 280px; }
.col-actions { width: 40px; text-align: right; }

.run-id {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11px;
  color: var(--text-muted, #5E5C5F); word-break: break-all;
}
.pipeline {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11px;
  color: var(--text, #2B2B2B); overflow: hidden; text-overflow: ellipsis;
  white-space: nowrap; display: inline-block; max-width: 100%;
}

.status-pill {
  display: inline-flex; align-items: center; gap: 4px;
  padding: 1px 9px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; font-weight: 600; white-space: nowrap;
}
.st-passed { background: #dcfce7; color: #166534; }
.st-failed { background: #fee2e2; color: #b91c1c; }
.st-running, .st-queued { background: #fef9c3; color: #854d0e; }
.st-skipped { background: var(--border-soft, #F3F4F6); color: var(--text-muted, #5E5C5F); }
.spin { display: inline-block; animation: ci-spin 1s linear infinite; }
@keyframes ci-spin { to { transform: rotate(360deg); } }

.log-card { padding: 12px 14px; display: flex; flex-direction: column; gap: 8px; }
.log-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.autoscroll-label { display: inline-flex; align-items: center; gap: 4px; cursor: pointer; }
.log-box {
  margin: 0; padding: 12px; background: var(--bg-code, #1e1e1e); color: #d4d4d4;
  border-radius: var(--radius-sm, 8px); font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 11.5px; line-height: 1.55; max-height: 420px; overflow: auto;
  white-space: pre-wrap; word-break: break-word;
}

.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-tiny { padding: 2px 7px; font-size: 11px; }
.btn-primary { background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff; }
.btn-primary:hover:not(:disabled) { background: #d44a1c; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.btn-ghost:hover { background: rgba(233, 84, 32, 0.08); }
</style>
