<script setup lang="ts">
// =============================================================================
// RepoPullsTab —— 仓库详情 Pull Requests Tab（v0.1.32，原 Tab6 归组进仓库域）。
// 提 PR 人人可（分支须已 push 到裸仓）；merge = 更改仓库内容，仅 admin /
// 仓库所有者（合并确认走站内 NexhubConfirm，替代原生 confirm()）。
// =============================================================================
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import type { RepoComment, RepoPull } from '@/api/client';
import { identiconSvg } from '@/composables/useIdenticon';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatRelative, shortHash } from '@/views/nexhub/model';
import { authorLabel, collabWriteErr, useCollabIdentity } from '@/views/nexhub/collab';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

// 身份 / 权限（owner 判定依据大厅条目 publisher——壳共享大厅数据）。
const idctx = useCollabIdentity(() => ctx.lobbyEntries.value);

type PullState = 'open' | 'merged' | 'closed' | 'all';
const pullsState = ref<PullState>('open');
const pulls = ref<RepoPull[]>([]);
const loading = ref(false);
const expandedPull = ref<number | null>(null);
const pullDetails = ref<Record<number, { pull: RepoPull; comments: RepoComment[]; diff_stat: string }>>({});
const pullCommentDrafts = ref<Record<number, string>>({});
const showCreatePull = ref(false);
const pullForm = ref({ title: '', body: '', from_branch: '', to_branch: '' });
/** 新建 PR 对话框的分支下拉数据（git for-each-ref 实时查）。 */
const pullBranchOptions = ref<string[]>([]);
const pullDefaultBranch = ref('main');

/** 待确认合并的 PR（站内确认弹窗数据；替代原生 confirm）。 */
const mergePending = ref<RepoPull | null>(null);
/** 确认弹窗开关代理（v-model 需可写成员表达式；null ↔ 展示态）。 */
const showMergeConfirm = computed<boolean>({
  get: () => mergePending.value !== null,
  set: (v) => {
    if (!v) mergePending.value = null;
  },
});

async function loadPulls(): Promise<void> {
  const repo = props.repoName.trim();
  if (!repo) return;
  loading.value = true;
  try {
    const r = await endpoints.codeRepoPulls(repo, pullsState.value);
    pulls.value = r.pulls ?? [];
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.pulls.loadFailed')}: ${errMsg(e)}`);
    pulls.value = [];
  } finally {
    loading.value = false;
  }
}

/** 展开/收起 PR 行；展开时懒加载详情（评论流 + diff 摘要）。 */
async function togglePull(p: RepoPull): Promise<void> {
  const num = p.number;
  if (expandedPull.value === num) {
    expandedPull.value = null;
    return;
  }
  expandedPull.value = num;
  if (!pullDetails.value[num]) {
    try {
      const r = await endpoints.codeRepoPullDetail(props.repoName.trim(), num);
      pullDetails.value[num] = {
        pull: r.pull,
        comments: r.comments ?? [],
        diff_stat: r.diff_stat ?? '',
      };
    } catch (e) {
      ctx.showMsg('error', `${t('nexhub.pulls.detailLoadFailed')}: ${errMsg(e)}`);
    }
  }
}

/** 打开新建 PR 对话框：实时拉仓库分支做下拉（from/to）。 */
async function openCreatePull(): Promise<void> {
  const repo = props.repoName.trim();
  pullForm.value = { title: '', body: '', from_branch: '', to_branch: '' };
  ctx.clearMsg();
  try {
    const r = (await endpoints.codeRepoContents(repo)) as {
      branches?: string[];
      default_branch?: string;
    };
    pullBranchOptions.value = r.branches ?? [];
    pullDefaultBranch.value = r.default_branch || 'main';
    pullForm.value.to_branch = pullDefaultBranch.value;
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.pulls.branchLoadFailed')}: ${errMsg(e)}`);
    pullBranchOptions.value = [];
  }
  showCreatePull.value = true;
}

