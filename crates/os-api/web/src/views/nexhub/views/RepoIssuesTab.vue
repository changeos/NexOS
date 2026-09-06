<script setup lang="ts">
// =============================================================================
// RepoIssuesTab —— 仓库详情 Issues Tab（v0.1.32，原 Tab5 归组进仓库域）。
// 项目级协作：读公开；写需身份（链上 nexhub token / admin 回落）；关闭/重开
// 仅作者本人或 admin。仓库由详情页传入（取消原全局下拉选仓模式）。
// =============================================================================

import { ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import type { RepoComment, RepoIssue } from '@/api/client';
import { identiconSvg } from '@/composables/useIdenticon';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatRelative } from '@/views/nexhub/model';
import { authorLabel, collabWriteErr, useCollabIdentity } from '@/views/nexhub/collab';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

// 身份 / 权限（owner 判定依据大厅条目 publisher——壳共享大厅数据）。
const idctx = useCollabIdentity(() => ctx.lobbyEntries.value);

type IssueState = 'open' | 'closed' | 'all';
const issuesState = ref<IssueState>('open');
const issues = ref<RepoIssue[]>([]);
const loading = ref(false);
/** 展开的 Issue 编号（行内展开详情：正文 + 评论流 + 发表框 + 关闭/重开）。 */
const expandedIssue = ref<number | null>(null);
const issueDetails = ref<Record<number, { issue: RepoIssue; comments: RepoComment[] }>>({});
const issueCommentDrafts = ref<Record<number, string>>({});
const showCreateIssue = ref(false);
const issueForm = ref({ title: '', body: '', labels: '' });

async function loadIssues(): Promise<void> {
  const repo = props.repoName.trim();
  if (!repo) return;
  loading.value = true;
  try {
    const r = await endpoints.codeRepoIssues(repo, issuesState.value);
    issues.value = r.issues ?? [];
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.issues.loadFailed')}: ${errMsg(e)}`);
    issues.value = [];
  } finally {
    loading.value = false;
  }
}

/** 展开/收起 Issue 行；展开时懒加载详情（含评论流）。 */
async function toggleIssue(i: RepoIssue): Promise<void> {
  const num = i.number;
  if (expandedIssue.value === num) {
    expandedIssue.value = null;
    return;
  }
  expandedIssue.value = num;
  if (!issueDetails.value[num]) {
    try {
      const r = await endpoints.codeRepoIssueDetail(props.repoName.trim(), num);
      issueDetails.value[num] = { issue: r.issue, comments: r.comments ?? [] };
    } catch (e) {
      ctx.showMsg('error', `${t('nexhub.issues.detailLoadFailed')}: ${errMsg(e)}`);
    }
  }
}

