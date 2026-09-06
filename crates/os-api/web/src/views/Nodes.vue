<!--
  Nodes.vue —— 节点发现页（三段分组：LAN 邻居 + P2P/WAN 远端 + 非活跃节点，os-p2p 真实数据）
  数据来源：os-api GET /api/v1/nodes/combined（NodeViewRouteHandler 聚合）：
    - lan：underlay 为私网地址（10/172.16-31/192.168）的直连 peer；
    - p2p：公网/NAT peer + Kademlia 桶非直连节点（source=peer/bucket）；
    - inactive：meta 组件判定非活跃的节点（五振出局，不混入 lan/p2p）——
      每条带「手动心跳」按钮（POST /api/v1/p2p/node-meta/:id/reactivate），
      探活成功即复活回活跃组；
    - self：本机 NodeID/昵称/角色/监听（P2P 未启用时仅 hostname 兜底）；
    - ladder：连接阶梯统计（P2P 卡片底部小字）；
    - 每节点行元数据富化字段（os-p2p meta 组件——节点存活判定的唯一账本）：
      status（活跃徽章）/ score（健康分）/ last_seen（最近在线相对时间）/
      meta_source（direct=本机直连观测 / gossip=他节点转述）；lan/p2p 组内按
      score 降序展示（心跳一直正常的排前面）；
    - 每节点行 im_public：对方 IM 大厅开放状态（P2P 探针缓存，默认不允许）——
      true → 「💬 进入 IM」可点（跳 /chat?node=<id>&name=<名>，Chat.vue 据此
      在会话列表创建/激活「🏨 <名> 的大厅」独立会话项）；
      false → 对方未开放（按钮灰）；null → 查询在途 / 桶短 ID 无从寻址。
  底部「手动添加节点」控制台：输入 ip:port（缺省端口 7070 由后端补）→
  POST /api/v1/p2p/add-peer 按地址直拨（bootstrap 拨号同款路径），成功后
  立即刷新节点列表（需 admin 令牌）。
  10s 自动刷新（沿用页面惯例）。
-->
<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { ApiError, endpoints } from '@/api/client';
import type {
  CombinedInactiveNode,
  CombinedLanNode,
  CombinedNodes,
  CombinedP2pNode,
} from '@/api/client';
import { identiconSvg, shortIdentity } from '@/composables/useIdenticon';

const REFRESH_MS = 10_000;
const router = useRouter();

const combined = ref<CombinedNodes | null>(null);
const loading = ref(false);
const errorMsg = ref('');
const lastUpdated = ref<number | null>(null);
const autoRefresh = ref(true);
let timer: number | null = null;

async function load(): Promise<void> {
  loading.value = true;
  errorMsg.value = '';
  try {
    // combined 已富化 meta 字段（status/score/last_seen/meta_source + inactive
    // 分组）——一次拉全页；p2pNodeMeta 端点留作调试/后续消费，不在此轮询。
    combined.value = await endpoints.nodeCombined();
    lastUpdated.value = Date.now();
  } catch (e) {
    errorMsg.value = errMsg(e);
  } finally {
    loading.value = false;
  }
}

function startTimer(): void {
  stopTimer();
  if (autoRefresh.value) {
    timer = window.setInterval(load, REFRESH_MS);
  }
}

function stopTimer(): void {
  if (timer !== null) {
    window.clearInterval(timer);
    timer = null;
  }
}

function toggleAuto(): void {
  autoRefresh.value = !autoRefresh.value;
  startTimer();
}

// —— 视图切片（combined 缺失时按空态渲染）——

/** 组内按健康分降序展示（心跳一直正常的排前面；同分保持后端次序）。 */
function sortByScore<T extends { score: number }>(list: T[]): T[] {
  return [...list].sort((a, b) => b.score - a.score);
}

const lanNodes = computed<CombinedLanNode[]>(() =>
  sortByScore(combined.value?.lan ?? []),
);
const p2pNodes = computed<CombinedP2pNode[]>(() =>
  sortByScore(combined.value?.p2p ?? []),
);
const inactiveNodes = computed<CombinedInactiveNode[]>(
  () => combined.value?.inactive ?? [],
);
const selfInfo = computed(() => combined.value?.self ?? null);
const ladder = computed(() => combined.value?.ladder ?? null);

