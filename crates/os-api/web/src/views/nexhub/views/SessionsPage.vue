<script setup lang="ts">
// =============================================================================
// SessionsPage —— AI 会话（v0.1.32，原 Tab7 整体迁移）。
// 时间线 + 归档弹窗；列表数据本页自持（原 refreshAll 的 loadSessions 语义）。
// =============================================================================
import { onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { agentColor, errMsg, formatDate, type AgentSession } from '@/views/nexhub/model';

const { t } = useI18n();
const ctx = useNexhub();

const sessions = ref<AgentSession[]>([]);
const showCreateSession = ref(false);
const newSession = ref({
  agent_name: 'zcode',
  repo_name: '',
  summary: '',
  files_changed: 0,
  commits: 0,
});
const agentOptions = [
  { value: 'zcode', label: 'ZCode' },
  { value: 'claude-code', label: 'Claude Code' },
  { value: 'codex', label: 'Codex' },
  { value: 'cursor', label: 'Cursor' },
  { value: 'aider', label: 'Aider' },
  { value: 'other', labelKey: 'nexhub.sessions.agentOther' },
];

async function loadSessions(): Promise<void> {
  try {
    const r = (await endpoints.codeRepoSessions()) as AgentSession[];
    sessions.value = Array.isArray(r) ? r : [];
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.sessions.loadFailed')}: ${errMsg(e)}`);
    sessions.value = [];
  }
}

async function createSession(): Promise<void> {
  if (!newSession.value.agent_name.trim()) {
    ctx.showMsg('error', t('nexhub.sessions.agentRequired'));
    return;
  }
  if (!newSession.value.repo_name.trim()) {
    ctx.showMsg('error', t('nexhub.sessions.repoRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    await endpoints.createCodeRepoSession({
      agent_name: newSession.value.agent_name.trim(),
      repo_name: newSession.value.repo_name.trim(),
      summary: newSession.value.summary.trim() || undefined,
      files_changed: Number(newSession.value.files_changed) || 0,
      commits: Number(newSession.value.commits) || 0,
    });
    ctx.showMsg('ok', t('nexhub.sessions.created'));
    showCreateSession.value = false;
    newSession.value = { agent_name: 'zcode', repo_name: '', summary: '', files_changed: 0, commits: 0 };
    await Promise.all([loadSessions(), ctx.loadStats()]);
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.sessions.createFailed')}: ${errMsg(e)}`);
  } finally {
    ctx.actionLoading.value = false;
  }
}

async function endSession(id: string): Promise<void> {
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    await endpoints.endCodeRepoSession(id);
    ctx.showMsg('ok', t('nexhub.sessions.ended', { id }));
    await loadSessions();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.sessions.endFailed')}: ${errMsg(e)}`);
  } finally {
    ctx.actionLoading.value = false;
  }
}

onMounted(() => {
  void loadSessions();
});
</script>

