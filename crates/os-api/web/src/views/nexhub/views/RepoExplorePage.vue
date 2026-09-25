<script setup lang="ts">
// =============================================================================
// RepoExplorePage —— Explore 仓库列表页（v0.1.32 P1，默认视图）。
//
// GitHub 风 visitor-first：搜索过滤 + 类别 facets（全部 / 应用仓库 nexos-app-*
// / 普通仓库）+ 排序（最近提交 / 名称）+ 仓库卡网格（RepoCard：应用仓库带
// manifest 徽章与一键部署）。顶部统计条（仓库 / 总占用 / AI 会话 / 累计提交）
// 与创建 / 导入入口沿用原「仓库列表」Tab 能力。
// =============================================================================

import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatBytes, isAppRepo } from '@/views/nexhub/model';
import RepoCard from '@/views/nexhub/components/RepoCard.vue';

const { t } = useI18n();
const ctx = useNexhub();

// —— facets / 排序（本地视图态，不进 URL）——
type Facet = 'all' | 'apps' | 'normal';
type SortKey = 'last_commit' | 'name';
const facet = ref<Facet>('all');
const sortBy = ref<SortKey>('last_commit');

/** 应用仓库计数（nexos-app-*）。 */
const appCount = computed(() => ctx.repos.value.filter((r) => isAppRepo(r.name)).length);
/** 普通仓库计数。 */
const normalCount = computed(() => ctx.repos.value.length - appCount.value);

/** 过滤（搜索词来自顶栏全局搜索 ctx.exploreQuery）+ 排序后的仓库列表。 */
const visibleRepos = computed(() => {
  const q = ctx.exploreQuery.value.trim().toLowerCase();
  let list = ctx.repos.value.filter((r) => {
    if (facet.value === 'apps' && !isAppRepo(r.name)) return false;
    if (facet.value === 'normal' && isAppRepo(r.name)) return false;
    if (!q) return true;
    const name = (r.name ?? '').toLowerCase();
    const desc = (r.description ?? '').toLowerCase();
    return name.includes(q) || desc.includes(q);
  });
  list = [...list].sort((a, b) => {
    if (sortBy.value === 'name') {
      return (a.name ?? '').localeCompare(b.name ?? '');
    }
    // 最近提交：last_commit_date 降序（空值沉底）
    const da = a.last_commit_date ? new Date(a.last_commit_date).getTime() : 0;
    const db = b.last_commit_date ? new Date(b.last_commit_date).getTime() : 0;
    if (da !== db) return db - da;
    return (a.name ?? '').localeCompare(b.name ?? '');
  });
  return list;
});

// —— 创建仓库对话框（原 Tab1 能力沿用）——
const showCreateRepo = ref(false);
const newRepo = ref({ name: '', description: '' });

async function createRepo(): Promise<void> {
  if (!newRepo.value.name.trim()) {
    ctx.showMsg('error', t('nexhub.explore.nameRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    await endpoints.createCodeRepo({
      name: newRepo.value.name.trim(),
      description: newRepo.value.description.trim() || undefined,
    });
    ctx.showMsg('ok', t('nexhub.explore.repoCreated', { name: newRepo.value.name }));
    showCreateRepo.value = false;
    newRepo.value = { name: '', description: '' };
    await ctx.refreshShared();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.explore.repoCreateFailed')}: ${errMsg(e)}`);
  } finally {
    ctx.actionLoading.value = false;
  }
}

// —— 导入目录对话框 ——
const showImport = ref(false);
const importForm = ref({ name: '', source_dir: '' });

function openImport(): void {
  importForm.value = { name: '', source_dir: '/tank/' };
  showImport.value = true;
}

async function doImport(): Promise<void> {
  if (!importForm.value.name.trim()) {
    ctx.showMsg('error', t('nexhub.explore.nameRequired'));
    return;
  }
  if (!importForm.value.source_dir.trim()) {
    ctx.showMsg('error', t('nexhub.explore.sourceDirRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const r = (await endpoints.codeRepoImport(
      importForm.value.name.trim(),
      importForm.value.source_dir.trim(),
    )) as { clone_url_ssh?: string; branch?: string };
    ctx.showMsg(
      'ok',
      t('nexhub.explore.imported', { name: importForm.value.name, branch: r.branch ?? 'master' }),
    );
    showImport.value = false;
    await ctx.refreshShared();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.explore.importFailed')}: ${errMsg(e)}`);
  } finally {
    ctx.actionLoading.value = false;
  }
}
</script>