/** 自机展示名：昵称 → 主机名 → '（未命名）'。 */
const selfName = computed(
  () => selfInfo.value?.name || selfInfo.value?.hostname || '（未命名）',
);
/** 自机头像：优先 NodeID（同身份恒同图），P2P 未启用时退回主机名。 */
const selfIdenticon = computed(() =>
  identiconSvg(selfInfo.value?.node_id || selfInfo.value?.hostname || 'self', 32),
);

const lanOnline = computed(() => lanNodes.value.filter((n) => n.connected).length);
const p2pOnline = computed(() => p2pNodes.value.filter((n) => n.connected).length);

/** 阶梯小字：Direct X / Punched Y / Relayed Z（失败计数并列展示）。 */
const ladderLabel = computed(() => {
  const l = ladder.value;
  if (!l) return '';
  return `Direct ${l.direct} / Punched ${l.punched} / Relayed ${l.relayed}` +
    (l.punch_failed > 0 ? ` / 打洞失败 ${l.punch_failed}` : '');
});

const lastUpdatedLabel = computed(() => {
  if (!lastUpdated.value) return '';
  return new Date(lastUpdated.value).toLocaleTimeString();
});

// —— 元数据富化展示（meta 组件字段：状态徽章 / 分数 / 最近在线 / 来源）——

/** last_seen（unix 秒）→ 相对时间（"刚刚 / N 分钟前 / N 小时前 / N 天前"）。 */
function relTime(ts: number): string {
  if (!ts) return '从未';
  const sec = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (sec < 60) return '刚刚';
  if (sec < 3600) return `${Math.floor(sec / 60)} 分钟前`;
  if (sec < 86_400) return `${Math.floor(sec / 3600)} 小时前`;
  return `${Math.floor(sec / 86_400)} 天前`;
}

/** 分数徽章配色：≥80 绿（一直健康）/ 50-79 中性 / <50 黄（近期掉分）/ 0 灰（未评分）。 */
function scoreClass(score: number): string {
  if (score >= 80) return 'score-hi';
  if (score >= 50) return 'score-mid';
  return score > 0 ? 'score-lo' : 'score-none';
}

/** 元数据来源徽标文案：direct=本机直连观测 / gossip=他节点转述。 */
function metaSourceLabel(src: 'direct' | 'gossip' | null): string {
  if (src === 'direct') return '直连观测';
  if (src === 'gossip') return '交互转述';
  return '';
}

function errMsg(e: unknown): string {
  return e instanceof ApiError || e instanceof Error ? e.message : String(e);
}

// —— 「进入 IM」按钮（跳转对方 IM 大厅的远程 Tab，联邦互联在 Chat 页完成）——

/** 全量 NodeID（0x + 66 hex）可经 P2P 寻址查询；Kademlia 桶短式（0x1234…cdef）不可。 */
function isFullNodeId(id: string): boolean {
  return /^0x[0-9a-fA-F]{66}$/.test(id);
}

/** 按钮四态：open=可进 / denied=对方未开放 / loading=查询在途 / unknown=短 ID 不可查。 */
type ImBtnState = 'open' | 'denied' | 'loading' | 'unknown';

function imBtnState(n: { node_id: string; im_public: boolean | null }): ImBtnState {
  if (n.im_public === true) return 'open';
  if (n.im_public === false) return 'denied';
  return isFullNodeId(n.node_id) ? 'loading' : 'unknown';
}

function imBtnTitle(n: { node_id: string; im_public: boolean | null }): string {
  switch (imBtnState(n)) {
    case 'open':
      return `进入 IM 查看 ${shortIdentity(n.node_id)} 的大厅（只读镜像 + 远程发言）`;
    case 'denied':
      return '对方未开放 IM 大厅';
    case 'loading':
      return '正在查询对方 IM 大厅开放状态…（下轮刷新可见）';
    default:
      return '未直连节点（Kademlia 桶短 ID），无法查询其 IM 大厅';
  }
}

/** 跳转 IM 页（Chat.vue 检测 ?node= 创建/激活「<名> 的大厅」会话项）。 */
function openIm(nodeId: string): void {
  void router.push({ path: '/chat', query: { node: nodeId, name: shortIdentity(nodeId) } });
}

// —— 非活跃节点「手动心跳」（POST /api/v1/p2p/node-meta/:id/reactivate）——

