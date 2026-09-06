<script setup lang="ts">
// =============================================================================
// LobbyPage —— NexHub 大厅（v0.1.32，原 Tab2 整体迁移）。
// 发现层 /api/v1/nexhub/lobby/*：本地/联邦二级 Tab、搜索/tag facet/排序、
// 行展开详情（README 摘要 + 双通道 clone 地址）、打赏 / 克隆 / 购买 / 发布 /
// 下架 / 联邦推送。条目与统计来自壳共享数据（ctx.lobbyEntries / loadLobby）；
// 下架确认走站内 NexhubConfirm（替代原生 confirm()）。
// =============================================================================
import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { ApiError, endpoints } from '@/api/client';
import type { NexhubOpts } from '@/api/client';
import i18n from '@/i18n';
import { identiconSvg, shortIdentity } from '@/composables/useIdenticon';
import { useChainIdentity } from '@/composables/useChainIdentity';
import { copyText } from '@/utils/clipboard';
import { errMsg, formatBytes, formatDate, formatRelative, type LobbyEntry } from '@/views/nexhub/model';
import { useNexhub } from '@/views/nexhub/context';
import TipButton from '@/components/TipButton.vue';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';

const { t } = useI18n();
const ctx = useNexhub();

// =============================================================================
// 链上身份（与 IM 共用同一密钥对；NexHub token 独立认证，见 useChainIdentity）
// =============================================================================
const { hasIdentity, pubkey: identityPubkey, evmAddress, nexhubAuthenticating, ensureNexhubToken } =
  useChainIdentity();

/** 发布者展示：链上身份（0x+66hex 公钥）→ `0x**…后四位` 短显；存量字符串条目原样。 */
function publisherLabel(e: LobbyEntry): string {
  const p = e.publisher ?? '';
  return /^0x[0-9a-fA-F]{66}$/.test(p) ? shortIdentity(p) : p || '—';
}

/** 联邦远程条目（source_node 非空且非 'local'——经 os-p2p 从其他 NexOS 节点同步）。 */
function isRemoteEntry(e: LobbyEntry): boolean {
  return !!e.source_node && e.source_node !== 'local';
}

/**
 * 大厅写操作身份参数：已初始化身份 → 自动走 challenge→sign→verify 取
 * nexhub token（服务端归因 pubkey；认证失败抛错不回落 admin）；未初始化 →
 * undefined（走全局 admin 回落）。
 */
async function requireNexhubOpts(): Promise<NexhubOpts | undefined> {
  if (!hasIdentity.value) return undefined;
  try {
    return { nexhubToken: (await ensureNexhubToken()).token };
  } catch (e) {
    throw new Error(`${t('nexhub.collab.authFailed')}：${errMsg(e)}`);
  }
}

/** 打赏（TipButton）链上 token 获取器（未初始化/失败 → undefined 回落）。 */
async function tipTokenGetter(): Promise<string | undefined> {
  if (!hasIdentity.value) return undefined;
  try {
    return (await ensureNexhubToken()).token;
  } catch {
    return undefined;
  }
}

/** 大厅写操作错误 → 用户文案（403 归一"仅项目所有者"提示；401 引导初始化身份）。 */
function lobbyWriteErr(action: string, e: unknown): string {
  const gt = i18n.global.t;
  if (e instanceof ApiError) {
    if (e.status === 403) return `${action}${gt('nexhub.lobby.err403Tail')}`;
    if (e.status === 401) return `${action}${gt('nexhub.lobby.err401Tail')}`;
  }
  return `${action}${gt('nexhub.collab.errGenericTail')}: ${errMsg(e)}`;
}

// —— 二级 Tab：本地大厅 / 联邦大厅（切换只换视图，搜索/排序/展开状态保留）——
type LobbyView = 'local' | 'fed';
const lobbyView = ref<LobbyView>('local');
/** 本地条目（source_node='local' 或缺省标记）。 */
const localLobbyEntries = computed(() => ctx.lobbyEntries.value.filter((e) => !isRemoteEntry(e)));
/** 联邦远程条目（source_node 非空且非 'local'，来自其他 NexOS 节点）。 */
const fedLobbyEntries = computed(() => ctx.lobbyEntries.value.filter((e) => isRemoteEntry(e)));
/** 当前二级 Tab 显示的条目（本地=默认）。 */
const visibleLobbyEntries = computed(() =>
  lobbyView.value === 'fed' ? fedLobbyEntries.value : localLobbyEntries.value,
);

