<script setup lang="ts">
// =============================================================================
// CodeHub.vue —— NexHub 代码仓库中心（壳组件，v0.1.32 网页化重排）。
//
// 职责（P0 拆分后仅剩）：双模式判定 / 共享状态（repos·stats·catalog·lobby /
// 提示条）/ 一键部署流程（useAppDeploy）/ 顶层导航。业务视图拆至
// views/nexhub/ 子组件：
//   Explore 仓库列表（默认视图）· LobbyPage 大厅 · SessionsPage AI 会话 ·
//   OnboardingPage 接入指南 · RepoDetailPage 仓库详情（Code/Commits/Manifest/
//   Issues/PR 内嵌 Tab）。
//
// 双模式（同一组件树自适应）：
//   - standalone 全屏（/s/codehub 子路由）：内容经 <RouterView> 渲染 children，
//     仓库 / Tab 状态进 URL（深链 / 刷新不丢）；顶栏显示「打开桌面」；
//   - 桌面浮窗（/?app=codehub → WindowFrame，appRegistry 复用本组件）：内部
//     currentView/selectedRepo 状态切换（window.history 属宿主页，不做 URL
//     同步）；顶栏显示「新标签全屏打开」外链（既有行为）。
// =============================================================================
import { onMounted, provide, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import type { CiRun } from '@/api/client';
import { useAppDeploy } from '@/composables/useAppDeploy';
import {
  nexhubContextKey,
  type HubView,
  type NexhubContext,
  type RepoDetailTab,
} from '@/views/nexhub/context';
import {
  errMsg,
  type HubMsg,
  type LobbyEntry,
  type LobbyStats,
  type Repo,
  type Stats,
} from '@/views/nexhub/model';
import NexhubTopBar from '@/views/nexhub/components/NexhubTopBar.vue';
import RepoExplorePage from '@/views/nexhub/views/RepoExplorePage.vue';
import RepoDetailPage from '@/views/nexhub/views/RepoDetailPage.vue';
import LobbyPage from '@/views/nexhub/views/LobbyPage.vue';
import SessionsPage from '@/views/nexhub/views/SessionsPage.vue';
import OnboardingPage from '@/views/nexhub/views/OnboardingPage.vue';

const { t } = useI18n();
const route = useRoute();
// router 实例（standalone 子路由深链用；桌面浮窗导航走内部状态，传 null）
const routerInstance = useRouter();

/** 独立全屏模式标记（当前路径已在 /s/ 下）——该模式经子路由深链。 */
const standalone = window.location.pathname.startsWith('/s/');

// =============================================================================
// 共享状态（provide 给 views/nexhub/* 子组件）
// =============================================================================
const stats = ref<Stats>({});
const repos = ref<Repo[]>([]);
const reposLoading = ref(false);
// catalog 单一数据源 = deploy.catalog（useAppDeploy 内部 ref）。v0.1.33 修复：
// 壳原自有 catalog ref 与 deploy.catalog 双份状态，refreshShared 只填壳份，
// DeployButton / RepoCard / ManifestTab 读 deploy.catalogEntry() 恒空 →
// 应用仓库卡上按钮与版本徽章静默不渲染。现 ctx 直连 deploy，消除双份。
const lobbyEntries = ref<LobbyEntry[]>([]);
const lobbyStats = ref<LobbyStats>({});
const lobbyLoading = ref(false);
// 内置 CI 各仓最新 run 摘要（repo_name → run；Explore 仓库卡 CI 徽章数据源）
const ciLatest = ref<Record<string, CiRun>>({});
const msg = ref<HubMsg | null>(null);
const actionLoading = ref(false);
/** Explore 仓库列表搜索词（顶栏全局搜索共享）。 */
const exploreQuery = ref('');

function showMsg(kind: HubMsg['kind'], text: string): void {
  msg.value = { kind, text };
}
function clearMsg(): void {
  msg.value = null;
}

async function loadStats(): Promise<void> {
  try {
    stats.value = (await endpoints.codeRepoStats()) as Stats;
  } catch (e) {
    console.warn('load stats failed', e);
  }
}

async function loadRepos(): Promise<void> {
  reposLoading.value = true;
  try {
    const r = (await endpoints.codeRepoRepos()) as { repos?: Repo[] };
    repos.value = r.repos ?? [];
  } catch (e) {
    showMsg('error', `${t('nexhub.shell.reposLoadFailed')}: ${errMsg(e)}`);
    repos.value = [];
  } finally {
    reposLoading.value = false;
  }
}

/** 大厅列表拉取（q/tag/sort 服务端参数；刷新后详情缓存语义由 LobbyPage 维护）。 */
async function loadLobby(opts?: { q?: string; tag?: string; sort?: 'recent' | 'downloads' }): Promise<void> {
  lobbyLoading.value = true;
  try {
    const [list, s] = await Promise.all([
      endpoints.nexhubLobbyList({
        q: opts?.q || undefined,
        tag: opts?.tag || undefined,
        sort: opts?.sort ?? 'recent',
      }),
      endpoints.nexhubLobbyStats(),
    ]);
    lobbyEntries.value = Array.isArray(list) ? (list as LobbyEntry[]) : [];
    lobbyStats.value = (s as LobbyStats) ?? {};
  } catch (e) {
    showMsg('error', `${t('nexhub.shell.lobbyLoadFailed')}: ${errMsg(e)}`);
    lobbyEntries.value = [];
  } finally {
    lobbyLoading.value = false;
  }
}

/** 内置 CI 各仓最新 run 摘要拉取（Explore 一次拉全；失败静默——徽章只是辅助态）。 */
async function loadCiLatest(): Promise<void> {
  try {
    const r = (await endpoints.codeRepoCiLatest()) as { latest?: CiRun[] };
    const map: Record<string, CiRun> = {};
    for (const run of r.latest ?? []) {
      if (run.repo_name) map[run.repo_name] = run;
    }
    ciLatest.value = map;
  } catch {
    ciLatest.value = {};
  }
}

/** 刷新共享数据（原 refreshAll 主体；大厅带默认参数）。 */
async function refreshShared(): Promise<void> {
  clearMsg();
  await Promise.all([loadStats(), loadRepos(), deploy.loadCatalog(), loadLobby(), loadCiLatest()]);
}

// =============================================================================
// 应用一键部署（useAppDeploy 单例：AppStore 同链路；结果反馈进全局提示条）
// =============================================================================
const deploy = useAppDeploy({
  onNoop: () => showMsg('info', t('nexhub.deploy.alreadyLatest')),
  onPending: () => showMsg('info', t('nexhub.deploy.pending')),
  onInstalled: async (_name, app) => {
    // 引擎门控联动提示（film/streaming/qrtransfer 装机即解锁）
    const engine = String(
      deploy.catalog.value.find((c) => c.id === app.id)?.engine ?? '',
    );
    const engineSuffix = engine ? ` ${t('nexhub.deploy.engineUnlocked', { engine })}` : '';
    showMsg('ok', `${t('nexhub.deploy.done', { name: app.name, v: app.version })} ${t('nexhub.deploy.hotRegistered')}${engineSuffix}`);
    await deploy.loadCatalog();
  },
  onRegisterFailed: async (_name, err) => {
    showMsg('info', t('nexhub.deploy.registerFailed', { err: errMsg(err) }));
    await deploy.loadCatalog();
  },
  onFailed: (err) => showMsg('error', t('nexhub.deploy.failed', { err: errMsg(err) })),
});

// =============================================================================
// 顶层导航（双模式适配：standalone → 子路由；桌面 → 内部状态）
// =============================================================================
const currentView = ref<HubView>('explore');
/** 桌面模式当前查看的仓库（详情页）。 */
const selectedRepo = ref('');

const STANDALONE_BASE = '/s/codehub';

/** standalone：路由 → 视图镜像（TopBar 激活态单一数据源）。 */
function syncViewFromRoute(): void {
  const path = route.path;
  if (path === STANDALONE_BASE || path === `${STANDALONE_BASE}/`) {
    currentView.value = 'explore';
    selectedRepo.value = '';
  } else if (path.startsWith(`${STANDALONE_BASE}/lobby`)) {
    currentView.value = 'lobby';
  } else if (path.startsWith(`${STANDALONE_BASE}/sessions`)) {
    currentView.value = 'sessions';
  } else if (path.startsWith(`${STANDALONE_BASE}/onboarding`)) {
    currentView.value = 'onboarding';
  } else if (path.startsWith(`${STANDALONE_BASE}/r/`)) {
    currentView.value = 'repo';
    selectedRepo.value = decodeURIComponent(path.slice(STANDALONE_BASE.length + 3));
  }
}

if (standalone) {
  watch(() => route.path, () => syncViewFromRoute(), { immediate: true });
}

function goView(view: Exclude<HubView, 'repo'>): void {
  if (standalone) {
    const target =
      view === 'explore' ? STANDALONE_BASE : `${STANDALONE_BASE}/${view}`;
    void routerInstance.push(target).catch(() => undefined);
    return;
  }
  currentView.value = view;
}

function openRepo(name: string, tab: RepoDetailTab = 'code'): void {
  if (!name) return;
  if (standalone) {
    void routerInstance
      .push({
        path: `${STANDALONE_BASE}/r/${encodeURIComponent(name)}`,
        query: tab === 'code' ? {} : { tab },
      })
      .catch(() => undefined);
    return;
  }
  currentView.value = 'repo';
  selectedRepo.value = name;
}

function goExplore(): void {
  goView('explore');
}

// =============================================================================
// 共享上下文（views/nexhub/* 子组件 inject 消费；接口见 nexhub/context.ts）
// =============================================================================
const ctx: NexhubContext = {
  standalone,
  router: standalone ? routerInstance : null,
  stats,
  repos,
  reposLoading,
  catalog: deploy.catalog,
  catalogLoading: deploy.catalogLoading,
  ciLatest,
  lobbyEntries,
  lobbyStats,
  lobbyLoading,
  loadRepos,
  loadStats,
  loadCatalog: () => deploy.loadCatalog(),
  loadCiLatest,
  loadLobby,
  refreshShared,
  msg,
  showMsg,
  clearMsg,
  actionLoading,
  exploreQuery,
  deploy,
  currentView,
  openRepo,
  goExplore,
  goView,
};
provide(nexhubContextKey, ctx);

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  void refreshShared();
});
</script>