/** 各节点探测 busy 态（node_id → 探测中；按钮锁定防重复提交）。 */
const heartbeatBusy = ref<Record<string, boolean>>({});
/** 结果提示（空串隐藏）：成功=复活回活跃组并刷新；失败=不可达。 */
const hbMsg = ref('');
const hbOk = ref<boolean | null>(null);

/** 手动触发元数据心跳：Inactive → Active{score:30} 并立即探测一次。
 *  探活成功 → 提示 + 刷新（条目回到 lan/p2p 活跃组）；失败 → 不可达提示。 */
async function heartbeat(n: CombinedInactiveNode): Promise<void> {
  if (heartbeatBusy.value[n.node_id]) return;
  heartbeatBusy.value = { ...heartbeatBusy.value, [n.node_id]: true };
  hbOk.value = null;
  hbMsg.value = `正在探测 ${shortIdentity(n.node_id)}…（逐地址 TCP 探测，最长数秒）`;
  try {
    const r = await endpoints.p2pNodeMetaReactivate(n.node_id);
    if (r.probed) {
      hbOk.value = true;
      hbMsg.value = `${shortIdentity(n.node_id)} 探活成功——节点已复活并回到活跃组`;
      await load();
    } else {
      hbOk.value = false;
      hbMsg.value = `${shortIdentity(n.node_id)} 不可达（探测未通过，保持非活跃）`;
    }
  } catch (e) {
    hbOk.value = false;
    hbMsg.value = `心跳请求失败: ${errMsg(e)}`;
  } finally {
    heartbeatBusy.value = { ...heartbeatBusy.value, [n.node_id]: false };
  }
}

// —— 手动添加节点控制台（POST /api/v1/p2p/add-peer 按地址直拨）——

/** 输入框值（ip:端口；无端口默认 7070 由后端补全）。 */
const manualAddr = ref('');
/** 连接中 busy 态（拨号+握手可耗时数秒，输入/按钮锁定防重复提交）。 */
const manualBusy = ref(false);
/** 结果提示（空串隐藏）；manualOk=null 表示"连接中"中性态。 */
const manualMsg = ref('');
const manualOk = ref<boolean | null>(null);

/** 按地址拨号：空输入忽略；成功清输入 + 立即刷新节点列表，失败展示原因。 */
async function addPeer(): Promise<void> {
  const addr = manualAddr.value.trim();
  if (!addr || manualBusy.value) return;
  manualBusy.value = true;
  manualMsg.value = `正在连接 ${addr}…（拨号 + 加密握手，最长数秒）`;
  manualOk.value = null;
  try {
    const r = await endpoints.p2pAddPeer(addr);
    manualOk.value = true;
    manualMsg.value =
      r.note === 'already-connected'
        ? `${r.addr} 已连接（${shortIdentity(r.node_id)}）`
        : `已连接 ${shortIdentity(r.node_id)}（${r.addr}）——节点已入列表`;
    manualAddr.value = '';
    await load();
  } catch (e) {
    manualOk.value = false;
    manualMsg.value = `连接失败: ${errMsg(e)}`;
  } finally {
    manualBusy.value = false;
  }
}

onMounted(async () => {
  await load();
  startTimer();
});

onUnmounted(stopTimer);
</script>

