<script setup lang="ts">
// =============================================================================
// AppStore.vue —— 应用中心 / NexOS 官方应用商店
//
// 4 Tab：商店 / 已安装 / 安装任务 / 发布
// 后端：/api/v1/appstore/* （AppStoreRouteHandler）
//
// 来源策略（2026-08-23 需求）：只显示 NexOS 自己的应用（source === 'nexos'），
// 不显示 Ubuntu snap 源（也不显示 apt/deb/flatpak 渠道）的软件：
// - 商店列表双重过滤（后端 retain + 前端兜底 filter）
// - 安装类型仅 'nexos'（内置模块即时就绪，无外部包管理器）
// - 发布表单已移除 apt/deb/snap/flatpak 渠道入口
// - "已安装" Tab 仅做本机 flatpak 应用管理（探测/卸载），不属于上架渠道
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints, type InstalledApp as InstalledPkgApp } from '@/api/client';
import { removeAppRegistration } from '@/appRuntime';
import { useAppDeploy } from '@/composables/useAppDeploy';
import AppIcon from '@/components/AppIcon.vue';

/** 通用应用包盒子图标（与 nexhub/RepoCard 同款，lucide package 风）。 */
const PKG_ICON_INNER =
  '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/>' +
  '<path d="M3.3 7l8.7 5 8.7-5"/>' +
  '<path d="M7.5 4.27l9 5.15"/>' +
  '<path d="M12 22V12"/>';

/** manifest.icon 内联 SVG 净化（与 nexhub/RepoCard 同款 regex 纵深防御）。 */
function pkgSanitizeSvg(raw: string): string {
  return raw
    .replace(/<script\b[\s\S]*?<\/script\s*>/gi, '')
    .replace(/<\/?(?:iframe|object|embed|foreignObject)\b[^>]*>/gi, '')
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/\son[a-z]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/gi, '')
    .replace(/((?:xlink:)?href\s*=\s*)(?:"javascript:[^"]*"|'javascript:[^']*')/gi, '$1""');
}