<template>
  <div class="ch-page" :class="{ 'is-standalone': standalone }">
    <NexhubTopBar />

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- standalone 全屏：children 子路由（深链 / 刷新不丢） -->
    <RouterView v-if="standalone" class="ch-outlet" />

    <!-- 桌面浮窗：内部状态切换（与拆分前 v-show 常驻渲染等价，数据保活） -->
    <template v-else>
      <div v-show="currentView === 'explore'" class="ch-outlet">
        <RepoExplorePage />
      </div>
      <div v-if="currentView === 'repo'" class="ch-outlet" :key="selectedRepo">
        <RepoDetailPage :name="selectedRepo" />
      </div>
      <div v-show="currentView === 'lobby'" class="ch-outlet">
        <LobbyPage />
      </div>
      <div v-show="currentView === 'sessions'" class="ch-outlet">
        <SessionsPage />
      </div>
      <div v-show="currentView === 'onboarding'" class="ch-outlet">
        <OnboardingPage />
      </div>
    </template>
  </div>
</template>

<style scoped>
.ch-page {
  padding: 0 0 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-height: 100%;
}
/* 桌面浮窗：内容区内边距（standalone 顶栏自带，浮窗由窗口提供外框） */
.ch-page:not(.is-standalone) {
  padding: 0 0 20px;
}
.ch-page:not(.is-standalone) :deep(.ntb) {
  padding: 10px 20px 0;
  border-bottom: none;
}
.ch-outlet {
  padding: 0 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/* 全局提示条（原 form-msg） */
.form-msg { padding: 8px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }
.form-msg.is-info { background: #eff6ff; color: #1e40af; border: 1px solid rgba(30, 64, 175, 0.2); }
.form-msg.is-error { background: #fee2e2; color: #b91c1c; border: 1px solid rgba(185, 28, 28, 0.2); }
.form-msg.is-ok { background: #dcfce7; color: #166534; border: 1px solid rgba(22, 101, 52, 0.2); }
</style>