<template>
  <div class="nodes-page">
    <div class="page-head">
      <div>
        <h2>节点发现</h2>
        <div class="page-sub">
          LAN 邻居 + P2P/WAN 远端（os-p2p 真实数据）
          <span v-if="lastUpdatedLabel" class="muted small">
            · 最近更新 {{ lastUpdatedLabel }}
          </span>
        </div>
      </div>
      <div class="row-gap-sm">
        <label class="form-check muted small" style="flex-direction: row">
          <input
            v-model="autoRefresh"
            type="checkbox"
            @change="toggleAuto"
          />
          自动刷新
        </label>
        <button class="btn" :disabled="loading" @click="load">
          ↻ 刷新
        </button>
      </div>
    </div>

    <div v-if="loading && !combined" class="loading">加载中...</div>
    <div v-else-if="errorMsg && !combined" class="error">
      加载失败: {{ errorMsg }}
    </div>

    <template v-else>
      <!-- 自机条：本机 NodeID + 昵称 + 角色 + 监听地址 -->
      <div class="card self-bar">
        <img class="self-avatar" :src="selfIdenticon" alt="" />
        <div class="self-main">
          <div class="self-name-row">
            <span class="self-name">{{ selfName }}</span>
            <span class="muted small">本机</span>
            <span
              v-if="selfInfo?.enabled"
              :class="[
                'badge',
                selfInfo.role === 'hub' ? 'badge-info' : 'badge-muted',
              ]"
            >
              {{ selfInfo.role === 'hub' ? 'hub' : 'edge' }}
            </span>
            <span
              v-else
              class="badge badge-warn"
              title="os-api 设 NEXOS_P2P_ENABLE=1 并重启后，本机成为对等节点"
            >
              P2P 未启用
            </span>
          </div>
          <div class="muted mono small self-id-row">
            <template v-if="selfInfo?.node_id">
              {{ shortIdentity(selfInfo.node_id) }}
              <span class="self-listen">· 监听 {{ selfInfo.listen || '—' }}</span>
            </template>
            <template v-else>
              {{ selfInfo?.hostname || '—' }}（设 NEXOS_P2P_ENABLE=1 启用组网身份）
            </template>
          </div>
        </div>
      </div>

      <!-- 双卡片：局域网节点（绿）+ P2P 网络（蓝） -->
      <div class="zone-grid">
        <!-- 卡片 1：局域网节点 -->
        <section class="card zone-card zone-lan">
          <header class="zone-head">
            <span class="zone-icon icon-lan" aria-hidden="true">
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <path d="M3 11.5 12 4l9 7.5" />
                <path d="M5.5 10v9h13v-9" />
                <path d="M10 19v-5h4v5" />
              </svg>
            </span>
            <h3 class="zone-title">局域网节点</h3>
            <span class="muted small">
              {{ lanNodes.length }} 台 · 在线 {{ lanOnline }}
            </span>
          </header>

          <ul v-if="lanNodes.length" class="node-list">
            <li v-for="n in lanNodes" :key="n.node_id" class="node-row">
              <img class="node-avatar" :src="identiconSvg(n.node_id, 28)" alt="" />
              <div class="node-main">
                <div class="mono node-title" :title="n.node_id">
                  {{ shortIdentity(n.node_id) }}
                </div>
                <div class="muted mono small">{{ n.addr }}</div>
                <div class="muted small meta-line">
                  <span class="status-badge" :title="`元数据状态：${n.status}（os-p2p meta 组件账本）`">
                    <span class="dot dot-on meta-dot"></span>活跃
                  </span>
                  <span class="score-badge" :class="scoreClass(n.score)">
                    {{ n.score > 0 ? `${n.score} 分` : '未评分' }}
                  </span>
                  <span :title="`last_seen: ${n.last_seen}`">
                    最近在线 {{ relTime(n.last_seen) }}
                  </span>
                  <span v-if="n.meta_source">{{ metaSourceLabel(n.meta_source) }}</span>
                </div>
              </div>
              <span
                :class="['dot', n.connected ? 'dot-on' : 'dot-off']"
                :title="n.connected ? '已连接' : '未连接'"
              ></span>
              <span class="node-state muted small">
                {{ n.connected ? '在线' : '离线' }}
              </span>
              <span
                :class="['badge', n.role === 'hub' ? 'badge-info' : 'badge-muted']"
              >
                {{ n.role }}
              </span>
              <button
                class="btn btn-small im-btn"
                :class="{ 'im-open': imBtnState(n) === 'open' }"
                type="button"
                :disabled="imBtnState(n) !== 'open'"
                :title="imBtnTitle(n)"
                @click="openIm(n.node_id)"
              >
                💬 进入 IM
              </button>
            </li>
          </ul>
          <div v-else class="zone-empty muted center">
            未发现局域网节点——确保同网段设备运行 NexOS
          </div>
        </section>

        <!-- 卡片 2：P2P 网络 -->
        <section class="card zone-card zone-p2p">
          <header class="zone-head">
            <span class="zone-icon icon-p2p" aria-hidden="true">
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <circle cx="12" cy="12" r="8.5" />
                <path d="M3.5 12h17M12 3.5c2.6 2.3 4 5.3 4 8.5s-1.4 6.2-4 8.5c-2.6-2.3-4-5.3-4-8.5s1.4-6.2 4-8.5z" />
              </svg>
            </span>
            <h3 class="zone-title">P2P 网络</h3>
            <span class="muted small">
              {{ p2pNodes.length }} 节点 · 直连 {{ p2pOnline }}
            </span>
          </header>

          <ul v-if="p2pNodes.length" class="node-list">
            <li v-for="n in p2pNodes" :key="`${n.source}:${n.node_id}`" class="node-row">
              <img class="node-avatar" :src="identiconSvg(n.node_id, 28)" alt="" />
              <div class="node-main">
                <div class="mono node-title" :title="n.node_id">
                  {{ shortIdentity(n.node_id) }}
                </div>
                <div class="muted mono small">
                  {{ n.addr || 'NAT（打洞/中继）' }}
                  <template v-if="n.route_via">
                    · 经 {{ shortIdentity(n.route_via) }}
                  </template>
                </div>
                <div class="muted small meta-line">
                  <span class="status-badge" :title="`元数据状态：${n.status}（os-p2p meta 组件账本）`">
                    <span class="dot dot-on meta-dot"></span>活跃
                  </span>
                  <span class="score-badge" :class="scoreClass(n.score)">
                    {{ n.score > 0 ? `${n.score} 分` : '未评分' }}
                  </span>
                  <span :title="`last_seen: ${n.last_seen}`">
                    最近在线 {{ relTime(n.last_seen) }}
                  </span>
                  <span v-if="n.meta_source">{{ metaSourceLabel(n.meta_source) }}</span>
                </div>
              </div>
              <span
                :class="['dot', n.connected ? 'dot-on' : 'dot-off']"
                :title="n.connected ? '已连接' : '未直连'"
              ></span>
              <span class="node-state muted small">
                {{ n.connected ? '已连接' : '未直连' }}
              </span>
              <span
                v-if="n.public"
                class="badge badge-info"
                title="公网服务节点（bootstrap 锚点 + 中继志愿者）"
              >
                public
              </span>
              <span
                v-if="n.source === 'bucket'"
                class="badge badge-muted"
                title="Kademlia 路由表已知、尚未直连"
              >
                桶
              </span>
              <button
                class="btn btn-small im-btn"
                :class="{ 'im-open': imBtnState(n) === 'open' }"
                type="button"
                :disabled="imBtnState(n) !== 'open'"
                :title="imBtnTitle(n)"
                @click="openIm(n.node_id)"
              >
                💬 进入 IM
              </button>
            </li>
          </ul>
          <div v-else class="zone-empty muted center">
            P2P 网络暂无远端节点——配置 NEXOS_P2P_BOOTSTRAP 连接公网锚点
          </div>

          <footer v-if="ladderLabel" class="zone-foot muted small">
            连接阶梯：{{ ladderLabel }}
          </footer>
        </section>

        <!-- 卡片 3：非活跃节点（meta 组件五振出局——手动心跳可复活） -->
        <section v-if="inactiveNodes.length || hbMsg" class="card zone-card zone-inactive">
          <header class="zone-head">
            <span class="zone-icon icon-inactive" aria-hidden="true">
              <svg
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                stroke-width="1.6"
                stroke-linecap="round"
                stroke-linejoin="round"
              >
                <path d="M20 14.5A8.5 8.5 0 1 1 9.5 4a7 7 0 0 0 10.5 10.5z" />
              </svg>
            </span>
            <h3 class="zone-title">非活跃节点</h3>
            <span class="muted small">{{ inactiveNodes.length }} 台 · 五振出局</span>
          </header>

          <ul v-if="inactiveNodes.length" class="node-list">
            <li
              v-for="n in inactiveNodes"
              :key="n.node_id"
              class="node-row node-row-inactive"
            >
              <img class="node-avatar avatar-dim" :src="identiconSvg(n.node_id, 28)" alt="" />
              <div class="node-main">
                <div class="mono node-title" :title="n.node_id">
                  {{ shortIdentity(n.node_id) }}
                </div>
                <div class="muted mono small">
                  {{ n.addrs[0] || '（无地址档案）' }}
                  <template v-if="n.addrs.length > 1">
                    · 历史 {{ n.addrs.length }} 条
                  </template>
                </div>
                <div class="muted small meta-line">
                  <span class="status-badge status-inactive" title="元数据状态：inactive（五振出局，不再心跳）">
                    <span class="dot dot-off meta-dot"></span>非活跃
                  </span>
                  <span :title="`last_seen: ${n.last_seen}`">
                    最近在线 {{ relTime(n.last_seen) }}
                  </span>
                  <span :title="`since: ${n.since}`">
                    掉线 {{ relTime(n.since) }}
                  </span>
                  <span>{{ metaSourceLabel(n.meta_source) }}</span>
                </div>
              </div>
              <button
                class="btn btn-small hb-btn"
                type="button"
                :disabled="heartbeatBusy[n.node_id]"
                title="手动触发元数据心跳：Inactive → Active{score:30} 并立即探测一次（复活仅此与他节点报告两条路）"
                @click="heartbeat(n)"
              >
                {{ heartbeatBusy[n.node_id] ? '探测中…' : '手动心跳' }}
              </button>
            </li>
          </ul>
          <div v-else class="zone-empty muted center">
            无非活跃节点
          </div>

          <div
            v-if="hbMsg"
            :class="['small add-msg', hbOk === null ? '' : hbOk ? 'add-ok' : 'add-err']"
          >
            {{ hbMsg }}
          </div>
          <footer class="zone-foot muted small">
            连续 5 次心跳失败即移入本组（不再自动探测）；「手动心跳」探活成功即复活，
            或等他节点交互报告其存活。
          </footer>
        </section>
      </div>
    </template>

    <!-- 手动添加节点：按 ip:port 直拨（无端口后端补默认 7070），成功即入上方列表 -->
    <section class="card add-card">
      <header class="zone-head">
        <span class="zone-icon icon-add" aria-hidden="true">
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.6"
            stroke-linecap="round"
          >
            <circle cx="12" cy="12" r="8.5" />
            <path d="M12 8.5v7M8.5 12h7" />
          </svg>
        </span>
        <h3 class="zone-title">手动添加节点</h3>
        <span class="muted small">按地址直拨（ip:端口，缺省 7070）</span>
      </header>
      <div class="add-row">
        <input
          v-model="manualAddr"
          class="add-input mono"
          type="text"
          placeholder="192.0.2.113:7070"
          :disabled="manualBusy"
          @keyup.enter="addPeer"
        />
        <button
          class="btn btn-primary add-btn"
          type="button"
          :disabled="manualBusy || !manualAddr.trim()"
          @click="addPeer"
        >
          {{ manualBusy ? '连接中…' : '连接' }}
        </button>
      </div>
      <div
        v-if="manualMsg"
        :class="[
          'small add-msg',
          manualOk === null ? '' : manualOk ? 'add-ok' : 'add-err',
        ]"
      >
        {{ manualMsg }}
      </div>
      <div class="muted small">
        输入对方 NexOS 的 P2P 监听地址直接拨号建立加密直连（与
        NEXOS_P2P_BOOTSTRAP 引导同款路径）；成功后节点出现在上方列表。
        需 admin 令牌（设置 → API 令牌）。
      </div>
    </section>
  </div>
