<script setup lang="ts">
// =============================================================================
// RepoDetailPage —— 仓库详情页（v0.1.32 P1，新增路由概念 /s/codehub/r/:name）。
//
// GitHub 风三层布局第三层：header（名称 / 描述 / 应用徽章 / 一键部署 / 删除）
// + 内嵌 Tab（Code / Commits / Manifest / Issues / PR —— 原「代码浏览 / 提交
// 历史 / Issues / PR」四个平级 Tab 归组进仓库域）。
// - Manifest Tab 仅应用仓库（nexos-app-*）显示；
// - standalone 模式 Tab 状态进 URL（?tab=code|commits|manifest|issues|pulls），
//   刷新不丢；桌面窗口模式内部状态（宿主页 history 不可写）。
// =============================================================================

import { computed, ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub, type RepoDetailTab } from '@/views/nexhub/context';
import { isAppRepo } from '@/views/nexhub/model';
import DeployButton from '@/views/nexhub/components/DeployButton.vue';
import CiBadge from '@/views/nexhub/components/CiBadge.vue';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';
import RepoCodeTab from '@/views/nexhub/views/RepoCodeTab.vue';
import RepoCommitsPanel from '@/views/nexhub/views/RepoCommitsPanel.vue';
import RepoManifestTab from '@/views/nexhub/views/RepoManifestTab.vue';
import RepoCiTab from '@/views/nexhub/views/RepoCiTab.vue';
import RepoIssuesTab from '@/views/nexhub/views/RepoIssuesTab.vue';
import RepoPullsTab from '@/views/nexhub/views/RepoPullsTab.vue';