const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
interface StoreApp {
  id?: string;
  name?: string;
  description?: string;
  category?: string;
  icon?: string;
  source?: string;
  install_type?: string;
  install_target?: string;
  publisher?: string;
  version?: string | null;
  rating?: number;
  downloads?: number;
  screenshot_urls?: string[];
  installed?: boolean;
  [k: string]: unknown;
}
interface CategoryInfo {
  id?: string;
  name?: string;
  count?: number;
  [k: string]: unknown;
}
interface InstallTask {
  id?: string;
  app_id?: string;
  app_name?: string;
  install_type?: string;
  status?: string;
  pid?: number | null;
  error?: string | null;
  log_tail?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface InstalledApp {
  name?: string;
  display_name?: string;
  version?: string;
  source?: string;
  [k: string]: unknown;
}
interface AppStoreStats {
  total_apps?: number;
  installed?: number;
  categories?: number;
  publishing_enabled?: boolean;
}

// =============================================================================
// 应用包（/api/v1/apps，docs/APPS.md）——NexHub 仓库安装 / 卸载 / 热注册
// =============================================================================
interface CatalogPkg {
  repo?: string;
  id?: string;
  name?: string;
  version?: string | null;
  category?: string;
  icon?: string;
  description?: string;
  installed?: boolean;
  installed_version?: string;
  [k: string]: unknown;
}

/** 应用包目录（GET /api/v1/apps/catalog）——状态与安装流程由 useAppDeploy 提供
 *  （v0.1.32 从本文件抽出，AppStore 与 NexHub DeployButton 共用同一链路）。 */
const {
  catalog: pkgCatalog,
  catalogLoading: pkgLoading,
  catalogError: pkgError,
  loadCatalog: loadPkgCatalog,
  installingRepo,
  install: deployPkg,
  stopPolling: stopPkgPolling,
} = useAppDeploy({
  onPending: (name) => {
    msg.value = {
      kind: 'info',
      text: t('apps.pkgInstalling') + t('apps.pkgInstallDone', { name }),
    };
  },
  onInstalled: async (name) => {
    msg.value = { kind: 'ok', text: t('apps.pkgInstallDone', { name }) };
    await loadPkgCatalog();
    await loadInstalledPkgs();
  },
  onRegisterFailed: async (name, e) => {
    // 装好了但注册失败：刷新后仍会注册（bootstrap 路径）。
    msg.value = {
      kind: 'info',
      text: t('apps.pkgInstallDone', { name }) + '（' + friendlyError(e) + '，刷新后生效）',
    };
    await loadPkgCatalog();
    await loadInstalledPkgs();
  },
  onFailed: (e) => {
    msg.value = { kind: 'err', text: t('apps.pkgInstallFailed', { err: friendlyError(e) }) };
  },
});

/** 安装应用包：清空消息后走公用部署链路（安装→轮询→热注册→刷新）。 */
async function installPkg(pkg: CatalogPkg): Promise<void> {
  msg.value = null;
  await deployPkg(pkg);
}

/** 正在卸载的应用包 id。 */
const uninstallingPkgId = ref('');

/** 已装应用包（「已安装」Tab 的应用包分组）。 */
const installedPkgs = ref<InstalledPkgApp[]>([]);
const installedPkgsLoading = ref(false);
const installedPkgsError = ref('');

async function loadInstalledPkgs(): Promise<void> {
  installedPkgsLoading.value = true;
  installedPkgsError.value = '';
  try {
    const raw = await endpoints.appsList();
    installedPkgs.value = Array.isArray(raw?.apps) ? (raw.apps as InstalledPkgApp[]) : [];
  } catch (e) {
    installedPkgs.value = [];
    installedPkgsError.value = friendlyError(e);
  } finally {
    installedPkgsLoading.value = false;
  }
}

const installedPkgColumns: Column<InstalledPkgApp>[] = [
  { key: 'name', title: '名称', sortable: true },
  { key: 'version', title: '版本', width: '110px' },
  { key: 'category', title: '类别', width: '110px' },
  { key: 'id', title: 'ID', width: '130px' },
  { key: 'installed_at', title: '安装时间', width: '180px' },
  { key: 'actions', title: '操作', width: '110px', align: 'right' },
];

/** 卸载应用包：确认 → DELETE → 本地注册表移除（窗口 / Dock / 路由 / 图标）。 */
async function uninstallPkg(row: InstalledPkgApp): Promise<void> {
  const id = String(row.id ?? '');
  const name = String(row.name ?? id);
  if (!id || uninstallingPkgId.value) return;
  if (!window.confirm(t('apps.pkgUninstallConfirm', { name }))) return;
  uninstallingPkgId.value = id;
  msg.value = null;
  try {
    await endpoints.appsUninstall(id);
    removeAppRegistration(id);
    msg.value = { kind: 'ok', text: t('apps.pkgUninstallDone', { name }) };
    await loadPkgCatalog();
    await loadInstalledPkgs();
  } catch (e) {
    msg.value = { kind: 'err', text: t('apps.pkgUninstallFailed', { err: friendlyError(e) }) };
  } finally {
    uninstallingPkgId.value = '';
  }
}

// （轮询定时器清理 stopPkgPolling 由 useAppDeploy 的 stopPolling 提供，见上方解构）

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'store' | 'installed' | 'tasks' | 'publish';
const activeTab = ref<TabKey>('store');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'store', label: '商店' },
  { key: 'installed', label: '已安装' },
  { key: 'tasks', label: '安装任务' },
  { key: 'publish', label: '发布' },
];

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// =============================================================================
// 统计
// =============================================================================
const stats = ref<AppStoreStats>({});
async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.appStoreStats();
    stats.value = (raw as AppStoreStats) ?? {};
  } catch {
    stats.value = {};
  }
}

// =============================================================================
// 工具：下载次数格式化（50000000 → 5000万）
// =============================================================================
function fmtDownloads(n?: number): string {
  const v = n ?? 0;
  if (v >= 100_000_000) return `${(v / 100_000_000).toFixed(1)}亿`;
  if (v >= 10_000) return `${(v / 10_000).toFixed(0)}万`;
  if (v >= 1000) return `${(v / 1000).toFixed(0)}千`;
  return `${v}`;
}

// =============================================================================
// Tab1：商店
// =============================================================================
const apps = ref<StoreApp[]>([]);
const categories = ref<CategoryInfo[]>([]);
const appsLoading = ref(false);
const appsError = ref('');
const selectedCategory = ref<string>(''); // 空表示全部
const search = ref('');
const busyAppId = ref<string>('');

async function loadCategories(): Promise<void> {
  try {
    const raw = await endpoints.appStoreCategories();
    categories.value = Array.isArray(raw) ? (raw as CategoryInfo[]) : [];
  } catch {
    categories.value = [];
  }
}