</template>

<style scoped>
.nodes-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

/* —— 自机条（顶部小条：NodeID + 昵称 + 角色 + 监听）—— */
.self-bar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 12px 16px;
}
.self-avatar {
  width: 32px;
  height: 32px;
  border-radius: 6px;
  flex: none;
}
.self-main {
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.self-name-row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.self-name {
  font-weight: 600;
}
.self-id-row {
  display: flex;
  gap: 4px;
  flex-wrap: wrap;
}

/* —— 双卡片布局 —— */
.zone-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(340px, 1fr));
  gap: 16px;
  align-items: start;
}
.zone-card {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.zone-head {
  display: flex;
  align-items: center;
  gap: 10px;
}
.zone-title {
  font-size: 15px;
  font-weight: 700;
}
/* 主题图标：LAN 绿 / P2P 蓝（圆形底 + currentColor 描边图形） */
.zone-icon {
  width: 34px;
  height: 34px;
  border-radius: 50%;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: none;
}
.zone-icon svg {
  width: 18px;
  height: 18px;
}
.icon-lan {
  color: var(--ok);
  background: rgba(14, 132, 32, 0.12);
}
.icon-p2p {
  color: var(--info);
  background: rgba(51, 82, 128, 0.12);
}
.icon-inactive {
  color: #8a8f98;
  background: rgba(138, 143, 152, 0.14);
}
/* 卡片顶部主题色细线（区分三卡视觉锚点） */
.zone-lan {
  border-top: 3px solid var(--ok);
}
.zone-p2p {
  border-top: 3px solid var(--info);
}
.zone-inactive {
  border-top: 3px solid var(--border);
}

/* —— 节点行：identicon + NodeID 缩略 + 地址 + 状态点 + 徽标 —— */
.node-list {
  list-style: none;
  display: flex;
  flex-direction: column;
}
.node-row {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 8px 2px;
}
.node-row + .node-row {
  border-top: 1px solid var(--border-soft);
}
.node-avatar {
  width: 28px;
  height: 28px;
  border-radius: 5px;
  flex: none;
}
.node-main {
  min-width: 0;
  flex: 1;
  display: flex;
  flex-direction: column;
  gap: 1px;
}
.node-title {
  font-size: 13px;
}
.node-state {
  flex: none;
}

/* 在线状态点：绿=已连接，灰=未连接/未直连 */
.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex: none;
}
.dot-on {
  background: var(--ok);
  box-shadow: 0 0 0 3px rgba(14, 132, 32, 0.15);
}
.dot-off {
  background: var(--border);
}