const props = defineProps<{
  /** 仓库名（standalone 路由 param / 桌面模式壳内部状态注入）。 */
  name: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();
const route = useRoute();

/** 仓库元数据（描述 / 分支数等，来自共享列表）。 */
const repoMeta = computed(() => ctx.repos.value.find((r) => r.name === props.name));

/** 是否应用仓库（nexos-app-*；决定 manifest Tab 与部署按钮）。 */
const isApp = computed(() => isAppRepo(props.name));

/** join 目录条目（版本徽章 / 部署）。 */
const appEntry = computed(() => (isApp.value ? ctx.deploy.catalogEntry(props.name) : undefined));

// —— Tab 状态（standalone 下与 ?tab= 查询参数双向同步）——
const TAB_KEYS: RepoDetailTab[] = ['code', 'commits', 'manifest', 'ci', 'issues', 'pulls'];

function tabFromQuery(): RepoDetailTab {
  const q = route.query.tab;
  const v = Array.isArray(q) ? String(q[0]) : String(q ?? '');
  return (TAB_KEYS as string[]).includes(v) ? (v as RepoDetailTab) : 'code';
}

const tab = ref<RepoDetailTab>(ctx.standalone ? tabFromQuery() : 'code');

// 仓库切换（深链换仓 / 桌面换仓）：Tab 回到 URL 指定值或默认 Code
watch(
  () => props.name,
  () => {
    tab.value = ctx.standalone ? tabFromQuery() : 'code';
  },
);

// Tab 变更 → 同步 URL（仅 standalone；replace 不产生历史记录）
watch(tab, (v) => {
  if (!ctx.standalone || !ctx.router) return;
  const query = { ...route.query };
  if (v === 'code') delete query.tab;
  else query.tab = v;
  void ctx.router.replace({ query }).catch(() => undefined);
});

/** Manifest Tab 可见性：应用仓库专属。 */
const showManifest = computed(() => isApp.value);

/** Tab 标题（i18n）。 */
function tabLabel(key: RepoDetailTab): string {
  switch (key) {
    case 'code':
      return t('nexhub.detail.tabCode');
    case 'commits':
      return t('nexhub.detail.tabCommits');
    case 'manifest':
      return t('nexhub.detail.tabManifest');
    case 'ci':
      return t('nexhub.ci.tab');
    case 'issues':
      return 'Issues';
    case 'pulls':
      return 'PR';
  }
}

// —— 删除仓库（原卡片删除入口移到详情页；原生 confirm → 站内弹窗）——
const showDelete = ref(false);
const deleting = ref(false);

async function doDelete(): Promise<void> {
  deleting.value = true;
  ctx.clearMsg();
  try {
    await endpoints.deleteCodeRepo(props.name);
    ctx.showMsg('ok', t('nexhub.detail.repoDeleted', { name: props.name }));
    showDelete.value = false;
    await ctx.refreshShared();
    ctx.goExplore();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.detail.repoDeleteFailed')}: ${e instanceof Error ? e.message : String(e)}`);
  } finally {
    deleting.value = false;
  }
}
</script>

<template>
  <section class="detail-page">
    <!-- header：返回 + 名称 + 徽章 + 行为区 -->
    <div class="detail-head">
      <div class="detail-head-line">
        <button class="btn btn-small btn-back" type="button" @click="ctx.goExplore()">
          ← {{ t('nexhub.detail.backToList') }}
        </button>
        <span class="detail-name">{{ props.name }}</span>
        <span v-if="isApp" class="badge badge-app">
          {{ t('nexhub.explore.appBadge') }}
          <template v-if="appEntry">&nbsp;v{{ appEntry.installed ? appEntry.installed_version : appEntry.version }}</template>
        </span>
        <span class="repo-meta-num" :title="t('nexhub.explore.branches')">⎇ {{ repoMeta?.branch_count ?? 0 }}</span>
        <!-- 内置 CI 徽章（点击切到 CI Tab） -->
        <CiBadge :repo="props.name" clickable @open="tab = 'ci'" />
        <span class="head-spacer" />
        <!-- 一键部署（应用仓库；状态机见 DeployButton） -->
        <DeployButton v-if="isApp" :repo="props.name" />
        <!-- 删除仓库（原 confirm() → 站内确认弹窗） -->
        <button class="btn btn-small btn-danger" type="button" @click="showDelete = true">
          {{ t('nexhub.detail.delete') }}
        </button>
      </div>
      <p class="detail-desc muted">
        {{ repoMeta?.description || t('nexhub.explore.noDescription') }}
      </p>
    </div>

    <!-- 内嵌 Tab：Code / Commits / Manifest(应用专属) / Issues / PR -->
    <nav class="detail-tabs" role="tablist">
      <template v-for="key in TAB_KEYS" :key="key">
        <button
          v-if="key !== 'manifest' || showManifest"
          class="detail-tab"
          :class="{ active: tab === key }"
          role="tab"
          :aria-selected="tab === key"
          type="button"
          @click="tab = key"
        >{{ tabLabel(key) }}</button>
      </template>
    </nav>

    <!-- Tab 内容（keep-alive 不必要：切仓由子组件 watch 自行重载） -->
    <div class="detail-body">
      <RepoCodeTab v-show="tab === 'code'" :repo-name="props.name" />
      <RepoCommitsPanel v-show="tab === 'commits'" :repo-name="props.name" />
      <RepoManifestTab v-if="showManifest" v-show="tab === 'manifest'" :repo-name="props.name" />
      <RepoCiTab v-show="tab === 'ci'" :repo-name="props.name" />
      <RepoIssuesTab v-show="tab === 'issues'" :repo-name="props.name" />
      <RepoPullsTab v-show="tab === 'pulls'" :repo-name="props.name" />
    </div>

    <!-- 删除仓库确认（站内弹窗，替代原生 confirm） -->
    <NexhubConfirm
      v-model:open="showDelete"
      :title="t('nexhub.detail.deleteConfirmTitle', { name: props.name })"
      :body="t('nexhub.detail.deleteConfirmBody')"
      :danger="true"
      :confirm-text="t('nexhub.detail.delete')"
      @confirm="doDelete"
    />
  </section>
</template>

<style scoped>
.detail-page { display: flex; flex-direction: column; gap: 14px; }
.muted { color: var(--text-muted, #5E5C5F); }
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.3); }
.btn-danger:hover { background: #fee2e2; }

.detail-head { display: flex; flex-direction: column; gap: 6px; }
.detail-head-line { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.detail-name { font-size: 19px; font-weight: 700; color: var(--text, #2B2B2B); word-break: break-all; }
.badge {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 11.5px; font-weight: 600;
}
.badge-app { background: rgba(233, 84, 32, 0.14); color: var(--accent, #E95420); }
.repo-meta-num { font-size: 12.5px; color: var(--accent, #E95420); font-weight: 600; }
.head-spacer { flex: 1; }
.detail-desc { margin: 0; font-size: 13px; line-height: 1.55; }

.detail-tabs {
  display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap;
}
.detail-tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.detail-tab:hover { color: var(--text, #2B2B2B); }
.detail-tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }
.detail-body { display: flex; flex-direction: column; gap: 12px; }
</style>

