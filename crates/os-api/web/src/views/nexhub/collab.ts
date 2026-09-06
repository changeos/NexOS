// =============================================================================
// nexhub/collab.ts —— Issues / PR 协作层共享逻辑（v0.1.32 拆分自 CodeHub.vue）。
//
// 权限模型（与后端 issues.rs 同源）：
//   - 读公开；写需身份（链上 nexhub token 优先归因 pubkey，无身份回落 admin）；
//   - merge 仅 admin / 仓库所有者（大厅条目 publisher == 我的 pubkey）；
//   - 关闭/重开仅 admin / 作者本人。
// =============================================================================
import { computed } from 'vue';
import { ApiError, getApiToken } from '@/api/client';
import type { NexhubOpts } from '@/api/client';
import i18n from '@/i18n';
import { shortIdentity } from '@/composables/useIdenticon';
import { useChainIdentity } from '@/composables/useChainIdentity';
import { errMsg } from '@/views/nexhub/model';

/** 大厅条目最小面（owner 判定用；避免依赖 LobbyPage 内部状态）。 */
export interface CollabLobbyEntry {
  repo_name?: string;
  publisher?: string;
}

/** 协作层身份上下文（useChainIdentity 为模块级单例，多组件共享同一密钥对状态）。 */
export function useCollabIdentity(lobbyEntries: () => CollabLobbyEntry[]) {
  const {
    hasIdentity,
    pubkey: identityPubkey,
    evmAddress,
    nexhubAuthenticating,
    ensureNexhubToken,
  } = useChainIdentity();

  /** 全局 admin token 已配置（写操作可走 admin 回落通道）。 */
  const hasAdminToken = computed(() => !!getApiToken());

  /** 仓库 owner 判定（与后端 merge 权限同源）：大厅条目 publisher == 我的 pubkey。 */
  function isRepoOwner(repo: string): boolean {
    const entry = lobbyEntries().find((e) => e.repo_name === repo);
    return hasIdentity.value && !!entry && (entry.publisher ?? '') === identityPubkey.value;
  }

  /** Merge 权限：admin 或仓库 owner（无权限时前端隐藏 Merge 按钮并提示）。 */
  function canMergePull(repo: string): boolean {
    return hasAdminToken.value || isRepoOwner(repo);
  }

  /** 关闭/重开权限：admin 或 author 本人（Issue 与 PR 同规则）。 */
  function canToggleState(author: string | undefined): boolean {
    return hasAdminToken.value || (hasIdentity.value && (author ?? '') === identityPubkey.value);
  }

  /**
   * 协作写操作身份参数：已初始化身份 → 自动走 challenge→sign→verify 取
   * nexhub token（覆盖全局 admin 注入，服务端归因 pubkey；认证失败抛错不回落
   * admin，避免误以 admin 身份归因）；未初始化 → undefined（走全局 admin 回落）。
   */
  async function requireNexhubOpts(): Promise<NexhubOpts | undefined> {
    if (!hasIdentity.value) return undefined;
    try {
      return { nexhubToken: (await ensureNexhubToken()).token };
    } catch (e) {
      throw new Error(`${i18n.global.t('nexhub.collab.authFailed')}：${errMsg(e)}`);
    }
  }

  return {
    hasIdentity,
    identityPubkey,
    evmAddress,
    nexhubAuthenticating,
    hasAdminToken,
    isRepoOwner,
    canMergePull,
    canToggleState,
    requireNexhubOpts,
  };
}

/** 协作写操作错误文案（403 归因权限、401 引导身份初始化，同大厅先例）。 */
export function collabWriteErr(action: string, e: unknown): string {
  const gt = i18n.global.t;
  if (e instanceof ApiError) {
    if (e.status === 403) return `${action}${gt('nexhub.collab.err403Tail')}`;
    if (e.status === 401) return `${action}${gt('nexhub.collab.err401Tail')}`;
  }
  return `${action}${gt('nexhub.collab.errGenericTail')}: ${errMsg(e)}`;
}

/** 作者展示：pubkey → EVM 短址 / 短公钥；'admin' 原样。 */
export function authorLabel(author: string | undefined, display: string | undefined): string {
  if (!author) return '—';
  if (author === 'admin') return 'admin';
  return display && display.startsWith('0x') ? shortIdentity(display) : shortIdentity(author);
}