/** 新建 Issue（需身份：链上 token 自动三步获取；无身份时走全局 admin 回落）。 */
async function createIssue(): Promise<void> {
  const repo = props.repoName.trim();
  if (!issueForm.value.title.trim()) {
    ctx.showMsg('error', t('nexhub.issues.titleRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.createCodeRepoIssue(
      repo,
      {
        title: issueForm.value.title.trim(),
        body: issueForm.value.body.trim() || undefined,
        labels: issueForm.value.labels
          .split(/[,，]/)
          .map((s) => s.trim())
          .filter(Boolean),
      },
      opts,
    );
    ctx.showMsg('ok', t('nexhub.issues.created', { author: idctx.evmAddress.value || 'admin' }));
    showCreateIssue.value = false;
    issueForm.value = { title: '', body: '', labels: '' };
    issuesState.value = 'open';
    await loadIssues();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.issues.createAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

/** 发表 Issue 评论。 */
async function submitIssueComment(num: number): Promise<void> {
  const repo = props.repoName.trim();
  const text = (issueCommentDrafts.value[num] ?? '').trim();
  if (!repo || !text) return;
  try {
    const opts = await idctx.requireNexhubOpts();
    const r = await endpoints.codeRepoIssueComment(repo, num, { body: text }, opts);
    const d = issueDetails.value[num];
    if (d) {
      d.comments.push(r.comment);
      d.issue = { ...d.issue, comment_count: (d.issue.comment_count ?? 0) + 1 };
    }
    issueCommentDrafts.value[num] = '';
    ctx.showMsg('ok', t('nexhub.collab.commentPosted'));
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.collab.commentAction'), e));
  }
}

/** 关闭/重开 Issue（仅作者或 admin；按当前状态自动取反）。 */
async function toggleIssueState(i: RepoIssue): Promise<void> {
  const repo = props.repoName.trim();
  if (!repo) return;
  const closing = i.state === 'open';
  try {
    const opts = await idctx.requireNexhubOpts();
    if (closing) {
      await endpoints.codeRepoIssueClose(repo, i.number, opts);
    } else {
      await endpoints.codeRepoIssueOpen(repo, i.number, opts);
    }
    ctx.showMsg('ok', t(closing ? 'nexhub.issues.closed' : 'nexhub.issues.reopened', { n: i.number }));
    const d = issueDetails.value[i.number];
    if (d) d.issue = { ...d.issue, state: closing ? 'closed' : 'open' };
    await loadIssues();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(closing ? t('nexhub.issues.closeAction') : t('nexhub.issues.reopenAction'), e));
  }
}

// 仓库切换：重置并重载
watch(
  () => props.repoName,
  () => {
    expandedIssue.value = null;
    issueDetails.value = {};
    void loadIssues();
  },
  { immediate: true },
);
</script>

<template>
  <section class="issues-tab">
    <div class="browser-toolbar">
      <select v-model="issuesState" class="search-input state-select" @change="loadIssues">
        <option value="open">{{ t('nexhub.collab.stateOpen') }}</option>
        <option value="closed">{{ t('nexhub.collab.stateClosed') }}</option>
        <option value="all">{{ t('nexhub.collab.stateAll') }}</option>
      </select>
      <button class="btn btn-small" type="button" :disabled="loading" @click="loadIssues">
        <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
        {{ t('nexhub.common.refresh') }}
      </button>
      <button class="btn btn-small btn-primary" type="button" @click="showCreateIssue = true">
        + {{ t('nexhub.issues.create') }}
      </button>
    </div>

    <p class="muted small collab-hint">{{ t('nexhub.issues.hint') }}</p>

    <div v-if="loading" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="issues.length === 0" class="card empty-card">{{ t('nexhub.issues.empty') }}</div>

    <div v-else class="collab-list">
      <div
        v-for="i in issues"
        :key="i.number"
        class="card collab-row"
        :class="{ expanded: expandedIssue === i.number }"
      >
        <div class="collab-row-line" :title="t('nexhub.collab.toggleDetail')" @click="toggleIssue(i)">
          <span class="collab-state-badge" :class="`is-${i.state}`">
            {{ i.state === 'open' ? '● Open' : '✓ Closed' }}
          </span>
          <span class="collab-num">#{{ i.number }}</span>
          <span class="collab-title">{{ i.title }}</span>
          <span v-if="i.labels?.length" class="collab-labels">
            <span v-for="l in i.labels" :key="l" class="meta-chip">{{ l }}</span>
          </span>
          <span class="collab-meta muted small">
            💬 {{ i.comment_count }} · {{ authorLabel(i.author, i.author_display) }} ·
            {{ formatRelative(i.updated_at) }}
          </span>
        </div>

        <!-- 展开详情：正文 + 评论流 + 发表框 + 关闭/重开（按权限显示） -->
        <div v-if="expandedIssue === i.number" class="collab-detail" @click.stop>
          <div v-if="issueDetails[i.number]" class="collab-body">
            <p class="collab-desc">{{ issueDetails[i.number].issue.body || t('nexhub.collab.noBody') }}</p>
            <div
              v-for="c in issueDetails[i.number].comments"
              :key="c.number"
              class="collab-comment"
            >
              <img
                class="identicon collab-avatar"
                :src="identiconSvg(c.author === 'admin' ? 'admin' : c.author, 18)"
                alt=""
              />
              <div class="collab-comment-main">
                <div class="collab-comment-head muted small">
                  <strong>{{ authorLabel(c.author, c.author_display) }}</strong>
                  <span v-if="c.owner_kind === 'pubkey'" class="meta-chip" :title="t('nexhub.collab.chainIdentity')">⛓</span>
                  · {{ formatRelative(c.created_at) }}
                </div>
                <p class="collab-comment-body">{{ c.body }}</p>
              </div>
            </div>
          </div>
          <div v-else class="muted small">{{ t('nexhub.collab.detailLoading') }}</div>

          <div class="collab-compose">
            <textarea
              v-model="issueCommentDrafts[i.number]"
              class="search-input collab-input"
              rows="2"
              :placeholder="t('nexhub.issues.commentPlaceholder')"
            ></textarea>
            <div class="collab-actions">
              <button
                v-if="idctx.canToggleState(i.author)"
                class="btn btn-small"
                type="button"
                @click="toggleIssueState(i)"
              >{{ i.state === 'open' ? t('nexhub.issues.closeBtn') : t('nexhub.issues.reopenBtn') }}</button>
              <span v-else class="muted small" :title="t('nexhub.collab.togglePermTitle')">
                {{ i.state === 'open' ? t('nexhub.issues.closePermHint') : '' }}
              </span>
              <button
                class="btn btn-small btn-primary"
                type="button"
                :disabled="!(issueCommentDrafts[i.number] ?? '').trim()"
                @click="submitIssueComment(i.number)"
              >{{ t('nexhub.collab.commentSubmit') }}</button>
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- 新建 Issue 对话框 -->
    <div v-if="showCreateIssue" class="modal-overlay" @click.self="showCreateIssue = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.issues.createTitle', { repo: props.repoName }) }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showCreateIssue = false">✕</button>
        </div>
        <div class="form-row">
          <span class="form-hint muted small">
            {{ t('nexhub.collab.authorAttribution', { author: idctx.evmAddress.value || t('nexhub.collab.adminFallback') }) }}
          </span>
        </div>
        <div class="form-row">
          <label class="form-label" for="is-title">{{ t('nexhub.collab.titleLabel') }} *</label>
          <input
            id="is-title"
            v-model="issueForm.title"
            class="search-input"
            :placeholder="t('nexhub.issues.titlePlaceholder')"
          />
        </div>
        <div class="form-row">
          <label class="form-label" for="is-body">{{ t('nexhub.collab.bodyLabel') }}</label>
          <textarea
            id="is-body"
            v-model="issueForm.body"
            class="search-input collab-input"
            rows="4"
            :placeholder="t('nexhub.issues.bodyPlaceholder')"
          ></textarea>
        </div>
        <div class="form-row">
          <label class="form-label" for="is-labels">{{ t('nexhub.issues.labelsLabel') }}</label>
          <input
            id="is-labels"
            v-model="issueForm.labels"
            class="search-input"
            placeholder="bug, ui, documentation"
          />
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showCreateIssue = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="createIssue">
            {{ t('nexhub.issues.create') }}
          </button>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.issues-tab { display: flex; flex-direction: column; gap: 12px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.browser-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.state-select { min-width: 120px; }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.collab-hint { margin: 0; line-height: 1.6; }
.meta-chip {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; color: var(--text-muted, #5E5C5F); background: var(--border-soft, #F3F4F6);
}
.collab-list { display: flex; flex-direction: column; gap: 10px; }
.collab-row { padding: 10px 14px; }
.collab-row-line { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; cursor: pointer; }
.collab-state-badge {
  display: inline-block; padding: 1px 8px; border-radius: 999px;
  font-size: 11px; font-weight: 700; white-space: nowrap;
}
.collab-state-badge.is-open { background: rgba(46, 160, 67, 0.14); color: #1a7f37; }
.collab-state-badge.is-closed { background: rgba(175, 82, 222, 0.12); color: #8250df; }
.collab-num { font-size: 12px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.collab-title { font-weight: 600; }
.collab-labels { display: inline-flex; gap: 4px; flex-wrap: wrap; }
.collab-meta { margin-left: auto; }
.collab-detail { margin-top: 10px; border-top: 1px dashed rgba(0, 0, 0, 0.12); padding-top: 10px; }
.collab-body { display: flex; flex-direction: column; gap: 8px; }
.collab-desc { margin: 0; white-space: pre-wrap; }
.collab-comment { display: flex; gap: 8px; align-items: flex-start; }
.identicon { width: 16px; height: 16px; border-radius: 3px; }
.collab-avatar { border-radius: 4px; flex-shrink: 0; margin-top: 2px; }
.collab-comment-main { flex: 1; min-width: 0; }
.collab-comment-head { display: flex; align-items: center; gap: 6px; }
.collab-comment-body { margin: 2px 0 0; white-space: pre-wrap; }
.collab-compose { margin-top: 10px; display: flex; flex-direction: column; gap: 8px; }
.collab-input { resize: vertical; font-family: inherit; }
.collab-actions { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }

.modal-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); display: flex;
  align-items: center; justify-content: center; z-index: 1000; padding: 20px;
}
.modal-card { width: 100%; max-width: 460px; padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.modal-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.modal-title { font-size: 16px; font-weight: 700; color: var(--text, #2B2B2B); margin: 0; }
.form-row { display: flex; flex-direction: column; gap: 6px; }
.form-label { font-size: 12px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.form-hint { font-size: 11px; }
.modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 4px; }
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
.spin { display: inline-block; }
.spin.spinning { animation: is-spin 1s linear infinite; }
@keyframes is-spin { to { transform: rotate(360deg); } }
</style>