/** 新建 PR（from 分支须已 push 到裸仓；需身份）。 */
async function createPull(): Promise<void> {
  const repo = props.repoName.trim();
  if (!pullForm.value.title.trim()) {
    ctx.showMsg('error', t('nexhub.pulls.titleRequired'));
    return;
  }
  if (!pullForm.value.from_branch) {
    ctx.showMsg('error', t('nexhub.pulls.fromRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.createCodeRepoPull(
      repo,
      {
        title: pullForm.value.title.trim(),
        body: pullForm.value.body.trim() || undefined,
        from_branch: pullForm.value.from_branch,
        to_branch: pullForm.value.to_branch || undefined,
      },
      opts,
    );
    ctx.showMsg(
      'ok',
      t('nexhub.pulls.created', { from: pullForm.value.from_branch, to: pullForm.value.to_branch || pullDefaultBranch.value }),
    );
    showCreatePull.value = false;
    pullsState.value = 'open';
    await loadPulls();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.pulls.createAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

/** 发表 PR 评论。 */
async function submitPullComment(num: number): Promise<void> {
  const repo = props.repoName.trim();
  const text = (pullCommentDrafts.value[num] ?? '').trim();
  if (!repo || !text) return;
  try {
    const opts = await idctx.requireNexhubOpts();
    const r = await endpoints.codeRepoPullComment(repo, num, { body: text }, opts);
    const d = pullDetails.value[num];
    if (d) {
      d.comments.push(r.comment);
      d.pull = { ...d.pull, comment_count: (d.pull.comment_count ?? 0) + 1 };
    }
    pullCommentDrafts.value[num] = '';
    ctx.showMsg('ok', t('nexhub.collab.commentPosted'));
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.collab.commentAction'), e));
  }
}

/** 确认弹窗点「合并」：真实执行 merge（仅 admin/仓库 owner 入口可见）。 */
async function doMerge(): Promise<void> {
  const p = mergePending.value;
  const repo = props.repoName.trim();
  mergePending.value = null;
  if (!p || !repo) return;
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    const r = (await endpoints.codeRepoPullMerge(repo, p.number, opts)) as {
      merged_sha?: string;
    };
    ctx.showMsg('ok', t('nexhub.pulls.merged', { n: p.number, sha: shortHash(r.merged_sha) }));
    const d = pullDetails.value[p.number];
    if (d) d.pull = { ...d.pull, state: 'merged' };
    await loadPulls();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.pulls.mergeAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

/** 关闭 PR（仅作者或 admin）。 */
async function closePull(p: RepoPull): Promise<void> {
  const repo = props.repoName.trim();
  if (!repo) return;
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.codeRepoPullClose(repo, p.number, opts);
    ctx.showMsg('ok', t('nexhub.pulls.closed', { n: p.number }));
    const d = pullDetails.value[p.number];
    if (d) d.pull = { ...d.pull, state: 'closed' };
    await loadPulls();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.pulls.closeAction'), e));
  }
}

// 仓库切换：重置并重载
watch(
  () => props.repoName,
  () => {
    expandedPull.value = null;
    pullDetails.value = {};
    void loadPulls();
  },
  { immediate: true },
);
</script>

<template>
  <section class="pulls-tab">
    <div class="browser-toolbar">
      <select v-model="pullsState" class="search-input state-select" @change="loadPulls">
        <option value="open">{{ t('nexhub.collab.stateOpen') }}</option>
        <option value="merged">{{ t('nexhub.collab.stateMerged') }}</option>
        <option value="closed">{{ t('nexhub.collab.stateClosed') }}</option>
        <option value="all">{{ t('nexhub.collab.stateAll') }}</option>
      </select>
      <button class="btn btn-small" type="button" :disabled="loading" @click="loadPulls">
        <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
        {{ t('nexhub.common.refresh') }}
      </button>
      <button class="btn btn-small btn-primary" type="button" @click="openCreatePull">+ {{ t('nexhub.pulls.create') }}</button>
    </div>

    <p class="muted small collab-hint">
      {{ t('nexhub.pulls.hintPre') }}
      <strong>{{ t('nexhub.pulls.hintStrong') }}</strong>
      {{ t('nexhub.pulls.hintPost', { state: idctx.canMergePull(props.repoName) ? t('nexhub.pulls.canMerge') : t('nexhub.pulls.cannotMerge') }) }}
    </p>

    <div v-if="loading" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="pulls.length === 0" class="card empty-card">{{ t('nexhub.pulls.empty') }}</div>

    <div v-else class="collab-list">
      <div
        v-for="p in pulls"
        :key="p.number"
        class="card collab-row"
        :class="{ expanded: expandedPull === p.number }"
      >
        <div class="collab-row-line" :title="t('nexhub.collab.toggleDetail')" @click="togglePull(p)">
          <span class="collab-state-badge" :class="`is-${p.state}`">
            {{ p.state === 'open' ? '● Open' : p.state === 'merged' ? '⇣ Merged' : '✕ Closed' }}
          </span>
          <span class="collab-num">#{{ p.number }}</span>
          <span class="collab-title">{{ p.title }}</span>
          <code class="collab-branches">{{ p.from_branch }} → {{ p.to_branch }}</code>
          <span class="collab-meta muted small">
            💬 {{ p.comment_count }} · {{ authorLabel(p.author, p.author_display) }} ·
            {{ formatRelative(p.updated_at) }}
          </span>
        </div>

        <!-- 展开详情：描述 + diff 摘要 + 评论流 + Merge/Close（按权限） -->
        <div v-if="expandedPull === p.number" class="collab-detail" @click.stop>
          <div v-if="pullDetails[p.number]" class="collab-body">
            <p class="collab-desc">{{ pullDetails[p.number].pull.body || t('nexhub.collab.noBody') }}</p>
            <pre
              v-if="pullDetails[p.number].diff_stat"
              class="collab-diffstat"
            >{{ pullDetails[p.number].diff_stat }}</pre>
            <div
              v-for="c in pullDetails[p.number].comments"
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
              v-model="pullCommentDrafts[p.number]"
              class="search-input collab-input"
              rows="2"
              :placeholder="t('nexhub.pulls.commentPlaceholder')"
            ></textarea>
            <div class="collab-actions">
              <button
                v-if="p.state === 'open' && idctx.canMergePull(props.repoName)"
                class="btn btn-small btn-primary"
                type="button"
                @click="mergePending = p"
              >⇣ {{ t('nexhub.pulls.mergeBtn', { from: p.from_branch, to: p.to_branch }) }}</button>
              <span
                v-else-if="p.state === 'open'"
                class="muted small"
                :title="t('nexhub.pulls.mergePermTitle')"
              >🔒 {{ t('nexhub.pulls.mergePermHint') }}</span>
              <button
                v-if="p.state === 'open' && idctx.canToggleState(p.author)"
                class="btn btn-small"
                type="button"
                @click="closePull(p)"
              >✕ {{ t('nexhub.pulls.closeBtn') }}</button>
              <button
                class="btn btn-small btn-primary"
                type="button"
                :disabled="!(pullCommentDrafts[p.number] ?? '').trim()"
                @click="submitPullComment(p.number)"
              >{{ t('nexhub.collab.commentSubmit') }}</button>
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- 新建 PR 对话框 -->
    <div v-if="showCreatePull" class="modal-overlay" @click.self="showCreatePull = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.pulls.createTitle', { repo: props.repoName }) }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showCreatePull = false">✕</button>
        </div>
        <div class="form-row">
          <span class="form-hint muted small">{{ t('nexhub.pulls.createHint') }}</span>
        </div>
        <div class="form-row">
          <label class="form-label" for="pr-title">{{ t('nexhub.collab.titleLabel') }} *</label>
          <input
            id="pr-title"
            v-model="pullForm.title"
            class="search-input"
            :placeholder="t('nexhub.pulls.titlePlaceholder')"
          />
        </div>
        <div class="form-row">
          <label class="form-label" for="pr-body">{{ t('nexhub.collab.bodyLabel') }}</label>
          <textarea
            id="pr-body"
            v-model="pullForm.body"
            class="search-input collab-input"
            rows="3"
            :placeholder="t('nexhub.pulls.bodyPlaceholder')"
          ></textarea>
        </div>
        <div class="form-row form-row-2col">
          <div>
            <label class="form-label" for="pr-from">{{ t('nexhub.pulls.fromLabel') }} *</label>
            <select id="pr-from" v-model="pullForm.from_branch" class="search-input">
              <option value="">{{ t('nexhub.pulls.pickBranch') }}</option>
              <option v-for="b in pullBranchOptions" :key="b" :value="b">{{ b }}</option>
            </select>
          </div>
          <div>
            <label class="form-label" for="pr-to">{{ t('nexhub.pulls.toLabel', { branch: pullDefaultBranch }) }}</label>
            <select id="pr-to" v-model="pullForm.to_branch" class="search-input">
              <option v-for="b in pullBranchOptions" :key="b" :value="b">{{ b }}</option>
            </select>
          </div>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showCreatePull = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="createPull">
            {{ t('nexhub.pulls.create') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 合并 PR 确认（站内弹窗，替代原生 confirm） -->
    <NexhubConfirm
      v-model:open="showMergeConfirm"
      :title="mergePending ? t('nexhub.pulls.mergeConfirmTitle', { n: mergePending.number }) : ''"
      :body="mergePending ? t('nexhub.pulls.mergeConfirmBody', { from: mergePending.from_branch, to: mergePending.to_branch }) : ''"
      :danger="true"
      :confirm-text="t('nexhub.pulls.mergeAction')"
      @confirm="doMerge"
    />
  </section>
</template>

<style scoped>
.pulls-tab { display: flex; flex-direction: column; gap: 12px; }
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
.collab-state-badge.is-merged { background: rgba(63, 127, 191, 0.14); color: #3573b9; }
.collab-num { font-size: 12px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.collab-title { font-weight: 600; }
.collab-branches {
  font-size: 11px; padding: 1px 8px; border-radius: 6px;
  background: rgba(0, 0, 0, 0.05); white-space: nowrap;
}
.collab-meta { margin-left: auto; }
.collab-detail { margin-top: 10px; border-top: 1px dashed rgba(0, 0, 0, 0.12); padding-top: 10px; }
.collab-body { display: flex; flex-direction: column; gap: 8px; }
.collab-desc { margin: 0; white-space: pre-wrap; }
.collab-diffstat {
  margin: 0; padding: 8px 10px; border-radius: 8px; background: rgba(0, 0, 0, 0.04);
  font-size: 11px; overflow-x: auto; white-space: pre;
}
.collab-comment { display: flex; gap: 8px; align-items: flex-start; }
.identicon { width: 16px; height: 16px; border-radius: 3px; }
.collab-avatar { border-radius: 4px; flex-shrink: 0; margin-top: 2px; }
.collab-comment-main { flex: 1; min-width: 0; }
.collab-comment-head { display: flex; align-items: center; gap: 6px; }
.collab-comment-body { margin: 2px 0 0; white-space: pre-wrap; }
.collab-compose { margin-top: 10px; display: flex; flex-direction: column; gap: 8px; }
.collab-input { resize: vertical; font-family: inherit; }
.collab-actions { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.form-row { display: flex; flex-direction: column; gap: 6px; }
.form-row-2col { flex-direction: row; gap: 12px; }
.form-row-2col > div { flex: 1; display: flex; flex-direction: column; gap: 6px; }
.form-label { font-size: 12px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.form-hint { font-size: 11px; }
.modal-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); display: flex;
  align-items: center; justify-content: center; z-index: 1000; padding: 20px;
}
.modal-card { width: 100%; max-width: 460px; padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.modal-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.modal-title { font-size: 16px; font-weight: 700; color: var(--text, #2B2B2B); margin: 0; }
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
.spin.spinning { animation: pr-spin 1s linear infinite; }
@keyframes pr-spin { to { transform: rotate(360deg); } }
</style>