// —— 视图本地状态：搜索 / 排序 / 展开 / 详情缓存 ——
const lobbyQuery = ref('');
const lobbyTag = ref('');
const lobbySort = ref<'recent' | 'downloads'>('recent');
/** 展开的行（repo_name；空=全部收起）。 */
const lobbyExpanded = ref('');
/** 详情缓存（repo_name → 详情接口返回，含双通道 clone 地址）。 */
const lobbyDetails = ref<Record<string, LobbyEntry>>({});
/** 正在克隆的条目名（空=无进行中）。 */
const cloningRepo = ref('');
/** 复制的 clone URL 反馈（`name:ssh` / `name:http`）。 */
const copiedLobby = ref('');
/** 正在推送联邦的条目名（空=无进行中）。 */
const federatingRepo = ref('');

/** 搜索 / 排序请求（q/tag/sort 为服务端参数，须重拉列表）。 */
async function searchLobby(): Promise<void> {
  await reloadLobby({
    q: lobbyQuery.value.trim() || undefined,
    tag: lobbyTag.value || undefined,
    sort: lobbySort.value,
  });
}

/** 重拉大厅（原 loadLobby 语义：列表已刷新 → 详情缓存作废、展开态收起）。 */
async function reloadLobby(opts?: { q?: string; tag?: string; sort?: 'recent' | 'downloads' }): Promise<void> {
  await ctx.loadLobby(opts);
  lobbyDetails.value = {};
  lobbyExpanded.value = '';
}

/** 点击标签 chip 过滤（再点同一个取消）。 */
function filterByTag(tag: string): void {
  lobbyTag.value = lobbyTag.value === tag ? '' : tag;
  void searchLobby();
}

/** 展开/收起大厅条目行；展开时懒加载详情接口（补双通道 clone 地址）。 */
async function toggleLobbyCard(e: LobbyEntry): Promise<void> {
  const name = e.repo_name ?? '';
  if (!name) return;
  if (lobbyExpanded.value === name) {
    lobbyExpanded.value = '';
    return;
  }
  lobbyExpanded.value = name;
  if (!lobbyDetails.value[name]) {
    try {
      lobbyDetails.value[name] = (await endpoints.nexhubLobbyDetail(name)) as LobbyEntry;
    } catch (err) {
      ctx.showMsg('error', `${t('nexhub.lobby.detailLoadFailed')}: ${errMsg(err)}`);
    }
  }
}

/** 详情缓存取值（未命中回退列表条目）。 */
function detailOf(e: LobbyEntry): LobbyEntry {
  return lobbyDetails.value[e.repo_name ?? ''] ?? e;
}

/** 一键复制双通道 clone 地址（SSH / Smart HTTP）。 */
async function copyLobbyUrl(name: string, kind: 'ssh' | 'http'): Promise<void> {
  const d = lobbyDetails.value[name];
  const url = kind === 'ssh' ? d?.clone_url_ssh : d?.clone_url_http;
  if (!url) {
    ctx.showMsg('error', t('nexhub.lobby.cloneUrlMissing'));
    return;
  }
  if (!(await copyText(url))) {
    ctx.showMsg('error', t('nexhub.common.copyFailed'));
    return;
  }
  copiedLobby.value = `${name}:${kind}`;
  ctx.showMsg('ok', t('nexhub.lobby.cloneUrlCopied', { kind: kind.toUpperCase(), url }));
  setTimeout(() => (copiedLobby.value = ''), 2000);
}

/** 克隆大厅条目到本地 /tank/git-repos/（需身份；成功后刷新列表+轻提示）。 */
async function cloneLobbyEntry(name: string): Promise<void> {
  if (!name || cloningRepo.value) return;
  cloningRepo.value = name;
  ctx.clearMsg();
  try {
    const opts = await requireNexhubOpts();
    const r = (await endpoints.nexhubLobbyClone(name, opts)) as {
      cloned?: boolean;
      local_path?: string;
      source_node?: string;
    };
    // 联邦远程条目：响应带 source_node + note——提示从远程节点拉取
    const remote = !!r.source_node && r.source_node !== 'local';
    ctx.showMsg('ok',
      r.cloned
        ? remote
          ? t('nexhub.lobby.clonedFromRemote', { name, node: r.source_node ?? '', path: r.local_path ?? '' })
          : t('nexhub.lobby.cloned', { name, path: r.local_path ?? '' })
        : t('nexhub.lobby.alreadyLocal', { name }),
    );
    // 克隆会注册本地仓库 → 同时刷新仓库列表与大厅计数
    await Promise.all([reloadLobby(), ctx.loadRepos()]);
  } catch (e) {
    ctx.showMsg('error', lobbyWriteErr(t('nexhub.lobby.cloneAction'), e));
  } finally {
    cloningRepo.value = '';
  }
}

// —— 下架（原生 confirm → 站内确认弹窗）——
const unpublishTarget = ref<LobbyEntry | null>(null);
/** 确认弹窗开关代理（v-model 需可写成员表达式；null ↔ 展示态）。 */
const showUnpublish = computed<boolean>({
  get: () => unpublishTarget.value !== null,
  set: (v) => {
    if (!v) unpublishTarget.value = null;
  },
});