<template>
  <section class="sessions-page">
    <div class="repo-toolbar">
      <span class="toolbar-info muted small">
        {{ t('nexhub.sessions.countInfo', { count: sessions.length, commits: ctx.stats.value.total_commits ?? 0 }) }}
      </span>
      <button class="btn btn-small btn-primary" type="button" @click="showCreateSession = true">
        + {{ t('nexhub.sessions.create') }}
      </button>
    </div>

    <div v-if="sessions.length === 0" class="card empty-card">
      {{ t('nexhub.sessions.empty') }}
    </div>
    <div v-else class="timeline">
      <div v-for="s in sessions" :key="s.id" class="timeline-item">
        <div class="timeline-dot" :style="{ background: agentColor(s.agent_name) }"></div>
        <div class="card timeline-card">
          <div class="timeline-head">
            <span class="agent-badge" :style="{ background: agentColor(s.agent_name) }">
              {{ s.agent_name }}
            </span>
            <span class="repo-link muted small">@{{ s.repo_name }}</span>
            <span v-if="s.ended_at" class="status-chip status-done">{{ t('nexhub.sessions.endedChip') }}</span>
            <span v-else class="status-chip status-active">{{ t('nexhub.sessions.activeChip') }}</span>
          </div>
          <p class="timeline-summary">{{ s.session_summary || t('nexhub.sessions.noSummary') }}</p>
          <div class="timeline-meta">
            <span class="meta-chip">{{ t('nexhub.sessions.filesChanged', { n: s.files_changed ?? 0 }) }}</span>
            <span class="meta-chip">{{ t('nexhub.sessions.commitsN', { n: s.commits ?? 0 }) }}</span>
            <span class="meta-chip">{{ t('nexhub.sessions.startedAt', { date: formatDate(s.started_at) }) }}</span>
            <span v-if="s.ended_at" class="meta-chip">{{ t('nexhub.sessions.endedAt', { date: formatDate(s.ended_at) }) }}</span>
          </div>
          <div v-if="!s.ended_at" class="timeline-actions">
            <button class="btn btn-small" type="button" @click="endSession(s.id ?? '')">{{ t('nexhub.sessions.end') }}</button>
          </div>
        </div>
      </div>
    </div>

    <!-- 创建会话对话框 -->
    <div v-if="showCreateSession" class="modal-overlay" @click.self="showCreateSession = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.sessions.archiveTitle') }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showCreateSession = false">✕</button>
        </div>
        <div class="form-row">
          <label class="form-label" for="ss-agent">{{ t('nexhub.sessions.agentLabel') }} *</label>
          <select id="ss-agent" v-model="newSession.agent_name" class="search-input">
            <option v-for="a in agentOptions" :key="a.value" :value="a.value">
              {{ a.labelKey ? t(a.labelKey) : a.label }}
            </option>
          </select>
        </div>
        <div class="form-row">
          <label class="form-label" for="ss-repo">{{ t('nexhub.sessions.repoLabel') }} *</label>
          <input
            id="ss-repo"
            v-model="newSession.repo_name"
            class="search-input"
            list="nexhub-repo-list"
            placeholder="my-project"
          />
          <datalist id="nexhub-repo-list">
            <option v-for="n in ctx.repos.value.map((r) => r.name)" :key="n" :value="n" />
          </datalist>
        </div>
        <div class="form-row">
          <label class="form-label" for="ss-summary">{{ t('nexhub.sessions.summaryLabel') }}</label>
          <textarea
            id="ss-summary"
            v-model="newSession.summary"
            class="search-input form-textarea"
            rows="3"
            :placeholder="t('nexhub.sessions.summaryPlaceholder')"
          ></textarea>
        </div>
        <div class="form-row form-row-inline">
          <div class="form-num">
            <label class="form-label" for="ss-files">{{ t('nexhub.sessions.filesLabel') }}</label>
            <input id="ss-files" v-model.number="newSession.files_changed" type="number" min="0"
              class="search-input form-num-input" />
          </div>
          <div class="form-num">
            <label class="form-label" for="ss-commits">{{ t('nexhub.sessions.commitsLabel') }}</label>
            <input id="ss-commits" v-model.number="newSession.commits" type="number" min="0"
              class="search-input form-num-input" />
          </div>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showCreateSession = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="createSession">
            {{ t('nexhub.sessions.archive') }}
          </button>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.sessions-page { display: flex; flex-direction: column; gap: 14px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.repo-toolbar { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.toolbar-info { font-size: 13px; }
.timeline { display: flex; flex-direction: column; gap: 14px; position: relative; padding-left: 8px; }
.timeline-item { display: flex; gap: 14px; position: relative; }
.timeline-dot {
  width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0; margin-top: 18px;
  border: 3px solid var(--bg-card, #fff); box-shadow: 0 0 0 2px var(--border-soft, #EDEDED);
  z-index: 1;
}
.timeline-item:not(:last-child)::before {
  content: ''; position: absolute; left: 5px; top: 30px; bottom: -14px; width: 2px;
  background: var(--border-soft, #EDEDED);
}
.timeline-card { flex: 1; padding: 14px 16px; display: flex; flex-direction: column; gap: 8px; }
.timeline-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.agent-badge {
  padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; color: #fff;
}
.repo-link { font-family: monospace; }
.status-chip { padding: 1px 8px; border-radius: var(--radius-pill, 20px); font-size: 11px; font-weight: 600; }
.status-active { background: #dcfce7; color: #166534; }
.status-done { background: var(--border-soft, #F3F4F6); color: var(--text-muted, #5E5C5F); }
.timeline-summary { margin: 0; font-size: 14px; line-height: 1.5; color: var(--text, #2B2B2B); }
.timeline-meta { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.meta-chip {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; color: var(--text-muted, #5E5C5F); background: var(--border-soft, #F3F4F6);
}
.timeline-actions { display: flex; gap: 6px; }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-primary { background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.modal-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); display: flex;
  align-items: center; justify-content: center; z-index: 1000; padding: 20px;
}
.modal-card { width: 100%; max-width: 460px; padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.modal-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.modal-title { font-size: 16px; font-weight: 700; color: var(--text, #2B2B2B); margin: 0; }
.form-row { display: flex; flex-direction: column; gap: 6px; }
.form-row-inline { flex-direction: row; gap: 16px; align-items: flex-end; flex-wrap: wrap; }
.form-label { font-size: 12px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.form-textarea { resize: vertical; font-family: inherit; }
.form-num { display: flex; flex-direction: column; gap: 6px; }
.form-num-input { width: 100px; }
.modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 4px; }
</style>