/* —— 元数据富化行（meta 组件字段：状态徽章 / 分数 / 最近在线 / 来源）—— */
.meta-line {
  display: flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  margin-top: 1px;
}
.meta-dot {
  width: 6px;
  height: 6px;
  box-shadow: none;
}
/* 状态徽章：active=绿点「活跃」/ inactive=灰点「非活跃」 */
.status-badge {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}
/* 分数徽章配色：≥80 一直健康（绿）/ 50-79 中性 / <50 近期掉分（黄）/ 0 未评分（灰） */
.score-badge {
  display: inline-block;
  padding: 0 6px;
  border-radius: 8px;
  font-size: 11px;
  line-height: 18px;
  border: 1px solid var(--border-soft);
}
.score-hi {
  color: var(--ok);
  border-color: rgba(14, 132, 32, 0.4);
  background: rgba(14, 132, 32, 0.08);
}
.score-mid {
  color: inherit;
  opacity: 0.9;
}
.score-lo {
  color: var(--warn);
  border-color: rgba(249, 155, 17, 0.45);
  background: rgba(249, 155, 17, 0.08);
}
.score-none {
  opacity: 0.6;
}
/* 非活跃行：头像压暗 +「手动心跳」按钮 */
.avatar-dim {
  opacity: 0.55;
}
.hb-btn {
  flex: none;
  padding: 3px 10px;
  font-size: 12px;
}