async function loadApps(): Promise<void> {
  appsLoading.value = true;
  appsError.value = '';
  try {
    const raw = await endpoints.appStoreApps(
      selectedCategory.value || undefined,
    );
    // 来源兜底过滤：仅显示 NexOS 官方应用（后端已过滤，前端再拦一道）
    apps.value = (Array.isArray(raw) ? (raw as StoreApp[]) : []).filter(
      (a) => (a.source ?? 'nexos') === 'nexos',
    );
  } catch (e) {
    apps.value = [];
    appsError.value = friendlyError(e);
  } finally {
    appsLoading.value = false;
  }
}

// 过滤后的应用列表（搜索框进一步过滤）
const filteredApps = computed<StoreApp[]>(() => {
  const q = search.value.trim().toLowerCase();
  if (!q) return apps.value;
  return apps.value.filter((a) => {
    const name = (a.name ?? '').toLowerCase();
    const desc = (a.description ?? '').toLowerCase();
    const target = (a.install_target ?? '').toLowerCase();
    return name.includes(q) || desc.includes(q) || target.includes(q);
  });
});

function selectCategory(c: string): void {
  selectedCategory.value = c;
  void loadApps();
}

async function installApp(app: StoreApp): Promise<void> {
  const id = String(app.id ?? '');
  if (!id) return;
  busyAppId.value = id;
  msg.value = null;
  try {
    await endpoints.appStoreInstall(id);
    msg.value = {
      kind: 'ok',
      text: `已创建安装任务：${app.name ?? id}（NexOS 内置模块，即时就绪）`,
    };
    activeTab.value = 'tasks';
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '安装失败：' + friendlyError(e) };
  } finally {
    busyAppId.value = '';
  }
}

// =============================================================================
// Tab2：已安装
// =============================================================================
const installedApps = ref<InstalledApp[]>([]);
const installedLoading = ref(false);
const installedError = ref('');
const uninstalling = ref<string>('');

async function loadInstalled(): Promise<void> {
  installedLoading.value = true;
  installedError.value = '';
  try {
    const raw = await endpoints.appStoreInstalled();
    installedApps.value = Array.isArray(raw) ? (raw as InstalledApp[]) : [];
  } catch (e) {
    installedApps.value = [];
    installedError.value = friendlyError(e);
  } finally {
    installedLoading.value = false;
  }
}

const installedColumns: Column<InstalledApp>[] = [
  { key: 'display_name', title: '名称', sortable: true },
  { key: 'version', title: '版本', width: '140px' },
  { key: 'source', title: '类型', width: '100px' },
  { key: 'actions', title: '操作', width: '120px', align: 'right' },
];

async function uninstallApp(row: InstalledApp): Promise<void> {
  const name = String(row.name ?? '');
  const source = String(row.source ?? 'flatpak');
  if (!name) return;
  if (!window.confirm(`确定卸载 ${name}？（flatpak uninstall）`)) return;
  uninstalling.value = name;
  msg.value = null;
  try {
    await endpoints.appStoreUninstall(name, source);
    msg.value = { kind: 'ok', text: `已创建卸载任务：${name}` };
    activeTab.value = 'tasks';
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '卸载失败：' + friendlyError(e) };
  } finally {
    uninstalling.value = '';
  }
}

// =============================================================================
// Tab3：安装任务
// =============================================================================
const tasks = ref<InstallTask[]>([]);
const tasksLoading = ref(false);
const tasksError = ref('');
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function loadTasks(): Promise<void> {
  tasksLoading.value = true;
  tasksError.value = '';
  try {
    const raw = await endpoints.appStoreTasks();
    tasks.value = Array.isArray(raw) ? (raw as InstallTask[]) : [];
  } catch (e) {
    tasks.value = [];
    tasksError.value = friendlyError(e);
  } finally {
    tasksLoading.value = false;
  }
}

