<script setup lang="ts">
// =============================================================================
// NexhubTopBar —— NexHub 顶栏（v0.1.32 P1 重排，GitHub 风三层布局第一层）。
//
// 左：NexHub 标识（点击回 Explore）；中：一级导航（仓库 / 大厅 / AI 会话 /
// 接入指南——原 8 Tab 归组，Issues/PR/浏览/提交收进仓库详情页内嵌 Tab）；
// 右：全局仓库搜索（过滤 Explore 列表）+ 刷新 + 外链。
// 双模式：桌面窗口模式显示「新标签全屏打开」外链图标（既有行为）；standalone
// 全屏模式显示「打开桌面」链接（回 /?app=codehub）。
// 导航在两种模式下走同一 ctx.goView / ctx.goExplore（内部状态 or 子路由）。
// =============================================================================

import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { useNexhub, type HubView } from '@/views/nexhub/context';

const { t } = useI18n();
const ctx = useNexhub();

/** 一级导航项（key 对应 HubView；label 走 i18n）。 */
const navItems: { key: Exclude<HubView, 'repo'>; icon: string; labelKey: string }[] = [
  { key: 'explore', icon: '📦', labelKey: 'nexhub.nav.repos' },
  { key: 'lobby', icon: '🏛', labelKey: 'nexhub.nav.lobby' },
  { key: 'sessions', icon: '🤖', labelKey: 'nexhub.nav.sessions' },
  { key: 'onboarding', icon: '📡', labelKey: 'nexhub.nav.onboarding' },
];

/** 当前激活视图（桌面 = 内部状态；standalone = 壳已同步路由镜像）。 */
const activeView = computed(() => ctx.currentView.value);

function onNav(key: Exclude<HubView, 'repo'>): void {
  ctx.goView(key);
}

/** 全局搜索：输入即过滤 Explore 列表；非 Explore 视图下输入自动跳回列表。 */
const searchQuery = computed<string>({
  get: () => ctx.exploreQuery.value,
  set: (v: string) => {
    ctx.exploreQuery.value = v;
  },
});

function onSearchInput(): void {
  if (ctx.currentView.value !== 'explore' && ctx.exploreQuery.value) {
    ctx.goExplore();
  }
}

/** 新标签页打开独立全屏版（桌面浮窗模式右上角外链；standalone 隐藏）。 */
function openStandalone(): void {
  window.open('/s/codehub', '_blank', 'noopener');
}

/** standalone 全屏 → 回桌面（打开 NexHub 桌面浮窗）。 */
function openDesktop(): void {
  window.location.href = '/?app=codehub';
}
</script>

<template>
  <header class="ntb" :class="{ standalone: ctx.standalone }">
    <div class="ntb-row">
      <button class="ntb-brand" type="button" @click="ctx.goExplore()">
        <span class="ntb-logo" aria-hidden="true">◆</span>
        <span class="ntb-name">NexHub</span>
      </button>

      <!-- 一级导航（原 8 Tab 归组：仓库/大厅/AI 会话/接入指南） -->
      <nav class="ntb-nav" role="navigation">
        <button
          v-for="item in navItems"
          :key="item.key"
          class="ntb-nav-item"
          :class="{ active: activeView === item.key }"
          type="button"
          @click="onNav(item.key)"
        >
          <span aria-hidden="true">{{ item.icon }}</span>
          {{ t(item.labelKey) }}
        </button>
      </nav>

      <!-- 全局仓库搜索（过滤 Explore 列表） -->
      <input
        v-model="searchQuery"
        class="ntb-search"
        type="search"
        :placeholder="t('nexhub.nav.searchPlaceholder')"
        :aria-label="t('nexhub.nav.searchPlaceholder')"
        @input="onSearchInput"
      />

      <div class="ntb-actions">
        <button
          class="btn btn-small"
          :disabled="ctx.reposLoading.value"
          :title="t('nexhub.common.refresh')"
          @click="ctx.refreshShared()"
        >
          <span class="spin" :class="{ spinning: ctx.reposLoading.value }" aria-hidden="true">↻</span>
        </button>
        <!-- 桌面浮窗模式：新标签页打开全屏版（既有外链行为） -->
        <button
          v-if="!ctx.standalone"
          class="btn btn-small btn-ext"
          type="button"
          :title="t('apps.openStandalone')"
          :aria-label="t('apps.openStandalone')"
          @click="openStandalone"
        >
          <svg class="ext-icon" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
            <polyline points="15 3 21 3 21 9" />
            <line x1="10" y1="14" x2="21" y2="3" />
          </svg>
        </button>
        <!-- standalone 全屏模式：回桌面入口（设计书 §3.5） -->
        <button
          v-else
          class="btn btn-small"
          type="button"
          :title="t('nexhub.nav.openDesktop')"
          @click="openDesktop"
        >
          {{ t('nexhub.nav.openDesktop') }}
        </button>
      </div>
    </div>
  </header>
</template>

<style scoped>
.ntb {
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  background: var(--bg-card, #fff);
  padding: 10px 20px;
}
.ntb-row { display: flex; align-items: center; gap: 14px; flex-wrap: wrap; }
.ntb-brand {
  display: inline-flex; align-items: center; gap: 7px; background: transparent; border: none;
  cursor: pointer; padding: 2px 4px; font-family: inherit;
}
.ntb-logo { color: var(--accent, #E95420); font-size: 17px; line-height: 1; }
.ntb-name { font-size: 17px; font-weight: 800; letter-spacing: -0.02em; color: var(--text, #2B2B2B); }

.ntb-nav { display: flex; align-items: center; gap: 2px; flex-wrap: wrap; }
.ntb-nav-item {
  padding: 6px 12px; background: transparent; border: none; border-radius: var(--radius-sm, 8px);
  font-size: 13.5px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: background 0.15s ease, color 0.15s ease;
}
.ntb-nav-item:hover { background: var(--border-soft, #F3F4F6); color: var(--text, #2B2B2B); }
.ntb-nav-item.active { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); font-weight: 600; }

.ntb-search {
  flex: 1; min-width: 180px; max-width: 420px; padding: 6px 12px;
  border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-pill, 20px);
  font-family: inherit; font-size: 13px; background: var(--bg-code, #fafafa);
  color: var(--text, #2B2B2B);
}
.ntb-search:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); background: var(--bg-card, #fff); }

.ntb-actions { display: flex; align-items: center; gap: 8px; margin-left: auto; }
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 6px 12px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-ext { padding: 4px 8px; }
.ext-icon { display: block; }
.spin { display: inline-block; }
.spin.spinning { animation: ntb-spin 1s linear infinite; }
@keyframes ntb-spin { to { transform: rotate(360deg); } }

/* 窄容器（桌面浮窗）：搜索收窄、导航换行 */
@media (max-width: 760px) {
  .ntb-search { order: 10; max-width: none; flex-basis: 100%; }
  .ntb-actions { margin-left: 0; }
}
</style>