/* 「进入 IM」按钮：open 态 accent 描边可点；其余灰置禁用（title 说明原因） */
.im-btn {
  flex: none;
  padding: 3px 10px;
  font-size: 12px;
}
.im-btn.im-open:not(:disabled) {
  border-color: var(--info);
  color: var(--info);
}

/* 空态 / 阶梯小字 */
.zone-empty {
  padding: 24px 12px;
  font-size: 13px;
  line-height: 1.6;
}
.zone-foot {
  border-top: 1px solid var(--border-soft);
  padding-top: 8px;
}

/* —— 手动添加节点控制台（底部卡片：地址输入 + 连接按钮 + 结果提示）—— */
.add-card {
  display: flex;
  flex-direction: column;
  gap: 10px;
  border-top: 3px solid var(--accent);
}
.icon-add {
  color: var(--accent);
  background: var(--accent-soft);
}
.add-row {
  display: flex;
  gap: 10px;
}
.add-input {
  flex: 1;
  min-width: 0;
  padding: 8px 12px;
  font-size: 13px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-card);
  color: inherit;
}
.add-input:focus {
  outline: none;
  border-color: var(--accent);
  box-shadow: 0 0 0 2px var(--accent-soft);
}
.add-input:disabled {
  opacity: 0.55;
}
.add-btn {
  flex: none;
  white-space: nowrap;
}
.add-msg {
  line-height: 1.5;
}
.add-ok {
  color: var(--ok);
}
.add-err {
  color: var(--err);
}
</style>
