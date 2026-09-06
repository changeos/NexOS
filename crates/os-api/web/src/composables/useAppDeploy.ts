// =============================================================================
// useAppDeploy —— 应用包安装流程公用 composable（v0.1.32 从 AppStore.vue 抽出）。
//
// 消费方：AppStore.vue（应用中心「应用包」卡）与 NexHub DeployButton（CodeHub
// 仓库详情页 / 应用仓库卡的一键部署）。封装同一条链路：
//   appsCatalog 拉取 → POST /api/v1/apps/install → 轮询 GET /api/v1/apps
//   → hotRegisterApp（appRuntime 热注册，免刷新桌面可见）。
// 状态提示（msg / toast）由调用方通过事件回调组装——composable 只负责流程与
// 安装态，不持有 UI 文案，保证 AppStore 既有文案零变化。
// =============================================================================
import { ref } from 'vue';
import { endpoints, type CatalogApp, type InstalledApp } from '@/api/client';
import { hotRegisterApp } from '@/appRuntime';

/** 安装入参（catalog 条目的最小面；AppStore 的 CatalogPkg / CodeHub 的 CatalogApp 均兼容）。 */
export interface DeployPkg {
  repo?: string;
  id?: string;
  name?: string;
  [k: string]: unknown;
}

/** 安装流程事件回调（全部可选；调用方据此渲染消息）。 */
export interface UseAppDeployEvents {
  /** 安装请求已受理但轮询超时未确认（任务型后端兜底提示）。 */
  onPending?: (name: string) => void;
  /** 安装完成且热注册成功（桌面 / 启动台已可见）。 */
  onInstalled?: (name: string, app: InstalledApp) => void | Promise<void>;
  /** 安装完成但热注册失败（刷新后 bootstrap 路径仍会注册）。 */
  onRegisterFailed?: (name: string, err: unknown) => void | Promise<void>;
  /** 安装请求失败（400 manifest 校验 / 409 同 id 异源 / 网络等）。 */
  onFailed?: (err: unknown) => void;
  /** 后端返回 action=noop（同版本重复安装，已是最新）。 */
  onNoop?: (name: string) => void;
}

/** 安装完成确认的轮询超时（毫秒）。 */
const INSTALL_POLL_TIMEOUT_MS = 60_000;
/** 轮询间隔（毫秒）。 */
const INSTALL_POLL_INTERVAL_MS = 2000;

/**
 * 等待应用包出现在已装清单（POST install 后轮询 GET /api/v1/apps，
 * 最长 60s；后端瞬时不可用继续等，超时 resolve null）。
 */
async function waitForInstalled(appId: string, timerRef: { timer: ReturnType<typeof setTimeout> | null }): Promise<InstalledApp | null> {
  return new Promise((resolve) => {
    const started = Date.now();
    const poll = async (): Promise<void> => {
      try {
        const resp = await endpoints.appsList();
        const found = (resp?.apps ?? []).find((a: InstalledApp) => a.id === appId);
        if (found) {
          resolve(found);
          return;
        }
      } catch {
        /* 后端瞬时不可用：继续等 */
      }
      if (Date.now() - started >= INSTALL_POLL_TIMEOUT_MS) {
        resolve(null);
        return;
      }
      timerRef.timer = setTimeout(poll, INSTALL_POLL_INTERVAL_MS);
    };
    void poll();
  });
}

/** 应用包安装 / 目录状态（AppStore 与 NexHub DeployButton 共用一条流程）。 */
export function useAppDeploy(events: UseAppDeployEvents = {}) {
  /** 应用包目录（GET /api/v1/apps/catalog）。 */
  const catalog = ref<CatalogApp[]>([]);
  const catalogLoading = ref(false);
  const catalogError = ref('');
  /** 正在安装的 repo（按钮态；空 = 无进行中安装）。 */
  const installingRepo = ref('');
  /** 轮询句柄（unmount 清理）。 */
  const pollRef: { timer: ReturnType<typeof setTimeout> | null } = { timer: null };

  function stopPolling(): void {
    if (pollRef.timer) {
      clearTimeout(pollRef.timer);
      pollRef.timer = null;
    }
  }

  /** 拉取应用包目录（安装态 / 版本随刷）。 */
  async function loadCatalog(): Promise<void> {
    catalogLoading.value = true;
    catalogError.value = '';
    try {
      const raw = await endpoints.appsCatalog();
      catalog.value = Array.isArray(raw?.apps) ? (raw.apps as CatalogApp[]) : [];
    } catch (e) {
      catalog.value = [];
      catalogError.value = e instanceof Error ? e.message : String(e);
    } finally {
      catalogLoading.value = false;
    }
  }

  /** 按 repo 名查目录条目（DeployButton 状态机数据源）。 */
  function catalogEntry(repo: string): CatalogApp | undefined {
    return catalog.value.find((c) => c.repo === repo);
  }

  /**
   * 安装应用包：POST install → 轮询已装 → 热注册（免刷新桌面可见）。
   * 消息 / 后续刷新经事件回调交由调用方（文案归 UI 层）。
   */
  async function install(pkg: DeployPkg): Promise<void> {
    const repo = String(pkg.repo ?? '');
    const appId = String(pkg.id ?? '');
    const name = String(pkg.name ?? appId);
    if (!repo || !appId || installingRepo.value) return;
    installingRepo.value = repo;
    try {
      const resp = await endpoints.appsInstall(repo);
      // 后端同版本重复安装返回 action=noop（200）：提示"已是最新"，无需轮询。
      const action = typeof (resp as { action?: unknown })?.action === 'string' ? String(resp.action) : '';
      if (action === 'noop') {
        events.onNoop?.(name);
        return;
      }
      // 兼容同步完成与任务型后端：轮询已装清单直到出现（最长 60s）。
      const installed = await waitForInstalled(appId, pollRef);
      if (!installed) {
        events.onPending?.(name);
        return;
      }
      // 热注册：免刷新，桌面 / 启动台立刻出现。
      try {
        await hotRegisterApp(installed);
        await events.onInstalled?.(name, installed);
      } catch (e) {
        // 装好了但注册失败：刷新后仍会注册（bootstrap 路径）。
        await events.onRegisterFailed?.(name, e);
      }
    } catch (e) {
      events.onFailed?.(e);
    } finally {
      installingRepo.value = '';
    }
  }

  return {
    catalog,
    catalogLoading,
    catalogError,
    loadCatalog,
    catalogEntry,
    installingRepo,
    install,
    stopPolling,
  };
}

export type UseAppDeployReturn = ReturnType<typeof useAppDeploy>;