// installing/pending 自动 3s 轮询刷新状态
function startPolling(): void {
  stopPolling();
  pollTimer = setInterval(async () => {
    const hasActive = tasks.value.some(
      (t) => t.status === 'installing' || t.status === 'pending',
    );
    await loadTasks();
    const stillActive = tasks.value.some(
      (t) => t.status === 'installing' || t.status === 'pending',
    );
    if (!hasActive && !stillActive) stopPolling();
  }, 3000);
}
function stopPolling(): void {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

const taskColumns: Column<InstallTask>[] = [
  { key: 'app_name', title: '应用', sortable: true },
  { key: 'install_type', title: '类型', width: '90px' },
  { key: 'status', title: '状态', width: '110px' },
  { key: 'pid', title: 'PID', width: '80px' },
  { key: 'created_at', title: '创建时间', width: '180px' },
  { key: 'actions', title: '操作', width: '100px', align: 'right' },
];

// 展开的任务（显示 log_tail）
const expandedTask = ref<string>('');

function toggleTaskLog(id: string): void {
  if (expandedTask.value === id) {
    expandedTask.value = '';
  } else {
    expandedTask.value = id;
  }
}

async function refreshTaskDetail(id: string): Promise<void> {
  try {
    const raw = await endpoints.appStoreTaskDetail(id);
    const detail = raw as InstallTask;
    const idx = tasks.value.findIndex((t) => t.id === id);
    if (idx >= 0 && detail) {
      tasks.value[idx] = detail;
    }
  } catch {
    // 忽略
  }
}

// =============================================================================
// Tab4：发布
// =============================================================================
const publishForm = ref({
  name: '',
  description: '',
  category: 'custom',
  // 仅 NexOS 渠道（apt/deb/snap/flatpak 渠道入口已移除）
  install_type: 'nexos',
  install_target: '',
});
const publishing = ref(false);

const categoryOptions = [
  { value: 'custom', label: '自定义' },
  { value: 'media', label: '媒体' },
  { value: 'dev', label: '开发' },
  { value: 'office', label: '办公' },
  { value: 'internet', label: '网络' },
  { value: 'system', label: '系统' },
];

async function submitPublish(): Promise<void> {
  if (!publishForm.value.name.trim()) {
    msg.value = { kind: 'err', text: '名称不可为空' };
    return;
  }
  if (!publishForm.value.install_target.trim()) {
    msg.value = { kind: 'err', text: '安装目标不可为空' };
    return;
  }
  publishing.value = true;
  msg.value = null;
  try {
    await endpoints.appStorePublish({
      name: publishForm.value.name.trim(),
      description: publishForm.value.description.trim(),
      category: publishForm.value.category,
      install_type: publishForm.value.install_type,
      install_target: publishForm.value.install_target.trim(),
    });
    msg.value = { kind: 'ok', text: '已发布，可在商店 Tab 的对应分类下查看' };
    // 重置表单（保留分类/类型选择）
    publishForm.value.name = '';
    publishForm.value.description = '';
    publishForm.value.install_target = '';
    // 刷新商店
    await loadApps();
    await loadCategories();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: '发布失败：' + friendlyError(e) };
  } finally {
    publishing.value = false;
  }
}

// =============================================================================
// 徽章映射
// =============================================================================
function statusClass(s?: string): string {
  switch (s) {
    case 'installing':
    case 'pending':
      return 'pill-blue';
    case 'completed':
      return 'pill-ok';
    case 'failed':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'installing':
      return '安装中';
    case 'pending':
      return '等待';
    case 'completed':
      return '已完成';
    case 'failed':
      return '失败';
    default:
      return s ?? '—';
  }
}

function installTypeBadge(t?: string): { cls: string; label: string } {
  switch (t) {
    case 'nexos':
      return { cls: 'pill-purple', label: 'NexOS' };
    // 历史任务兜底显示（商店不再提供 apt/deb/snap/flatpak 渠道）
    default:
      return { cls: 'pill-muted', label: t ?? '—' };
  }
}

function sourceBadge(s?: string): { cls: string; label: string } {
  switch (s) {
    // "已安装"页仅探测本机 flatpak 应用（管理用途，非商店渠道）
    case 'flatpak':
      return { cls: 'pill-ok', label: 'flatpak' };
    default:
      return { cls: 'pill-muted', label: s ?? '—' };
  }
}

// 评分星：返回 ★ 字符串（满 5）
function ratingStars(r?: number): string {
  const v = Math.round(r ?? 0);
  return '★★★★★'.slice(0, Math.max(0, Math.min(5, v))) || '—';
}

const hasActiveTasks = computed(() =>
  tasks.value.some((t) => t.status === 'installing' || t.status === 'pending'),
);

// =============================================================================
// 刷新与初始化
// =============================================================================
async function refreshAll(): Promise<void> {
  await Promise.all([
    loadApps(),
    loadCategories(),
    loadInstalled(),
    loadTasks(),
    loadStats(),
    loadPkgCatalog(),
    loadInstalledPkgs(),
  ]);
  if (hasActiveTasks.value) startPolling();
}

onMounted(() => {
  void refreshAll();
});

onBeforeUnmount(() => {
  stopPolling();
  stopPkgPolling();
});
</script>