async function doUnpublish(): Promise<void> {
  const name = unpublishTarget.value?.repo_name ?? '';
  unpublishTarget.value = null;
  if (!name) return;
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await requireNexhubOpts();
    await endpoints.nexhubLobbyUnpublish(name, opts);
    ctx.showMsg('ok', t('nexhub.lobby.unpublished', { name }));
    await reloadLobby();
  } catch (e) {
    ctx.showMsg('error', lobbyWriteErr(t('nexhub.lobby.unpublishAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

/**
 * 推送/重新推送到联邦大厅（两步联邦第二步——条目须已发布在本地大厅）：
 * POST /:name/federate 广播最新快照并置 federated=true；重复调用=重新推送。
 */
async function federateLobbyEntry(name: string, alreadyFederated: boolean): Promise<void> {
  if (!name || federatingRepo.value) return;
  federatingRepo.value = name;
  ctx.clearMsg();
  try {
    const opts = await requireNexhubOpts();
    const r = (await endpoints.nexhubLobbyFederate(name, opts)) as { note?: string };
    ctx.showMsg('ok', r.note || t(alreadyFederated ? 'nexhub.lobby.refederated' : 'nexhub.lobby.federated', { name }));
    await reloadLobby();
  } catch (e) {
    ctx.showMsg('error', lobbyWriteErr(t(alreadyFederated ? 'nexhub.lobby.refederateAction' : 'nexhub.lobby.federateAction'), e));
  } finally {
    federatingRepo.value = '';
  }
}

// —— 发布对话框 ——
const showPublish = ref(false);
const publishForm = ref({ repo: '', description: '', tags: '' });

/** 可发布仓库 = 本地仓库 - 已在大厅的（发布对话框下拉）。 */
const publishableRepos = computed(() => {
  const published = new Set(
    ctx.lobbyEntries.value.map((e) => e.repo_name ?? '').filter(Boolean),
  );
  return ctx.repos.value
    .map((r) => r.name)
    .filter((n): n is string => !!n && !published.has(n));
});

/** 打开发布对话框（清空表单）。 */
function openPublish(): void {
  publishForm.value = { repo: '', description: '', tags: '' };
  ctx.clearMsg();
  showPublish.value = true;
}

/**
 * 发布本地仓库到大厅（带链上身份）：未初始化身份 → 提示去 IM 页；已初始化 →
 * 自动 challenge→sign→verify 取 nexhub token（服务端归因 pubkey）。
 */
async function doPublish(): Promise<void> {
  if (!publishForm.value.repo.trim()) {
    ctx.showMsg('error', t('nexhub.lobby.pickRepoRequired'));
    return;
  }
  if (!hasIdentity.value) {
    ctx.showMsg('error', t('nexhub.lobby.identityRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await requireNexhubOpts();
    const tags = publishForm.value.tags
      .split(/[,，]/)
      .map((s) => s.trim())
      .filter(Boolean);
    const r = (await endpoints.nexhubLobbyPublish(
      {
        repo: publishForm.value.repo.trim(),
        description: publishForm.value.description.trim() || undefined,
        tags,
      },
      opts,
    )) as LobbyEntry & { owner_kind?: string; publisher_display?: string };
    const who = r.publisher_display || evmAddress.value || t('nexhub.lobby.chainIdentityFallback');
    ctx.showMsg('ok', t('nexhub.lobby.published', {
      name: r.repo_name ?? publishForm.value.repo,
      who,
      chain: r.owner_kind === 'pubkey' ? t('nexhub.lobby.publishedChainSuffix') : '',
    }));
    showPublish.value = false;
    await reloadLobby();
  } catch (e) {
    ctx.showMsg('error', lobbyWriteErr(t('nexhub.lobby.publishAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

// —— 购买授权对话框（付费条目）——
const showPurchase = ref(false);
const purchaseForm = ref({ name: '', txid: '', amount_sats: 0, currency: '' });

/** 打开购买授权对话框（付费条目；buyer=token 身份，需链上身份）。 */
function openPurchase(e: LobbyEntry): void {
  purchaseForm.value = {
    name: e.repo_name ?? '',
    txid: '',
    amount_sats: e.price_sats ?? 0,
    currency: e.currency ?? '',
  };
  ctx.clearMsg();
  showPurchase.value = true;
}

/** 购买付费条目授权（自证收据 txid；成功后可克隆）。 */
async function doPurchase(): Promise<void> {
  if (!hasIdentity.value) {
    ctx.showMsg('error', t('nexhub.lobby.purchaseIdentityRequired'));
    return;
  }
  if (!purchaseForm.value.txid.trim()) {
    ctx.showMsg('error', t('nexhub.lobby.txidRequired'));
    return;
  }
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await requireNexhubOpts();
    await endpoints.nexhubLobbyPurchase(
      purchaseForm.value.name,
      {
        txid: purchaseForm.value.txid.trim(),
        amount_sats: purchaseForm.value.amount_sats || undefined,
        currency: purchaseForm.value.currency || undefined,
      },
      opts,
    );
    ctx.showMsg('ok', t('nexhub.lobby.purchased', { name: purchaseForm.value.name }));
    showPurchase.value = false;
  } catch (e) {
    ctx.showMsg('error', lobbyWriteErr(t('nexhub.lobby.purchaseAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}
</script>

<template>
  <section class="lobby-page">
    <!-- 二级 Tab：本地大厅 / 联邦大厅（切换只换视图；搜索/排序/展开状态保留） -->
    <nav class="tabs sub-tabs" role="tablist" :aria-label="t('nexhub.lobby.sourceSwitch')">
      <button
        class="tab sub-tab"
        :class="{ active: lobbyView === 'local' }"
        role="tab"
        :aria-selected="lobbyView === 'local'"
        type="button"
        @click="lobbyView = 'local'"
      >🏠 {{ t('nexhub.lobby.localTab') }} ({{ localLobbyEntries.length }})</button>
      <button
        class="tab sub-tab"
        :class="{ active: lobbyView === 'fed' }"
        role="tab"
        :aria-selected="lobbyView === 'fed'"
        type="button"
        @click="lobbyView = 'fed'"
      ><span class="fed-icon" aria-hidden="true">🌐</span> {{ t('nexhub.lobby.fedTab') }} ({{ fedLobbyEntries.length }})</button>
    </nav>

    <!-- 链上身份状态卡：大厅写操作（发布/下架/购买/克隆付费条目）的身份闸门 -->
    <div v-if="hasIdentity" class="card identity-card" :title="t('nexhub.lobby.identityCardTitle')">
      <span class="identity-badge" :title="t('nexhub.lobby.identityBadgeTitle')">
        <img class="identicon" :src="identiconSvg(identityPubkey, 20)" alt="" />
      </span>
      <div class="identity-info">
        <span class="identity-label">{{ t('nexhub.lobby.identityReady') }}</span>
        <code class="identity-addr" :title="evmAddress">{{ shortIdentity(evmAddress) }}</code>
        <span class="muted small identity-pk" :title="identityPubkey">{{ t('nexhub.lobby.pubkey') }} {{ shortIdentity(identityPubkey) }}</span>
      </div>
      <span v-if="nexhubAuthenticating" class="identity-status muted small">
        <span class="spin spinning" aria-hidden="true">↻</span> {{ t('nexhub.lobby.authenticating') }}
      </span>
      <span v-else class="identity-status identity-ok">{{ t('nexhub.lobby.attributionReady') }}</span>
    </div>
    <div v-else class="card identity-card identity-missing">
      <span class="identity-badge">⛓</span>
      <div class="identity-info">
        <span class="identity-label">{{ t('nexhub.lobby.identityMissing') }}</span>
        <span class="muted small">{{ t('nexhub.lobby.identityMissingHint') }}</span>
      </div>
      <RouterLink class="btn btn-small btn-primary" to="/chat">{{ t('nexhub.lobby.goIdentity') }}</RouterLink>
    </div>

    <!-- 顶部工具条：搜索 q / 排序切换 / 发布 -->
    <div class="repo-toolbar">
      <div class="lobby-search">
        <input
          v-model="lobbyQuery"
          class="search-input lobby-q"
          :placeholder="t('nexhub.lobby.searchPlaceholder')"
          @keyup.enter="searchLobby"
        />
        <button class="btn btn-small" type="button" :disabled="ctx.lobbyLoading.value" @click="searchLobby">
          {{ t('nexhub.lobby.search') }}
        </button>
      </div>
      <div class="toolbar-actions">
        <select v-model="lobbySort" class="search-input lobby-sort" @change="searchLobby">
          <option value="recent">{{ t('nexhub.lobby.sortRecent') }}</option>
          <option value="downloads">{{ t('nexhub.lobby.sortDownloads') }}</option>
        </select>
        <button class="btn btn-small btn-primary" type="button" @click="openPublish">
          🏛 {{ t('nexhub.lobby.publish') }}
        </button>
      </div>
    </div>

    <!-- 统计条：发布数 / 总下载 / top 标签（点击过滤） -->
    <div class="lobby-statsbar">
      <span class="lobby-stat">
        <strong>{{ ctx.lobbyStats.value.published_count ?? 0 }}</strong> {{ t('nexhub.lobby.projectsCount') }}
      </span>
      <span class="lobby-stat">
        <strong>⬇ {{ ctx.lobbyStats.value.total_downloads ?? 0 }}</strong> {{ t('nexhub.lobby.clonesCount') }}
      </span>
      <span
        v-if="lobbyTag"
        class="meta-chip lobby-tag-chip active"
        :title="t('nexhub.lobby.clearTagFilter')"
        @click="filterByTag(lobbyTag)"
      >#{{ lobbyTag }} ✕</span>
      <span
        v-for="tg in ctx.lobbyStats.value.top_tags ?? []"
        :key="tg.tag"
        class="meta-chip lobby-tag-chip"
        :class="{ active: lobbyTag === tg.tag }"
        :title="t('nexhub.lobby.filterByTag', { tag: tg.tag })"
        @click="filterByTag(tg.tag)"
      >#{{ tg.tag }} × {{ tg.count }}</span>
    </div>

    <div v-if="ctx.lobbyLoading.value" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="visibleLobbyEntries.length === 0" class="card empty-card">
      <span v-if="lobbyView === 'fed'">{{ t('nexhub.lobby.emptyFed') }}</span>
      <span v-else>{{ t('nexhub.lobby.emptyLocal') }}</span>
    </div>

    <!-- 项目行列表：一行一个项目（点行展开详情）
         响应式契约：项目名永不截断/省略——空间不足时其余元素依次换行让位 -->
    <div v-else class="lobby-list">
      <div
        v-for="e in visibleLobbyEntries"
        :key="e.repo_name"
        class="card lobby-row"
        :class="{ expanded: lobbyExpanded === e.repo_name }"
        :title="t('nexhub.lobby.toggleDetail')"
        @click="toggleLobbyCard(e)"
      >
        <div class="lobby-row-line">
          <span class="lobby-row-icon" aria-hidden="true">📁</span>
          <span class="lobby-row-name">{{ e.repo_name }}</span>
          <span v-if="e.tags && e.tags.length" class="lobby-row-tags">
            <span
              v-for="tg in e.tags"
              :key="tg"
              class="meta-chip lobby-tag-chip"
              :class="{ active: lobbyTag === tg }"
              :title="t('nexhub.lobby.filterByTag', { tag: tg })"
              @click.stop="filterByTag(tg)"
            >#{{ tg }}</span>
          </span>
          <span
            class="meta-chip lobby-owner-chip"
            :title="t('nexhub.lobby.publisherTitle', { publisher: e.publisher || '—' })"
          >
            <img
              v-if="e.publisher"
              class="identicon"
              :src="identiconSvg(e.publisher, 16)"
              alt=""
            />{{ publisherLabel(e) }}</span>
          <span
            v-if="isRemoteEntry(e)"
            class="meta-chip fed-chip"
            :title="t('nexhub.lobby.remoteEntryTitle', { node: e.source_node })"
          >
            🌐 {{ t('nexhub.lobby.fromNode', { node: e.source_node }) }}</span>
          <!-- 已推送联邦的本地条目：🌐 同步小标记（远程条目已有「来自节点」徽章，不重复打标） -->
          <span
            v-if="!isRemoteEntry(e) && e.federated"
            class="meta-chip fed-sync-chip"
            :title="t('nexhub.lobby.fedSyncTitle')"
          >🌐 {{ t('nexhub.lobby.fedSync') }}</span>
          <span v-if="(e.price_sats ?? 0) > 0" class="meta-chip price-chip">
            💰 {{ e.price_sats }} {{ e.currency }}
          </span>
          <span class="lobby-row-desc" :title="e.description || t('nexhub.explore.noDescription')">
            {{ e.description || t('nexhub.explore.noDescription') }}
          </span>
          <span class="lobby-row-metrics">
            <span :title="t('nexhub.explore.commits')">◷ {{ e.commit_count ?? 0 }}</span>
            <span class="lobby-metric-sep" aria-hidden="true">·</span>
            <span>{{ formatBytes(e.size_bytes) }}</span>
            <span class="lobby-metric-sep" aria-hidden="true">·</span>
            <span :title="t('nexhub.lobby.cloneCount')">⬇ {{ e.download_count ?? 0 }}</span>
            <span class="lobby-row-date" :title="t('nexhub.lobby.publishedAt', { date: formatDate(e.published_at) })">
              {{ formatRelative(e.published_at) }}
            </span>
          </span>
          <span class="lobby-row-actions" @click.stop>
            <button class="btn btn-small btn-ghost" type="button" @click="toggleLobbyCard(e)">
              {{ lobbyExpanded === e.repo_name ? t('nexhub.lobby.collapse') : t('nexhub.lobby.detail') }}
            </button>
            <!-- 打赏条目所有者（target_ref=nexhub:<repo_name>） -->
            <TipButton
              target-kind="lobby_entry"
              :target-ref="`nexhub:${e.repo_name}`"
              :get-token="tipTokenGetter"
              size="small"
            />
            <button
              v-if="(e.price_sats ?? 0) > 0"
              class="btn btn-small btn-ghost"
              type="button"
              :disabled="ctx.actionLoading.value"
              @click="openPurchase(e)"
            >💰 {{ t('nexhub.lobby.purchase') }}</button>
            <button
              class="btn btn-small btn-primary"
              type="button"
              :disabled="cloningRepo !== ''"
              :title="isRemoteEntry(e)
                ? t('nexhub.lobby.cloneRemoteTitle', { node: e.source_node })
                : t('nexhub.lobby.cloneLocalTitle')"
              @click="cloneLobbyEntry(e.repo_name ?? '')"
            >
              <span
                class="spin"
                :class="{ spinning: cloningRepo === e.repo_name }"
                aria-hidden="true"
              >↻</span>
              {{ cloningRepo === e.repo_name ? t('nexhub.lobby.cloning') : t('nexhub.lobby.cloneBtn') }}
            </button>
            <!-- 两步联邦第二步：推送/重新推送到联邦大厅（仅本地条目） -->
            <button
              v-if="!isRemoteEntry(e)"
              class="btn btn-small btn-ghost"
              type="button"
              :disabled="federatingRepo !== ''"
              :title="e.federated
                ? t('nexhub.lobby.refederateTitle')
                : t('nexhub.lobby.federateTitle')"
              @click="federateLobbyEntry(e.repo_name ?? '', !!e.federated)"
            >
              {{ federatingRepo === e.repo_name ? t('nexhub.lobby.federating') : e.federated ? t('nexhub.lobby.refederateBtn') : t('nexhub.lobby.federateBtn') }}
            </button>
            <button
              class="btn btn-small btn-danger"
              type="button"
              :disabled="ctx.actionLoading.value"
              @click="unpublishTarget = e"
            >{{ t('nexhub.lobby.unpublish') }}</button>
          </span>
        </div>

        <!-- 展开详情：README 摘要 + 双通道 clone 地址（一键复制） -->
        <div v-if="lobbyExpanded === e.repo_name" class="lobby-detail" @click.stop>
          <div class="lobby-detail-head muted small">
            {{ t('nexhub.lobby.readmeExcerpt') }} · {{ t('nexhub.lobby.defaultBranch') }} {{ detailOf(e).default_branch || '—' }}
          </div>
          <pre class="lobby-readme">{{ e.readme_excerpt || t('nexhub.lobby.noReadme') }}</pre>
          <div class="clone-row">
            <code class="clone-url" :title="detailOf(e).clone_url_ssh">
              SSH&nbsp;{{ detailOf(e).clone_url_ssh || t('nexhub.lobby.fetching') }}
            </code>
            <button
              class="btn btn-small btn-ghost"
              type="button"
              :class="{ copied: copiedLobby === (e.repo_name ?? '') + ':ssh' }"
              @click.stop="copyLobbyUrl(e.repo_name ?? '', 'ssh')"
            >{{ copiedLobby === (e.repo_name ?? '') + ':ssh' ? t('nexhub.common.copied') : t('nexhub.common.copy') }}</button>
          </div>
          <div class="clone-row">
            <code class="clone-url" :title="detailOf(e).clone_url_http">
              HTTP&nbsp;{{ detailOf(e).clone_url_http || t('nexhub.lobby.fetching') }}
            </code>
            <button
              class="btn btn-small btn-ghost"
              type="button"
              :class="{ copied: copiedLobby === (e.repo_name ?? '') + ':http' }"
              @click.stop="copyLobbyUrl(e.repo_name ?? '', 'http')"
            >{{ copiedLobby === (e.repo_name ?? '') + ':http' ? t('nexhub.common.copied') : t('nexhub.common.copy') }}</button>
          </div>
          <div v-if="e.last_commit" class="muted small lobby-lastcommit">
            {{ t('nexhub.lobby.lastCommit', { commit: e.last_commit, date: formatDate(e.last_commit_date) }) }}
          </div>
        </div>

      </div>
    </div>

    <!-- 下架确认（站内弹窗，替代原生 confirm） -->
    <NexhubConfirm
      v-model:open="showUnpublish"
      :title="unpublishTarget ? t('nexhub.lobby.unpublishConfirmTitle', { name: unpublishTarget.repo_name ?? '' }) : ''"
      :body="t('nexhub.lobby.unpublishConfirmBody')"
      :danger="true"
      :confirm-text="t('nexhub.lobby.unpublish')"
      @confirm="doUnpublish"
    />

    <!-- 发布到大厅对话框 -->
    <div v-if="showPublish" class="modal-overlay" @click.self="showPublish = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.lobby.publishTitle') }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showPublish = false">✕</button>
        </div>
        <div class="form-row">
          <label class="form-label" for="pb-repo">{{ t('nexhub.lobby.localRepo') }} *</label>
          <select id="pb-repo" v-model="publishForm.repo" class="search-input">
            <option value="">{{ t('nexhub.lobby.pickRepo') }}</option>
            <option v-for="n in publishableRepos" :key="n" :value="n">{{ n }}</option>
          </select>
          <span class="form-hint muted small">{{ t('nexhub.lobby.publishHint') }}</span>
          <span v-if="publishableRepos.length === 0" class="form-hint muted small">
            {{ t('nexhub.lobby.publishEmptyHint') }}
          </span>
        </div>
        <div class="form-row">
          <label class="form-label" for="pb-desc">{{ t('nexhub.explore.descLabel') }}</label>
          <textarea
            id="pb-desc"
            v-model="publishForm.description"
            class="search-input form-textarea"
            rows="2"
            :placeholder="t('nexhub.lobby.publishDescPlaceholder')"
          ></textarea>
        </div>
        <div class="form-row">
          <label class="form-label" for="pb-tags">{{ t('nexhub.lobby.tagsLabel') }}</label>
          <input
            id="pb-tags"
            v-model="publishForm.tags"
            class="search-input"
            :placeholder="t('nexhub.lobby.tagsPlaceholder')"
          />
        </div>
        <!-- 发布身份：链上身份归因（publisher 字段移除——服务端一律从 token 反查 pubkey） -->
        <div class="form-row">
          <label class="form-label">{{ t('nexhub.lobby.publishIdentity') }}</label>
          <div v-if="hasIdentity" class="identity-note identity-note-ok">
            🔗 {{ t('nexhub.lobby.publishAsChain') }}
            <code class="identity-addr" :title="t('nexhub.lobby.evmAddrTitle')">{{ evmAddress }}</code>
            <span class="muted small">{{ t('nexhub.lobby.publishAttribution') }}</span>
          </div>
          <div v-else class="identity-note identity-note-warn">
            ⚠ {{ t('nexhub.lobby.publishNoIdentity') }}
            <RouterLink to="/chat">{{ t('nexhub.lobby.goIdentityLink') }}</RouterLink>
          </div>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showPublish = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="doPublish">
            {{ t('nexhub.lobby.publish') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 购买授权对话框（付费条目） -->
    <div v-if="showPurchase" class="modal-overlay" @click.self="showPurchase = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.lobby.purchaseTitle', { name: purchaseForm.name }) }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showPurchase = false">✕</button>
        </div>
        <div class="form-row">
          <span class="form-hint muted small">
            {{ t('nexhub.lobby.purchaseHint', { amount: purchaseForm.amount_sats, currency: purchaseForm.currency, addr: evmAddress || t('nexhub.lobby.identityMissingShort') }) }}
          </span>
        </div>
        <div class="form-row">
          <label class="form-label" for="pu-txid">{{ t('nexhub.lobby.txidLabel') }} *</label>
          <input
            id="pu-txid"
            v-model="purchaseForm.txid"
            class="search-input"
            :placeholder="t('nexhub.lobby.txidPlaceholder')"
          />
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showPurchase = false">{{ t('common.cancel') }}</button>
          <button class="btn btn-small btn-primary" type="button" :disabled="ctx.actionLoading.value" @click="doPurchase">
            {{ t('nexhub.lobby.purchaseSubmit') }}
          </button>
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.lobby-page { display: flex; flex-direction: column; gap: 14px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }
.sub-tabs { gap: 2px; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.sub-tab { padding: 5px 12px; font-size: 13px; }
.sub-tab .fed-icon { color: var(--accent, #E95420); }
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
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.3); }
.btn-danger:hover { background: #fee2e2; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.btn-ghost:hover { background: rgba(233, 84, 32, 0.08); }
.btn-ghost.copied { color: #166534; }
.spin { display: inline-block; }
.spin.spinning { animation: lb-spin 1s linear infinite; }
@keyframes lb-spin { to { transform: rotate(360deg); } }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.identicon {
  width: 16px; height: 16px; border-radius: 3px; flex-shrink: 0;
  vertical-align: -3px; margin-right: 4px;
}
.identity-card { display: flex; align-items: center; gap: 12px; padding: 12px 16px; flex-wrap: wrap; }
.identity-badge { font-size: 18px; line-height: 1; }
.identity-badge .identicon { width: 20px; height: 20px; border-radius: 4px; display: block; margin-right: 0; vertical-align: baseline; }
.identity-info { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; flex: 1; min-width: 0; }
.identity-label { font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
.identity-addr {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12px;
  padding: 2px 8px; border-radius: var(--radius-sm, 6px);
  background: rgba(233, 84, 32, 0.1); color: var(--accent, #E95420); word-break: break-all;
}
.identity-pk { font-family: 'Ubuntu Mono', Consolas, monospace; }
.identity-status { display: inline-flex; align-items: center; gap: 4px; }
.identity-ok { color: #166534; font-size: 12px; }
.identity-missing { border-color: rgba(245, 158, 11, 0.45); background: #fffbeb; }
.price-chip { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); font-weight: 600; }
.fed-chip { background: rgba(14, 132, 32, 0.1); color: #0e8420; font-weight: 600; flex-shrink: 0; }
.fed-sync-chip { background: rgba(14, 132, 32, 0.1); color: #0e8420; font-weight: 600; flex-shrink: 0; }
.identity-note { font-size: 13px; line-height: 1.6; display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.identity-note-ok { padding: 8px 10px; border-radius: var(--radius-sm, 8px); background: #f0fdf4; }
.identity-note-warn { padding: 8px 10px; border-radius: var(--radius-sm, 8px); background: #fffbeb; color: #92400e; }
.identity-note-warn a { color: var(--accent, #E95420); font-weight: 600; }
.repo-toolbar { display: flex; align-items: center; justify-content: space-between; gap: 12px; flex-wrap: wrap; }
.toolbar-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.lobby-search { display: flex; align-items: center; gap: 8px; flex: 1; min-width: 220px; max-width: 420px; }
.lobby-q { flex: 1; min-width: 0; }
.lobby-sort { min-width: 118px; }
.lobby-statsbar {
  display: flex; align-items: center; gap: 8px; flex-wrap: wrap;
  padding: 10px 16px; border-radius: var(--radius-md, 12px);
  background: var(--bg-card, #fff); border: 1px solid var(--border-soft, #EDEDED);
}
.lobby-stat { font-size: 13px; color: var(--text-muted, #5E5C5F); }
.lobby-stat strong { color: var(--accent, #E95420); font-size: 15px; font-weight: 700; }
.meta-chip {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; color: var(--text-muted, #5E5C5F); background: var(--border-soft, #F3F4F6);
}
.lobby-tag-chip { cursor: pointer; transition: background 0.15s ease, color 0.15s ease; user-select: none; }
.lobby-tag-chip:hover { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); }
.lobby-tag-chip.active { background: rgba(233, 84, 32, 0.15); color: var(--accent, #E95420); font-weight: 600; }
.lobby-list { display: flex; flex-direction: column; gap: 8px; }
.lobby-row {
  padding: 10px 14px; display: flex; flex-direction: column; gap: 10px;
  cursor: pointer; transition: background 0.15s ease, border-color 0.15s ease;
}
.lobby-row:hover { background: var(--border-soft, #F3F4F6); }
.lobby-row.expanded { border-color: var(--accent, #E95420); }
.lobby-row-line { display: flex; align-items: center; gap: 8px; row-gap: 8px; flex-wrap: wrap; min-width: 0; }
.lobby-row-icon { flex-shrink: 0; font-size: 15px; line-height: 1; }
.lobby-row-name {
  flex: 0 0 auto; max-width: 100%;
  font-size: clamp(12px, 1.9vw, 14px); font-weight: 700;
  color: var(--text, #2B2B2B); white-space: normal; overflow-wrap: anywhere; word-break: break-word;
}
.lobby-row-tags { display: inline-flex; align-items: center; gap: 4px; flex-shrink: 0; flex-wrap: wrap; min-width: 0; }
.lobby-owner-chip { flex-shrink: 0; }
.lobby-row-desc {
  flex: 1 1 120px; min-width: 60px; font-size: 12.5px;
  color: var(--text-muted, #5E5C5F); white-space: nowrap;
  overflow: hidden; text-overflow: ellipsis;
}
.lobby-row-metrics {
  display: inline-flex; align-items: center; gap: 6px; flex-shrink: 0; flex-wrap: wrap;
  font-size: 12px; color: var(--text-muted, #5E5C5F); white-space: nowrap;
}
.lobby-metric-sep { opacity: 0.5; }
.lobby-row-date { margin-left: 2px; font-size: 11px; opacity: 0.85; }
.lobby-row-actions { display: inline-flex; align-items: center; gap: 6px; flex-shrink: 0; flex-wrap: wrap; }
@media (max-width: 899px) {
  .lobby-row-desc {
    flex-basis: 100%; white-space: normal;
    display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical;
  }
  .lobby-row-metrics { margin-left: auto; }
}
@media (max-width: 640px) {
  .lobby-row-desc { display: none; }
}
.lobby-detail {
  display: flex; flex-direction: column; gap: 8px; padding: 10px 12px;
  border-radius: var(--radius-sm, 8px); background: var(--bg-code, #fafafa);
  cursor: default;
}
.lobby-detail-head { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.lobby-readme {
  margin: 0; font-size: 12.5px; line-height: 1.55; color: var(--text, #2B2B2B);
  white-space: pre-wrap; word-break: break-word; max-height: 200px; overflow: auto;
  font-family: 'Ubuntu Mono', Consolas, monospace;
}
.lobby-lastcommit { word-break: break-all; }
.clone-row { display: flex; align-items: center; gap: 6px; }
.clone-url {
  flex: 1; min-width: 0; font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11px;
  color: var(--text-muted, #5E5C5F); background: var(--bg-code, #fafafa);
  padding: 4px 8px; border-radius: var(--radius-sm, 6px); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
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
</style>
