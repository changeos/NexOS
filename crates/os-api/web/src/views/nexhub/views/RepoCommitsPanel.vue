<script setup lang="ts">
// =============================================================================
// RepoCommitsPanel —— 仓库详情 Commits Tab（v0.1.32，原「提交历史」归组进仓库域）。
// GET /api/v1/coderepo/repos/:name/commits（最近 20 条）。
// =============================================================================

import { ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatDate, shortHash, type CommitInfo } from '@/views/nexhub/model';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

const commits = ref<CommitInfo[]>([]);
const loading = ref(false);

async function loadCommits(): Promise<void> {
  if (!props.repoName.trim()) return;
  loading.value = true;
  try {
    const r = (await endpoints.codeRepoCommits(props.repoName.trim())) as {
      commits?: CommitInfo[];
    };
    commits.value = r.commits ?? [];
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.commits.loadFailed')}: ${errMsg(e)}`);
    commits.value = [];
  } finally {
    loading.value = false;
  }
}

// 仓库切换：重载提交历史
watch(() => props.repoName, () => void loadCommits(), { immediate: true });
</script>

<template>
  <section class="commits-panel">
    <div v-if="props.repoName && commits.length === 0 && !loading" class="card empty-card">
      {{ t('nexhub.commits.empty') }}
    </div>
    <div v-else-if="loading && commits.length === 0" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="commits.length" class="commit-list">
      <div v-for="(c, idx) in commits" :key="(c.hash ?? '') + idx" class="card commit-item">
        <div class="commit-head">
          <code class="commit-hash">{{ shortHash(c.hash) }}</code>
          <span class="commit-msg">{{ c.message }}</span>
        </div>
        <div class="commit-meta muted small">
          <span>{{ c.author }}</span>
          <span>·</span>
          <span>{{ formatDate(c.date) }}</span>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.commit-list { display: flex; flex-direction: column; gap: 10px; }
.commit-item { padding: 12px 16px; display: flex; flex-direction: column; gap: 6px; }
.commit-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.commit-hash {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12px; font-weight: 600;
  padding: 2px 8px; background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420);
  border-radius: var(--radius-sm, 6px); flex-shrink: 0;
}
.commit-msg { font-size: 14px; color: var(--text, #2B2B2B); word-break: break-word; }
.commit-meta { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
</style>