<template>
  <div class="as-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">应用中心</h2>
        <div class="page-sub muted">仅 NexOS 官方应用 · 浏览 · 安装 · 发布 · 管理</div>
      </div>
      <div class="head-actions">
        <button
          class="btn btn-small"
          :disabled="appsLoading || tasksLoading"
          @click="refreshAll"
        >
          <span
            class="spin"
            :class="{ spinning: appsLoading || tasksLoading }"
            aria-hidden="true"
          >↻</span>
          刷新
        </button>
      </div>
    </div>

    <!-- Tab 切换 -->
    <nav class="tabs" role="tablist">
      <button
        v-for="t in tabs"
        :key="t.key"
        class="tab"
        :class="{ active: activeTab === t.key }"
        role="tab"
        :aria-selected="activeTab === t.key"
        @click="activeTab = t.key"
      >{{ t.label }}</button>
    </nav>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== Tab1 商店 =================== -->
    <section v-show="activeTab === 'store'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">应用总数</div>
          <div class="stat-value">{{ stats.total_apps ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已安装</div>
          <div class="stat-value">{{ stats.installed ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">分类</div>
          <div class="stat-value">{{ stats.categories ?? 0 }}</div>
        </div>
      </section>

      <!-- NexOS 应用包（/api/v1/apps/catalog，NexHub 仓库） -->
      <section class="card pkg-section">
        <div class="pkg-head">
          <div>
            <div class="pkg-title">{{ t('apps.pkgGroupTitle') }}</div>
            <div class="muted small">{{ t('apps.pkgGroupSub') }}</div>
          </div>
          <span class="pill pill-purple">{{ pkgCatalog.length }}</span>
        </div>
        <div v-if="pkgError" class="error-box">
          {{ t('apps.pkgLoadError') }}{{ pkgError }}
          <button class="btn btn-small" @click="loadPkgCatalog">{{ t('apps.retry') }}</button>
        </div>
        <div v-else-if="pkgLoading && !pkgCatalog.length" class="muted small pkg-empty">
          …
        </div>
        <div v-else-if="!pkgCatalog.length" class="muted small pkg-empty">
          {{ t('apps.pkgEmpty') }}
        </div>
        <div v-else class="pkg-grid">
          <div v-for="p in pkgCatalog" :key="p.repo ?? p.id" class="pkg-card">
            <div class="pkg-card-head">
              <span
                v-if="p.icon && p.icon.includes('<')"
                class="pkg-icon"
                aria-hidden="true"
              ><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" v-html="pkgSanitizeSvg(p.icon)"></svg></span>
              <AppIcon v-else :name="p.icon || ''" :size="18" :fallback="PKG_ICON_INNER" />
              <div class="pkg-card-title">
                <span class="pkg-name">{{ p.name ?? p.id }}</span>
                <span class="muted small">
                  {{ t('apps.pkgVersion') }} {{ p.version ?? '—' }}
                  <template v-if="p.installed && p.installed_version && p.installed_version !== p.version">
                    · {{ t('apps.pkgInstalledVersion', { v: p.installed_version }) }}
                  </template>
                </span>
              </div>
              <span v-if="p.installed" class="pill pill-ok">{{ t('apps.pkgInstalled') }}</span>
            </div>
            <p class="pkg-desc">{{ p.description ?? '' }}</p>
            <div class="pkg-card-foot">
              <span class="muted small">
                {{ t('apps.pkgRepo') }} <code class="mono">{{ p.repo ?? '—' }}</code>
                · {{ t('apps.pkgCategory') }} {{ p.category ?? '—' }}
              </span>
              <button
                v-if="!p.installed"
                class="btn btn-small btn-primary"
                :disabled="installingRepo !== ''"
                @click.stop="installPkg(p)"
              >
                {{ installingRepo === p.repo ? t('apps.pkgInstalling') : t('apps.pkgInstall') }}
              </button>
            </div>
          </div>
        </div>
      </section>

      <div class="store-layout">
        <!-- 分类侧栏 -->
        <aside class="cat-sidebar card">
          <button
            class="cat-item"
            :class="{ active: selectedCategory === '' }"
            type="button"
            @click="selectCategory('')"
          >
            <span class="cat-name">全部</span>
            <span class="cat-count">{{ stats.total_apps ?? 0 }}</span>
          </button>
          <button
            v-for="c in categories"
            :key="c.id"
            class="cat-item"
            :class="{ active: selectedCategory === c.id }"
            type="button"
            @click="selectCategory(String(c.id ?? ''))"
          >
            <span class="cat-name">{{ c.name ?? c.id }}</span>
            <span class="cat-count">{{ c.count ?? 0 }}</span>
          </button>
        </aside>

        <!-- 应用卡片网格 -->
        <div class="store-main">
          <div class="store-toolbar">
            <input
              v-model="search"
              class="search-input"
              type="text"
              placeholder="搜索应用名称 / 描述 / 模块名…"
            />
            <span class="muted small">{{ filteredApps.length }} 个应用</span>
          </div>

          <div v-if="appsError" class="error-box">{{ appsError }}</div>
          <div v-if="appsLoading && !apps.length" class="card empty-card">加载中…</div>
          <div v-else-if="!filteredApps.length" class="card empty-card">
            未找到匹配的应用，去<a class="link" @click="activeTab = 'publish'">发布页</a>添加。
          </div>
          <div v-else class="app-grid">
            <div v-for="a in filteredApps" :key="a.id" class="card app-card">
              <div class="app-card-head">
                <span class="app-icon-emoji">{{ a.icon || '📦' }}</span>
                <div class="app-card-title">
                  <span class="app-name">{{ a.name ?? '—' }}</span>
                  <span class="app-publisher muted small">{{ a.publisher ?? '' }}</span>
                </div>
                <span class="pill" :class="installTypeBadge(a.install_type).cls">
                  {{ installTypeBadge(a.install_type).label }}
                </span>
              </div>
              <p class="app-desc">{{ a.description ?? '' }}</p>
              <div class="app-meta">
                <span class="app-stars" :title="`评分 ${a.rating ?? 0}`">{{ ratingStars(a.rating) }}</span>
                <span class="muted small">{{ fmtDownloads(a.downloads) }} 次下载</span>
              </div>
              <div class="app-target-row">
                <span class="muted small">目标</span>
                <code class="mono app-target">{{ a.install_target ?? '—' }}</code>
              </div>
              <div class="app-card-actions">
                <span v-if="a.installed" class="pill pill-ok">已安装 · 内置</span>
                <button
                  v-else
                  class="btn btn-small btn-primary"
                  :disabled="busyAppId === a.id"
                  @click.stop="installApp(a)"
                >
                  {{ busyAppId === a.id ? '创建中…' : '安装' }}
                </button>
              </div>
            </div>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 已安装 =================== -->
    <section v-show="activeTab === 'installed'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">已安装应用（flatpak 探测）</span>
      </div>
      <div v-if="installedError" class="error-box">{{ installedError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="installedColumns"
            :rows="installedApps"
            :loading="installedLoading"
            empty-text="暂无已安装的 flatpak 应用，或 flatpak 不可用。"
          >
            <template #cell-source="{ row }">
              <span class="pill" :class="sourceBadge(row.source).cls">
                {{ sourceBadge(row.source).label }}
              </span>
            </template>
            <template #cell-version="{ row }">
              <span class="mono small">{{ row.version || '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                :disabled="uninstalling === row.name"
                @click.stop="uninstallApp(row)"
              >卸载</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 应用包分组（/api/v1/apps，NexHub 仓库安装） -->
      <div class="panel-head pkg-installed-head">
        <span class="panel-title">{{ t('apps.pkgInstalledGroup') }}</span>
        <button class="btn btn-small" :disabled="installedPkgsLoading" @click="loadInstalledPkgs">
          <span class="spin" :class="{ spinning: installedPkgsLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div v-if="installedPkgsError" class="error-box">{{ installedPkgsError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="installedPkgColumns"
            :rows="installedPkgs"
            :loading="installedPkgsLoading"
            :empty-text="t('apps.pkgEmpty')"
          >
            <template #cell-version="{ row }">
              <span class="mono small">{{ row.version || '—' }}</span>
            </template>
            <template #cell-id="{ row }">
              <span class="mono small">{{ row.id }}</span>
            </template>
            <template #cell-installed_at="{ row }">
              <span class="mono small">{{ row.installed_at ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                :disabled="uninstallingPkgId === row.id"
                @click.stop="uninstallPkg(row)"
              >{{ t('apps.pkgUninstall') }}</button>
            </template>
          </DataTable>
        </div>
      </div>
    </section>

    <!-- =================== Tab3 安装任务 =================== -->
    <section v-show="activeTab === 'tasks'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">安装任务</span>
        <button class="btn btn-small" :disabled="tasksLoading" @click="loadTasks">
          <span class="spin" :class="{ spinning: tasksLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>

      <div v-if="tasksError" class="error-box">{{ tasksError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="taskColumns"
            :rows="tasks"
            :loading="tasksLoading"
            empty-text="暂无安装任务，去商店 Tab 安装应用。"
          >
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-install_type="{ row }">
              <span class="pill" :class="installTypeBadge(row.install_type).cls">
                {{ installTypeBadge(row.install_type).label }}
              </span>
            </template>
            <template #cell-pid="{ row }">
              <span class="mono">{{ row.pid ?? '—' }}</span>
            </template>
            <template #cell-created_at="{ row }">
              <span class="mono small">{{ row.created_at ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                @click.stop="toggleTaskLog(String(row.id ?? ''))"
              >
                {{ expandedTask === row.id ? '收起' : '日志' }}
              </button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 展开的任务日志 -->
      <div v-if="expandedTask" class="card task-detail">
        <div class="task-detail-head">
          <span class="panel-title">任务日志 / 错误（{{ expandedTask }}）</span>
          <button
            class="btn btn-small"
            @click="refreshTaskDetail(expandedTask)"
          >刷新</button>
        </div>
        <template v-for="t in tasks" :key="t.id">
          <template v-if="t.id === expandedTask">
            <div v-if="t.error" class="task-error">错误：{{ t.error }}</div>
            <pre v-if="t.log_tail" class="task-log">{{ t.log_tail }}</pre>
            <p v-if="!t.error && !t.log_tail" class="muted small">暂无日志输出。</p>
          </template>
        </template>
      </div>

      <p v-if="hasActiveTasks" class="muted small">
        检测到安装中任务，状态每 3 秒自动刷新。
      </p>
    </section>

    <!-- =================== Tab4 发布 =================== -->
    <section v-show="activeTab === 'publish'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">发布新应用</span>
      </div>
      <div class="card publish-card">
        <form class="publish-form" @submit.prevent="submitPublish">
          <div class="field">
            <label for="as-name">名称 *</label>
            <input
              id="as-name"
              v-model="publishForm.name"
              type="text"
              placeholder="如 My Custom App"
              :disabled="publishing"
            />
          </div>
          <div class="field">
            <label for="as-desc">简介</label>
            <input
              id="as-desc"
              v-model="publishForm.description"
              type="text"
              placeholder="一句话简介"
              :disabled="publishing"
            />
          </div>
          <div class="field-row">
            <div class="field">
              <label for="as-cat">分类</label>
              <select id="as-cat" v-model="publishForm.category" :disabled="publishing">
                <option v-for="o in categoryOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
              </select>
            </div>
            <div class="field">
              <label>安装类型</label>
              <div class="fixed-type">
                <span class="pill pill-purple">NexOS</span>
                <span class="muted small">内置模块（唯一渠道）</span>
              </div>
            </div>
          </div>
          <div class="field">
            <label for="as-target">安装目标 *</label>
            <input
              id="as-target"
              v-model="publishForm.install_target"
              type="text"
              placeholder="NexOS 模块名（如 nexos-chat）"
              :disabled="publishing"
            />
          </div>
          <p class="muted small">
            发布的应用将出现在商店 Tab 的对应分类下（默认自定义）。应用中心仅支持
            NexOS 原生应用（内置模块）：安装任务即时就绪，不经
            <code class="mono">apt</code> / <code class="mono">snap</code> /
            <code class="mono">flatpak</code> 等外部渠道。
          </p>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="submit" class="btn btn-primary" :disabled="publishing">
              {{ publishing ? '发布中…' : '发布应用' }}
            </button>
          </div>
        </form>
      </div>
    </section>
  </div>
</template>

<style scoped>
.as-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.link { color: var(--accent, #E95420); cursor: pointer; text-decoration: underline; }

/* Tabs */
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover { color: var(--text, #2B2B2B); }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }

.tab-panel { display: flex; flex-direction: column; gap: 14px; }

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* 商店布局：侧栏 + 主区 */
.store-layout { display: grid; grid-template-columns: 200px 1fr; gap: 14px; align-items: start; }
.cat-sidebar { padding: 8px; display: flex; flex-direction: column; gap: 2px; position: sticky; top: 0; }
.cat-item {
  display: flex; justify-content: space-between; align-items: center; gap: 8px;
  padding: 8px 12px; background: transparent; border: none; border-radius: var(--radius-sm, 8px);
  font-size: 13px; color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit;
  transition: background 0.15s ease;
}
.cat-item:hover { background: rgba(0, 0, 0, 0.04); }
.cat-item.active { background: rgba(233, 84, 32, 0.1); color: var(--accent, #E95420); font-weight: 600; }
.cat-name { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.cat-count {
  display: inline-block; min-width: 22px; padding: 1px 6px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; font-weight: 600; color: var(--text-muted, #5E5C5F);
  background: var(--border-soft, #F3F4F6); text-align: center;
}
.cat-item.active .cat-count { background: rgba(233, 84, 32, 0.18); color: var(--accent, #E95420); }

.store-main { display: flex; flex-direction: column; gap: 12px; min-width: 0; }
.store-toolbar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.search-input {
  flex: 1; min-width: 200px; padding: 7px 12px;
  border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }

/* 应用卡片网格 */
.app-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 14px; }
.app-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.app-card-head { display: flex; align-items: center; gap: 10px; }
.app-icon-emoji { font-size: 28px; line-height: 1; }
.app-card-title { display: flex; flex-direction: column; gap: 2px; flex: 1; min-width: 0; }
.app-name { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); word-break: break-word; }
.app-publisher { font-size: 11px; }
.app-desc { margin: 0; font-size: 13px; line-height: 1.5; color: var(--text, #2B2B2B); min-height: 20px; }
.app-meta { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.app-stars { color: #f59e0b; font-size: 13px; letter-spacing: 1px; }
.app-target-row { display: flex; align-items: center; gap: 8px; font-size: 12px; }
.app-target { background: var(--border-soft, #F3F4F6); padding: 1px 6px; border-radius: 4px; font-size: 11px; word-break: break-all; }
.app-card-actions { display: flex; justify-content: flex-end; margin-top: 4px; }

/* —— 应用包（NexHub 仓库）卡片 —— */
.pkg-section { padding: 14px 16px; display: flex; flex-direction: column; gap: 12px; }
.pkg-head { display: flex; align-items: center; justify-content: space-between; gap: 10px; }
.pkg-title { font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); }
.pkg-empty { padding: 6px 2px; }
.pkg-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 12px; }
.pkg-card {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 10px);
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  background: var(--bg-card, #fff);
}
.pkg-card-head { display: flex; align-items: center; gap: 10px; }
.pkg-icon { font-size: 24px; line-height: 1; display: inline-flex; flex-shrink: 0; }
.pkg-icon svg { display: block; width: 20px; height: 20px; }
.pkg-card-title { display: flex; flex-direction: column; gap: 2px; flex: 1; min-width: 0; }
.pkg-name { font-size: 14.5px; font-weight: 600; color: var(--text, #2B2B2B); word-break: break-word; }
.pkg-desc { margin: 0; font-size: 12.5px; line-height: 1.5; color: var(--text, #2B2B2B); min-height: 19px; }
.pkg-card-foot {
  display: flex; align-items: center; justify-content: space-between;
  gap: 8px; flex-wrap: wrap; border-top: 1px solid var(--border-soft, #EDEDED); padding-top: 8px;
}
.pkg-installed-head { margin-top: 6px; }

/* 任务日志 */
.task-detail { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.task-detail-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.task-error { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 8px 12px; border-radius: var(--radius-sm, 8px); font-size: 13px; }
.task-log {
  margin: 0; padding: 10px 12px; background: #1e1e1e; color: #e5e5e5;
  border-radius: var(--radius-sm, 8px); font-size: 12px; line-height: 1.5;
  font-family: var(--mono, monospace); white-space: pre-wrap; word-break: break-all; max-height: 320px; overflow: auto;
}

/* 发布表单 */
.publish-card { padding: 18px 20px; }
.publish-form { display: flex; flex-direction: column; gap: 14px; }
.field-row { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
}
.field input:focus, .field select:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.fixed-type { display: flex; align-items: center; gap: 8px; min-height: 34px; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-purple { color: #7c3aed; background: #ede9fe; }
.pill-cyan { color: #0e7490; background: #cffafe; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }

.btn {
  padding: 6px 14px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
  transition: background 0.15s ease;
}
.btn:hover:not(:disabled) { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
.btn + .btn { margin-left: 6px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.mono { font-family: var(--mono, monospace); }
code.mono { background: var(--border-soft, #F3F4F6); padding: 1px 4px; border-radius: 4px; font-size: 12px; }

@media (max-width: 720px) {
  .store-layout { grid-template-columns: 1fr; }
  .cat-sidebar { position: static; flex-direction: row; overflow-x: auto; }
  .cat-item { flex-shrink: 0; }
  .field-row { grid-template-columns: 1fr; }
}
</style>