<template>
  <section class="explore-page">
    <!-- 统计概览（原 Tab1 统计条沿用） -->
    <section class="card stats-row">
      <div class="stat-item">
        <span class="stat-num">{{ ctx.stats.value.repo_count ?? 0 }}</span>
        <span class="stat-label">{{ t('nexhub.stats.repos') }}</span>
      </div>
      <div class="stat-item">
        <span class="stat-num">{{ formatBytes(ctx.stats.value.total_size) }}</span>
        <span class="stat-label">{{ t('nexhub.stats.totalSize') }}</span>
      </div>
      <div class="stat-item">
        <span class="stat-num">{{ ctx.stats.value.session_count ?? 0 }}</span>
        <span class="stat-label">{{ t('nexhub.stats.sessions') }}</span>
      </div>
      <div class="stat-item">
        <span class="stat-num">{{ ctx.stats.value.total_commits ?? 0 }}</span>
        <span class="stat-label">{{ t('nexhub.stats.totalCommits') }}</span>
      </div>
    </section>

    <!-- 工具条：facets + 排序 + 创建/导入 -->
    <div class="repo-toolbar">
      <div class="facet-row" role="group" :aria-label="t('nexhub.explore.facetLabel')">
        <button
          class="facet-chip"
          :class="{ active: facet === 'all' }"
          type="button"
          @click="facet = 'all'"
        >{{ t('nexhub.explore.facetAll') }} ({{ ctx.repos.value.length }})</button>
        <button
          class="facet-chip"
          :class="{ active: facet === 'apps' }"
          type="button"
          @click="facet = 'apps'"
        >{{ t('nexhub.explore.facetApps') }} ({{ appCount }})</button>
        <button
          class="facet-chip"
          :class="{ active: facet === 'normal' }"
          type="button"
          @click="facet = 'normal'"
        >{{ t('nexhub.explore.facetNormal') }} ({{ normalCount }})</button>
      </div>
      <div class="toolbar-actions">
        <label class="sort-label muted small" for="nexhub-sort">{{ t('nexhub.explore.sort') }}</label>
        <select id="nexhub-sort" v-model="sortBy" class="search-input sort-select">
          <option value="last_commit">{{ t('nexhub.explore.sortLastCommit') }}</option>
          <option value="name">{{ t('nexhub.explore.sortName') }}</option>
        </select>
        <button class="btn btn-small" type="button" @click="openImport">{{ t('nexhub.explore.importDir') }}</button>
        <button class="btn btn-small btn-primary" type="button" @click="showCreateRepo = true">
          + {{ t('nexhub.explore.createRepo') }}
        </button>
      </div>
    </div>

    <!-- 仓库卡网格（应用仓库卡带 manifest 徽章 + 一键部署） -->
    <div v-if="ctx.reposLoading.value && ctx.repos.value.length === 0" class="card empty-card">
      {{ t('common.loading') }}
    </div>
    <div v-else-if="visibleRepos.length === 0" class="card empty-card">
      {{ t('nexhub.explore.empty') }}
    </div>
    <div v-else class="repo-grid">
      <RepoCard v-for="r in visibleRepos" :key="r.name" :repo="r" />
    </div>

    <!-- 创建仓库对话框 -->
    <div v-if="showCreateRepo" class="modal-overlay" @click.self="showCreateRepo = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.explore.createTitle') }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showCreateRepo = false">✕</button>
        </div>
        <div class="form-row">
          <label class="form-label" for="nr-name">{{ t('nexhub.explore.nameLabel') }} *</label>
          <input id="nr-name" v-model="newRepo.name" class="search-input" placeholder="my-project" />
          <span class="form-hint muted small">git init --bare /tank/git-repos/&lt;name&gt;.git</span>
        </div>
        <div class="form-row">
          <label class="form-label" for="nr-desc">{{ t('nexhub.explore.descLabel') }}</label>
          <textarea id="nr-desc" v-model="newRepo.description" class="search-input form-textarea" rows="2"
            :placeholder="t('nexhub.explore.descPlaceholder')"></textarea>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showCreateRepo = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="createRepo">
            {{ t('nexhub.explore.createSubmit') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 导入目录对话框 -->
    <div v-if="showImport" class="modal-overlay" @click.self="showImport = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.explore.importTitle') }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showImport = false">✕</button>
        </div>
        <div class="form-row">
          <label class="form-label" for="im-name">{{ t('nexhub.explore.nameLabel') }} *</label>
          <input id="im-name" v-model="importForm.name" class="search-input" placeholder="my-project" />
        </div>
        <div class="form-row">
          <label class="form-label" for="im-src">{{ t('nexhub.explore.sourceDirLabel') }} *</label>
          <input id="im-src" v-model="importForm.source_dir" class="search-input" placeholder="/tank/project" />
          <span class="form-hint muted small">{{ t('nexhub.explore.importHint') }}</span>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showImport = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="doImport">
            {{ t('nexhub.explore.importSubmit') }}
          </button>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.explore-page { display: flex; flex-direction: column; gap: 14px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }

.stats-row { display: flex; align-items: center; gap: 28px; padding: 14px 20px; flex-wrap: wrap; }
.stat-item { display: flex; flex-direction: column; gap: 2px; }
.stat-num { font-size: 20px; font-weight: 700; color: var(--accent, #E95420); }
.stat-label { font-size: 12px; color: var(--text-muted, #5E5C5F); }

.repo-toolbar { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.facet-row { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.facet-chip {
  padding: 5px 12px; border-radius: var(--radius-pill, 20px); border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #fff); font-size: 12.5px; font-weight: 500; color: var(--text-muted, #5E5C5F);
  cursor: pointer; font-family: inherit; transition: background 0.15s ease, color 0.15s ease;
}
.facet-chip:hover { background: var(--border-soft, #F3F4F6); }
.facet-chip.active { background: rgba(233, 84, 32, 0.12); border-color: rgba(233, 84, 32, 0.4); color: var(--accent, #E95420); font-weight: 600; }
.toolbar-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.sort-label { margin-right: -2px; }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.sort-select { min-width: 130px; }

.repo-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; }

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
.form-textarea { resize: vertical; font-family: inherit; }
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
.btn-primary:hover { background: #d44a1c; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
</style>

