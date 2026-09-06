// =============================================================================
// nexhub/context.ts —— CodeHub 壳 ↔ 子页面 共享状态契约（v0.1.32 拆分）。
//
// 壳（CodeHub.vue）持有共享数据（repos / stats / catalog / 提示条 / 安装流程）
// 并 provide 本上下文；子页面（views/nexhub/views/*）inject 消费。
//
// 双模式导航（P1 重排核心，接口一次定死）：
//   - standalone 全屏（/s/codehub 子路由）：openRepo / goExplore 等走 router.push，
//     仓库 / Tab 状态进 URL，刷新不丢（深链）；
//   - 桌面浮窗（/?app=codehub → WindowFrame）：走壳内 currentView / selectedRepo
//     内部状态（window.history 属宿主页，不做 URL 同步）。
// 子页面不感知模式差异——只调本上下文的导航方法。
// =============================================================================
import { inject, type InjectionKey, type Ref } from 'vue';
import type { Router } from 'vue-router';
import type { CatalogApp, CiRun } from '@/api/client';
import type { UseAppDeployReturn } from '@/composables/useAppDeploy';
import type { HubMsg, LobbyEntry, LobbyStats, Repo, Stats } from '@/views/nexhub/model';

/** 仓库详情页内嵌 Tab（Code/Commits/Manifest/CI + 协作层 Issues/PR）。 */
export type RepoDetailTab = 'code' | 'commits' | 'manifest' | 'ci' | 'issues' | 'pulls';

/** 顶层视图（桌面窗口模式的内部状态；standalone 模式对应 children 路由）。 */
export type HubView = 'explore' | 'lobby' | 'sessions' | 'onboarding' | 'repo';

/** 共享上下文（壳 provide，子页面 inject）。 */
export interface NexhubContext {
  /** standalone 全屏模式（/s/ 前缀路径）——导航走 router，其余走内部状态。 */
  standalone: boolean;
  /** vue-router 实例（standalone 子页面深链 / 返回；桌面模式为 null）。 */
  router: Router | null;

  // —— 共享数据（壳加载）——
  stats: Ref<Stats>;
  repos: Ref<Repo[]>;
  reposLoading: Ref<boolean>;
  /** 应用包目录（nexos-app-* 仓库的 manifest 徽章 / DeployButton 数据源）。 */
  catalog: Ref<CatalogApp[]>;
  catalogLoading: Ref<boolean>;
  /** 内置 CI 各仓最新 run 摘要（repo_name → run；仓库卡 CI 徽章数据源）。 */
  ciLatest: Ref<Record<string, CiRun>>;
  /** 大厅条目（原壳 refreshAll 全局加载；PR owner 判定 / 发布下拉共用）。 */
  lobbyEntries: Ref<LobbyEntry[]>;
  lobbyStats: Ref<LobbyStats>;
  lobbyLoading: Ref<boolean>;
  loadRepos(): Promise<void>;
  loadStats(): Promise<void>;
  loadCatalog(): Promise<void>;
  /** 内置 CI 各仓最新 run 摘要拉取（Explore 页一次拉全 + 刷新按钮联动）。 */
  loadCiLatest(): Promise<void>;
  /** 大厅列表拉取（q/tag/sort 为服务端参数；缺省取当前默认视图）。 */
  loadLobby(opts?: { q?: string; tag?: string; sort?: 'recent' | 'downloads' }): Promise<void>;
  /** 刷新共享数据（repos + stats + catalog + lobby + CI 最新摘要）。 */
  refreshShared(): Promise<void>;

  // —— 提示条（壳页头下方统一渲染）——
  msg: Ref<HubMsg | null>;
  showMsg(kind: HubMsg['kind'], text: string): void;
  clearMsg(): void;
  /** 危险操作进行中（按钮禁用态，原 actionLoading）。 */
  actionLoading: Ref<boolean>;

  /** Explore 仓库列表搜索词（顶栏全局搜索 ↔ 列表页过滤共享）。 */
  exploreQuery: Ref<string>;

  // —— 应用一键部署（useAppDeploy 单例：所有 DeployButton 共享安装态）——
  deploy: UseAppDeployReturn;

  // —— 导航（双模式适配；子页面唯一入口）——
  /** 当前视图（桌面模式内部状态；standalone 模式由路由推导镜像）。 */
  currentView: Ref<HubView>;
  /** 打开仓库详情（tab 可选，默认 Code；standalone → /s/codehub/r/:name）。 */
  openRepo(name: string, tab?: RepoDetailTab): void;
  /** 回 Explore 仓库列表。 */
  goExplore(): void;
  /** 切顶层视图（lobby / sessions / onboarding；standalone → 对应子路由）。 */
  goView(view: Exclude<HubView, 'repo'>): void;
}

export const nexhubContextKey: InjectionKey<NexhubContext> = Symbol('nexhub-context');

/** 子页面取共享上下文（必须在 CodeHub 壳的组件树内使用）。 */
export function useNexhub(): NexhubContext {
  const ctx = inject(nexhubContextKey);
  if (!ctx) {
    throw new Error('useNexhub() must be called inside the CodeHub shell component tree (provide(nexhubContextKey))');
  }
  return ctx;
}
