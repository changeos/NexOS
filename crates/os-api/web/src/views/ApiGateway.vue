<script setup lang="ts">
// =============================================================================
// ApiGateway.vue —— LLM API 网关（One API 风格）
//
// 信息架构（2026-09-03 两级化，照 LlmModels「推理｜仓库｜诊断」成功模式）：
// 一级组「网关｜大厅｜运营」3 项 + 组内二级 Tab（虚线下边线 sub-tabs）——
//   网关（运行核心，默认组）：总览* / 渠道 / 实例 / 令牌 / 日志
//   大厅（对外经济）：API 大厅（本地/联邦）/ 我的发布*（owner 操作集中地）
//   运营：充值订单 / 接入说明
//   *总览 = 默认落地 Tab（统计卡 + 快捷入口 + 最近调用摘要；既有 stats/
//    channels/tokens/logs/payments 端点拼装，无新后端）；
//   *我的发布 = 从大厅卡片 owner 操作位（心跳/推送联邦/下架/接入信息）集中
//    拆出，复用大厅同一份 marketListings 数据与操作函数（不复制逻辑）。
// 深链：?tab=<旧 TabKey> 反查所属组直达（TAB_GROUP_OF，照 /llm 先例）；
//   ?tab=<组名>（gateway/lobby/ops）落该组首个子 Tab；缺省/非法回落总览。
//
// 后端：/api/v1/gateway/* （ApiGatewayRouteHandler，已在线）
//       /api/v1/api-market/*（ApiMarketRouteHandler，推理服务市场）
//
// 「实例监控」Tab（2026-08-30）：复用 components/InstanceMonitor.vue 可复用组件
// （与 LlmModels.vue「实例监控」Tab 同一组件）——网关运营者在此直看本机哪些
// vLLM 实例在跑、健康状态、以及网关可路由模型聚合（真实探测 /v1/models，
// gateway_visible / unreachable 两组）。组件数据自包含，宿主零接线。
//
// 添加渠道「从本地发现导入」（2026-08-30 真实化）：对话框打开时拉
// GET /api/v1/llm/gateway/models（实例表 running 实测 + 8123/8000-8010 端口
// 扫描发现），列 gateway_visible 条目（端口/模型数/是否扫描发现），点击预填
// base_url=http://127.0.0.1:<port>/v1 + models=model_ids + provider=local-vllm；
// 手填路径完整保留（预填后可任意改）。提交走完整 body；后端另支持
// {from_discovery:{port}} 由服务端实测填充（脚本直连用）。
//
// 设计：Ubuntu Yaru 风格 .card / .page-head，统计卡 + 表格 + 对话框，三态加载。
// 代理转发不强求上游真实在线（失败降级记日志），令牌创建后显示完整 key（只一次）。
// 计费：令牌支持 free/per_token/per_image/credits 四种模式，配额单位=积分（free 为 ∞）。
//
// API 大厅（docs/API_MARKET.md）：卡片流市场页（搜索/价格排序/发布）+ 实时负载
// 徽章（每卡片错峰拉 /:id/metrics，大厅 Tab 活跃时 15s 自动轮询、切出/隐藏暂停）
// + 本机条目 60s 自动心跳上报 + 链上身份发布对话框（发布者=区块链公钥，
// 复用 useChainIdentity 的 nexhub token，无 admin 回落）。
//
// 徽章语义分流（2026-09-03，修「明明可以调用却显示不可达」）：
// - 本地条目：负载徽章=直连探测（绿=心跳≤60s / 灰=metrics_url 代拉降级 /
//   红=不可达）——对发布者自己有意义的探测，语义保留；
// - 联邦条目：**不再显示「不可达」**——改「🌐 经源节点中继」常驻徽章（中继
//   路径可达性由消费行为证明：调用即通，不做主动探测）+ 独立「源节点心跳：
//   N 分钟前」时间差行（无心跳 '--' 不猜；快照可见性 ≤30min 联邦重播周期，
//   服务端另有 60s 常驻心跳兜底，见 api_market.rs refresh_local_heartbeats）；
//   联邦条目相应跳过 metrics 轮询（不再主动探测远程）。
//
// 条目卡分区（2026-09-03 重排，修「标签/值一行一条」纵向堆叠）：
// ① 主区：标题行（名称+价格+🌐来源+状态徽章**同行不折行**，名称超长 ellipsis）
//    + 描述两行 clamp（点卡片展开全文）+ 联邦心跳时间差行 + tags 行内小 pill
// ② 硬件/规格区：真两列自适应网格 repeat(auto-fill, minmax(180px,1fr))——每格
//    标签小字灰色上标 / 值主体（换行不截断）；GPU 型号+显存/统一内存同格合并；
//    上下文=context_len 优先回落 max_model_len，皆缺=—不猜
// ③ 接入信息折叠面板（details/summary 默认收起）：密钥/鉴权头（深色行内代码）
//    + curl 示例块（MD 代码块样式：等宽/深色底/横滚/右上复制——ob-*/acc-* 先例）；
//    curl 鉴权头两分支：明文视角拼真实 key / 脱敏视角拼 <你的令牌> 占位符并附
//    索取说明（与后端 curl_auth_header_line 同一契约）
// ④ 操作区（底部单行不折行：发布者超长 ellipsis/打赏/下载计数/时间右贴齐）
// =============================================================================
import { computed, onMounted, onUnmounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { useRoute } from 'vue-router';
import DataTable from '@/components/DataTable.vue';
import InstanceMonitor from '@/components/InstanceMonitor.vue';
import type { Column } from '@/components/data-table';
import { ApiError, endpoints } from '@/api/client';
import { useChainIdentity } from '@/composables/useChainIdentity';
import { identiconSvg, shortIdentity } from '@/composables/useIdenticon';
import { copyText } from '@/utils/clipboard';
// 统一打赏按钮（docs/TIPS.md：API 大厅条目打赏，target_kind=lobby_entry，ref=apimarket:<id>）
import TipButton from '@/components/TipButton.vue';

const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
type Provider =
  | 'openai'
  | 'deepseek'
  | 'anthropic'
  | 'local-vllm'
  | 'azure'
  | 'ollama'
  | string;

interface Channel {
  id?: string;
  name?: string;
  provider?: Provider;
  base_url?: string;
  api_key?: string;
  models?: string[];
  priority?: number;
  weight?: number;
  status?: 'enabled' | 'disabled' | 'error' | string;
  enabled?: boolean;
  created_at?: string;
  last_used?: string | null;
  request_count?: number;
  /** 联邦中继来源 NodeID（非空 = 中继渠道：转发经 overlay 定向源节点代发）。 */
  via_node?: string;
  [k: string]: unknown;
}
type BillingMode = 'free' | 'per_token' | 'per_image' | 'credits' | string;
type PayCurrency = 'usdt' | 'btc' | 'evm' | string;

interface ApiToken {
  id?: string;
  name?: string;
  key?: string;
  status?: 'active' | 'disabled' | 'expired' | string;
  enabled?: boolean;
  billing_mode?: BillingMode;
  quota_limit?: number;
  quota_used?: number;
  allowed_models?: string[];
  allowed_channels?: string[];
  expires_at?: string | null;
  created_at?: string;
  last_used?: string | null;
  request_count?: number;
  [k: string]: unknown;
}

/** 充值订单（计费单位=积分；amount_crypto 单位随币种：usdt 两位小数 / btc 聪 / evm wei）。 */
interface PaymentOrder {
  id?: string;
  token_id?: string;
  status?: 'pending' | 'confirmed' | 'rejected' | string;
  currency?: PayCurrency;
  amount_crypto?: number | string;
  credits?: number;
  address?: string;
  memo?: string | null;
  warning?: string | null;
  txid?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface CallLog {
  id?: string;
  token_id?: string;
  token_name?: string;
  channel_id?: string;
  channel_name?: string;
  model?: string;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  latency_ms?: number;
  status?: 'success' | 'failed' | 'timeout' | string;
  error?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface ModelMapping {
  public_name?: string;
  channel_id?: string;
  upstream_model?: string;
  [k: string]: unknown;
}
interface GatewayStats {
  channels_total?: number;
  channels_enabled?: number;
  tokens_total?: number;
  tokens_active?: number;
  total_requests?: number;
  total_tokens?: number;
  success_rate?: number;
  [k: string]: unknown;
}

// —— API 大厅（docs/API_MARKET.md；GET /api/v1/api-market 数组元素，15 列）——
/** 挂牌 server_config 的 GPU 条目（探测带 index；发布简化形态省略）。 */
interface MarketGpuEntry {
  index?: number | null;
  name?: string | null;
  /** 独立显存 MiB；统一内存架构（GB10/Jetson，nvidia-smi 报 [N/A]）→ null。 */
  vram_mb?: number | null;
  /** 统一内存架构标记（CPU/GPU 共享 LPDDR5x 池；探测侧显存 N/A 时 true）。 */
  unified_memory?: boolean | null;
  /** 统一内存池总量 MiB（/proc/meminfo MemTotal；unified 时探测填）。 */
  unified_vram_mb?: number | null;
}
/** 挂牌 server_config（硬件探测 + 发布覆盖；null=未知，UI 显示占位不猜）。 */
interface MarketServerConfig {
  gpu_name?: string | null;
  /** 首卡显存镜像（GB10 首卡 vram null → 镜像 null，真值在 gpus[0].unified_vram_mb）。 */
  gpu_vram_mb?: number | null;
  /** GPU 数量（探测=gpus.length；无卡=0——CPU-only 节点可发布）。 */
  gpu_count?: number | null;
  /** 全部 GPU（同型号多卡=多条目，index 区分）。 */
  gpus?: MarketGpuEntry[] | null;
  /** CPU 型号（/proc/cpuinfo model name；aarch64 回退 lscpu，如 Cortex-X925 + Cortex-A725）。 */
  cpu_model?: string | null;
  cpu_cores?: number | null;
  ram_gb?: number | null;
  model_name?: string | null;
  max_model_len?: number | null;
  /** 上下文长度（发布端自报别名，2026-09-02 起后端透传；展示优先本字段，缺省回落 max_model_len）。 */
  context_len?: number | null;
  quantization?: string | null;
  region?: string | null;
}
/** 挂牌 pricing（单价格字段；per_image 的 price_per_1k_tokens 语义=每图单价）。 */
interface MarketPricing {
  mode?: 'free' | 'per_token' | 'per_image' | string;
  price_per_1k_tokens?: number | null;
  currency?: string;
  note?: string | null;
}
/** 挂牌接入信息（access_info；api_key 按视角脱敏——本人/admin 明文，其他 `<前4>***<后4>`）。 */
interface MarketAccessInfo {
  api_key?: string;
  /** 鉴权头用法（缺省 Authorization Bearer；自定义如 `X-Api-Key: <key>`）。 */
  auth_header?: string;
  notes?: string;
}
/** 大厅挂牌条目。 */
interface MarketListing {
  id?: string;
  api_name?: string;
  description?: string;
  endpoint_url?: string;
  /** 发布者压缩公钥（0x+66hex，token 反查）——与本地身份比对识别「本机已发布」。 */
  publisher_pubkey?: string;
  /** 发布者 EVM 展示名（0x+40hex）。 */
  publisher_display?: string;
  server_config?: MarketServerConfig;
  pricing?: MarketPricing;
  metrics_url?: string | null;
  tags?: string[];
  status?: string;
  created_at?: string;
  heartbeat_at?: string | null;
  load?: Record<string, number | null> | null;
  download_count?: number;
  /** 联邦来源节点：本地条目 'local'/缺省；远程条目=发布节点名（联邦 Tab 分流）。 */
  source_node?: string;
  /** 发布侧「已推送联邦大厅」标志（两步联邦第二步 POST /:id/federate 置位）。 */
  federated?: boolean;
  /** 消费者接入信息（2026-08-31；无则不出现）。 */
  access_info?: MarketAccessInfo;
  [k: string]: unknown;
}
/** GET /api/v1/api-market/:id/metrics 三态响应（心跳优先→代拉→降级）。 */
interface MarketMetricsResp {
  id?: string;
  reachable?: boolean;
  stale?: boolean;
  source?: 'heartbeat' | 'metrics_url' | 'none' | string;
  metrics?: {
    load_pct?: number | null;
    running?: number | null;
    waiting?: number | null;
    gpu_cache?: number | null;
    tokens_per_sec?: number | null;
    latency_ms?: number | null;
  } | null;
  ts?: string | null;
  error?: string | null;
}

// =============================================================================
// Tab 状态（两级：一级组 网关｜大厅｜运营 + 组内二级 Tab）
// =============================================================================
type TabGroup = 'gateway' | 'lobby' | 'ops';
type TabKey =
  | 'overview'
  | 'channels'
  | 'llmmon'
  | 'tokens'
  | 'logs'
  | 'market'
  | 'mylistings'
  | 'payments'
  | 'access';
/** 一级组（label 走 i18n——gwTab 命名空间，四语言）。 */
const tabGroups: { key: TabGroup; label: string }[] = [
  { key: 'gateway', label: t('gwTab.groupGateway') },
  { key: 'lobby', label: t('gwTab.groupLobby') },
  { key: 'ops', label: t('gwTab.groupOps') },
];
/** 二级 Tab 定义（顺序即展示顺序；总览 = 网关组默认落地 Tab）。 */
const groupTabs: Record<TabGroup, { key: TabKey; label: string }[]> = {
  gateway: [
    { key: 'overview', label: t('gwOverview.tab') },
    { key: 'channels', label: '渠道' },
    { key: 'llmmon', label: '实例' },
    { key: 'tokens', label: '令牌' },
    { key: 'logs', label: '日志' },
  ],
  lobby: [
    { key: 'market', label: 'API 大厅' },
    { key: 'mylistings', label: t('gwMine.tab') },
  ],
  ops: [
    { key: 'payments', label: '充值订单' },
    { key: 'access', label: t('gatewayAccess.tab') },
  ],
};
/** TabKey → 所属组（深链 ?tab=<key> 反查组用；旧 8 Tab key 全部保留兼容）。 */
const TAB_GROUP_OF: Record<TabKey, TabGroup> = {
  overview: 'gateway',
  channels: 'gateway',
  llmmon: 'gateway',
  tokens: 'gateway',
  logs: 'gateway',
  market: 'lobby',
  mylistings: 'lobby',
  payments: 'ops',
  access: 'ops',
};
/** 组名 → 组内首个子 Tab（?tab=<组名> 深链与一级组切换共用）。 */
function firstTabOf(group: TabGroup): TabKey {
  return groupTabs[group][0].key;
}

// 深链支持（照 /llm TAB_GROUP_OF 反查先例）：?tab=<旧 TabKey> 直达对应子 Tab
// （旧 8 Tab key 均有效——channels/tokens/logs/payments/overview/llmmon/
// market/access）；?tab=<组名>（gateway/lobby/ops）落该组首个子 Tab；非法/
// 缺省回落默认（网关组·总览）。仅首载读一次，后续切换不回写 query。
const route = useRoute();
const initialTab = (route.query.tab as string) || '';
const initialTabGroup: TabGroup | undefined = TAB_GROUP_OF[initialTab as TabKey];
const initialIsGroup = tabGroups.some((g) => g.key === initialTab);
const activeGroup = ref<TabGroup>(
  initialIsGroup ? (initialTab as TabGroup) : (initialTabGroup ?? 'gateway'),
);
const activeTab = ref<TabKey>(initialTabGroup ? (initialTab as TabKey) : firstTabOf(activeGroup.value));

/** 一级组切换：落到该组首个子 Tab。 */
function switchGroup(group: TabGroup): void {
  if (activeGroup.value === group) return;
  activeGroup.value = group;
  activeTab.value = firstTabOf(group);
}

/** 子 Tab 切换：同步所属一级组（快捷入口/查看全部等跨组跳转也走这里）。 */
function switchTab(key: TabKey): void {
  activeTab.value = key;
  activeGroup.value = TAB_GROUP_OF[key];
}

/** 计费模式选项（创建令牌下拉，value 对齐后端 billing_mode 枚举）。 */
const billingModeOptions: { value: 'free' | 'per_token' | 'per_image' | 'credits'; label: string; hint: string }[] = [
  { value: 'free', label: '免费', hint: '不计量不计费，任意调用（配额显示 ∞）' },
  { value: 'per_token', label: '按 Token', hint: '按上游实际用量逐 Token 扣减配额（默认）' },
  { value: 'per_image', label: '按生成图', hint: '每次图片生成计一次，按次扣减配额' },
  { value: 'credits', label: '积分', hint: '预付积分池，创建时写入初始积分，调用按积分扣减' },
];

/** 支付币种选项（value 对齐后端 currency 枚举）。 */
const currencyOptions: { value: 'usdt' | 'btc' | 'evm'; label: string }[] = [
  { value: 'usdt', label: 'USDT' },
  { value: 'btc', label: 'BTC' },
  { value: 'evm', label: 'EVM（ETH 系）' },
];

// —— 占位收款地址识别 ——
// 服务端在 env 未配置真实收款钱包时，会返回以下明显假值（与主代理 env 配置一致）。
// UI 命中即醒目警示并禁用复制，防止用户向占位地址真实转账。
const PLACEHOLDER_PAY_ADDRESSES: readonly string[] = [
  'TPLACEHOLDER9USDT9DO9NOT9SENDxxxxxxxxx', // USDT（TRON）
  'bc1qplaceholder9do9not9send9real9btcxxxx', // BTC
  '0x000000000000000000000000000000000000dEaD', // EVM
];

/** 判断订单收款地址是否为占位值（EVM 地址大小写不敏感比对）。 */
function isPlaceholderPayAddress(addr?: string | null): boolean {
  if (!addr) return false;
  const a = addr.trim();
  if (!a) return false;
  return PLACEHOLDER_PAY_ADDRESSES.some((p) => p === a || p.toLowerCase() === a.toLowerCase());
}

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

// =============================================================================
// 数据状态
// =============================================================================
const channels = ref<Channel[]>([]);
const tokens = ref<ApiToken[]>([]);
const logs = ref<CallLog[]>([]);
const stats = ref<GatewayStats>({});
const aggregatedModels = ref<{ models?: string[]; count?: number }>({});
const mappings = ref<ModelMapping[]>([]);
const payments = ref<PaymentOrder[]>([]);

const channelsLoading = ref(false);
const tokensLoading = ref(false);
const logsLoading = ref(false);
const statsLoading = ref(false);
const modelsLoading = ref(false);
const mappingsLoading = ref(false);
const paymentsLoading = ref(false);

const channelsError = ref('');
const tokensError = ref('');
const logsError = ref('');
const mappingsError = ref('');
const paymentsError = ref('');

const busyId = ref('');
const logLimit = ref(50);

// =============================================================================
// 对话框状态
// =============================================================================
const showChannelDialog = ref(false);
const showTokenDialog = ref(false);
const showMappingDialog = ref(false);
const showConfirmPayDialog = ref(false);
const showRejectPayDialog = ref(false);
const editingChannel = ref<Channel | null>(null);
const submitting = ref(false);

// 新建充值订单后醒目展示的收款信息（地址/金额/memo/warning）
const createdPayment = ref<PaymentOrder | null>(null);

// 确认到账 / 拒绝对话框目标订单与输入
const confirmingOrder = ref<PaymentOrder | null>(null);
const rejectingOrder = ref<PaymentOrder | null>(null);
const confirmPayTxid = ref('');
const rejectPayReason = ref('');

const channelForm = ref<{
  name: string;
  provider: Provider;
  base_url: string;
  api_key: string;
  models: string;
  priority: number;
  weight: number;
  via_node: string;
}>({
  name: '',
  provider: 'openai',
  base_url: '',
  api_key: '',
  models: '',
  priority: 0,
  weight: 1,
  via_node: '',
});

// —— 添加渠道：从外部 API 导入（2026-09-03 联邦中继双向打通）——

/** 外部 API 登记行（llm_external_apis 的脱敏形态——导入走后端 from_external_api
 * 复制明文 key，本列表只做选择展示）。 */
interface ExternalApiRow {
  id?: string;
  name?: string;
  base_url?: string;
  models?: string[];
  via_node?: string;
  [k: string]: unknown;
}

/** 外部 API 登记列表（添加渠道对话框打开时拉取）。 */
const externalApis = ref<ExternalApiRow[]>([]);
const externalLoading = ref(false);
/** 拉取失败提示（失败不阻塞手填路径）。 */
const externalError = ref('');
/** 一键导入进行中（逐条禁用）。 */
const importingExtId = ref('');

/** 拉外部 API 登记列表（对话框打开时调用）。 */
async function loadExternalApis(): Promise<void> {
  externalLoading.value = true;
  externalError.value = '';
  try {
    const raw = await endpoints.llmExternalApis();
    // LlmExternalApi（闭合接口）→ ExternalApiRow（带索引签名）需显式收窄：
    // 运行时字段为超集（id/name/base_url/models/via_node 全在），仅类型声明差异。
    externalApis.value = Array.isArray(raw.apis) ? (raw.apis as unknown as ExternalApiRow[]) : [];
  } catch (e) {
    externalApis.value = [];
    externalError.value = e instanceof Error ? e.message : String(e);
  } finally {
    externalLoading.value = false;
  }
}

/** 一键导入：POST /gateway/channels {from_external_api}——后端复制
 * name/base_url/api_key/models/via_node（models 空先探回填），成功后关对话框。 */
async function importFromExternalApi(row: ExternalApiRow): Promise<void> {
  if (!row.id || submitting.value) return;
  importingExtId.value = row.id;
  msg.value = null;
  try {
    const created = (await endpoints.createGatewayChannel({
      from_external_api: row.id,
    })) as { id?: string; name?: string; models?: string[]; warning?: string | null };
    const n = (created.models ?? []).length;
    msg.value = {
      kind: 'ok',
      text:
        `渠道「${created.name ?? row.name ?? row.id}」已导入` +
        (n > 0 ? `（${n} 个模型）` : '') +
        (created.warning ? `；${created.warning}` : ''),
    };
    showChannelDialog.value = false;
    editingChannel.value = null;
    await loadChannels();
    await loadStats();
    await loadModels();
  } catch (e) {
    msg.value = { kind: 'err', text: `导入失败：${e instanceof Error ? e.message : String(e)}` };
  } finally {
    importingExtId.value = '';
  }
}

// —— 添加渠道：从本地发现导入（GET /api/v1/llm/gateway/models 真实探测）——

/** 网关可路由模型聚合的可见条目（gateway_visible 元素；含实例表内与扫描发现）。 */
interface DiscoveryEntry {
  /** 所属实例 id；端口扫描发现的为 null。 */
  instance_id?: string | null;
  name?: string;
  port?: number;
  /** /v1/models 探测出的模型 id 列表（预填 models 用）。 */
  model_ids?: string[];
  /** 实例表内 false / 端口扫描发现 true。 */
  discovered?: boolean;
  [k: string]: unknown;
}

/** 本地发现的 vLLM 列表（添加渠道对话框打开时拉取）。 */
const discoveryEntries = ref<DiscoveryEntry[]>([]);
const discoveryLoading = ref(false);
/** 发现列表拉取失败提示（失败不阻塞手填路径）。 */
const discoveryError = ref('');

/** 拉本地发现列表（gateway/models 真实探测；对话框打开时调用）。 */
async function loadDiscovery(): Promise<void> {
  discoveryLoading.value = true;
  discoveryError.value = '';
  try {
    const raw = await endpoints.llmGatewayModels();
    const resp = raw as { gateway_visible?: DiscoveryEntry[] };
    discoveryEntries.value = Array.isArray(resp.gateway_visible) ? resp.gateway_visible : [];
  } catch (e) {
    discoveryEntries.value = [];
    discoveryError.value = e instanceof Error ? e.message : String(e);
  } finally {
    discoveryLoading.value = false;
  }
}

/** 点击发现条目 → 预填渠道表单（base_url/models/provider/name；手填路径保留，
 * 预填后仍可改）。提交走完整 body（后端也支持 from_discovery，脚本直连可用）。 */
function prefillFromDiscovery(e: DiscoveryEntry): void {
  const port = e.port ?? 0;
  channelForm.value.name = e.name ?? `发现的 vLLM :${port}`;
  channelForm.value.provider = 'local-vllm';
  channelForm.value.base_url = `http://127.0.0.1:${port}/v1`;
  channelForm.value.api_key = '';
  channelForm.value.models = (e.model_ids ?? []).join(', ');
}

const tokenForm = ref<{
  name: string;
  billing_mode: 'free' | 'per_token' | 'per_image' | 'credits';
  quota_limit: number;
  initial_credits: number;
  allowed_models: string;
  expires_at: string;
}>({
  name: '',
  billing_mode: 'per_token',
  quota_limit: 0,
  initial_credits: 0,
  allowed_models: '',
  expires_at: '',
});

/** 当前选中计费模式的一句话说明。 */
const billingModeHint = computed(
  () => billingModeOptions.find((o) => o.value === tokenForm.value.billing_mode)?.hint ?? '',
);

/** 充值订单创建表单。 */
const paymentForm = ref<{ token_id: string; currency: 'usdt' | 'btc' | 'evm'; credits: number }>({
  token_id: '',
  currency: 'usdt',
  credits: 100,
});

const mappingForm = ref<{ public_name: string; channel_id: string; upstream_model: string }>({
  public_name: '',
  channel_id: '',
  upstream_model: '',
});

// =============================================================================
// 表格列定义
// =============================================================================
const channelColumns: Column<Channel>[] = [
  { key: 'name', title: '名称' },
  { key: 'provider', title: 'Provider' },
  { key: 'base_url', title: 'Base URL' },
  { key: 'models', title: '模型' },
  { key: 'priority', title: '优先级·权重' },
  { key: 'status', title: '状态' },
  { key: 'request_count', title: '请求数' },
  { key: 'actions', title: '操作' },
];

const tokenColumns: Column<ApiToken>[] = [
  { key: 'name', title: '名称' },
  { key: 'key', title: 'API Key' },
  { key: 'status', title: '状态' },
  { key: 'billing_mode', title: '计费' },
  { key: 'quota', title: '配额 / 积分余量' },
  { key: 'allowed_models', title: '允许模型' },
  { key: 'request_count', title: '请求数' },
  { key: 'created_at', title: '创建时间' },
  { key: 'actions', title: '操作' },
];

const logColumns: Column<CallLog>[] = [
  { key: 'created_at', title: '时间' },
  { key: 'token_name', title: '令牌' },
  { key: 'channel_name', title: '渠道' },
  { key: 'model', title: '模型' },
  { key: 'prompt_tokens', title: 'Prompt' },
  { key: 'completion_tokens', title: 'Completion' },
  { key: 'total_tokens', title: '总Tokens' },
  { key: 'latency_ms', title: '延迟(ms)' },
  { key: 'status', title: '状态' },
];

const paymentColumns: Column<PaymentOrder>[] = [
  { key: 'created_at', title: '时间' },
  { key: 'token_id', title: '令牌' },
  { key: 'currency', title: '币种' },
  { key: 'amount_crypto', title: '应付金额' },
  { key: 'credits', title: '积分' },
  { key: 'status', title: '状态' },
  { key: 'txid', title: 'TxID' },
  { key: 'actions', title: '操作' },
];

// =============================================================================
// 计算属性
// =============================================================================
const channelErrorCount = computed(
  () => channels.value.filter((c) => c.status === 'error').length,
);
const pendingPaymentCount = computed(
  () => payments.value.filter((p) => p.status === 'pending').length,
);
const confirmedPaymentCount = computed(
  () => payments.value.filter((p) => p.status === 'confirmed').length,
);
const rejectedPaymentCount = computed(
  () => payments.value.filter((p) => p.status === 'rejected').length,
);
const quotaUsagePct = (t: ApiToken): number => {
  const limit = t.quota_limit ?? 0;
  if (!limit) return 0;
  return Math.min(100, Math.round(((t.quota_used ?? 0) / limit) * 100));
};
/** 积分余量（quota_limit - quota_used；无上限时为 null）。 */
function tokenQuotaRemaining(t: ApiToken): number | null {
  if (!t.quota_limit) return null;
  return t.quota_limit - (t.quota_used ?? 0);
}

// =============================================================================
// 总览（默认落地 Tab）：既有端点数据拼装——stats/channels/tokens/logs/
// payments 各取所需，无新后端；口径如实（今日数按已拉取日志窗口统计）。
// =============================================================================
/** 本地时区今日日期键（YYYY-MM-DD；日志 created_at 为 ISO，slice 对齐取日）。 */
function localDateKey(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}
const todayKey = localDateKey(new Date());

/** 今日调用（已拉取日志窗口内 created_at 为今日的条目——窗口=当前每页条数）。 */
const todayLogs = computed(() =>
  logs.value.filter((l) => (l.created_at ?? '').slice(0, 10) === todayKey),
);
const todayCalls = computed(() => todayLogs.value.length);
/** 今日成功率（今日无调用 = null → UI 显示「—」诚实占位）。 */
const todaySuccessRate = computed<number | null>(() => {
  const n = todayLogs.value.length;
  if (!n) return null;
  const ok = todayLogs.value.filter((l) => l.status === 'success').length;
  return Math.round((ok / n) * 100);
});

/** 活跃渠道（enabled）分列：直连（无 via_node）/ 🌐中继（via_node 非空）。 */
const enabledChannels = computed(() =>
  channels.value.filter((c) => channelStatusLabel(c) === 'enabled'),
);
const directChannelCount = computed(() => enabledChannels.value.filter((c) => !c.via_node).length);
const relayChannelCount = computed(() => enabledChannels.value.filter((c) => !!c.via_node).length);

/** 积分流水（充值订单拼装）：已入账 = confirmed 积分合计；待确认 = pending 合计。 */
const settledCredits = computed(() =>
  payments.value
    .filter((p) => p.status === 'confirmed')
    .reduce((s, p) => s + (p.credits ?? 0), 0),
);
const pendingCredits = computed(() =>
  payments.value
    .filter((p) => p.status === 'pending')
    .reduce((s, p) => s + (p.credits ?? 0), 0),
);

/** 最近调用摘要（日志前 5 条；日志 Tab 同源数据）。 */
const recentLogs = computed(() => logs.value.slice(0, 5));

// —— 快捷入口（切到对应子 Tab 再开对话框——对话框渲染在对应 Tab 面板内）——
/** 建渠道：切「渠道」+ 开添加渠道对话框（含本地发现/外部 API 两个导入区）。 */
function quickCreateChannel(): void {
  switchTab('channels');
  openCreateChannel();
}
/** 一键从外部 API 导入：同开添加渠道对话框（外部 API 导入区在表单上方）。 */
function quickImportExternal(): void {
  switchTab('channels');
  openCreateChannel();
}
/** 建令牌：切「令牌」+ 开创建令牌对话框。 */
function quickCreateToken(): void {
  switchTab('tokens');
  openCreateToken();
}
/** 发布 API：切「API 大厅」+ 开发布对话框（发布对话框挂页面根，任意 Tab 可开）。 */
function quickPublishApi(): void {
  switchTab('market');
  openMarketPublish();
}

// =============================================================================
// 辅助：徽章样式
// =============================================================================
function providerClass(p?: string): string {
  switch (p) {
    case 'openai':
      return 'pill pill-ok';
    case 'deepseek':
      return 'pill pill-purple';
    case 'anthropic':
      return 'pill pill-err';
    case 'local-vllm':
      return 'pill pill-cyan';
    case 'azure':
      return 'pill pill-blue';
    case 'ollama':
      return 'pill pill-orange';
    default:
      return 'pill pill-muted';
  }
}
function channelStatusClass(s?: string): string {
  if (s === 'enabled') return 'pill pill-ok';
  if (s === 'error') return 'pill pill-err';
  if (s === 'disabled') return 'pill pill-muted';
  return 'pill pill-muted';
}
function channelStatusLabel(c: Channel): string {
  return c.status ?? (c.enabled ? 'enabled' : 'disabled');
}
function tokenStatusClass(t: ApiToken): string {
  if (!t.enabled || t.status === 'disabled') return 'pill pill-muted';
  if (t.status === 'expired') return 'pill pill-err';
  return 'pill pill-ok';
}
function tokenStatusLabel(t: ApiToken): string {
  return t.status ?? (t.enabled ? 'active' : 'disabled');
}
function logStatusClass(s?: string): string {
  if (s === 'success') return 'pill pill-ok';
  if (s === 'timeout') return 'pill pill-warning';
  return 'pill pill-err';
}

// —— 计费模式 ——
function billingModeOf(t: ApiToken): string {
  return t.billing_mode ?? 'per_token';
}
/** 计费模式 → 标签（按 mode 直取：令牌行/接入信息面板回显共用）。 */
function billingModeLabelOf(mode: string): string {
  switch (mode) {
    case 'free':
      return '免费';
    case 'per_image':
      return '按生成图';
    case 'credits':
      return '积分';
    case 'per_token':
    default:
      return '按 Token';
  }
}
/** 计费模式 → 徽章样式（按 mode 直取，同上共用）。 */
function billingModeClassOf(mode: string): string {
  switch (mode) {
    case 'free':
      return 'pill pill-muted';
    case 'per_image':
      return 'pill pill-purple';
    case 'credits':
      return 'pill pill-orange';
    case 'per_token':
    default:
      return 'pill pill-blue';
  }
}
function billingModeLabel(t: ApiToken): string {
  return billingModeLabelOf(billingModeOf(t));
}
function billingModeClass(t: ApiToken): string {
  return billingModeClassOf(billingModeOf(t));
}

// —— 充值订单 ——
function payStatusClass(s?: string): string {
  if (s === 'confirmed') return 'pill pill-ok';
  if (s === 'rejected') return 'pill pill-err';
  return 'pill pill-warning'; // pending 黄
}
function payStatusLabel(s?: string): string {
  if (s === 'confirmed') return '已到账';
  if (s === 'rejected') return '已拒绝';
  return '待确认';
}
function currencyLabel(c?: string): string {
  switch (c) {
    case 'usdt':
      return 'USDT';
    case 'btc':
      return 'BTC';
    case 'evm':
      return 'EVM';
    default:
      return c ?? '—';
  }
}
/** 应付金额单位说明（amount_crypto 原始单位：usdt 两位小数 / btc 聪 / evm wei）。 */
function amountUnitHint(c?: string): string {
  switch (c) {
    case 'usdt':
      return 'USDT（两位小数）';
    case 'btc':
      return '聪（satoshi，1 BTC = 1e8 聪）';
    case 'evm':
      return 'wei（1 ETH = 1e18 wei）';
    default:
      return '';
  }
}
function payAmount(o: PaymentOrder): string {
  const a = o.amount_crypto;
  if (a === undefined || a === null || a === '') return '—';
  return String(a);
}
function tokenNameOf(id?: string): string {
  if (!id) return '—';
  return tokens.value.find((t) => t.id === id)?.name ?? id;
}

function truncKey(k?: string): string {
  if (!k) return '—';
  if (k.length <= 20) return k;
  return k.slice(0, 12) + '…' + k.slice(-4);
}

/** 复制到剪贴板（安全上下文 Clipboard API，HTTP 非安全上下文回退
 *  execCommand——见 utils/clipboard.ts）并给出消息反馈。 */
async function copyWithToast(text: string): Promise<void> {
  if (await copyText(text)) {
    msg.value = { kind: 'ok', text: '已复制到剪贴板' };
  } else {
    msg.value = { kind: 'err', text: '复制失败（浏览器不支持）' };
  }
}

// =============================================================================
// 「接入说明」Tab（对外接入，2026-08-31；同日按用户澄清重构为 AI 交接优先）
// =============================================================================
// 核心场景（用户原话方向）："我正在跟一个 AI 助手对话，想把接入所需的信息直接
// 给到它"。面板以**一键复制完整接入块**为中心：Base URL + 完整 sk-os- key +
// 实时可用模型清单 + 非流式/流式 curl + OpenAI SDK 片段——AI 拿到这一块即可
// 直接开调。配套「生成接入令牌」一键建 free 令牌（创建响应回完整 key，正好
// 并入接入块）；模型清单每项带「复制接入」精简块（单模型）。
// i18n 走 gatewayAccess 命名空间（四语言，zh-TW 繁化：閘道/權杖/串流）；
// 复制复用 utils/clipboard 的 copyText。接入块本体面向机器（AI），标签用固定
// 英文协议词（Base URL/API Key/Models），不随 UI 语言切换。

/** 网关对外端口：浏览器当前访问端口优先（Web UI 与 API 同源，生产 8558），
 *  无端口（80/443 反代）回落 8558。 */
const GATEWAY_DEFAULT_PORT = '8558';

/** 对外 Base URL：http://<当前主机名>:<端口>/api/v1/gateway/v1。 */
const accessBaseUrl = computed(() => {
  const loc = typeof window !== 'undefined' ? window.location : null;
  const host = loc?.hostname || '127.0.0.1';
  const port = loc?.port || GATEWAY_DEFAULT_PORT;
  return `http://${host}:${port}/api/v1/gateway/v1`;
});

/** 接入令牌 key（单一来源：生成或粘贴都会写这里）。 */
const accessKey = ref('');
/** 是否已有一个可用形态的 key（sk-os- 前缀）。 */
const accessKeyReady = computed(() => accessKey.value.trim().startsWith('sk-os-'));

/** curl 示例里的鉴权头值：有真 key 用真值，否则占位提示。 */
const accessKeyForCurl = computed(() =>
  accessKeyReady.value ? accessKey.value.trim() : '<sk-os-你的令牌>',
);

/** 当前 key 打码展示（首 7 + *** + 尾 4；完整 key 只进复制块/剪贴板）。 */
const accessKeyMasked = computed(() => {
  const k = accessKey.value.trim();
  if (!k) return '';
  if (k.length <= 12) return `${k.slice(0, 4)}***`;
  return `${k.slice(0, 7)}***${k.slice(-4)}`;
});

/**
 * 接入令牌的计费模式（接入块 Quota 行如实标注，不瞎写 free）：用打码 key
 * （前4+***+后4，与后端 mask_secret 同格式）在令牌列表里匹配出该令牌的
 * billing_mode；列表未加载/匹配不到返回 ''（块内标 unknown）。
 */
const accessTokenBilling = computed<string>(() => {
  const k = accessKey.value.trim();
  if (!k.startsWith('sk-os-') || k.length < 8) return '';
  const masked = `${k.slice(0, 4)}***${k.slice(-4)}`;
  const hit = tokens.value.find((t) => t.key === masked);
  return typeof hit?.billing_mode === 'string' ? hit.billing_mode : '';
});

// —— 可用模型实时拉取（真实数据：按令牌过滤，不猜模型名）——
const accessModels = ref<string[]>([]);
const accessModelsLoading = ref(false);
const accessModelsError = ref('');
const accessModelsLoaded = ref(false);

/** 拉取该令牌可用模型（GET /api/v1/gateway/v1/models，Bearer=接入令牌）。 */
async function loadAccessModels(): Promise<void> {
  const key = accessKey.value.trim();
  if (!key.startsWith('sk-os-')) {
    accessModelsError.value = t('gatewayAccess.modelsErr', { err: 'key 未填或格式不对' });
    return;
  }
  accessModelsLoading.value = true;
  accessModelsError.value = '';
  try {
    const raw = await endpoints.gatewayV1Models(key);
    const resp = raw as { data?: Array<{ id?: string }> };
    accessModels.value = (resp.data ?? [])
      .map((m) => (typeof m.id === 'string' ? m.id : ''))
      .filter(Boolean);
    accessModelsLoaded.value = true;
  } catch (e) {
    accessModels.value = [];
    accessModelsLoaded.value = false;
    accessModelsError.value = t('gatewayAccess.modelsErr', {
      err: e instanceof Error ? e.message : String(e),
    });
  } finally {
    accessModelsLoading.value = false;
  }
}

// key 一就绪（生成或粘贴）且模型未拉过 → 自动拉取（接入块需要真实模型清单）
watch(
  () => accessKey.value,
  (k) => {
    if (k.trim().startsWith('sk-os-') && !accessModelsLoaded.value && !accessModelsLoading.value) {
      void loadAccessModels();
    }
  },
);

// —— 一键生成接入令牌（AI 交接的 key 落地）——
const accessCreatingToken = ref(false);

/**
 * 生成接入令牌：POST /api/v1/gateway/tokens（free 模式——AI 试调用不被配额
 * 卡住；需要计量可在「令牌」Tab 另建 per_token 令牌）。创建响应含一次性完整
 * key，当场并入接入块并自动拉取可用模型。
 *
 * 鉴权语义（如实说明）：该端点是 admin 写接口——测试期服务端未设
 * NEXOS_AUTH_DEFAULT_ADMIN=0 时无凭据默认按 admin 放行；设了 admin token 的
 * 部署需先在 设置 → API 令牌 填好（页面全局 Authorization 注入）。
 */
async function createAgentAccessToken(): Promise<void> {
  accessCreatingToken.value = true;
  try {
    const date = new Date().toISOString().slice(0, 10);
    const raw = await endpoints.createGatewayToken({
      name: `agent-access-${date}`,
      billing_mode: 'free',
      quota_limit: 0,
      allowed_models: [],
    });
    const created = raw as { key?: string; name?: string };
    if (typeof created.key === 'string' && created.key.startsWith('sk-os-')) {
      accessKey.value = created.key;
      msg.value = { kind: 'ok', text: t('gatewayAccess.genTokenDone') };
    } else {
      msg.value = { kind: 'err', text: t('gatewayAccess.genTokenNoKey') };
    }
    // 刷新令牌列表（新令牌进「令牌」Tab；列表只显示打码 key）
    void loadTokens();
  } catch (e) {
    msg.value = {
      kind: 'err',
      text: t('gatewayAccess.genTokenFail', {
        err: e instanceof Error ? e.message : String(e),
      }),
    };
  } finally {
    accessCreatingToken.value = false;
  }
}

// —— 接入块（面向 AI 的自包含文本；固定英文协议词，机器可读优先）——
// 组装函数抽成 baseUrl/apiKey 直传的纯函数（gatewayCurlChat / gatewayCurlStream /
// gatewayAgentBlock）：「接入说明」hero 块与「令牌」Tab 的接入信息面板（创建
// 成功/存量令牌）共用同一套产物，只有 key 与模型清单来源不同。

/** 非流式 curl（model 用真实模型名优先，无清单时占位）。 */
function gatewayCurlChat(baseUrl: string, apiKey: string, model: string): string {
  return [
    `curl ${baseUrl}/chat/completions \\`,
    `  -H 'Authorization: Bearer ${apiKey}' \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -d '{`,
    `    "model": "${model}",`,
    `    "messages": [{"role": "user", "content": "你好"}]`,
    `  }'`,
  ].join('\n');
}

/** 流式 curl（SSE 逐块透传；include_usage 让上游末块上报用量供网关计费）。 */
function gatewayCurlStream(baseUrl: string, apiKey: string, model: string): string {
  return [
    `curl -N ${baseUrl}/chat/completions \\`,
    `  -H 'Authorization: Bearer ${apiKey}' \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -d '{`,
    `    "model": "${model}",`,
    `    "messages": [{"role": "user", "content": "你好"}],`,
    `    "stream": true,`,
    `    "stream_options": {"include_usage": true}`,
    `  }'`,
  ].join('\n');
}

/** 接入块 Quota 行：free=unmetered；其余=metered（429 when exhausted）；
 *  匹配不到（''）= unknown——不瞎写。 */
function gatewayQuotaLine(billing: string): string {
  if (billing === 'free') return '- Quota: free (unmetered)';
  if (billing) return `- Quota: ${billing} (metered — 429 when exhausted)`;
  return '- Quota: unknown';
}

/**
 * 完整接入块（公用组装：「接入说明」hero 块与令牌接入面板同一产物）：粘给
 * 任何 AI 助手/agent 即可开调。内容全部真实——baseUrl/apiKey/models 由调用方
 * 注入（接入说明=当前 key + 实时清单；令牌面板=该令牌 key + 该令牌可用清单；
 * 存量令牌无完整 key 时传占位符，块内如实展示）。
 */
function gatewayAgentBlock(opts: {
  baseUrl: string;
  /** 完整 key；未就绪/占位形态由调用方传占位字符串。 */
  apiKey: string;
  models: string[];
  modelsLoaded: boolean;
  /** 计费模式（Quota 行；'' = unknown）。 */
  billing: string;
  /** 模型未拉取时的提示行（接入说明形态带操作指引；缺省通用占位）。 */
  modelsPendingNote?: string;
}): string {
  const keyLine = `- API Key: ${opts.apiKey}  (send as: Authorization: Bearer <API Key>)`;
  const modelLine = opts.modelsLoaded
    ? opts.models.length > 0
      ? `- Models: ${opts.models.join(', ')}`
      : '- Models: (该令牌暂无可路由模型——检查网关渠道配置)'
    : `- Models: ${opts.modelsPendingNote ?? '<未拉取>'}`;
  const model0 = opts.models[0] ?? '<模型名>';
  return [
    `### NexOS API Gateway (OpenAI-compatible)`,
    ``,
    `- Base URL: ${opts.baseUrl}`,
    keyLine,
    modelLine,
    gatewayQuotaLine(opts.billing),
    ``,
    `#### Chat (non-streaming)`,
    gatewayCurlChat(opts.baseUrl, opts.apiKey, model0),
    ``,
    `#### Chat (SSE streaming, chunk-by-chunk passthrough)`,
    gatewayCurlStream(opts.baseUrl, opts.apiKey, model0),
    ``,
    `#### Python (openai SDK)`,
    `from openai import OpenAI`,
    ``,
    `client = OpenAI(base_url="${opts.baseUrl}", api_key="${opts.apiKey}")`,
    `resp = client.chat.completions.create(`,
    `    model="${model0}",`,
    `    messages=[{"role": "user", "content": "你好"}],`,
    `)`,
    `print(resp.choices[0].message.content)`,
    ``,
    `# 模型清单（同一鉴权）: GET ${opts.baseUrl}/models`,
  ].join('\n');
}

/** 面板展示用 curl：无 key 时用占位模型名提示，有清单用第一个真实模型。 */
const displayModel = computed(() => accessModels.value[0] ?? '<模型名>');
const accessCurlChat = computed(() =>
  gatewayCurlChat(accessBaseUrl.value, accessKeyForCurl.value, displayModel.value),
);
const accessCurlStream = computed(() =>
  gatewayCurlStream(accessBaseUrl.value, accessKeyForCurl.value, displayModel.value),
);
const accessCurlModels = computed(
  () => `curl ${accessBaseUrl.value}/models -H 'Authorization: Bearer ${accessKeyForCurl.value}'`,
);

/**
 * 完整接入块（本面板的核心产物）：粘给任何 AI 助手/agent 即可开调。
 * 内容全部真实——Base URL 按当前访问动态拼、key 为真实令牌（未就绪则显式
 * 占位并提示）、模型清单来自 /gateway/v1/models 实时数据（未拉取则注明）。
 */
const accessAgentBlock = computed(() =>
  gatewayAgentBlock({
    baseUrl: accessBaseUrl.value,
    apiKey: accessKeyReady.value
      ? accessKey.value.trim()
      : '<尚未生成——在 NexOS 网关页「接入说明」点「生成接入令牌」后重新复制>',
    models: accessModels.value,
    modelsLoaded: accessModelsLoaded.value,
    billing: accessTokenBilling.value,
    modelsPendingNote: '<未拉取——key 就绪后会自动拉取，或点「拉取模型」>',
  }),
);

/** 单模型精简接入块（模型清单每项的「复制接入」产物）。 */
function accessModelBlock(model: string): string {
  return [
    `### NexOS API Gateway · ${model}`,
    ``,
    `- Base URL: ${accessBaseUrl.value}`,
    `- API Key: ${accessKeyForCurl.value}`,
    `- Model: ${model}`,
    ``,
    gatewayCurlChat(accessBaseUrl.value, accessKeyForCurl.value, model),
  ].join('\n');
}

/** 创建令牌契约示例（POST /api/v1/gateway/tokens，管理员鉴权；body 为合法
 *  JSON 可直接复制——字段语义说明见面板文案，不放注释进 JSON）。 */
const accessTokenContract = computed(() =>
  [
    `curl -X POST http://<节点IP>:${GATEWAY_DEFAULT_PORT}/api/v1/gateway/tokens \\`,
    `  -H 'Authorization: Bearer <管理员token>' \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -d '{`,
    `    "name": "我的应用",`,
    `    "billing_mode": "per_token",`,
    `    "quota_limit": 0,`,
    `    "allowed_models": []`,
    `  }'`,
  ].join('\n'),
);

/** 刚复制成功的块 key（按钮 ✓ 反馈 1.5s；'' = 无；LlmModels 接入面板同款）。 */
const accessCopied = ref('');
let accessCopyTimer: ReturnType<typeof setTimeout> | undefined;

/** 复制接入面板的一个块（复用 copyText 剪贴板工具；失败走全局 msg）。 */
async function copyAccess(key: string, text: string): Promise<void> {
  if (await copyText(text)) {
    accessCopied.value = key;
    clearTimeout(accessCopyTimer);
    accessCopyTimer = setTimeout(() => {
      accessCopied.value = '';
    }, 1500);
  } else {
    msg.value = { kind: 'err', text: t('gatewayAccess.copyFail') };
  }
}

/** 计费模式说明条目（与 billingModeOptions 四模式一一对应，一句话语义）。 */
const accessBillingItems = computed(() => [
  t('gatewayAccess.billingFree'),
  t('gatewayAccess.billingPerToken'),
  t('gatewayAccess.billingImage'),
  t('gatewayAccess.billingCredits'),
]);

// =============================================================================
// 「令牌」Tab · 接入信息面板（2026-09-03：创建成功后弹 + 存量令牌行「接入」打开）
// =============================================================================
// 用户原话「创建令牌后，要有一个接入的信息」：令牌创建响应含一次性完整 key——
// 当场弹出三段式钉底弹窗（警示条 + 完整 key 大字等宽 + 一键复制完整接入块 +
// 计费/配额回显 + curl 两例），接入块组装复用接入说明 hero 块的公用函数
// （gatewayAgentBlock/gatewayCurlChat/gatewayCurlStream，Base URL 同源动态拼）。
// 存量令牌行「接入」打开同款面板，但完整 key 后端不可回取（列表只存打码形态）
// → 以 <该令牌的完整密钥> 占位并注明「可重建令牌获取」；curl/接入块其余同构。
//
// 可用模型清单（从渠道实时取）：
// - 创建成功形态：有完整 key → GET /gateway/v1/models（Bearer=该 key），与
//   「接入说明」Tab 同源同口径（按令牌 allowed_models/allowed_channels 过滤）；
// - 存量令牌形态：无 key 可带 → GET /gateway/models（管理员端聚合，渠道实时）
//   再按该令牌 allowed_models 白名单求交集——如实展示该令牌可调的模型。

/** 存量令牌形态的接入块 key 占位符（块面向机器，协议占位词不随语言切换）。 */
const TOKEN_KEY_PLACEHOLDER = '<该令牌的完整密钥>';

/** 接入信息面板状态（fullKey 非空 = 创建成功形态；空 = 存量令牌占位形态）。 */
interface TokenAccessPanelState {
  name: string;
  /** 一次性完整 key（仅创建响应带回；存量令牌恒 ''）。 */
  fullKey: string;
  billingMode: string;
  quotaLimit: number;
  allowedModels: string[];
}

const tokenAccess = ref<TokenAccessPanelState | null>(null);
const tokenAccessModels = ref<string[]>([]);
const tokenAccessModelsLoading = ref(false);
const tokenAccessModelsError = ref('');
const tokenAccessModelsLoaded = ref(false);

/** 面板内接入块/curl 用的 key：创建形态=完整 key；存量形态=占位符。 */
const tokenAccessKeyForBlock = computed(
  () => tokenAccess.value?.fullKey || TOKEN_KEY_PLACEHOLDER,
);

/** 面板 curl 示例模型名：清单首个真实模型，未就绪占位。 */
const tokenAccessModel0 = computed(() => tokenAccessModels.value[0] ?? '<模型名>');
const tokenAccessCurlChat = computed(() =>
  gatewayCurlChat(accessBaseUrl.value, tokenAccessKeyForBlock.value, tokenAccessModel0.value),
);
const tokenAccessCurlStream = computed(() =>
  gatewayCurlStream(accessBaseUrl.value, tokenAccessKeyForBlock.value, tokenAccessModel0.value),
);

/** 面板完整接入块（与接入说明 hero 块同一组装函数，key/模型/计费注入本令牌）。 */
const tokenAccessAgentBlock = computed(() => {
  const p = tokenAccess.value;
  if (!p) return '';
  return gatewayAgentBlock({
    baseUrl: accessBaseUrl.value,
    apiKey: tokenAccessKeyForBlock.value,
    models: tokenAccessModels.value,
    modelsLoaded: tokenAccessModelsLoaded.value,
    billing: p.billingMode,
    // 拉取中/失败瞬态如实标注（成功则 Models 行直接列清单，不走此提示）
    modelsPendingNote: tokenAccessModelsError.value
      ? '<未拉取——模型清单拉取失败，可关闭面板重开重试>'
      : '<未拉取——正在从渠道实时获取>',
  });
});

/** 计费/配额回显行文案（创建参数回显：billing_mode + quota 描述）。 */
const tokenAccessQuotaText = computed(() => {
  const p = tokenAccess.value;
  if (!p) return '';
  switch (p.billingMode) {
    case 'free':
      return t('gwTokenAccess.quotaFree');
    case 'credits':
      return t('gwTokenAccess.quotaCredits', { n: p.quotaLimit });
    default:
      return p.quotaLimit > 0
        ? t('gwTokenAccess.quotaMetered', { n: p.quotaLimit })
        : t('gwTokenAccess.quotaMeteredUnlimited');
  }
});

/**
 * 拉取面板可用模型清单（从渠道实时取，见本段头注释的两形态分流）。
 * 失败不阻塞面板——接入块内 Models 行如实标注，可重开面板重试。
 */
async function loadTokenAccessModels(fullKey: string, allowed: string[]): Promise<void> {
  tokenAccessModelsLoading.value = true;
  tokenAccessModelsError.value = '';
  try {
    if (fullKey) {
      // 创建成功形态：与接入说明同源（Bearer=该完整 key，服务端按白名单过滤）
      const raw = await endpoints.gatewayV1Models(fullKey);
      const resp = raw as { data?: Array<{ id?: string }> };
      tokenAccessModels.value = (resp.data ?? [])
        .map((m) => (typeof m.id === 'string' ? m.id : ''))
        .filter(Boolean);
    } else {
      // 存量令牌形态：管理员端渠道聚合实时刷新，再交白名单求交集
      await loadModels();
      const all = aggregatedModels.value.models ?? [];
      tokenAccessModels.value = allowed.length
        ? all.filter((m) => allowed.includes(m))
        : all;
    }
    tokenAccessModelsLoaded.value = true;
  } catch (e) {
    tokenAccessModels.value = [];
    tokenAccessModelsLoaded.value = false;
    tokenAccessModelsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    tokenAccessModelsLoading.value = false;
  }
}

/** 打开接入信息面板（重置模型清单状态并按形态分流拉取）。 */
function openTokenAccess(s: TokenAccessPanelState): void {
  tokenAccess.value = s;
  tokenAccessModels.value = [];
  tokenAccessModelsLoaded.value = false;
  tokenAccessModelsError.value = '';
  void loadTokenAccessModels(s.fullKey, s.allowedModels);
}

/** 存量令牌行「接入」入口：完整 key 不可回取 → 占位形态（面板内注明）。 */
function openTokenAccessForRow(row: ApiToken): void {
  openTokenAccess({
    name: row.name ?? row.id ?? '',
    fullKey: '',
    billingMode: billingModeOf(row),
    quotaLimit: row.quota_limit ?? 0,
    allowedModels: Array.isArray(row.allowed_models) ? row.allowed_models : [],
  });
}

function closeTokenAccess(): void {
  tokenAccess.value = null;
}

// =============================================================================
// 数据加载
// =============================================================================
async function loadChannels(): Promise<void> {
  channelsLoading.value = true;
  channelsError.value = '';
  try {
    channels.value = (await endpoints.gatewayChannels()) as Channel[];
  } catch (e) {
    channelsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    channelsLoading.value = false;
  }
}

async function loadTokens(): Promise<void> {
  tokensLoading.value = true;
  tokensError.value = '';
  try {
    tokens.value = (await endpoints.gatewayTokens()) as ApiToken[];
  } catch (e) {
    tokensError.value = e instanceof Error ? e.message : String(e);
  } finally {
    tokensLoading.value = false;
  }
}

async function loadLogs(): Promise<void> {
  logsLoading.value = true;
  logsError.value = '';
  try {
    logs.value = (await endpoints.gatewayLogs(logLimit.value)) as CallLog[];
  } catch (e) {
    logsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    logsLoading.value = false;
  }
}

async function loadStats(): Promise<void> {
  statsLoading.value = true;
  try {
    stats.value = (await endpoints.gatewayStats()) as GatewayStats;
  } catch {
    /* 降级 */
  } finally {
    statsLoading.value = false;
  }
}

async function loadModels(): Promise<void> {
  modelsLoading.value = true;
  try {
    aggregatedModels.value = (await endpoints.gatewayModels()) as {
      models?: string[];
      count?: number;
    };
  } catch {
    /* 降级 */
  } finally {
    modelsLoading.value = false;
  }
}

async function loadMappings(): Promise<void> {
  mappingsLoading.value = true;
  mappingsError.value = '';
  try {
    mappings.value = (await endpoints.gatewayMappings()) as ModelMapping[];
  } catch (e) {
    mappingsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    mappingsLoading.value = false;
  }
}

async function loadPayments(): Promise<void> {
  paymentsLoading.value = true;
  paymentsError.value = '';
  try {
    payments.value = (await endpoints.gatewayPayments()) as PaymentOrder[];
  } catch (e) {
    paymentsError.value = e instanceof Error ? e.message : String(e);
  } finally {
    paymentsLoading.value = false;
  }
}

async function refreshAll(): Promise<void> {
  msg.value = null;
  await Promise.all([
    loadChannels(),
    loadTokens(),
    loadStats(),
    loadModels(),
    loadMappings(),
    loadLogs(),
    loadPayments(),
  ]);
}

// =============================================================================
// 渠道操作
// =============================================================================
function openCreateChannel(): void {
  editingChannel.value = null;
  channelForm.value = {
    name: '',
    provider: 'openai',
    base_url: '',
    api_key: '',
    models: '',
    priority: 0,
    weight: 1,
    via_node: '',
  };
  showChannelDialog.value = true;
  // 拉本地发现列表 + 外部 API 登记列表（失败不阻塞手填——各自只影响导入区）
  void loadDiscovery();
  void loadExternalApis();
}

function openEditChannel(c: Channel): void {
  editingChannel.value = c;
  channelForm.value = {
    name: c.name ?? '',
    provider: c.provider ?? 'openai',
    base_url: c.base_url ?? '',
    api_key: c.api_key ?? '',
    models: (c.models ?? []).join(', '),
    priority: c.priority ?? 0,
    weight: c.weight ?? 1,
    via_node: c.via_node ?? '',
  };
  showChannelDialog.value = true;
}

function closeChannelDialog(): void {
  if (submitting.value) return;
  showChannelDialog.value = false;
  editingChannel.value = null;
}

async function submitChannel(): Promise<void> {
  msg.value = null;
  submitting.value = true;
  const models = channelForm.value.models
    .split(',')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const viaNode = channelForm.value.via_node.trim();
  // via_node：0x+66hex（中继渠道）。创建时非空才带；**编辑时恒带**——空串
  // 即「清除回直连」（后端 PUT 语义：提供即覆盖）。
  const viaNodeField = editingChannel.value?.id ? { via_node: viaNode } : viaNode ? { via_node: viaNode } : {};
  const body = {
    name: channelForm.value.name.trim(),
    provider: channelForm.value.provider,
    base_url: channelForm.value.base_url.trim(),
    api_key: channelForm.value.api_key,
    models,
    priority: channelForm.value.priority,
    weight: channelForm.value.weight,
    ...viaNodeField,
  };
  try {
    if (editingChannel.value?.id) {
      await endpoints.updateGatewayChannel(editingChannel.value.id, body);
      msg.value = { kind: 'ok', text: '渠道已更新' };
    } else {
      await endpoints.createGatewayChannel(body);
      msg.value = { kind: 'ok', text: '渠道已添加' };
    }
    showChannelDialog.value = false;
    editingChannel.value = null;
    await loadChannels();
    await loadStats();
    await loadModels();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    submitting.value = false;
  }
}

async function removeChannel(c: Channel): Promise<void> {
  if (!c.id) return;
  if (!confirm(`确认删除渠道「${c.name ?? c.id}」？`)) return;
  busyId.value = c.id;
  msg.value = null;
  try {
    await endpoints.deleteGatewayChannel(c.id);
    msg.value = { kind: 'ok', text: '渠道已删除' };
    await loadChannels();
    await loadStats();
    await loadModels();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    busyId.value = '';
  }
}

async function testChannel(c: Channel): Promise<void> {
  if (!c.id) return;
  busyId.value = c.id;
  msg.value = null;
  try {
    const r = (await endpoints.testGatewayChannel(c.id)) as {
      models_count?: number;
      ok?: boolean;
    };
    msg.value = {
      kind: 'ok',
      text: `连通性测试成功，探测到 ${r.models_count ?? 0} 个模型`,
    };
    await loadChannels();
  } catch (e) {
    msg.value = {
      kind: 'err',
      text: `连通性测试失败：${e instanceof Error ? e.message : String(e)}`,
    };
    await loadChannels();
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 令牌操作
// =============================================================================
function openCreateToken(): void {
  tokenForm.value = {
    name: '',
    billing_mode: 'per_token',
    quota_limit: 0,
    initial_credits: 0,
    allowed_models: '',
    expires_at: '',
  };
  showTokenDialog.value = true;
}

function closeTokenDialog(): void {
  if (submitting.value) return;
  showTokenDialog.value = false;
}

async function submitToken(): Promise<void> {
  msg.value = null;
  submitting.value = true;
  const models = tokenForm.value.allowed_models
    .split(',')
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
  const isCredits = tokenForm.value.billing_mode === 'credits';
  const body = {
    name: tokenForm.value.name.trim(),
    billing_mode: tokenForm.value.billing_mode,
    // credits 模式的积分池上限由 initial_credits 决定，不再单独传 quota_limit
    quota_limit: isCredits ? undefined : tokenForm.value.quota_limit,
    initial_credits: isCredits ? tokenForm.value.initial_credits : undefined,
    allowed_models: models,
    expires_at: tokenForm.value.expires_at || undefined,
  };
  try {
    const r = (await endpoints.createGatewayToken(body)) as ApiToken;
    // 创建成功：关表单弹「令牌已创建·接入信息」面板（完整 key 仅此一次展示，
    // 接入块/curl/计费配额回显一并提供——见 tokenAccess 段注释）
    showTokenDialog.value = false;
    if (typeof r.key === 'string' && r.key) {
      openTokenAccess({
        name: r.name ?? body.name,
        fullKey: r.key,
        billingMode: String(r.billing_mode ?? body.billing_mode),
        // 回显优先取创建响应实值；缺省回落表单参数（credits 上限=初始积分）
        quotaLimit:
          typeof r.quota_limit === 'number'
            ? r.quota_limit
            : isCredits
              ? tokenForm.value.initial_credits
              : tokenForm.value.quota_limit,
        allowedModels: Array.isArray(r.allowed_models) ? r.allowed_models : models,
      });
    } else {
      // 后端异常：响应未带一次性完整 key——如实报错（无法展示接入信息）
      msg.value = { kind: 'err', text: t('gwTokenAccess.noKeyInResp') };
    }
    await loadTokens();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    submitting.value = false;
  }
}

async function disableToken(t: ApiToken): Promise<void> {
  if (!t.id) return;
  busyId.value = t.id;
  msg.value = null;
  try {
    await endpoints.disableGatewayToken(t.id);
    msg.value = { kind: 'ok', text: '令牌已禁用' };
    await loadTokens();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    busyId.value = '';
  }
}

async function enableToken(t: ApiToken): Promise<void> {
  if (!t.id) return;
  busyId.value = t.id;
  msg.value = null;
  try {
    await endpoints.enableGatewayToken(t.id);
    msg.value = { kind: 'ok', text: '令牌已启用' };
    await loadTokens();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    busyId.value = '';
  }
}

async function removeToken(t: ApiToken): Promise<void> {
  if (!t.id) return;
  if (!confirm(`确认删除令牌「${t.name ?? t.id}」？此操作不可恢复。`)) return;
  busyId.value = t.id;
  msg.value = null;
  try {
    await endpoints.deleteGatewayToken(t.id);
    msg.value = { kind: 'ok', text: '令牌已删除' };
    await loadTokens();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 映射操作
// =============================================================================
function openCreateMapping(): void {
  mappingForm.value = { public_name: '', channel_id: '', upstream_model: '' };
  showMappingDialog.value = true;
}

function closeMappingDialog(): void {
  if (submitting.value) return;
  showMappingDialog.value = false;
}

async function submitMapping(): Promise<void> {
  msg.value = null;
  submitting.value = true;
  try {
    await endpoints.createGatewayMapping({
      public_name: mappingForm.value.public_name.trim(),
      channel_id: mappingForm.value.channel_id.trim(),
      upstream_model: mappingForm.value.upstream_model.trim(),
    });
    msg.value = { kind: 'ok', text: '映射已添加' };
    showMappingDialog.value = false;
    await loadMappings();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    submitting.value = false;
  }
}

async function removeMapping(m: ModelMapping): Promise<void> {
  if (!m.public_name) return;
  if (!confirm(`确认删除映射「${m.public_name}」？`)) return;
  msg.value = null;
  try {
    await endpoints.deleteGatewayMapping(m.public_name);
    msg.value = { kind: 'ok', text: '映射已删除' };
    await loadMappings();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  }
}

// =============================================================================
// 充值订单操作
// =============================================================================
async function submitPayment(): Promise<void> {
  msg.value = null;
  if (!paymentForm.value.token_id) {
    msg.value = { kind: 'err', text: '请选择要充值的令牌' };
    return;
  }
  submitting.value = true;
  try {
    const r = (await endpoints.createGatewayPayment({
      token_id: paymentForm.value.token_id,
      currency: paymentForm.value.currency,
      credits: paymentForm.value.credits,
    })) as PaymentOrder;
    createdPayment.value = r;
    msg.value = { kind: 'ok', text: '充值订单已创建，请按收款信息转账' };
    await loadPayments();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    submitting.value = false;
  }
}

function dismissCreatedPayment(): void {
  createdPayment.value = null;
}

function openConfirmPayment(o: PaymentOrder): void {
  confirmingOrder.value = o;
  confirmPayTxid.value = '';
  showConfirmPayDialog.value = true;
}

function closeConfirmPayDialog(): void {
  if (submitting.value) return;
  showConfirmPayDialog.value = false;
  confirmingOrder.value = null;
}

async function submitConfirmPayment(): Promise<void> {
  if (!confirmingOrder.value?.id) return;
  msg.value = null;
  submitting.value = true;
  try {
    const r = (await endpoints.confirmGatewayPayment(
      confirmingOrder.value.id,
      confirmPayTxid.value.trim() || undefined,
    )) as { ok?: boolean; added_credits?: number };
    msg.value = {
      kind: 'ok',
      text: `已确认到账，为令牌「${tokenNameOf(confirmingOrder.value.token_id)}」充值 ${r.added_credits ?? confirmingOrder.value.credits ?? 0} 积分`,
    };
    showConfirmPayDialog.value = false;
    confirmingOrder.value = null;
    await Promise.all([loadPayments(), loadTokens()]);
  } catch (e) {
    // 409 = 该订单已被确认过（幂等冲突），刷新列表即可
    if (e instanceof ApiError && e.status === 409) {
      msg.value = { kind: 'info', text: '该订单此前已确认过，无需重复操作' };
      showConfirmPayDialog.value = false;
      confirmingOrder.value = null;
      await loadPayments();
    } else {
      msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
    }
  } finally {
    submitting.value = false;
  }
}

function openRejectPayment(o: PaymentOrder): void {
  rejectingOrder.value = o;
  rejectPayReason.value = '';
  showRejectPayDialog.value = true;
}

function closeRejectPayDialog(): void {
  if (submitting.value) return;
  showRejectPayDialog.value = false;
  rejectingOrder.value = null;
}

async function submitRejectPayment(): Promise<void> {
  if (!rejectingOrder.value?.id) return;
  msg.value = null;
  submitting.value = true;
  try {
    await endpoints.rejectGatewayPayment(
      rejectingOrder.value.id,
      rejectPayReason.value.trim() || undefined,
    );
    msg.value = { kind: 'ok', text: '订单已拒绝' };
    showRejectPayDialog.value = false;
    rejectingOrder.value = null;
    await loadPayments();
  } catch (e) {
    msg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  } finally {
    submitting.value = false;
  }
}

// =============================================================================
// API 大厅（推理服务市场，/api/v1/api-market/*；docs/API_MARKET.md）
// =============================================================================
// 链上身份（与 IM 共用同一密钥对；nexhub token 与大厅写端点互通——api-market
// 发布者身份=区块链公钥**唯一通道**，明确不回落 admin token）。
const {
  hasIdentity: marketHasIdentity,
  pubkey: identityPubkey,
  evmAddress: identityEvm,
  nexhubAuthenticating: identityAuthing,
  ensureNexhubToken,
  forceNexhubReauth,
} = useChainIdentity();

const marketListings = ref<MarketListing[]>([]);
const marketLoading = ref(false);
const marketError = ref('');
const marketQ = ref('');
const marketSort = ref<'recent' | 'price'>('recent');

// —— 二级 Tab：本地大厅 / 联邦大厅（API 大厅 Tab 内；切换只换视图，搜索/排序状态保留）——
// 联邦化（2026-08-31）：列表仍一次拉全量（scope=all 平铺），客户端按 source_node
// 分流——本地 Tab=本机发布（source_node='local'/缺省），联邦 Tab=远程条目
// （api_market_lobby 载荷经 os-p2p 幂等合并入库）。
type MarketView = 'local' | 'fed';
const marketView = ref<MarketView>('local');
/** 联邦远程条目（source_node 非空且非 'local'）。 */
function isRemoteListing(l: MarketListing): boolean {
  return !!l.source_node && l.source_node !== 'local';
}
const localMarketListings = computed(() => marketListings.value.filter((l) => !isRemoteListing(l)));
const fedMarketListings = computed(() => marketListings.value.filter((l) => isRemoteListing(l)));
/** 当前二级 Tab 显示的挂牌（本地=默认全量）。 */
const visibleMarketListings = computed(() =>
  marketView.value === 'fed' ? fedMarketListings.value : localMarketListings.value,
);

/** id → 最新 metrics 三态响应（null=已探测但不可达；undefined=尚未拉取）。 */
const marketMetrics = ref<Record<string, MarketMetricsResp | null>>({});
/** id → metrics 拉取中（徽章显示 …）。 */
const marketMetricsLoading = ref<Record<string, boolean>>({});

/** 错峰拉取定时器（2s 间隔逐卡触发；重载/卸载/暂停时清空）。 */
let metricTimers: ReturnType<typeof setTimeout>[] = [];

function clearMetricTimers(): void {
  metricTimers.forEach(clearTimeout);
  metricTimers = [];
}

/** 大厅 metrics 自动轮询间隔（15s，文档建议 10-30s 折中；错峰 2s/卡下
 *  一轮在下一周期前自然完成，新一轮先清旧定时器不堆积）。 */
const MARKET_METRICS_POLL_MS = 15000;
/** 本机条目心跳自动上报间隔（60s = 服务端新鲜窗口长度，保住绿色在线徽章）。 */
const MARKET_HEARTBEAT_POLL_MS = 60000;
let metricsPollTimer: ReturnType<typeof setInterval> | null = null;
let heartbeatPollTimer: ReturnType<typeof setInterval> | null = null;

/** 轮询生效条件：大厅组任一子 Tab（API 大厅 / 我的发布）活跃 且 页面在前台
 *  （切出组 / 最小化一律暂停外呼；我的发布同样需要心跳保活与负载徽章）。 */
function isLobbyTabActive(): boolean {
  return activeTab.value === 'market' || activeTab.value === 'mylistings';
}
function marketPollingActive(): boolean {
  return isLobbyTabActive() && typeof document !== 'undefined' && !document.hidden;
}

/** 启动大厅自动轮询（metrics 15s + 本机心跳 60s；重复调用先停旧再启新）。 */
function startMarketPolling(): void {
  stopMarketPolling();
  if (!isLobbyTabActive()) return;
  metricsPollTimer = setInterval(() => {
    if (marketPollingActive()) scheduleMetricsFetch();
  }, MARKET_METRICS_POLL_MS);
  heartbeatPollTimer = setInterval(() => {
    if (marketPollingActive()) void autoHeartbeatOwnListings();
  }, MARKET_HEARTBEAT_POLL_MS);
}

/** 停止大厅自动轮询（含进行中的错峰拉取）。 */
function stopMarketPolling(): void {
  if (metricsPollTimer !== null) {
    clearInterval(metricsPollTimer);
    metricsPollTimer = null;
  }
  if (heartbeatPollTimer !== null) {
    clearInterval(heartbeatPollTimer);
    heartbeatPollTimer = null;
  }
  clearMetricTimers();
}

async function loadMarket(): Promise<void> {
  marketLoading.value = true;
  marketError.value = '';
  try {
    marketListings.value = (await endpoints.apiMarketList({
      q: marketQ.value.trim() || undefined,
      sort: marketSort.value,
    })) as MarketListing[];
    scheduleMetricsFetch();
  } catch (e) {
    marketError.value = e instanceof Error ? e.message : String(e);
  } finally {
    marketLoading.value = false;
  }
}

/** 单卡拉取 metrics（失败降级为不可达三态，不弹全局错误）。 */
async function fetchMarketMetrics(id: string): Promise<void> {
  marketMetricsLoading.value[id] = true;
  try {
    marketMetrics.value[id] = (await endpoints.apiMarketMetrics(id)) as MarketMetricsResp;
  } catch {
    marketMetrics.value[id] = { id, reachable: false, stale: true, metrics: null, ts: null };
  } finally {
    marketMetricsLoading.value[id] = false;
  }
}

/**
 * 错峰拉取：逐卡 GET /:id/metrics（间隔 2s，避免集中外呼打爆服务端代拉
 * 通道）。由列表加载与 15s 自动轮询触发（`startMarketPolling`）；每轮先清
 * 上一轮残留定时器，卡片多时旧轮未完即被新轮取代，不堆积。
 * 2026-09-03 起联邦远程条目不拉——中继路径可达性由消费行为证明（调用即通），
 * 主动探测（metrics 代拉）只对本地条目有意义（发布者视角的直连探测）。
 */
function scheduleMetricsFetch(): void {
  clearMetricTimers();
  marketListings.value.forEach((l, i) => {
    const id = l.id;
    if (!id) return;
    if (isRemoteListing(l)) return; // 联邦条目：不主动探测（心跳时间差行已给活性参考）
    metricTimers.push(setTimeout(() => void fetchMarketMetrics(id), i * 2000));
  });
}

// —— 负载徽章（仅本地条目：绿=新鲜心跳≤60s / 灰=metrics_url 代拉降级 /
//    红=不可达——发布者自己的直连探测语义。联邦条目改走「🌐 经源节点中继」
//    常驻徽章 + 源节点心跳时间差行，见模板 mkt-title-row 分流）——
function metricsOf(id?: string): MarketMetricsResp | null | undefined {
  if (!id) return undefined;
  return marketMetrics.value[id];
}
function loadDotClass(id?: string): string {
  const m = metricsOf(id);
  if (m === undefined || marketMetricsLoading.value[id ?? '']) return 'dot dot-pending';
  if (m === null) return 'dot dot-red';
  if (m.reachable && !m.stale) return 'dot dot-green';
  if (m.reachable) return 'dot dot-gray';
  return 'dot dot-red';
}
/** 徽章正文：负载百分比优先，缺则运行队列数（running/waiting），全缺=—。 */
function loadBadgeText(id?: string): string {
  const m = metricsOf(id);
  if (marketMetricsLoading.value[id ?? '']) return '…';
  if (!m || !m.reachable || !m.metrics) return '不可达';
  const mt = m.metrics;
  if (mt.load_pct !== null && mt.load_pct !== undefined) return `${Math.round(mt.load_pct)}%`;
  if (mt.running !== null && mt.running !== undefined) {
    const w = mt.waiting ?? 0;
    return `运行 ${Math.round(mt.running)}${w ? ` · 排队 ${Math.round(w)}` : ''}`;
  }
  return '—';
}
function loadBadgeTitle(id?: string): string {
  const m = metricsOf(id);
  if (!m) return '实时负载（拉取中）';
  const src = m.source === 'heartbeat' ? '节点心跳' : m.source === 'metrics_url' ? '服务端代拉' : '无数据源';
  const fresh = m.reachable && !m.stale ? '新鲜' : m.reachable ? '数据可能过期' : '不可达';
  return `实时负载 · ${src} · ${fresh}（点击查看详情）`;
}

// —— 联邦条目：源节点心跳时间差（独立行，不映射可达性）——
/**
 * heartbeat_at（RFC3339）距今的时间差文案：<60s「刚刚」→ 分钟 → 小时 → 天。
 * 无心跳 / 解析失败 → '--'（不猜）。年龄基于**本副本最后一次联邦快照**——
 * 消费者侧可见性上限 30min（重播周期，tooltip 详述）；节点间时钟偏差会直接
 * 反映在差值里（展示语义，不做判定）。
 */
function heartbeatAgeLabel(l: MarketListing): string {
  const hb = l.heartbeat_at;
  if (!hb) return t('apiMarket.hbNone');
  const ts = Date.parse(hb);
  if (Number.isNaN(ts)) return t('apiMarket.hbNone');
  const secs = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (secs < 60) return t('apiMarket.hbJustNow');
  const mins = Math.floor(secs / 60);
  if (mins < 60) return t('apiMarket.hbMinAgo', { n: mins });
  const hours = Math.floor(mins / 60);
  if (hours < 24) return t('apiMarket.hbHourAgo', { n: hours });
  return t('apiMarket.hbDayAgo', { n: Math.floor(hours / 24) });
}

// —— 大厅卡片展开态（描述默认两行 clamp，点卡片切换全文）——
const expandedMarketIds = ref<Set<string>>(new Set());
function isMarketCardExpanded(l: MarketListing): boolean {
  return !!l.id && expandedMarketIds.value.has(l.id);
}
/**
 * 点卡片切换展开/收起：卡片根挂 click，交互元素（负载徽章/打赏/接入折叠面板/
 * 链接等）冒泡上来的点击忽略——只响应卡片空白处与文本区。
 */
function toggleMarketCard(ev: MouseEvent, l: MarketListing): void {
  if (!l.id) return;
  const el = ev.target as HTMLElement | null;
  if (el?.closest('button, a, input, select, textarea, label, details, summary')) return;
  const next = new Set(expandedMarketIds.value);
  if (next.has(l.id)) next.delete(l.id);
  else next.add(l.id);
  expandedMarketIds.value = next;
}

// —— metrics 详情对话框 ——
const metricsDetail = ref<{ listing: MarketListing; resp: MarketMetricsResp | null } | null>(null);

/** 打开详情（即时刷新一次 metrics，弹窗内展示 6 指标小表 + 新鲜度时间）。 */
async function openMetricsDetail(l: MarketListing): Promise<void> {
  if (!l.id) return;
  metricsDetail.value = { listing: l, resp: metricsOf(l.id) ?? null };
  await fetchMarketMetrics(l.id);
  // 等待期间弹窗可能已关闭
  if (metricsDetail.value?.listing.id === l.id) {
    metricsDetail.value.resp = metricsOf(l.id) ?? null;
  }
}
function closeMetricsDetail(): void {
  metricsDetail.value = null;
}
function metricsFmt(v?: number | null): string {
  return v === null || v === undefined ? '—' : String(Math.round(v * 10) / 10);
}
function fmtTime(ts?: string | null): string {
  return ts ? ts.slice(0, 19).replace('T', ' ') : '—';
}

// —— 价格徽章 ——
function priceBadge(l: MarketListing): { text: string; cls: string } {
  const p = l.pricing ?? {};
  const cur = p.currency && p.currency !== 'free' ? p.currency : 'sats';
  switch (p.mode) {
    case 'free':
      return { text: '免费', cls: 'pill pill-ok' };
    case 'per_image':
      return { text: `${p.price_per_1k_tokens ?? '—'} ${cur}/图`, cls: 'pill pill-purple' };
    case 'per_token':
    default:
      return { text: `${p.price_per_1k_tokens ?? '—'} ${cur} /1k tok`, cls: 'pill pill-blue' };
  }
}

// —— 发布者 / 服务器配置展示 ——
/** 发布者 EVM 短显：0x 身份 → `0x**…后四位`；非 0x 存量字符串保持原截断。 */
function shortEvm(addr?: string): string {
  if (!addr) return '—';
  if (/^0x/i.test(addr)) return shortIdentity(addr);
  return addr.length > 14 ? `${addr.slice(0, 8)}…${addr.slice(-6)}` : addr;
}
/** 发布者 identicon：优先 pubkey（与 Chat/CodeHub 同身份同图），兜底 EVM 展示名。 */
function publisherIdenticon(l: MarketListing): string | null {
  const id = l.publisher_pubkey || l.publisher_display;
  return id ? identiconSvg(id, 16) : null;
}
function vramLabel(mb?: number | null): string {
  if (!mb) return '';
  const gb = mb / 1024;
  return `${gb % 1 === 0 ? gb : gb.toFixed(1)} GB`;
}
/** GPU 概要：优先 gpus 列表——单卡原样「名 · 显存」、多卡同型「名 ×N · 显存/卡」
 *  （混合型号「首卡名 等 N 卡」）；**统一内存卡**（GB10：vram null + unified
 *  标记）容量改「统一内存 N GB」（CPU/GPU 共享池，不按卡倍乘）；无 gpus 落回
 *  旧字段 gpu_name/gpu_vram_mb/gpu_count。 */
function gpuSummary(c: MarketServerConfig): string {
  const cards = (c.gpus ?? [])
    .filter((g) => !!g.name)
    .map((g) => ({
      name: g.name ?? '',
      vram: g.vram_mb ?? null,
      unified: !!g.unified_memory && !g.vram_mb,
      unifiedVram: g.unified_vram_mb ?? null,
    }));
  if (cards.length > 0) {
    const first = cards[0];
    if (!first) return '—';
    const cap = first.unified
      ? vramLabel(first.unifiedVram)
      : first.vram
        ? vramLabel(first.vram)
        : '';
    const per = cap
      ? ` · ${first.unified ? '统一内存 ' : ''}${cap}${!first.unified && cards.length > 1 ? '/卡' : ''}`
      : '';
    if (cards.length === 1) return `${first.name}${per}`;
    const sameModel = cards.every((g) => g.name === first.name);
    return sameModel ? `${first.name} ×${cards.length}${per}` : `${first.name} 等 ${cards.length} 卡${per}`;
  }
  const n = Math.max(c.gpu_count ?? 0, c.gpu_name ? 1 : 0);
  if (!c.gpu_name) return '—';
  const per = c.gpu_vram_mb ? ` · ${vramLabel(c.gpu_vram_mb)}${n > 1 ? '/卡' : ''}` : '';
  return n > 1 ? `${c.gpu_name} ×${n}${per}` : `${c.gpu_name}${per}`;
}
/** CPU 概要：型号+核数（有型号拼一起；只有核数退回「N 核」；全无 → —）。 */
function cpuSummary(c: MarketServerConfig): string {
  if (c.cpu_model) return c.cpu_cores ? `${c.cpu_model} · ${c.cpu_cores} 核` : c.cpu_model;
  return c.cpu_cores ? `${c.cpu_cores} 核` : '—';
}
/** 上下文长度展示值：context_len（发布端自报，2026-09-02）优先，缺省回落老字段
 * max_model_len（vLLM --max-model-len，同义）；两者皆缺 = 真实无值显示 —（不猜）。 */
function contextLenOf(c: MarketServerConfig): number | null {
  return c.context_len ?? c.max_model_len ?? null;
}
/** 配置小表行（紧凑两列：GPU/CPU/内存/模型/上下文，量化/区域有值才显示）。 */
function configRows(l: MarketListing): { k: string; v: string }[] {
  const c = l.server_config ?? {};
  const ctx = contextLenOf(c);
  const rows = [
    { k: 'GPU', v: gpuSummary(c) },
    { k: 'CPU', v: cpuSummary(c) },
    { k: '内存', v: c.ram_gb ? `${c.ram_gb} GB` : '—' },
    { k: '模型', v: c.model_name ?? '—' },
    { k: '上下文', v: ctx !== null ? ctx.toLocaleString() : '—' },
  ];
  if (c.quantization) rows.push({ k: '量化', v: c.quantization });
  if (c.region) rows.push({ k: '区域', v: c.region });
  return rows;
}

/** 本机已发布条目（publisher_pubkey 与当前链上身份一致）。 */
function isOwnListing(l: MarketListing): boolean {
  return !!l.publisher_pubkey && !!identityPubkey.value && l.publisher_pubkey === identityPubkey.value;
}

/** 「我的发布」Tab 数据源：本机已发布条目（与大厅同一份 marketListings，
 *  仅过滤视角——不另拉接口、不复制逻辑；owner 操作复用大厅的函数）。 */
const ownListings = computed(() => marketListings.value.filter((l) => l.id && isOwnListing(l)));

// —— 接入信息（access_info，2026-08-31）：展示 + 完整 curl 示例 ——
/** 条目是否带接入信息（任一字段有值即展示块）。 */
function hasAccessInfo(l: MarketListing): boolean {
  const a = l.access_info;
  return !!a && !!(a.api_key || a.auth_header || a.notes);
}
/** api_key 是否为服务端脱敏值（非本人/admin 视角——`***` 标记）。 */
function isMaskedAccessKey(l: MarketListing): boolean {
  return !!l.access_info?.api_key?.includes('***');
}
/**
 * curl 示例的鉴权头行（2026-09-02 修复「-H 'Authorization Bearer' 只有头名没有值」）：
 * 规则与后端纯函数 `curl_auth_header_line`（api_market.rs）同一契约——
 * - **令牌值**：明文视角（api_key 为服务端原文，无 `***` 脱敏标记）→ 真实 key
 *   （`-H 'Authorization: Bearer sk-os-xxx'`）；脱敏视角/未配 key → i18n 占位符
 *   `<你的令牌>`（脱敏残值**永不**拼进 curl——复制即用才是示例的意义）；
 * - **头形态**：`auth_header` 含 `<key>` 占位 → 字面替换；缺省/标准
 *   `Authorization Bearer` → 规范化为 `Authorization: Bearer <令牌>`（带冒号——
 *   旧缺陷正是缺省值按字面拼出无冒号无值的头）；其他自定义（如 `X-Api-Key`）
 *   → 补冒号拼令牌。
 */
function accessAuthHeaderLines(l: MarketListing): { lines: string[]; placeholder: boolean } {
  const rawKey = l.access_info?.api_key?.trim();
  const plaintext = !!rawKey && !isMaskedAccessKey(l);
  const token = plaintext ? (rawKey as string) : t('apiMarket.tokenPlaceholder');
  const custom = l.access_info?.auth_header?.trim();
  let line: string;
  if (custom) {
    if (custom.includes('<key>')) line = custom.split('<key>').join(token);
    else if (/^authorization:?\s*bearer$/i.test(custom)) line = `Authorization: Bearer ${token}`;
    else if (custom.includes(':')) line = `${custom} ${token}`;
    else line = `${custom}: ${token}`;
  } else {
    line = `Authorization: Bearer ${token}`;
  }
  return { lines: [`-H '${line}'`], placeholder: !plaintext };
}
/** curl 是否落在占位令牌分支（脱敏视角/发布端未配 key）→ 面板附一行索取说明。 */
function accessCurlUsesPlaceholder(l: MarketListing): boolean {
  return accessAuthHeaderLines(l).placeholder;
}
/** curl 示例的模型名：server_config.model_name → tags 首个 → 接入备注兜底。 */
function accessModelName(l: MarketListing): string {
  return (
    l.server_config?.model_name?.trim() ||
    (l.tags ?? []).map((x) => x.trim()).find(Boolean) ||
    l.access_info?.notes?.trim() ||
    'MODEL_NAME'
  );
}
/** 完整接入 curl（endpoint_url + 鉴权头 + 模型名的一发即用示例）。 */
function buildAccessCurl(l: MarketListing): string {
  const model = accessModelName(l);
  const body = JSON.stringify({ model, messages: [{ role: 'user', content: '你好' }] });
  const auth = accessAuthHeaderLines(l);
  const lines = [`curl ${l.endpoint_url ?? '<endpoint_url>'} \\`, ...auth.lines.map((h) => `  ${h} \\`)];
  lines.push(`  -H 'Content-Type: application/json' \\`, `  -d '${body}'`);
  return lines.join('\n');
}
/** 刚复制接入 curl 的条目 id（按钮 ✓ 反馈 1.5s；'' = 无）。 */
const copiedAccessId = ref('');
/** 复制条目的完整接入 curl（复用 copyText 剪贴板工具；失败走全局 msg）。 */
async function copyAccessCurl(l: MarketListing): Promise<void> {
  if (!l.id) return;
  if (await copyText(buildAccessCurl(l))) {
    copiedAccessId.value = l.id;
    setTimeout(() => {
      if (copiedAccessId.value === l.id) copiedAccessId.value = '';
    }, 1500);
  } else {
    msg.value = { kind: 'err', text: '复制失败（浏览器不支持）' };
  }
}

// —— 推送联邦（两步联邦第二步：本地已发布条目 → 广播给其他 NexOS 节点）——
async function federateMarket(l: MarketListing): Promise<void> {
  if (!l.id) return;
  busyId.value = l.id;
  msg.value = null;
  try {
    const opts = await requireMarketIdentity();
    const r = (await endpoints.apiMarketFederate(l.id, opts)) as {
      first_push?: boolean;
      note?: string;
    };
    msg.value = {
      kind: 'ok',
      text: `${r.first_push === false ? '已重新推送' : '已推送'}「${l.api_name ?? l.id}」到联邦大厅${r.note ? `（${r.note}）` : ''}`,
    };
    await loadMarket();
  } catch (e) {
    msg.value = { kind: 'err', text: marketWriteErr('推送联邦', e) };
  } finally {
    busyId.value = '';
  }
}

// —— 发布对话框 ——
const showMarketPublish = ref(false);
const showServerOverride = ref(false);
/** 默认消费端点=本机网关中转地址（OpenAI 兼容代理路径）。 */
function defaultMarketEndpoint(): string {
  return `http://${window.location.hostname}:8080/api/v1/gateway/v1/chat/completions`;
}
/** 本机 LLM 实例指标端点提示（metrics_url 输入框 placeholder）。 */
const localMetricsUrlHint = `http://${window.location.hostname}:8080/api/v1/llm/instances/llm-101/metrics`;
const marketPublishForm = ref<{
  api_name: string;
  endpoint_url: string;
  model_name: string;
  /** 上下文长度（可选，server_config.context_len——2026-09-02 起后端透传）。 */
  context_len: number | null;
  description: string;
  tags: string;
  pricing_mode: 'free' | 'per_token' | 'per_image';
  price: number;
  metrics_url: string;
  gpu_model: string;
  gpu_count: number | null;
  gpu_vram_mb: number | null;
  cpu_cores: number | null;
  ram_gb: number | null;
  /** 接入信息三件（可选——消费者直连凭据；发布者本人/admin 可见明文）。 */
  access_api_key: string;
  access_auth_header: string;
  access_notes: string;
}>({
  api_name: '',
  endpoint_url: defaultMarketEndpoint(),
  model_name: '',
  context_len: null,
  description: '',
  tags: '',
  pricing_mode: 'per_token',
  price: 50,
  metrics_url: '',
  gpu_model: '',
  gpu_count: null,
  gpu_vram_mb: null,
  cpu_cores: null,
  ram_gb: null,
  access_api_key: '',
  access_auth_header: '',
  access_notes: '',
});

function openMarketPublish(): void {
  marketPublishForm.value = {
    api_name: '',
    endpoint_url: defaultMarketEndpoint(),
    model_name: '',
    context_len: null,
    description: '',
    tags: '',
    pricing_mode: 'per_token',
    price: 50,
    metrics_url: '',
    gpu_model: '',
    gpu_count: null,
    gpu_vram_mb: null,
    cpu_cores: null,
    ram_gb: null,
    access_api_key: '',
    access_auth_header: '',
    access_notes: '',
  };
  showServerOverride.value = false;
  msg.value = null;
  showMarketPublish.value = true;
}

function closeMarketPublish(): void {
  if (submitting.value) return;
  showMarketPublish.value = false;
}

/** 价格字段标签（per_image 复用单价格字段，语义=每图单价）。 */
const priceFieldLabel = computed(() =>
  marketPublishForm.value.pricing_mode === 'per_image' ? '每图单价（sats）' : '每 1k token 单价（sats）',
);

/** api-market 写操作身份：必须链上 token（无 admin 回落）；未初始化直接引导。 */
async function requireMarketIdentity(): Promise<{ nexhubToken: string }> {
  const s = await ensureNexhubToken();
  return { nexhubToken: s.token };
}

/**
 * 打赏（TipButton）链上 token 获取器：已初始化身份 → nexhub token（tips 服务端
 * IM/nexhub 两桶依次验，反查 from pubkey）；未初始化/失败 → undefined（回落
 * 网关 Principal，测试期默认 admin 归因）。见 docs/TIPS.md。
 */
async function tipTokenGetter(): Promise<string | undefined> {
  if (!marketHasIdentity.value) return undefined;
  try {
    return (await ensureNexhubToken()).token;
  } catch {
    return undefined;
  }
}

/** api-market 写操作 401/403 → 用户文案（401 强制下次重走认证并引导初始化身份）。 */
function marketWriteErr(action: string, e: unknown): string {
  if (e instanceof ApiError && e.status === 401) {
    forceNexhubReauth();
    return `${action}失败（401）：api-market 发布者身份=区块链公钥（不接受 admin token）——请先到 IM（聊天）页生成/导入密钥对后重试`;
  }
  if (e instanceof ApiError && e.status === 403) {
    return `${action}失败（403）：仅发布者本人（链上身份）可操作`;
  }
  return `${action}失败：${e instanceof Error ? e.message : String(e)}`;
}

async function submitMarketPublish(): Promise<void> {
  const f = marketPublishForm.value;
  msg.value = null;
  if (!f.api_name.trim()) {
    msg.value = { kind: 'err', text: '请填写 API 名称' };
    return;
  }
  if (!f.endpoint_url.trim()) {
    msg.value = { kind: 'err', text: '请填写消费端点 endpoint_url' };
    return;
  }
  if (!f.model_name.trim()) {
    msg.value = { kind: 'err', text: '请填写模型名（server_config.model_name 必填——硬件探测拿不到）' };
    return;
  }
  if (f.pricing_mode !== 'free' && (!f.price || f.price <= 0)) {
    msg.value = { kind: 'err', text: '付费模式必须给出单价 > 0' };
    return;
  }
  if (!marketHasIdentity.value) {
    msg.value = { kind: 'err', text: '发布需链上身份：请先到 IM（聊天）页生成/导入密钥对（同一密钥与 API 大厅通用）' };
    return;
  }
  submitting.value = true;
  try {
    const opts = await requireMarketIdentity();
    const pricing =
      f.pricing_mode === 'free'
        ? { mode: 'free' as const }
        : { mode: f.pricing_mode, price_per_1k_tokens: f.price, currency: 'sats' };
    // server_config：默认只传 model_name（硬件字段后端自动探测），展开覆盖表单才带覆盖值
    const serverConfig: Record<string, unknown> = { model_name: f.model_name.trim() };
    // 上下文长度（可选，探测拿不到只认自报；>0 才提交，空=不猜不传）。
    if (f.context_len && f.context_len > 0) serverConfig.context_len = Math.floor(f.context_len);
    if (showServerOverride.value) {
      // GPU 覆盖=「型号 ×数量 / 单卡显存」三输入 → 组装简化形态 gpus=[{name,vram_mb}]×count
      // （后端从列表推导 gpu_count 与旧字段 gpu_name/gpu_vram_mb，index 可省）
      if (f.gpu_model.trim()) {
        const count = Math.max(1, Math.floor(f.gpu_count ?? 1));
        const vram = f.gpu_vram_mb && f.gpu_vram_mb > 0 ? f.gpu_vram_mb : 0;
        serverConfig.gpus = Array.from({ length: count }, () => ({
          name: f.gpu_model.trim(),
          vram_mb: vram,
        }));
      }
      if (f.cpu_cores && f.cpu_cores > 0) serverConfig.cpu_cores = f.cpu_cores;
      if (f.ram_gb && f.ram_gb > 0) serverConfig.ram_gb = f.ram_gb;
    }
    // 接入信息：三字段全空不带（重发布缺省保留既有凭据）；任一非空才提交。
    const accessInfo =
      f.access_api_key.trim() || f.access_auth_header.trim() || f.access_notes.trim()
        ? {
            api_key: f.access_api_key.trim() || undefined,
            auth_header: f.access_auth_header.trim() || undefined,
            notes: f.access_notes.trim() || undefined,
          }
        : undefined;
    const r = (await endpoints.apiMarketPublish(
      {
        api_name: f.api_name.trim(),
        endpoint_url: f.endpoint_url.trim(),
        description: f.description.trim() || undefined,
        tags: f.tags
          .split(/[,，]/)
          .map((s) => s.trim())
          .filter(Boolean),
        metrics_url: f.metrics_url.trim() || undefined,
        pricing,
        server_config: serverConfig,
        access_info: accessInfo,
      },
      opts,
    )) as MarketListing & { refreshed?: boolean };
    msg.value = {
      kind: 'ok',
      text: `已${r.refreshed ? '刷新' : '发布'}「${r.api_name ?? f.api_name}」到 API 大厅（发布者 ${
        r.publisher_display ?? identityEvm.value ?? '链上身份'
      }）`,
    };
    showMarketPublish.value = false;
    await loadMarket();
  } catch (e) {
    msg.value = { kind: 'err', text: marketWriteErr('发布', e) };
  } finally {
    submitting.value = false;
  }
}

// —— 本机条目：心跳上报 / 下架 ——
/**
 * 手动上报心跳：body 复用该条目已缓存的规范化 metrics（6 键名同时是心跳
 * 别名表可接受输入）；无缓存时上报空 body（仅标记节点存活，load 置空）。
 * 自动定时上报见 autoHeartbeatOwnListings（60s 周期，本函数是人肉兜底）。
 */
async function sendHeartbeat(l: MarketListing): Promise<void> {
  if (!l.id) return;
  busyId.value = l.id;
  msg.value = null;
  try {
    const opts = await requireMarketIdentity();
    const mt = metricsOf(l.id)?.metrics;
    const body = mt
      ? {
          load_pct: mt.load_pct ?? undefined,
          running_req: mt.running ?? undefined,
          waiting_req: mt.waiting ?? undefined,
          gpu_cache_usage: mt.gpu_cache ?? undefined,
          tokens_per_sec: mt.tokens_per_sec ?? undefined,
          latency_ms: mt.latency_ms ?? undefined,
        }
      : undefined;
    const r = (await endpoints.apiMarketHeartbeat(l.id, body, opts)) as {
      heartbeat_at?: string;
    };
    msg.value = {
      kind: 'ok',
      text: `心跳已上报（${fmtTime(r.heartbeat_at)}）——大厅 60s 内显示为新鲜在线`,
    };
    await fetchMarketMetrics(l.id);
  } catch (e) {
    msg.value = { kind: 'err', text: marketWriteErr('心跳上报', e) };
  } finally {
    busyId.value = '';
  }
}

async function unlistMarket(l: MarketListing): Promise<void> {
  if (!l.id) return;
  if (!confirm(`确认下架「${l.api_name ?? l.id}」？该操作从大厅删除挂牌条目。`)) return;
  busyId.value = l.id;
  msg.value = null;
  try {
    const opts = await requireMarketIdentity();
    await endpoints.apiMarketUnpublish(l.id, opts);
    msg.value = { kind: 'ok', text: `已下架「${l.api_name ?? l.id}」` };
    await loadMarket();
  } catch (e) {
    msg.value = { kind: 'err', text: marketWriteErr('下架', e) };
  } finally {
    busyId.value = '';
  }
}

/**
 * 自动心跳（60s 周期，startMarketPolling 驱动）：对本机已发布条目静默 POST
 * heartbeat——body 复用该条目已缓存的规范化 metrics；无缓存时空 body（仅
 * 标记存活，load 置空）。与手动上报的差异：完全静默（无身份/403/网络抖动
 * 都不打断界面；手动按钮仍可看错误文案），也不即时回拉 metrics（下一轮
 * 15s metrics 轮询自然刷新徽章）。
 */
async function autoHeartbeatOwnListings(): Promise<void> {
  const own = marketListings.value.filter((l) => l.id && isOwnListing(l));
  if (!own.length) return;
  let opts: { nexhubToken: string };
  try {
    opts = await requireMarketIdentity();
  } catch {
    return; // 无链上身份 / token 签发失败：静默跳过本轮
  }
  await Promise.allSettled(
    own.map(async (l) => {
      const mt = metricsOf(l.id!)?.metrics;
      const body = mt
        ? {
            load_pct: mt.load_pct ?? undefined,
            running_req: mt.running ?? undefined,
            waiting_req: mt.waiting ?? undefined,
            gpu_cache_usage: mt.gpu_cache ?? undefined,
            tokens_per_sec: mt.tokens_per_sec ?? undefined,
            latency_ms: mt.latency_ms ?? undefined,
          }
        : undefined;
      try {
        await endpoints.apiMarketHeartbeat(l.id!, body, opts);
      } catch {
        // 静默：下架/网络抖动不打断界面
      }
    }),
  );
}

// —— Tab 懒加载 + 自动轮询开关 ——
// 首次切入大厅组任一子 Tab（API 大厅 / 我的发布）拉列表（metrics 随列表
// 错峰）；活跃期间保持 15s metrics 轮询 + 60s 本机心跳自动上报，切出即停
// （回到大厅组子 Tab 重新启动）。
watch(activeTab, (t) => {
  if (t === 'market' || t === 'mylistings') {
    if (marketListings.value.length === 0 && !marketLoading.value) {
      void loadMarket();
    }
    startMarketPolling();
  } else {
    stopMarketPolling();
  }
});

/** 页面可见性变化：隐藏时暂停外呼（进行中的错峰也清）；回前台且大厅组子 Tab
 *  活跃立即补一轮 metrics（隐藏期徽章可能已过期）。 */
function onVisibilityChange(): void {
  if (document.hidden) {
    clearMetricTimers();
  } else if (isLobbyTabActive()) {
    scheduleMetricsFetch();
  }
}

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  void refreshAll();
  document.addEventListener('visibilitychange', onVisibilityChange);
});

onUnmounted(() => {
  // 清理错峰拉取定时器与自动轮询（离开页面后不再外呼 metrics/心跳）
  stopMarketPolling();
  document.removeEventListener('visibilitychange', onVisibilityChange);
});
</script>

<template>
  <div class="gw-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">API 网关</h2>
        <div class="page-sub muted">渠道聚合 · 令牌管理 · 代理转发 · 积分充值 · 用量统计</div>
      </div>
      <div class="head-actions">
        <button
          class="btn btn-small"
          :disabled="channelsLoading || tokensLoading"
          @click="refreshAll"
        >
          <span
            class="spin"
            :class="{ spinning: channelsLoading || tokensLoading }"
            aria-hidden="true"
          >↻</span>
          刷新
        </button>
      </div>
    </div>

    <!-- Tab 切换（两级）：一级组 网关｜大厅｜运营 + 组内二级 Tab（虚线下边线） -->
    <nav class="tabs" role="tablist">
      <button
        v-for="g in tabGroups"
        :key="g.key"
        class="tab"
        :class="{ active: activeGroup === g.key }"
        role="tab"
        :aria-selected="activeGroup === g.key"
        @click="switchGroup(g.key)"
      >{{ g.label }}</button>
    </nav>

    <nav class="tabs sub-tabs" role="tablist" aria-label="组内子页切换">
      <button
        v-for="st in groupTabs[activeGroup]"
        :key="st.key"
        class="tab sub-tab"
        :class="{ active: activeTab === st.key }"
        role="tab"
        :aria-selected="activeTab === st.key"
        @click="switchTab(st.key)"
      >{{ st.label }}</button>
    </nav>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== 网关组 · 渠道 =================== -->
    <section v-show="activeTab === 'channels'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">渠道总数</div>
          <div class="stat-value">{{ stats.channels_total ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已启用</div>
          <div class="stat-value">{{ stats.channels_enabled ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">错误</div>
          <div class="stat-value stat-err">{{ channelErrorCount }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">上游渠道列表</span>
        <button class="btn btn-small btn-primary" @click="openCreateChannel">＋ 添加渠道</button>
      </div>

      <div v-if="channelsError" class="error-box">{{ channelsError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="channelColumns"
            :rows="channels"
            :loading="channelsLoading"
            empty-text="暂无渠道，点击右上角「添加渠道」。"
          >
            <template #cell-name="{ row }">
              <span class="ch-name-cell">
                <span :title="row.name ?? ''">{{ row.name ?? '—' }}</span>
                <!-- 中继渠道徽章：via_node 非空 → 转发经联邦 overlay 定向源节点代发 -->
                <span
                  v-if="row.via_node"
                  class="pill pill-fed"
                  :title="`联邦中继渠道 · 经源节点 ${row.via_node.slice(0, 10)}… 代发`"
                >🌐 中继</span>
              </span>
            </template>
            <template #cell-provider="{ row }">
              <span :class="providerClass(row.provider)">{{ row.provider ?? '—' }}</span>
            </template>
            <template #cell-base_url="{ row }">
              <span class="mono small">{{ row.base_url ?? '—' }}</span>
            </template>
            <template #cell-models="{ row }">
              <span class="small">{{ (row.models ?? []).length }} 个</span>
            </template>
            <template #cell-priority="{ row }">
              <span class="mono small">{{ row.priority ?? 0 }} · w{{ row.weight ?? 1 }}</span>
            </template>
            <template #cell-status="{ row }">
              <span :class="channelStatusClass(row.status)">{{ channelStatusLabel(row) }}</span>
            </template>
            <template #cell-request_count="{ row }">
              <span class="mono">{{ row.request_count ?? 0 }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="testChannel(row)"
              >测试</button>
              <button
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="openEditChannel(row)"
              >编辑</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === row.id"
                @click.stop="removeChannel(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 添加/编辑渠道对话框 -->
      <div v-if="showChannelDialog" class="modal-backdrop" @click.self="closeChannelDialog">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-ch-title">
          <div class="modal-head">
            <h3 id="gw-ch-title">{{ editingChannel ? '编辑渠道' : '添加渠道' }}</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeChannelDialog">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitChannel">
            <!-- 从本地发现导入（仅添加模式）：列 gateway/models 真实探测的本地
                 vLLM（实例表内 + 端口扫描发现），点击预填 base_url/models -->
            <div v-if="!editingChannel" class="discovery-box">
              <div class="discovery-head">
                <span class="discovery-title">从本地发现导入</span>
                <span class="muted small">点击条目预填（探测 /v1/models，真实可用）</span>
              </div>
              <div v-if="discoveryLoading" class="muted small">探测本机 vLLM 中…</div>
              <div v-else-if="discoveryError" class="muted small">
                发现列表拉取失败：{{ discoveryError }}（可手填）
              </div>
              <div v-else-if="discoveryEntries.length === 0" class="muted small">
                本机暂未探测到可路由的 vLLM（实例表外会扫描 8123/8000-8010 端口），可手填
              </div>
              <div v-else class="discovery-list">
                <button
                  v-for="e in discoveryEntries"
                  :key="String(e.instance_id ?? `port-${e.port}`)"
                  type="button"
                  class="discovery-row"
                  :disabled="submitting"
                  @click="prefillFromDiscovery(e)"
                >
                  <span class="discovery-name">{{ e.name || `vLLM :${e.port}` }}</span>
                  <span class="mono small muted">:{{ e.port }}</span>
                  <span class="pill pill-cyan">{{ (e.model_ids ?? []).length }} 模型</span>
                  <span v-if="e.discovered" class="pill pill-warning">扫描发现</span>
                </button>
              </div>
            </div>
            <!-- 从外部 API 导入（仅添加模式，2026-09-03 联邦中继双向打通）：
                 点击条目一键 POST from_external_api——后端复制
                 name/base_url/api_key/models/via_node（models 空先探回填） -->
            <div v-if="!editingChannel" class="discovery-box">
              <div class="discovery-head">
                <span class="discovery-title">从外部 API 导入</span>
                <span class="muted small">模型管理「外部 API」登记一键转渠道（含联邦中继条目）</span>
              </div>
              <div v-if="externalLoading" class="muted small">拉取外部 API 登记中…</div>
              <div v-else-if="externalError" class="muted small">
                登记列表拉取失败：{{ externalError }}（可手填）
              </div>
              <div v-else-if="externalApis.length === 0" class="muted small">
                暂无外部 API 登记（模型管理 → 推理环境 → 外部 API 可登记/从联邦大厅导入）
              </div>
              <div v-else class="discovery-list">
                <button
                  v-for="api in externalApis"
                  :key="String(api.id ?? '')"
                  type="button"
                  class="discovery-row"
                  :disabled="submitting || importingExtId !== ''"
                  @click="importFromExternalApi(api)"
                >
                  <span class="discovery-name">{{ api.name || api.id }}</span>
                  <span v-if="api.via_node" class="pill pill-fed" title="联邦中继条目（经源节点代发）">🌐 中继</span>
                  <span class="pill pill-cyan">{{ (api.models ?? []).length }} 模型</span>
                  <span class="muted small">{{ importingExtId === api.id ? '导入中…' : '一键导入' }}</span>
                </button>
              </div>
            </div>
            <div class="field">
              <label for="gw-ch-name">渠道名称</label>
              <input id="gw-ch-name" v-model="channelForm.name" type="text" placeholder="本地vLLM-7B / OpenAI官方" :disabled="submitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-ch-provider">Provider</label>
                <select id="gw-ch-provider" v-model="channelForm.provider" :disabled="submitting">
                  <option value="openai">openai</option>
                  <option value="deepseek">deepseek</option>
                  <option value="anthropic">anthropic</option>
                  <option value="local-vllm">local-vllm</option>
                  <option value="azure">azure</option>
                  <option value="ollama">ollama</option>
                </select>
              </div>
              <div class="field">
                <label for="gw-ch-url">Base URL</label>
                <input id="gw-ch-url" v-model="channelForm.base_url" type="text" placeholder="https://api.openai.com/v1" :disabled="submitting" />
              </div>
            </div>
            <div class="field">
              <label for="gw-ch-key">API Key（上游密钥）</label>
              <input id="gw-ch-key" v-model="channelForm.api_key" type="text" placeholder="sk-xxx（local-vllm 可留空）" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-ch-models">支持的模型（逗号分隔）</label>
              <input id="gw-ch-models" v-model="channelForm.models" type="text" placeholder="gpt-4o, gpt-4o-mini" :disabled="submitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-ch-prio">优先级（数字越小越优先）</label>
                <input id="gw-ch-prio" v-model.number="channelForm.priority" type="number" min="0" :disabled="submitting" />
              </div>
              <div class="field">
                <label for="gw-ch-w">权重（同优先级负载均衡）</label>
                <input id="gw-ch-w" v-model.number="channelForm.weight" type="number" min="1" :disabled="submitting" />
              </div>
            </div>
            <div class="field">
              <label for="gw-ch-via">联邦中继 via_node（可选，0x+66hex NodeID；留空 = 直连）</label>
              <input
                id="gw-ch-via"
                v-model="channelForm.via_node"
                type="text"
                class="mono"
                placeholder="0x…（外部 API 一键导入自动填；非空 = 经源节点中继转发）"
                :disabled="submitting"
              />
              <p class="muted small" style="margin-top: 4px;">
                非空 = 中继渠道：转发不直连上游，经联邦 overlay 定向该源节点代发（源端白名单裁决）。
              </p>
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeChannelDialog">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '保存中…' : '保存' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== 网关组 · 令牌 =================== -->
    <section v-show="activeTab === 'tokens'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">令牌总数</div>
          <div class="stat-value">{{ stats.tokens_total ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">活跃</div>
          <div class="stat-value">{{ stats.tokens_active ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">总请求数</div>
          <div class="stat-value">{{ stats.total_requests ?? 0 }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">下游令牌列表</span>
        <button class="btn btn-small btn-primary" @click="openCreateToken">＋ 创建令牌</button>
      </div>

      <div v-if="tokensError" class="error-box">{{ tokensError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="tokenColumns"
            :rows="tokens"
            :loading="tokensLoading"
            empty-text="暂无令牌，点击右上角「创建令牌」。"
          >
            <template #cell-key="{ row }">
              <span class="key-cell">
                <span class="mono small" :title="row.key ?? ''">{{ truncKey(row.key) }}</span>
                <button
                  v-if="row.key"
                  class="btn btn-small copy-btn"
                  type="button"
                  @click.stop="copyWithToast(String(row.key))"
                >复制</button>
              </span>
            </template>
            <template #cell-status="{ row }">
              <span :class="tokenStatusClass(row)">{{ tokenStatusLabel(row) }}</span>
            </template>
            <template #cell-billing_mode="{ row }">
              <span :class="billingModeClass(row)">{{ billingModeLabel(row) }}</span>
            </template>
            <template #cell-quota="{ row }">
              <div v-if="billingModeOf(row) === 'free'" class="quota-cell">
                <span class="mono strong">∞</span>
                <span class="muted small">免费 · 无限制</span>
              </div>
              <div v-else-if="billingModeOf(row) === 'credits'" class="quota-cell">
                <span class="mono strong">余 {{ tokenQuotaRemaining(row) ?? '—' }} 积分</span>
                <span class="mono small">{{ row.quota_used ?? 0 }} / {{ row.quota_limit ?? 0 }}</span>
                <span v-if="(row.quota_limit ?? 0) > 0" class="prog-wrap">
                  <span class="prog-bar"><span class="prog-fill" :class="{ 'fill-warn': quotaUsagePct(row) >= 90 }" :style="{ width: quotaUsagePct(row) + '%' }" /></span>
                  <span class="prog-text">{{ quotaUsagePct(row) }}%</span>
                </span>
              </div>
              <div v-else class="quota-cell">
                <span class="mono small">{{ row.quota_used ?? 0 }} / {{ row.quota_limit ?? 0 }}</span>
                <span v-if="(row.quota_limit ?? 0) > 0" class="prog-wrap">
                  <span class="prog-bar"><span class="prog-fill" :class="{ 'fill-warn': quotaUsagePct(row) >= 90 }" :style="{ width: quotaUsagePct(row) + '%' }" /></span>
                  <span class="prog-text">{{ quotaUsagePct(row) }}%</span>
                </span>
                <span v-else class="muted small">无限制</span>
              </div>
            </template>
            <template #cell-allowed_models="{ row }">
              <span class="small">{{ (row.allowed_models ?? []).length ? (row.allowed_models ?? []).join(', ') : '全部' }}</span>
            </template>
            <template #cell-request_count="{ row }">
              <span class="mono">{{ row.request_count ?? 0 }}</span>
            </template>
            <template #cell-created_at="{ row }">
              <span class="mono small">{{ (row.created_at ?? '').slice(0, 19).replace('T', ' ') || '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="busyId === row.id"
                :title="t('gwTokenAccess.rowBtnTitle')"
                @click.stop="openTokenAccessForRow(row)"
              >{{ t('gwTokenAccess.rowBtn') }}</button>
              <button
                v-if="row.enabled"
                class="btn btn-small btn-warning"
                :disabled="busyId === row.id"
                @click.stop="disableToken(row)"
              >禁用</button>
              <button
                v-else
                class="btn btn-small btn-primary"
                :disabled="busyId === row.id"
                @click.stop="enableToken(row)"
              >启用</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === row.id"
                @click.stop="removeToken(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 创建令牌对话框（创建成功即关，并弹出下方「接入信息」面板展示一次性完整 key） -->
      <div v-if="showTokenDialog" class="modal-backdrop" @click.self="closeTokenDialog">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-tok-title">
          <div class="modal-head">
            <h3 id="gw-tok-title">创建令牌</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeTokenDialog">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitToken">
            <div class="field">
              <label for="gw-tok-name">令牌名称</label>
              <input id="gw-tok-name" v-model="tokenForm.name" type="text" placeholder="前端应用 / 测试key" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-tok-billing">计费模式</label>
              <select id="gw-tok-billing" v-model="tokenForm.billing_mode" :disabled="submitting">
                <option v-for="o in billingModeOptions" :key="o.value" :value="o.value">{{ o.label }}</option>
              </select>
              <span class="muted small">{{ billingModeHint }}</span>
            </div>
            <div v-if="tokenForm.billing_mode === 'credits'" class="field">
              <label for="gw-tok-credits">初始积分（写入积分池上限）</label>
              <input id="gw-tok-credits" v-model.number="tokenForm.initial_credits" type="number" min="0" :disabled="submitting" />
            </div>
            <div v-else class="field">
              <label for="gw-tok-quota">配额上限（0=无限）</label>
              <input id="gw-tok-quota" v-model.number="tokenForm.quota_limit" type="number" min="0" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-tok-models">允许模型（逗号分隔，空=全部）</label>
              <input id="gw-tok-models" v-model="tokenForm.allowed_models" type="text" placeholder="gpt-4o, gpt-4o-mini" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-tok-exp">过期时间（可选，留空=永不过期）</label>
              <input id="gw-tok-exp" v-model="tokenForm.expires_at" type="text" placeholder="2026-12-31T23:59:59+08:00" :disabled="submitting" />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeTokenDialog">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 令牌「接入信息」面板：创建成功自动弹（含一次性完整 key）/ 存量令牌行
           「接入」打开（key 以 <该令牌的完整密钥> 占位——完整密钥仅创建时展示）。
           三段式钉底：head 固定 + body 滚动 + 底部操作 sticky；接入块/curl 复用
           接入说明 acc-* 代码块样式，组装走 gatewayAgentBlock 公用函数。 -->
      <div v-if="tokenAccess" class="modal-backdrop" @click.self="closeTokenAccess">
        <div class="modal token-access-modal" role="dialog" aria-modal="true" aria-labelledby="gw-tokacc-title">
          <div class="modal-head">
            <h3 id="gw-tokacc-title">
              {{ tokenAccess.fullKey ? t('gwTokenAccess.titleCreated') : t('gwTokenAccess.titleExisting') }}
            </h3>
            <button class="modal-close" type="button" @click="closeTokenAccess">×</button>
          </div>
          <div class="modal-body">
            <!-- ① 警示条 + 完整密钥（创建形态大字等宽+独立复制 / 存量形态占位说明） -->
            <div class="key-reveal-box">
              <p v-if="tokenAccess.fullKey" class="key-reveal-warn">
                ⚠️ {{ t('gwTokenAccess.warnOnce', { name: tokenAccess.name }) }}
              </p>
              <p v-else class="key-reveal-warn">{{ t('gwTokenAccess.keyPlaceholderNote') }}</p>
              <div v-if="tokenAccess.fullKey" class="key-reveal-value">
                <code class="mono tok-key">{{ tokenAccess.fullKey }}</code>
                <button
                  class="btn btn-small btn-primary"
                  type="button"
                  @click="copyAccess('tokKey', tokenAccess.fullKey)"
                >{{ accessCopied === 'tokKey' ? '✓ ' + t('gatewayAccess.copied') : t('gwTokenAccess.copyKey') }}</button>
              </div>
              <div v-else class="key-reveal-value">
                <code class="mono tok-key tok-key-ph">{{ TOKEN_KEY_PLACEHOLDER }}</code>
              </div>
            </div>

            <!-- ② 接入要点：Base URL / 计费·配额回显 / 允许模型 / 模型清单状态 -->
            <div class="tok-kv-list">
              <div class="acc-kv">
                <span class="acc-k">{{ t('gatewayAccess.baseUrlLabel') }}</span>
                <code class="acc-v">{{ accessBaseUrl }}</code>
                <button
                  class="btn btn-small acc-copy-inline"
                  :class="{ copied: accessCopied === 'tokBaseUrl' }"
                  type="button"
                  @click="copyAccess('tokBaseUrl', accessBaseUrl)"
                >{{ accessCopied === 'tokBaseUrl' ? '✓' : t('gatewayAccess.copy') }}</button>
              </div>
              <div class="acc-kv">
                <span class="acc-k">{{ t('gwTokenAccess.billingLabel') }}</span>
                <span :class="billingModeClassOf(tokenAccess.billingMode)">{{ billingModeLabelOf(tokenAccess.billingMode) }}</span>
                <span class="muted small">{{ tokenAccessQuotaText }}</span>
              </div>
              <div class="acc-kv">
                <span class="acc-k">{{ t('gwTokenAccess.allowedModelsLabel') }}</span>
                <span class="small">{{ tokenAccess.allowedModels.length ? tokenAccess.allowedModels.join(', ') : t('gwTokenAccess.allowedModelsAll') }}</span>
              </div>
              <p v-if="tokenAccessModelsLoading" class="acc-note">{{ t('gwTokenAccess.modelsLoading') }}</p>
              <p v-else-if="tokenAccessModelsError" class="acc-note acc-err">{{ t('gwTokenAccess.modelsErr', { err: tokenAccessModelsError }) }}</p>
              <p v-else-if="tokenAccessModelsLoaded && tokenAccessModels.length" class="acc-note">{{ t('gwTokenAccess.modelsOk', { n: tokenAccessModels.length }) }}</p>
              <p v-else-if="tokenAccessModelsLoaded && tokenAccess.allowedModels.length" class="acc-note">{{ t('gwTokenAccess.modelsEmptyWhitelist') }}</p>
            </div>

            <!-- ③ 一键复制完整接入块（Base URL + 该令牌 key + 模型清单 + curl 两例 + SDK） -->
            <div class="acc-step">{{ t('gwTokenAccess.blockTitle') }}</div>
            <p class="acc-note">{{ t('gwTokenAccess.blockHint') }}</p>
            <div class="acc-code acc-block-code">
              <pre class="acc-pre">{{ tokenAccessAgentBlock }}</pre>
              <button
                class="btn btn-small acc-copy acc-copy-big"
                :class="{ copied: accessCopied === 'tokBlock' }"
                type="button"
                @click="copyAccess('tokBlock', tokenAccessAgentBlock)"
              >{{ accessCopied === 'tokBlock' ? '✓ ' + t('gatewayAccess.copied') : t('gwTokenAccess.copyBlockBtn') }}</button>
            </div>

            <!-- ④ 单条 curl 直取（非流式/流式，Bearer 用该令牌 key/占位符） -->
            <div class="acc-step">{{ t('gwTokenAccess.curlChatTitle') }}</div>
            <div class="acc-code">
              <pre class="acc-pre">{{ tokenAccessCurlChat }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'tokCurlChat' }"
                type="button"
                @click="copyAccess('tokCurlChat', tokenAccessCurlChat)"
              >{{ accessCopied === 'tokCurlChat' ? '✓' : t('gatewayAccess.copy') }}</button>
            </div>
            <div class="acc-step">{{ t('gwTokenAccess.curlStreamTitle') }}</div>
            <div class="acc-code">
              <pre class="acc-pre">{{ tokenAccessCurlStream }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'tokCurlStream' }"
                type="button"
                @click="copyAccess('tokCurlStream', tokenAccessCurlStream)"
              >{{ accessCopied === 'tokCurlStream' ? '✓' : t('gatewayAccess.copy') }}</button>
            </div>

            <!-- 底部操作（sticky 钉底——复用 .modal-body .form-actions 三段式先例） -->
            <div class="form-actions">
              <button type="button" class="btn btn-primary" @click="closeTokenAccess">
                {{ tokenAccess.fullKey ? t('gwTokenAccess.doneBtn') : t('gwTokenAccess.closeBtn') }}
              </button>
            </div>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== 网关组 · 日志 =================== -->
    <section v-show="activeTab === 'logs'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">调用日志（最新 {{ logLimit }} 条）</span>
        <div class="head-actions">
          <label class="small muted">每页：</label>
          <select v-model.number="logLimit" class="limit-select" @change="loadLogs">
            <option :value="20">20</option>
            <option :value="50">50</option>
            <option :value="100">100</option>
          </select>
        </div>
      </div>

      <div v-if="logsError" class="error-box">{{ logsError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="logColumns"
            :rows="logs"
            :loading="logsLoading"
            empty-text="暂无调用日志。"
          >
            <template #cell-created_at="{ row }">
              <span class="mono small">{{ (row.created_at ?? '').slice(0, 19).replace('T', ' ') || '—' }}</span>
            </template>
            <template #cell-prompt_tokens="{ row }">
              <span class="mono">{{ row.prompt_tokens ?? 0 }}</span>
            </template>
            <template #cell-completion_tokens="{ row }">
              <span class="mono">{{ row.completion_tokens ?? 0 }}</span>
            </template>
            <template #cell-total_tokens="{ row }">
              <span class="mono strong">{{ row.total_tokens ?? 0 }}</span>
            </template>
            <template #cell-latency_ms="{ row }">
              <span class="mono">{{ row.latency_ms ?? 0 }}</span>
            </template>
            <template #cell-status="{ row }">
              <span :class="logStatusClass(row.status)">{{ row.status ?? '—' }}</span>
            </template>
          </DataTable>
        </div>
      </div>
    </section>

    <!-- =================== 运营组 · 充值订单 =================== -->
    <section v-show="activeTab === 'payments'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">待确认</div>
          <div class="stat-value stat-warn">{{ pendingPaymentCount }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已到账</div>
          <div class="stat-value">{{ confirmedPaymentCount }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已拒绝</div>
          <div class="stat-value stat-err">{{ rejectedPaymentCount }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">订单总数</div>
          <div class="stat-value">{{ payments.length }}</div>
        </div>
      </section>

      <!-- 创建订单表单 -->
      <div class="panel-head">
        <span class="panel-title">创建充值订单</span>
        <span class="muted small">计费单位 = 积分；金额随币种换算</span>
      </div>
      <div class="card pay-form-card">
        <form class="pay-form" @submit.prevent="submitPayment">
          <div class="field">
            <label for="gw-pay-token">充值令牌</label>
            <select id="gw-pay-token" v-model="paymentForm.token_id" :disabled="submitting">
              <option value="">— 选择令牌 —</option>
              <option v-for="t in tokens" :key="t.id" :value="t.id">
                {{ t.name ?? t.id }}（{{ billingModeLabel(t) }}）
              </option>
            </select>
          </div>
          <div class="field">
            <label for="gw-pay-cur">支付币种</label>
            <select id="gw-pay-cur" v-model="paymentForm.currency" :disabled="submitting">
              <option v-for="c in currencyOptions" :key="c.value" :value="c.value">{{ c.label }}</option>
            </select>
          </div>
          <div class="field">
            <label for="gw-pay-credits">充值积分数</label>
            <input id="gw-pay-credits" v-model.number="paymentForm.credits" type="number" min="1" :disabled="submitting" />
          </div>
          <div class="field pay-form-actions">
            <button type="submit" class="btn btn-primary" :disabled="submitting || paymentsLoading">
              {{ submitting ? '创建中…' : '创建订单' }}
            </button>
          </div>
        </form>
      </div>

      <!-- 新建订单收款信息（醒目展示） -->
      <div v-if="createdPayment" class="card pay-info-card">
        <div class="pay-info-head">
          <h3 class="pay-info-title">收款信息（订单 {{ createdPayment.id ?? '—' }} · 待支付）</h3>
          <button class="modal-close pay-info-close" type="button" @click="dismissCreatedPayment">×</button>
        </div>
        <div v-if="createdPayment.warning" class="error-box">{{ createdPayment.warning }}</div>
        <div class="pay-info-grid">
          <div class="pay-info-item">
            <div class="pay-info-label">应付金额（{{ currencyLabel(createdPayment.currency) }}）</div>
            <div class="pay-info-value mono">{{ payAmount(createdPayment) }}</div>
            <div class="muted small">单位：{{ amountUnitHint(createdPayment.currency) }}</div>
          </div>
          <div class="pay-info-item">
            <div class="pay-info-label">充值积分</div>
            <div class="pay-info-value mono">{{ createdPayment.credits ?? 0 }}</div>
            <div class="muted small">到账后计入令牌「{{ tokenNameOf(createdPayment.token_id) }}」积分池</div>
          </div>
        </div>
        <div class="pay-info-item pay-info-addr">
          <div class="pay-info-label">收款地址（{{ currencyLabel(createdPayment.currency) }}）</div>
          <!-- 占位收款地址（env 未配置真实钱包）→ 红色醒目警示 + 禁用复制 -->
          <div v-if="isPlaceholderPayAddress(createdPayment.address)" class="pay-placeholder-banner">
            ⚠️ 占位收款地址（未配置真实钱包）——请勿真实转账，支付通道上线前仅供流程演示
          </div>
          <div v-if="createdPayment.address" class="pay-addr-row">
            <code
              class="mono pay-addr"
              :class="{ 'pay-addr-placeholder': isPlaceholderPayAddress(createdPayment.address) }"
            >{{ createdPayment.address }}</code>
            <button
              class="btn btn-small btn-primary"
              type="button"
              :disabled="isPlaceholderPayAddress(createdPayment.address)"
              @click="copyWithToast(createdPayment.address ?? '')"
            >一键复制</button>
          </div>
          <div v-else class="error-box">未获取到收款地址（服务端未配置该币种收款地址 env），请勿转账，联系管理员后再试。</div>
        </div>
        <p v-if="createdPayment.memo" class="muted small pay-memo">备注：{{ createdPayment.memo }}</p>
      </div>

      <!-- 订单列表 -->
      <div class="panel-head">
        <span class="panel-title">订单列表（最新在前）</span>
        <button class="btn btn-small" :disabled="paymentsLoading" @click="loadPayments">
          <span class="spin" :class="{ spinning: paymentsLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div v-if="paymentsError" class="error-box">{{ paymentsError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="paymentColumns"
            :rows="payments"
            :loading="paymentsLoading"
            empty-text="暂无充值订单，请在上方创建。"
          >
            <template #cell-created_at="{ row }">
              <span class="mono small">{{ (row.created_at ?? '').slice(0, 19).replace('T', ' ') || '—' }}</span>
            </template>
            <template #cell-token_id="{ row }">
              <span class="small">{{ tokenNameOf(row.token_id) }}</span>
            </template>
            <template #cell-currency="{ row }">
              <span :class="row.currency === 'usdt' ? 'pill pill-cyan' : row.currency === 'btc' ? 'pill pill-orange' : 'pill pill-purple'">{{ currencyLabel(row.currency) }}</span>
            </template>
            <template #cell-amount_crypto="{ row }">
              <div class="quota-cell">
                <span class="mono">{{ payAmount(row) }}</span>
                <span class="muted small">{{ amountUnitHint(row.currency) }}</span>
              </div>
            </template>
            <template #cell-credits="{ row }">
              <span class="mono strong">{{ row.credits ?? 0 }}</span>
            </template>
            <template #cell-status="{ row }">
              <span :class="payStatusClass(row.status)">{{ payStatusLabel(row.status) }}</span>
            </template>
            <template #cell-txid="{ row }">
              <span v-if="row.txid" class="mono small" :title="row.txid">{{ truncKey(row.txid) }}</span>
              <span v-else class="muted small">—</span>
            </template>
            <template #cell-actions="{ row }">
              <template v-if="row.status === 'pending'">
                <button
                  class="btn btn-small btn-primary"
                  @click.stop="openConfirmPayment(row)"
                >确认到账</button>
                <button
                  class="btn btn-small btn-danger"
                  @click.stop="openRejectPayment(row)"
                >拒绝</button>
              </template>
              <span v-else class="muted small">—</span>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 确认到账对话框（txid 可选） -->
      <div v-if="showConfirmPayDialog" class="modal-backdrop" @click.self="closeConfirmPayDialog">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-payc-title">
          <div class="modal-head">
            <h3 id="gw-payc-title">确认到账</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeConfirmPayDialog">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitConfirmPayment">
            <p class="muted small pay-confirm-summary">
              订单 <span class="mono">{{ confirmingOrder?.id ?? '—' }}</span> ·
              {{ currencyLabel(confirmingOrder?.currency) }}
              <span class="mono">{{ confirmingOrder ? payAmount(confirmingOrder) : '—' }}</span> →
              为令牌「{{ confirmingOrder ? tokenNameOf(confirmingOrder.token_id) : '—' }}」充值
              <span class="mono">{{ confirmingOrder?.credits ?? 0 }}</span> 积分。
              确认后立即入账，<strong>请先核对链上/钱包转账已收到</strong>。
            </p>
            <div class="field">
              <label for="gw-payc-txid">链上交易哈希 TxID（可选，用于对账）</label>
              <input id="gw-payc-txid" v-model="confirmPayTxid" type="text" placeholder="0x… / 链上 txid，可留空" :disabled="submitting" />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeConfirmPayDialog">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '确认中…' : '确认已到账' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 拒绝订单对话框（原因可选） -->
      <div v-if="showRejectPayDialog" class="modal-backdrop" @click.self="closeRejectPayDialog">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-payr-title">
          <div class="modal-head">
            <h3 id="gw-payr-title">拒绝订单</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeRejectPayDialog">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitRejectPayment">
            <p class="muted small pay-confirm-summary">
              拒绝订单 <span class="mono">{{ rejectingOrder?.id ?? '—' }}</span>
              （{{ rejectingOrder ? tokenNameOf(rejectingOrder.token_id) : '—' }} ·
              {{ rejectingOrder?.credits ?? 0 }} 积分）。拒绝后订单关闭，不会入账。
            </p>
            <div class="field">
              <label for="gw-payr-reason">拒绝原因（可选）</label>
              <input id="gw-payr-reason" v-model="rejectPayReason" type="text" placeholder="未收到转账 / 金额不符…" :disabled="submitting" />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeRejectPayDialog">取消</button>
              <button type="submit" class="btn btn-danger" :disabled="submitting">
                {{ submitting ? '提交中…' : '确认拒绝' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== 网关组 · 总览（默认落地 Tab） =================== -->
    <section v-show="activeTab === 'overview'" class="tab-panel">
      <!-- ① 顶部统计卡：既有端点拼装（stats/tokens + channels/logs/payments 客户端聚合） -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">{{ t('gwOverview.callsToday') }}</div>
          <div class="stat-value">{{ todayCalls }}</div>
          <div class="stat-sub muted small">
            {{ t('gwOverview.successRate') }}：
            <span class="mono">{{ todaySuccessRate === null ? '—' : `${todaySuccessRate}%` }}</span>
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('gwOverview.channelsActive') }}</div>
          <div class="stat-value">{{ enabledChannels.length }}</div>
          <div class="stat-sub muted small">
            {{ t('gwOverview.directCount', { n: directChannelCount }) }} ·
            {{ t('gwOverview.relayCount', { n: relayChannelCount }) }}
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('gwOverview.tokensTotal') }}</div>
          <div class="stat-value">{{ stats.tokens_total ?? 0 }}</div>
          <div class="stat-sub muted small">
            {{ t('gwOverview.tokensActiveSub', { n: stats.tokens_active ?? 0 }) }}
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('gwOverview.creditsFlow') }}</div>
          <div class="stat-value">{{ settledCredits }}</div>
          <div class="stat-sub muted small">
            {{ t('gwOverview.creditsSettled', { n: settledCredits }) }} ·
            {{ t('gwOverview.creditsPending', { n: pendingCredits }) }}
          </div>
        </div>
        <!-- 累计卡（原「总览」总请求/总 Tokens/成功率 三个存量指标收拢为一张，
             数据同源 /gateway/stats——避免信息丢失） -->
        <div class="card stat-card">
          <div class="stat-label">{{ t('gwOverview.lifetime') }}</div>
          <div class="stat-value">{{ stats.total_requests ?? 0 }}</div>
          <div class="stat-sub muted small">
            {{ t('gwOverview.lifetimeSub', {
              tokens: (stats.total_tokens ?? 0).toLocaleString(),
              rate: stats.success_rate === undefined || stats.success_rate === null
                ? '—'
                : `${Math.round(stats.success_rate)}%`,
            }) }}
          </div>
        </div>
      </section>
      <p class="muted small ov-stat-note">{{ t('gwOverview.statsNote', { n: logLimit }) }}</p>

      <!-- ② 快捷入口卡：高频动作一步直达（切 Tab + 开对话框） -->
      <div class="panel-head">
        <span class="panel-title">{{ t('gwOverview.quickTitle') }}</span>
      </div>
      <section class="quick-grid">
        <button type="button" class="card quick-card" @click="quickCreateChannel">
          <span class="quick-title">＋ {{ t('gwOverview.quickChannel') }}</span>
          <span class="muted small">{{ t('gwOverview.quickChannelHint') }}</span>
        </button>
        <button type="button" class="card quick-card" @click="quickCreateToken">
          <span class="quick-title">＋ {{ t('gwOverview.quickToken') }}</span>
          <span class="muted small">{{ t('gwOverview.quickTokenHint') }}</span>
        </button>
        <button type="button" class="card quick-card" @click="quickPublishApi">
          <span class="quick-title">🌐 {{ t('gwOverview.quickPublish') }}</span>
          <span class="muted small">{{ t('gwOverview.quickPublishHint') }}</span>
        </button>
        <button type="button" class="card quick-card" @click="quickImportExternal">
          <span class="quick-title">⇩ {{ t('gwOverview.quickImport') }}</span>
          <span class="muted small">{{ t('gwOverview.quickImportHint') }}</span>
        </button>
      </section>

      <!-- ③ 最近调用摘要（日志前 5 条；与「日志」Tab 同源数据） -->
      <div class="panel-head">
        <span class="panel-title">{{ t('gwOverview.recentTitle') }}</span>
        <button class="btn btn-small" @click="switchTab('logs')">{{ t('gwOverview.recentMore') }}</button>
      </div>
      <div class="card recent-card">
        <div v-if="logsLoading" class="empty-inline muted small">{{ t('gwOverview.loading') }}</div>
        <div v-else-if="recentLogs.length === 0" class="empty-inline muted small">
          {{ t('gwOverview.recentEmpty') }}
        </div>
        <ul v-else class="recent-list">
          <li
            v-for="l in recentLogs"
            :key="l.id ?? `${l.created_at ?? ''}-${l.model ?? ''}-${l.latency_ms ?? ''}`"
            class="recent-row"
          >
            <span class="mono small muted recent-time">{{ (l.created_at ?? '').slice(0, 19).replace('T', ' ') || '—' }}</span>
            <span class="small recent-token">{{ l.token_name ?? '—' }}</span>
            <span class="mono small recent-model">{{ l.model ?? '—' }}</span>
            <span :class="logStatusClass(l.status)">{{ l.status ?? '—' }}</span>
            <span class="mono small muted">{{ l.latency_ms ?? 0 }} ms</span>
            <span class="mono small muted">{{ l.total_tokens ?? 0 }} tok</span>
          </li>
        </ul>
      </div>

      <!-- ④ 可用模型聚合（原「总览」既有功能保留） -->
      <div class="panel-head">
        <span class="panel-title">可用模型（聚合所有渠道，去重）</span>
        <span class="muted small">共 {{ aggregatedModels.count ?? 0 }} 个</span>
      </div>
      <div class="card models-card">
        <div v-if="modelsLoading" class="empty-inline muted small">加载中…</div>
        <div v-else-if="(aggregatedModels.models ?? []).length === 0" class="empty-inline muted small">
          暂无可用模型（请先添加渠道并启用）。
        </div>
        <div v-else class="model-tag-list">
          <span v-for="m in aggregatedModels.models" :key="m" class="model-tag mono">{{ m }}</span>
        </div>
      </div>

      <!-- ⑤ 模型映射管理（原「总览」既有功能保留） -->
      <div class="panel-head">
        <span class="panel-title">模型映射（对外模型名 → 渠道 + 上游真实模型）</span>
        <button class="btn btn-small btn-primary" @click="openCreateMapping">＋ 添加映射</button>
      </div>
      <div v-if="mappingsError" class="error-box">{{ mappingsError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="[
              { key: 'public_name', title: '对外模型名' },
              { key: 'channel_id', title: '渠道' },
              { key: 'upstream_model', title: '上游模型' },
              { key: 'actions', title: '操作' },
            ]"
            :rows="mappings"
            :loading="mappingsLoading"
            empty-text="暂无映射。"
          >
            <template #cell-channel_id="{ row }">
              <span class="mono small">{{ channels.find((c) => c.id === row.channel_id)?.name ?? row.channel_id }}</span>
            </template>
            <template #cell-upstream_model="{ row }">
              <span class="mono small">{{ row.upstream_model }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                @click.stop="removeMapping(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 添加映射对话框 -->
      <div v-if="showMappingDialog" class="modal-backdrop" @click.self="closeMappingDialog">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-map-title">
          <div class="modal-head">
            <h3 id="gw-map-title">添加模型映射</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeMappingDialog">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitMapping">
            <div class="field">
              <label for="gw-map-pub">对外模型名</label>
              <input id="gw-map-pub" v-model="mappingForm.public_name" type="text" placeholder="gpt-4" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-map-ch">渠道</label>
              <select id="gw-map-ch" v-model="mappingForm.channel_id" :disabled="submitting">
                <option value="">— 选择渠道 —</option>
                <option v-for="c in channels" :key="c.id" :value="c.id">{{ c.name }} ({{ c.id }})</option>
              </select>
            </div>
            <div class="field">
              <label for="gw-map-up">上游真实模型名</label>
              <input id="gw-map-up" v-model="mappingForm.upstream_model" type="text" placeholder="gpt-4o" :disabled="submitting" />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeMappingDialog">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '保存中…' : '保存' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== 网关组 · 实例（复用 InstanceMonitor 组件） =================== -->
    <!-- v-if 挂载：Tab 激活才拉数据/轮询，卸载即停。组件自包含（实例列表拉取 +
         10s metrics 轮询 + 30s 网关聚合刷新），网关运营者在此直看本机哪些
         vLLM 实例在跑、健康状态、可路由模型（真实探测 /v1/models）。 -->
    <section v-if="activeTab === 'llmmon'" class="tab-panel">
      <InstanceMonitor />
    </section>

    <!-- =================== 大厅组 · API 大厅（推理服务市场；浏览与消费视角） =================== -->
    <section v-show="activeTab === 'market'" class="tab-panel">
      <!-- 大厅来源切换（本地/联邦，原二级 Tab 功能保留——组内已有二级 Tab，
           这里改为分段开关避免两级 Tab 视觉堆叠；切换只换视图，搜索/排序保留） -->
      <div class="market-view-switch" role="tablist" aria-label="大厅来源切换">
        <button
          type="button"
          class="seg-btn"
          :class="{ active: marketView === 'local' }"
          role="tab"
          :aria-selected="marketView === 'local'"
          @click="marketView = 'local'"
        >🏠 {{ t('apiMarket.viewLocal') }} ({{ localMarketListings.length }})</button>
        <button
          type="button"
          class="seg-btn"
          :class="{ active: marketView === 'fed' }"
          role="tab"
          :aria-selected="marketView === 'fed'"
          @click="marketView = 'fed'"
        ><span class="fed-icon" aria-hidden="true">🌐</span> {{ t('apiMarket.viewFed') }} ({{ fedMarketListings.length }})</button>
      </div>

      <!-- 工具条：搜索 + 排序 + 发布 -->
      <div class="panel-head market-toolbar">
        <span class="panel-title">API 大厅 · 推理服务市场</span>
        <div class="head-actions">
          <input
            v-model="marketQ"
            class="market-search"
            type="text"
            placeholder="搜索名称 / 描述 / 标签"
            :disabled="marketLoading"
            @keyup.enter="loadMarket"
          />
          <select
            v-model="marketSort"
            class="limit-select"
            :disabled="marketLoading"
            @change="loadMarket"
          >
            <option value="recent">最近发布</option>
            <option value="price">价格优先</option>
          </select>
          <button class="btn btn-small" :disabled="marketLoading" @click="loadMarket">
            <span class="spin" :class="{ spinning: marketLoading }" aria-hidden="true">↻</span>
            刷新
          </button>
          <button class="btn btn-small btn-primary" @click="openMarketPublish">＋ 发布 API</button>
        </div>
      </div>
      <p class="muted small market-hint">
        发布者=链上身份（区块链公钥，无 admin 回落）· 价格排序=付费升序免费垫底 ·
        本地徽章=心跳≤60s 绿 / 代拉降级灰 / 不可达红 · 联邦徽章=🌐 经源节点中继（不做主动探测，附源节点心跳时间差）
      </p>

      <div v-if="marketError" class="error-box">{{ marketError }}</div>
      <div v-if="marketLoading && marketListings.length === 0" class="card market-empty">
        <span class="muted small">加载中…</span>
      </div>
      <div v-else-if="!marketError && visibleMarketListings.length === 0" class="card market-empty">
        <span v-if="marketView === 'fed'" class="muted small">
          暂无联邦条目——其他 NexOS 节点发布的项目会自动出现在这里
        </span>
        <span v-else class="muted small">大厅暂无挂牌——点击右上角「发布 API」把本机推理服务挂上来。</span>
      </div>
      <div v-else class="market-grid">
        <div
          v-for="l in visibleMarketListings"
          :key="l.id"
          class="card market-card"
          :class="{ 'is-expanded': isMarketCardExpanded(l) }"
          @click="toggleMarketCard($event, l)"
        >
          <!-- ① 主区：标题行（名称+价格+来源+状态徽章，一行不折行）+ 描述（默认两行
               clamp，点卡片展开全文）+ 联邦心跳时间差行 + tags 行内小 pill -->
          <div class="mkt-main">
            <div class="mkt-title-row">
              <span class="market-name" :title="l.api_name ?? ''">{{ l.api_name ?? '—' }}</span>
              <span :class="priceBadge(l).cls">{{ priceBadge(l).text }}</span>
              <span
                v-if="isOwnListing(l)"
                class="pill pill-cyan"
                title="publisher_pubkey 与当前链上身份一致"
              >
                <img v-if="publisherIdenticon(l)" class="identicon" :src="publisherIdenticon(l) ?? ''" alt="" />本机
              </span>
              <span
                v-if="isRemoteListing(l)"
                class="pill pill-fed"
                :title="`联邦远程条目：来自 ${l.source_node} 节点`"
              >🌐 {{ l.source_node }}</span>
              <!-- 状态徽章（2026-09-03 语义分流）：本地条目=负载直连探测（心跳≤60s 绿
                   /代拉降级灰/不可达红）；联邦条目=🌐 中继常驻徽章——中继路径可达性由
                   消费行为证明（调用即通），不做主动探测 -->
              <button
                v-if="!isRemoteListing(l)"
                type="button"
                class="load-badge"
                :title="loadBadgeTitle(l.id)"
                @click.stop="openMetricsDetail(l)"
              >
                <span :class="loadDotClass(l.id)" aria-hidden="true"></span>
                <span class="mono small">{{ loadBadgeText(l.id) }}</span>
              </button>
              <span
                v-else
                class="load-badge mkt-relay"
                :title="t('apiMarket.relayBadgeTitle', { node: l.source_node ?? '' })"
              >🌐 {{ t('apiMarket.relayBadge') }}</span>
            </div>
            <p
              class="market-desc muted small"
              :class="{ 'desc-clamped': !isMarketCardExpanded(l) }"
            >{{ l.description || t('apiMarket.noDesc') }}</p>
            <!-- 联邦条目心跳新鲜度独立行：只展示时间差（无心跳 '--' 不猜；快照可见性
                 ≤30min 重播周期，tooltip 详述），不再映射成「不可达」 -->
            <p v-if="isRemoteListing(l)" class="mkt-hb-row muted small" :title="t('apiMarket.hbAgeTitle')">
              {{ t('apiMarket.hbAgeLabel') }}：<span class="mono">{{ heartbeatAgeLabel(l) }}</span>
            </p>
            <div v-if="(l.tags ?? []).length" class="market-tags">
              <span v-for="tg in l.tags" :key="tg" class="market-tag mono">#{{ tg }}</span>
            </div>
            <p v-if="l.pricing?.note" class="mkt-price-note muted small">{{ l.pricing.note }}</p>
          </div>

          <!-- ② 硬件/规格区：GPU/CPU/内存/模型/上下文（+量化/区域有值才显示）真两列
               自适应网格（auto-fill minmax(180px,1fr)——窄卡自动回单列）；标签小字
               灰色上标、值主体；GPU 型号+显存/统一内存同格（gpuSummary 合并） -->
          <div class="market-config">
            <div v-for="r in configRows(l)" :key="r.k" class="market-config-row">
              <span class="market-config-k">{{ r.k }}</span>
              <span class="market-config-v" :title="r.v">{{ r.v }}</span>
            </div>
          </div>

          <!-- ③ 接入信息折叠面板（默认收起）：密钥/鉴权头/备注 + curl 示例代码块
               （MD 代码块样式：等宽/深色底/横滚/右上复制按钮——复用 ob-* 与 acc-* 先例） -->
          <details v-if="hasAccessInfo(l)" class="mkt-access">
            <summary class="mkt-access-summary">
              <span class="mkt-caret" aria-hidden="true">▸</span>
              <span class="mkt-access-title">{{ t('apiMarket.accessSection') }}</span>
              <span class="muted small mkt-access-hint">{{ t('apiMarket.accessHint') }}</span>
              <span class="mkt-fold mkt-fold-expand">{{ t('apiMarket.expand') }}</span>
              <span class="mkt-fold mkt-fold-collapse">{{ t('apiMarket.collapse') }}</span>
            </summary>
            <div class="mkt-access-body">
              <div v-if="l.access_info?.api_key" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.keyLabel') }}</span>
                <code
                  class="mkt-inline-code"
                  :title="isMaskedAccessKey(l) ? t('apiMarket.maskedKeyNote') : l.access_info.api_key"
                >{{ l.access_info.api_key }}</code>
                <span v-if="isMaskedAccessKey(l)" class="muted small">{{ t('apiMarket.maskedKeyNote') }}</span>
              </div>
              <div v-if="l.access_info?.auth_header" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.authHeaderLabel') }}</span>
                <code class="mkt-inline-code">{{ l.access_info.auth_header }}</code>
              </div>
              <div v-if="l.access_info?.notes" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.notesLabel') }}</span>
                <span class="muted small mkt-notes">{{ l.access_info.notes }}</span>
              </div>
              <div class="mkt-code">
                <pre class="mkt-pre">{{ buildAccessCurl(l) }}</pre>
                <button
                  type="button"
                  class="btn btn-small mkt-copy"
                  :class="{ copied: copiedAccessId === l.id }"
                  :title="t('apiMarket.copyCurlTitle', { model: accessModelName(l) })"
                  @click="copyAccessCurl(l)"
                >{{ copiedAccessId === l.id ? t('apiMarket.copied') : t('apiMarket.copyCurl') }}</button>
              </div>
              <!-- 占位令牌说明（脱敏视角/未配 key）：完整令牌的获取途径一行 -->
              <p v-if="accessCurlUsesPlaceholder(l)" class="mkt-token-note muted small">
                {{ t('apiMarket.tokenPlaceholderNote') }}
              </p>
            </div>
          </details>

          <!-- ④ 操作区（卡片底部单行不折行）：发布者（超长 ellipsis）/ 打赏 / 下载计数 /
               发布时间（右贴齐）——owner 操作位已集中到「我的发布」Tab -->
          <div class="market-card-foot">
            <span
              class="mono small muted market-pub"
              :title="l.publisher_display ?? l.publisher_pubkey ?? ''"
            >
              <img
                v-if="publisherIdenticon(l)"
                class="identicon"
                :src="publisherIdenticon(l) ?? ''"
                alt=""
              />发布者 {{ shortEvm(l.publisher_display) }}
            </span>
            <!-- 打赏挂牌发布者（target_ref=apimarket:<id>；收款方=publisher_pubkey） -->
            <TipButton
              target-kind="lobby_entry"
              :target-ref="`apimarket:${l.id}`"
              :get-token="tipTokenGetter"
              size="small"
            />
            <span class="mono small muted">下载 {{ l.download_count ?? 0 }}</span>
            <span class="muted small market-time">{{ fmtTime(l.created_at) }}</span>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== 大厅组 · 我的发布（本节点挂牌条目管理；owner 操作集中地。
         数据与操作全部复用 API 大厅：ownListings = marketListings 过滤本人条目，
         心跳/推送联邦/下架/接入信息沿用同一批函数——不复制逻辑） =================== -->
    <section v-show="activeTab === 'mylistings'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">{{ t('gwMine.title') }}</span>
        <div class="head-actions">
          <button class="btn btn-small" :disabled="marketLoading" @click="loadMarket">
            <span class="spin" :class="{ spinning: marketLoading }" aria-hidden="true">↻</span>
            {{ t('gwMine.refresh') }}
          </button>
          <button class="btn btn-small btn-primary" @click="openMarketPublish">{{ t('gwMine.publishBtn') }}</button>
        </div>
      </div>
      <p class="muted small market-hint">{{ t('gwMine.hint') }}</p>

      <!-- 链上身份状态（发布者=区块链公钥；无身份则写操作全不可用，引导去 IM 初始化） -->
      <div v-if="marketHasIdentity" class="mkt-identity mkt-identity-ok">
        {{ t('gwMine.identityOk') }}：<code class="mono">{{ identityEvm || identityPubkey }}</code>
        <span v-if="identityAuthing" class="muted small">…</span>
      </div>
      <div v-else class="mkt-identity mkt-identity-warn">{{ t('gwMine.identityNone') }}</div>

      <div v-if="marketError" class="error-box">{{ marketError }}</div>
      <div v-if="marketLoading && marketListings.length === 0" class="card market-empty">
        <span class="muted small">{{ t('gwMine.loading') }}</span>
      </div>
      <div v-else-if="!marketError && ownListings.length === 0" class="card market-empty">
        <span class="muted small">{{ t('gwMine.empty') }}</span>
      </div>
      <div v-else class="mine-grid">
        <div v-for="l in ownListings" :key="l.id" class="card mine-card">
          <!-- 主区：名称 + 价格 + 联邦推送状态 + 实时负载徽章（同一行不折行——大厅卡
               同款 mkt-title-row 紧凑结构；ownListings 恒本地条目，负载徽章语义不变） -->
          <div class="mine-head">
            <div class="mkt-title-row">
              <span class="market-name" :title="l.api_name ?? ''">{{ l.api_name ?? '—' }}</span>
              <span :class="priceBadge(l).cls">{{ priceBadge(l).text }}</span>
              <span
                v-if="l.federated"
                class="pill pill-fed"
                :title="t('gwMine.refederateTitle')"
              >🌐 {{ t('gwMine.fedPushed') }}</span>
              <span v-else class="pill pill-muted" :title="t('gwMine.federateTitle')">
                {{ t('gwMine.fedNotPushed') }}
              </span>
              <button
                type="button"
                class="load-badge"
                :title="loadBadgeTitle(l.id)"
                @click.stop="openMetricsDetail(l)"
              >
                <span :class="loadDotClass(l.id)" aria-hidden="true"></span>
                <span class="mono small">{{ loadBadgeText(l.id) }}</span>
              </button>
            </div>
          </div>

          <!-- 元信息：心跳 / 下载 / 发布时间 / 消费端点 -->
          <div class="mine-meta">
            <span class="muted small">
              {{ t('gwMine.heartbeatAt') }}：{{ l.heartbeat_at ? fmtTime(l.heartbeat_at) : t('gwMine.heartbeatNone') }}
            </span>
            <span class="mono small muted">{{ t('gwMine.downloads', { n: l.download_count ?? 0 }) }}</span>
            <span class="muted small">{{ t('gwMine.publishedAt') }} {{ fmtTime(l.created_at) }}</span>
            <span class="mono small muted mine-endpoint" :title="l.endpoint_url ?? ''">
              {{ l.endpoint_url ?? '—' }}
            </span>
          </div>

          <!-- 接入信息（access_info；与大厅卡片同一契约：脱敏视角拼占位符） -->
          <details v-if="hasAccessInfo(l)" class="mkt-access">
            <summary class="mkt-access-summary">
              <span class="mkt-caret" aria-hidden="true">▸</span>
              <span class="mkt-access-title">{{ t('apiMarket.accessSection') }}</span>
              <span class="muted small mkt-access-hint">{{ t('apiMarket.accessHint') }}</span>
              <span class="mkt-fold mkt-fold-expand">{{ t('apiMarket.expand') }}</span>
              <span class="mkt-fold mkt-fold-collapse">{{ t('apiMarket.collapse') }}</span>
            </summary>
            <div class="mkt-access-body">
              <div v-if="l.access_info?.api_key" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.keyLabel') }}</span>
                <code
                  class="mkt-inline-code"
                  :title="isMaskedAccessKey(l) ? t('apiMarket.maskedKeyNote') : l.access_info.api_key"
                >{{ l.access_info.api_key }}</code>
                <span v-if="isMaskedAccessKey(l)" class="muted small">{{ t('apiMarket.maskedKeyNote') }}</span>
              </div>
              <div v-if="l.access_info?.auth_header" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.authHeaderLabel') }}</span>
                <code class="mkt-inline-code">{{ l.access_info.auth_header }}</code>
              </div>
              <div v-if="l.access_info?.notes" class="mkt-kv">
                <span class="mkt-kv-k muted small">{{ t('apiMarket.notesLabel') }}</span>
                <span class="muted small mkt-notes">{{ l.access_info.notes }}</span>
              </div>
              <div class="mkt-code">
                <pre class="mkt-pre">{{ buildAccessCurl(l) }}</pre>
                <button
                  type="button"
                  class="btn btn-small mkt-copy"
                  :class="{ copied: copiedAccessId === l.id }"
                  :title="t('apiMarket.copyCurlTitle', { model: accessModelName(l) })"
                  @click="copyAccessCurl(l)"
                >{{ copiedAccessId === l.id ? t('apiMarket.copied') : t('apiMarket.copyCurl') }}</button>
              </div>
              <p v-if="accessCurlUsesPlaceholder(l)" class="mkt-token-note muted small">
                {{ t('apiMarket.tokenPlaceholderNote') }}
              </p>
            </div>
          </details>

          <!-- owner 操作：推送联邦 / 上报心跳 / 下架（复用大厅的函数与 busyId 互斥） -->
          <div class="mine-actions">
            <button
              class="btn btn-small"
              :disabled="busyId === l.id"
              :title="l.federated ? t('gwMine.refederateTitle') : t('gwMine.federateTitle')"
              @click="federateMarket(l)"
            >{{ l.federated ? t('gwMine.refederate') : t('gwMine.federate') }}</button>
            <button
              class="btn btn-small btn-primary"
              :disabled="busyId === l.id"
              :title="t('gwMine.heartbeatTitle')"
              @click="sendHeartbeat(l)"
            >{{ t('gwMine.heartbeat') }}</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="busyId === l.id"
              @click="unlistMarket(l)"
            >{{ t('gwMine.unlist') }}</button>
          </div>
        </div>
      </div>
    </section>

      <!-- 发布 API 对话框（链上身份；挂页面根——API 大厅 / 总览快捷入口 /
           我的发布 任意位置可开，不受 v-show Tab 面板隐藏影响） -->
      <div v-if="showMarketPublish" class="modal-backdrop" @click.self="closeMarketPublish">
        <div class="modal market-publish-modal" role="dialog" aria-modal="true" aria-labelledby="gw-mkt-title">
          <div class="modal-head">
            <h3 id="gw-mkt-title">发布 API 到大厅</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeMarketPublish">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitMarketPublish">
            <!-- 身份状态：未初始化引导去 IM 页；已初始化显示 EVM -->
            <div v-if="marketHasIdentity" class="mkt-identity mkt-identity-ok">
              发布者（链上身份）：<code class="mono">{{ identityEvm || identityPubkey }}</code>
              <span v-if="identityAuthing" class="muted small">（认证中…）</span>
            </div>
            <div v-else class="mkt-identity mkt-identity-warn">
              尚未初始化链上身份——请先到 <strong>IM（聊天）页</strong>生成/导入密钥对
              （与 API 大厅共用同一密钥），否则无法发布（本市场不接受 admin token）。
            </div>

            <div class="field">
              <label for="gw-mkt-name">API 名称</label>
              <input id="gw-mkt-name" v-model="marketPublishForm.api_name" type="text" placeholder="qwen3.5-9b chat" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="gw-mkt-url">消费端点 endpoint_url（OpenAI 兼容；预填本机网关中转地址）</label>
              <input id="gw-mkt-url" v-model="marketPublishForm.endpoint_url" type="text" :disabled="submitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-mkt-model">模型名 model_name（必填——硬件探测拿不到）</label>
                <input id="gw-mkt-model" v-model="marketPublishForm.model_name" type="text" placeholder="Qwen3.5-9B" :disabled="submitting" />
              </div>
              <div class="field">
                <label for="gw-mkt-ctx">上下文长度 context_len（可空——探测拿不到，自报透传）</label>
                <input id="gw-mkt-ctx" v-model.number="marketPublishForm.context_len" type="number" min="0" placeholder="如 32768" :disabled="submitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-mkt-desc">描述</label>
                <input id="gw-mkt-desc" v-model="marketPublishForm.description" type="text" placeholder="本地 3090 跑 Qwen3.5-9B，OpenAI 兼容" :disabled="submitting" />
              </div>
              <div class="field">
                <label for="gw-mkt-tags">标签（逗号分隔）</label>
                <input id="gw-mkt-tags" v-model="marketPublishForm.tags" type="text" placeholder="llm, chat" :disabled="submitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-mkt-mode">计价模式</label>
                <select id="gw-mkt-mode" v-model="marketPublishForm.pricing_mode" :disabled="submitting">
                  <option value="free">免费</option>
                  <option value="per_token">按 Token</option>
                  <option value="per_image">按图</option>
                </select>
              </div>
              <div v-if="marketPublishForm.pricing_mode !== 'free'" class="field">
                <label for="gw-mkt-price">{{ priceFieldLabel }}</label>
                <input id="gw-mkt-price" v-model.number="marketPublishForm.price" type="number" min="1" :disabled="submitting" />
              </div>
            </div>
            <div class="field">
              <label for="gw-mkt-murl">负载监控端点 metrics_url（可空）</label>
              <input id="gw-mkt-murl" v-model="marketPublishForm.metrics_url" type="text" :placeholder="localMetricsUrlHint" :disabled="submitting" />
              <span class="muted small">可填本机实例指标端点（如 {{ localMetricsUrlHint }}）——节点无心跳时由服务端代拉</span>
            </div>
            <!-- 接入信息（可选）：消费者直连凭据——仅发布者本人/admin 可见明文，其他身份脱敏 -->
            <div class="field">
              <label for="gw-mkt-akey">接入 api_key（可空——消费者调用凭据，如网关 sk-os- 令牌）</label>
              <input id="gw-mkt-akey" v-model="marketPublishForm.access_api_key" type="text" placeholder="sk-os-…" :disabled="submitting" autocomplete="off" />
              <span class="muted small">大厅卡片仅发布者本人/admin 可见明文，其他身份显示 &lt;前4&gt;***&lt;后4&gt; 脱敏</span>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="gw-mkt-ahdr">鉴权头用法（可空，缺省 Authorization Bearer）</label>
                <input id="gw-mkt-ahdr" v-model="marketPublishForm.access_auth_header" type="text" placeholder="Authorization Bearer" :disabled="submitting" />
                <span class="muted small">自定义如 X-Api-Key: &lt;key&gt;（&lt;key&gt; 占位替换为 api_key）</span>
              </div>
              <div class="field">
                <label for="gw-mkt-anote">接入备注（可空）</label>
                <input id="gw-mkt-anote" v-model="marketPublishForm.access_notes" type="text" placeholder="如：额外参数 / 限流说明" :disabled="submitting" />
              </div>
            </div>
            <div class="mkt-server-cfg">
              <div class="head-actions">
                <button type="button" class="btn btn-small" @click="showServerOverride = !showServerOverride">
                  {{ showServerOverride ? '收起硬件覆盖' : '覆盖硬件配置' }}
                </button>
                <span class="muted small">默认自动探测本机硬件（GPU/CPU/内存），无需填写</span>
              </div>
              <div v-if="showServerOverride" class="mkt-override-grid">
                <div class="field">
                  <label for="gw-mkt-gpu">GPU 型号（覆盖探测）</label>
                  <input id="gw-mkt-gpu" v-model="marketPublishForm.gpu_model" type="text" placeholder="NVIDIA GeForce RTX 4090" :disabled="submitting" />
                </div>
                <div class="field">
                  <label for="gw-mkt-gpu-count">GPU 数量</label>
                  <input id="gw-mkt-gpu-count" v-model.number="marketPublishForm.gpu_count" type="number" min="1" placeholder="1" :disabled="submitting" />
                </div>
                <div class="field">
                  <label for="gw-mkt-vram">单卡显存 MiB（覆盖探测）</label>
                  <input id="gw-mkt-vram" v-model.number="marketPublishForm.gpu_vram_mb" type="number" min="0" :disabled="submitting" />
                </div>
                <div class="field">
                  <label for="gw-mkt-cores">CPU 核数（覆盖探测）</label>
                  <input id="gw-mkt-cores" v-model.number="marketPublishForm.cpu_cores" type="number" min="0" :disabled="submitting" />
                </div>
                <div class="field">
                  <label for="gw-mkt-ram">内存 GiB（覆盖探测）</label>
                  <input id="gw-mkt-ram" v-model.number="marketPublishForm.ram_gb" type="number" min="0" :disabled="submitting" />
                </div>
              </div>
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeMarketPublish">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '发布中…' : '发布' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 实时负载详情对话框（6 指标小表 + 新鲜度时间；挂页面根——大厅 /
           我的发布 两个 Tab 共用） -->
      <div v-if="metricsDetail" class="modal-backdrop" @click.self="closeMetricsDetail">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gw-mx-title">
          <div class="modal-head">
            <h3 id="gw-mx-title">实时负载 · {{ metricsDetail.listing.api_name ?? '—' }}</h3>
            <button class="modal-close" type="button" @click="closeMetricsDetail">×</button>
          </div>
          <div class="modal-body">
            <template v-if="metricsDetail.resp">
              <div class="mx-meta">
                <span class="load-badge" style="cursor: default">
                  <span :class="loadDotClass(metricsDetail.listing.id)" aria-hidden="true"></span>
                  {{ metricsDetail.resp.reachable ? (metricsDetail.resp.stale ? '数据可能过期' : '新鲜在线') : '不可达' }}
                </span>
                <span class="muted small">来源：{{ metricsDetail.resp.source === 'heartbeat' ? '节点心跳' : metricsDetail.resp.source === 'metrics_url' ? 'metrics_url 服务端代拉' : '无数据源' }}</span>
                <span class="muted small">数据时间：{{ fmtTime(metricsDetail.resp.ts) }}</span>
              </div>
              <div v-if="metricsDetail.resp.error" class="error-box">{{ metricsDetail.resp.error }}</div>
              <div v-if="metricsDetail.resp.metrics" class="mx-table">
                <div class="mx-row"><span class="muted">负载</span><span class="mono strong">{{ metricsFmt(metricsDetail.resp.metrics.load_pct) }}%</span></div>
                <div class="mx-row"><span class="muted">运行中请求</span><span class="mono">{{ metricsFmt(metricsDetail.resp.metrics.running) }}</span></div>
                <div class="mx-row"><span class="muted">排队请求</span><span class="mono">{{ metricsFmt(metricsDetail.resp.metrics.waiting) }}</span></div>
                <div class="mx-row"><span class="muted">显存缓存占用</span><span class="mono">{{ metricsFmt(metricsDetail.resp.metrics.gpu_cache) }}%</span></div>
                <div class="mx-row"><span class="muted">吞吐</span><span class="mono">{{ metricsFmt(metricsDetail.resp.metrics.tokens_per_sec) }} tok/s</span></div>
                <div class="mx-row"><span class="muted">端到端时延</span><span class="mono">{{ metricsFmt(metricsDetail.resp.metrics.latency_ms) }} ms</span></div>
              </div>
              <p v-else class="muted small" style="margin: 0">
                暂无负载数据（无新鲜心跳且 metrics_url 代拉失败或未配置）——未知指标按「—」展示，不猜。
              </p>
            </template>
            <div v-else class="muted small">拉取中…</div>
            <div class="form-actions">
              <button type="button" class="btn" @click="closeMetricsDetail">关闭</button>
            </div>
          </div>
        </div>
      </div>

    <!-- =================== 运营组 · 接入说明（AI 交接优先：一键复制完整接入块；
         结构与样式复用 LlmModels 接入说明面板的 acc-* 模式） =================== -->
    <section v-show="activeTab === 'access'" class="tab-panel">
      <div class="card acc-card">
        <!-- ① 接入令牌：AI 调通需要一个真实 key（生成或粘贴） -->
        <section class="acc-section">
          <div class="acc-sec-title">{{ t('gatewayAccess.sec1Title') }}</div>
          <div class="acc-token-row">
            <button
              class="btn btn-small btn-primary acc-gen-btn"
              :disabled="accessCreatingToken"
              @click="createAgentAccessToken"
            >{{ accessCreatingToken ? '…' : t('gatewayAccess.genTokenBtn') }}</button>
            <span v-if="accessKeyReady" class="acc-key-now">
              <span class="muted small">{{ t('gatewayAccess.keyStatus') }}:</span>
              <code class="acc-v mono">{{ accessKeyMasked }}</code>
              <span
                v-if="accessModelsLoaded"
                class="muted small"
              >· {{ t('gatewayAccess.modelsOkShort', { n: accessModels.length }) }}</span>
            </span>
            <span v-else class="muted small">{{ t('gatewayAccess.keyNone') }}</span>
          </div>
          <p class="acc-note">{{ t('gatewayAccess.genTokenNote') }}</p>
          <div class="acc-models-form">
            <input
              v-model="accessKey"
              class="acc-key-input mono"
              type="password"
              autocomplete="off"
              spellcheck="false"
              :placeholder="t('gatewayAccess.keyInput')"
              @keyup.enter="loadAccessModels"
            />
            <button
              class="btn btn-small"
              :disabled="accessModelsLoading"
              @click="loadAccessModels"
            >{{ accessModelsLoading ? '…' : t('gatewayAccess.loadModels') }}</button>
          </div>
          <p class="acc-note">{{ t('gatewayAccess.pasteHint') }}</p>
        </section>

        <!-- ② 一键接入块（核心：粘给 AI 即可开调） -->
        <section class="acc-section acc-hero">
          <div class="acc-sec-title">{{ t('gatewayAccess.blockSecTitle') }}</div>
          <p class="acc-note">{{ t('gatewayAccess.blockHint') }}</p>
          <div class="acc-code acc-block-code">
            <pre class="acc-pre">{{ accessAgentBlock }}</pre>
            <button
              class="btn btn-small acc-copy acc-copy-big"
              :class="{ copied: accessCopied === 'agentBlock' }"
              @click="copyAccess('agentBlock', accessAgentBlock)"
            >{{ accessCopied === 'agentBlock' ? '✓ ' + t('gatewayAccess.copied') : t('gatewayAccess.copyBlockBtn') }}</button>
          </div>
          <!-- 模型清单（实时）：每项「复制接入」= 单模型精简块 -->
          <template v-if="accessModelsLoaded && accessModels.length > 0">
            <div class="acc-step">{{ t('gatewayAccess.perModelTitle') }}</div>
            <div class="acc-model-tags">
              <span v-for="m in accessModels" :key="m" class="acc-model-item">
                <code class="acc-model-tag mono">{{ m }}</code>
                <button
                  class="btn btn-small acc-copy-inline"
                  :class="{ copied: accessCopied === `model:${m}` }"
                  type="button"
                  :title="t('gatewayAccess.perModelCopy')"
                  @click="copyAccess(`model:${m}`, accessModelBlock(m))"
                >{{ accessCopied === `model:${m}` ? '✓' : t('gatewayAccess.perModelCopy') }}</button>
              </span>
            </div>
          </template>
          <p v-if="accessModelsError" class="acc-note acc-err">{{ accessModelsError }}</p>
        </section>

        <!-- ③ 调用示例（辅助：单条 curl 直取） -->
        <section class="acc-section">
          <div class="acc-sec-title">{{ t('gatewayAccess.sec3Title') }}</div>
          <div class="acc-step">{{ t('gatewayAccess.curlNonStream') }}</div>
          <div class="acc-code">
            <pre class="acc-pre">{{ accessCurlChat }}</pre>
            <button
              class="btn btn-small acc-copy"
              :class="{ copied: accessCopied === 'curlChat' }"
              @click="copyAccess('curlChat', accessCurlChat)"
            >{{ accessCopied === 'curlChat' ? '✓' : t('gatewayAccess.copy') }}</button>
          </div>
          <div class="acc-step">{{ t('gatewayAccess.curlStream') }}</div>
          <div class="acc-code">
            <pre class="acc-pre">{{ accessCurlStream }}</pre>
            <button
              class="btn btn-small acc-copy"
              :class="{ copied: accessCopied === 'curlStream' }"
              @click="copyAccess('curlStream', accessCurlStream)"
            >{{ accessCopied === 'curlStream' ? '✓' : t('gatewayAccess.copy') }}</button>
          </div>
          <p class="acc-note">{{ t('gatewayAccess.streamNote') }}</p>
          <div class="acc-step">{{ t('gatewayAccess.curlModels') }}</div>
          <div class="acc-code">
            <pre class="acc-pre">{{ accessCurlModels }}</pre>
            <button
              class="btn btn-small acc-copy"
              :class="{ copied: accessCopied === 'curlModels' }"
              @click="copyAccess('curlModels', accessCurlModels)"
            >{{ accessCopied === 'curlModels' ? '✓' : t('gatewayAccess.copy') }}</button>
          </div>
        </section>

        <!-- ④ 辅助说明：地址拼法 / 建令牌契约 / 计费与配额 -->
        <section class="acc-section">
          <div class="acc-sec-title">{{ t('gatewayAccess.sec4Title') }}</div>
          <div class="acc-kv">
            <span class="acc-k">{{ t('gatewayAccess.baseUrlLabel') }}</span>
            <code class="acc-v">{{ accessBaseUrl }}</code>
            <button
              class="btn btn-small acc-copy-inline"
              :class="{ copied: accessCopied === 'baseUrl' }"
              @click="copyAccess('baseUrl', accessBaseUrl)"
            >{{ accessCopied === 'baseUrl' ? '✓' : t('gatewayAccess.copy') }}</button>
            <span class="muted small">
              {{ t('gatewayAccess.baseUrlHint', { port: GATEWAY_DEFAULT_PORT }) }}
            </span>
          </div>
          <div class="acc-kv">
            <span class="acc-k">{{ t('gatewayAccess.authLabel') }}</span>
            <code class="acc-v">Authorization: Bearer sk-os-…</code>
          </div>
          <p class="acc-note">{{ t('gatewayAccess.authNote') }}</p>
          <div class="acc-step">{{ t('gatewayAccess.tokenContract') }}</div>
          <div class="acc-code">
            <pre class="acc-pre">{{ accessTokenContract }}</pre>
            <button
              class="btn btn-small acc-copy"
              :class="{ copied: accessCopied === 'tokenContract' }"
              @click="copyAccess('tokenContract', accessTokenContract)"
            >{{ accessCopied === 'tokenContract' ? '✓' : t('gatewayAccess.copy') }}</button>
          </div>
          <p class="acc-note">{{ t('gatewayAccess.adminNote') }}</p>
          <!-- 局域网共享联邦模型（2026-09-03 渠道中继）：via_node 渠道把 P2P 收到
               的 API 再发布给本局域网 AI——接入面不变（同一 Base URL + sk-os- 令牌） -->
          <div class="acc-step">{{ t('gatewayAccess.lanShareTitle') }}</div>
          <p class="acc-note">{{ t('gatewayAccess.lanShareNote') }}</p>
          <ul class="acc-steps">
            <li>{{ t('gatewayAccess.lanShareStep1') }}</li>
            <li>{{ t('gatewayAccess.lanShareStep2') }}</li>
            <li>{{ t('gatewayAccess.lanShareStep3') }}</li>
          </ul>
          <p class="acc-note">{{ t('gatewayAccess.lanShareTrust') }}</p>
          <div class="acc-step">{{ t('gatewayAccess.billingTitle') }}</div>
          <ul class="acc-steps">
            <li v-for="(b, i) in accessBillingItems" :key="i">{{ b }}</li>
          </ul>
          <p class="acc-note">{{ t('gatewayAccess.quotaNote') }}</p>
          <p class="acc-note">{{ t('gatewayAccess.whitelistNote') }}</p>
          <p class="acc-note">{{ t('gatewayAccess.failoverNote') }}</p>
        </section>
      </div>
    </section>
  </div>
</template>

<style scoped>
.gw-page {
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
.strong { font-weight: 700; }

/* Tabs */
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover { color: var(--text, #2B2B2B); }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }

/* 二级 Tab（组内子页切换，照模型管理/API 大厅既有二级 Tab 同款）：比一级
   Tab 小一号，虚线下边线区分层级 */
.sub-tabs { gap: 2px; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.sub-tab { padding: 5px 12px; font-size: 13px; }

.tab-panel { display: flex; flex-direction: column; gap: 14px; }

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-sub { font-size: 12px; }
.stat-err { color: #b91c1c; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.empty-inline { padding-top: 4px; }
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* 进度条 */
.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { display: block; height: 100%; background: #0E8420; border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-warn { background: #b91c1c; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #1e40af; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-purple { color: #7c3aed; background: #ede9fe; }
.pill-cyan { color: #0e7490; background: #cffafe; }
.pill-orange { color: #9a3412; background: #ffedd5; }
.pill-warning { color: #92400e; background: #fef3c7; }
/* 联邦远程条目徽章（source_node 非空且非 local）：🌐 来源节点 */
.pill-fed { color: #0e8420; background: rgba(14, 132, 32, 0.1); }
/* 渠道名单元格：名称 + 中继徽章同行（徽章不换行占位） */
.ch-name-cell { display: inline-flex; align-items: center; gap: 6px; min-width: 0; }
.ch-name-cell > span:first-child { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

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
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #c84318); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
.btn-warning { color: #92400e; border-color: rgba(245, 158, 11, 0.45); background: #fffbeb; }
.btn-warning:hover:not(:disabled) { background: #fef3c7; }
.btn + .btn { margin-left: 6px; }

.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
}
.field-row { display: flex; gap: 12px; }

/* —— 添加渠道：从本地发现导入区（列表条目即预填按钮）—— */
.discovery-box { display: flex; flex-direction: column; gap: 8px; padding: 12px 14px; background: var(--border-soft, #FAFAFA); border: 1px dashed var(--border, #D9D9D9); border-radius: var(--radius-sm, 8px); }
.discovery-head { display: flex; align-items: baseline; gap: 8px; flex-wrap: wrap; }
.discovery-title { font-size: 13px; font-weight: 600; }
.discovery-list { display: flex; flex-direction: column; gap: 4px; }
.discovery-row { display: flex; align-items: center; gap: 8px; padding: 6px 10px; background: var(--bg-card, #fff); border: 1px solid var(--border, #D9D9D9); border-radius: var(--radius-sm, 8px); cursor: pointer; font: inherit; text-align: left; }
.discovery-row:hover:not(:disabled) { border-color: var(--accent, #E95420); }
.discovery-row:disabled { opacity: 0.5; cursor: not-allowed; }
.discovery-name { font-size: 13px; font-weight: 500; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.mono { font-family: var(--mono); }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
/* 三段式钉底（照 LlmModels 2026-08-31 修复先例）：弹窗自身不整体滚动——
 * head 固定 + body 滚动 + 操作区 sticky 钉底（底部按钮不随内容滚走）；
 * 高度约束 max-height:90vh 仅为上限，布局无 100vh 公式。 */
.modal {
  width: min(560px, 100%); max-height: 90vh; overflow: hidden;
  background: var(--bg-card, #fff); border-radius: var(--radius, 16px);
  box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25);
  display: flex; flex-direction: column;
}
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-shrink: 0; }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; flex: 1 1 auto; min-height: 0; overflow-y: auto; }
/* 表单操作区钉在弹窗底部（sticky 抵消 body 滚动；负 margin 贴边 + 上边线分区） */
.modal-body .form-actions {
  position: sticky; bottom: -18px; margin: 0 -20px -18px; padding: 12px 20px;
  background: var(--bg-card, #fff); border-top: 1px solid var(--border-soft, #EDEDED);
}

/* 令牌 key 单元格 */
.key-cell { display: inline-flex; align-items: center; gap: 6px; }
.copy-btn { padding: 2px 8px; font-size: 11px; }
.quota-cell { display: flex; flex-direction: column; gap: 4px; min-width: 120px; }

/* 日志分页 */
.limit-select { padding: 4px 8px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 13px; background: var(--bg-card, #fff); }

/* 总览 - 模型标签 */
.models-card { padding: 16px 18px; }
.model-tag-list { display: flex; flex-wrap: wrap; gap: 8px; }
.model-tag { display: inline-block; padding: 4px 12px; border: 1px solid var(--border, #D9D9D9); border-radius: var(--radius-pill, 20px); font-size: 13px; background: var(--border-soft, #FAFAFA); color: var(--text, #2B2B2B); }

/* 令牌创建后 key 显示 */
.key-reveal-box { display: flex; flex-direction: column; gap: 10px; }
.key-reveal-warn { margin: 0; padding: 10px 14px; background: #fffbeb; border: 1px solid rgba(245, 158, 11, 0.4); border-radius: var(--radius-sm, 8px); font-size: 13px; color: #92400e; line-height: 1.6; }
.key-reveal-warn strong { font-weight: 700; }
.key-reveal-value { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.key-reveal-value code { display: inline-block; padding: 8px 12px; background: var(--border-soft, #FAFAFA); border: 1px dashed var(--border, #D9D9D9); border-radius: var(--radius-sm, 8px); font-size: 14px; word-break: break-all; flex: 1; min-width: 200px; }

/* —— 令牌「接入信息」面板（创建成功/存量令牌同款三段式钉底弹窗）：接入块/
   curl 复用 acc-* 代码块样式；此处只补弹窗宽度与完整 key 大字等宽 —— */
.token-access-modal { width: min(720px, 100%); }
.tok-key { font-size: 15px; font-weight: 600; letter-spacing: 0.01em; }
/* 占位形态密钥：弱化视觉（非可复制真值） */
.tok-key-ph { color: var(--text-muted, #5E5C5F); font-weight: 500; }
/* 接入要点 kv 纵排（Base URL/计费配额/允许模型/模型清单状态） */
.tok-kv-list { display: flex; flex-direction: column; gap: 8px; }

/* 充值订单 */
.stat-warn { color: #92400e; }
.pay-form-card { padding: 16px 18px; }
.pay-form { display: flex; gap: 12px; align-items: flex-end; flex-wrap: wrap; }
.pay-form .field { min-width: 180px; }
.pay-form-actions { justify-content: flex-end; }
.pay-info-card { padding: 16px 20px; display: flex; flex-direction: column; gap: 14px; border-color: rgba(245, 158, 11, 0.55); }
.pay-info-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.pay-info-title { font-size: 15px; font-weight: 700; }
.pay-info-close { font-size: 20px; }
.pay-info-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(200px, 1fr)); gap: 14px; }
.pay-info-item { display: flex; flex-direction: column; gap: 4px; }
.pay-info-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.pay-info-value { font-size: 22px; font-weight: 700; letter-spacing: -0.02em; word-break: break-all; }
.pay-addr-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.pay-addr { display: inline-block; padding: 10px 14px; background: var(--border-soft, #FAFAFA); border: 1px dashed var(--border, #D9D9D9); border-radius: var(--radius-sm, 8px); font-size: 14px; word-break: break-all; flex: 1; min-width: 220px; }
/* 占位收款地址：红色醒目横幅 + 地址码红框标注 */
.pay-placeholder-banner {
  color: #b91c1c; background: #fee2e2; border: 1.5px solid #f5b5b0;
  padding: 10px 14px; border-radius: var(--radius-sm, 8px);
  font-size: 13.5px; font-weight: 700; line-height: 1.6;
}
.pay-addr-placeholder { border-color: #f5b5b0; color: #b91c1c; background: #fff5f5; }
.pay-memo { margin: 0; }
.pay-confirm-summary { margin: 0; line-height: 1.7; }

/* ===================== API 大厅（Tab7） ===================== */
.market-toolbar { flex-wrap: wrap; }
.market-search {
  width: 220px; padding: 6px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 13px;
  background: var(--bg-card, #fff);
}
.market-hint { margin: 0; }
.market-empty { padding: 32px 20px; text-align: center; }
/* 卡片网格：列宽下限 min(340px,100%)——窄于 340px 的视口不横向溢出 */
.market-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(340px, 100%), 1fr)); gap: 14px; }
/* 点卡片展开/收起描述（toggleMarketCard）——手型光标提示可点；卡内按钮/折叠
   面板有自己的交互与光标，不受影响 */
.market-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; cursor: pointer; }
/* identicon 固定头像：由发布者身份确定性生成，同身份恒同图（与文字间 4px 间距） */
.identicon {
  width: 16px; height: 16px; border-radius: 3px; flex-shrink: 0;
  vertical-align: -3px; margin-right: 4px;
}
/* 标题行（2026-09-03 排版修复）：名称+价格+🌐来源+状态徽章同行**不折行**——
   名称吃剩余宽度（超长 ellipsis，title 悬浮看全名），其余徽章/pill 全部
   flex-shrink:0。旧实现 market-card-title flex-wrap:wrap 在窄卡上折成
   两三行，与右侧 load-badge（space-between 分栏）挤成纵向堆叠观感。 */
.mkt-title-row {
  display: flex; align-items: center; gap: 8px; flex-wrap: nowrap;
  min-width: 0; flex: 1 1 auto;
}
.mkt-title-row > *:not(.market-name) { flex-shrink: 0; }
.mkt-title-row .market-name {
  flex: 1 1 auto; min-width: 0;
  white-space: nowrap; overflow: hidden; text-overflow: ellipsis;
}
.market-name { font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); }
/* 描述：默认两行 clamp（-webkit-line-clamp，Chrome/Edge/Firefox 均支持），
   点卡片切换全显（expandedMarketIds → desc-clamped 摘下） */
.market-desc { margin: 0; }
.market-desc.desc-clamped {
  display: -webkit-box; -webkit-line-clamp: 2; -webkit-box-orient: vertical;
  overflow: hidden;
}
/* 联邦条目源节点心跳时间差独立行（徽章语义分流后唯一的新鲜度展示） */
.mkt-hb-row { margin: 0; }
.market-tags { display: flex; flex-wrap: wrap; gap: 6px; }
.market-tag { font-size: 11.5px; color: #0e7490; background: #cffafe; padding: 1px 8px; border-radius: var(--radius-pill, 20px); }
/* 服务器配置区（2026-09-03 重排，修用户端"标签/值一行一条"纵向堆叠）：
   真两列自适应网格 repeat(auto-fill, minmax(180px,1fr))——卡内容宽 ~300px 时
   1 列、≥376px 时 2 列，窄卡自然回单列（不再依赖媒体查询硬塌）。
   单列根因（旧实现）：固定 1fr 1fr 在 340px 下限卡内每列仅 ~130px，标签左值
   右的 flex 行把值挤成省略号碎片；≤640px 视口媒体查询又把整块塌成 1fr——
   两列形同虚设。新结构每格 = 标签（小字灰色上标）上 / 值（主体字号）下，
   值允许换行不截断（信息保真）；GPU 型号+显存/统一内存由 gpuSummary 合并
   在同一格（如「NVIDIA GB10 · 统一内存 121.7 GB」）。 */
.market-config {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(min(180px, 100%), 1fr));
  gap: 8px 16px;
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
  padding: 8px 12px; background: var(--border-soft, #FAFAFA);
}
.market-config-row { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.market-config-k {
  font-size: 10.5px; letter-spacing: 0.05em; text-transform: uppercase;
  color: var(--text-muted, #5E5C5F);
}
.market-config-v { font-size: 12.5px; font-weight: 600; text-align: left; white-space: normal; overflow-wrap: anywhere; }
/* —— 条目卡分区（2026-09-02 重排）：① 主区 / ② 规格区（market-config）/
   ③ 接入信息折叠面板 / ④ 操作区（market-card-foot）—— */
.mkt-main { display: flex; flex-direction: column; gap: 6px; }
.mkt-price-note { margin: 0; }
/* ③ 接入信息折叠面板：details/summary 原生交互（默认收起，ob-pitfalls 同款手法） */
.mkt-access {
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
  background: var(--bg-soft, #FAFAFA);
}
.mkt-access-summary {
  display: flex; align-items: center; gap: 8px; flex-wrap: wrap;
  padding: 7px 10px; cursor: pointer; font-size: 12.5px;
  color: var(--text, #2B2B2B); list-style: none; user-select: none;
}
.mkt-access-summary::-webkit-details-marker { display: none; }
.mkt-access-title { font-weight: 600; }
.mkt-access-hint { font-weight: 400; }
.mkt-caret {
  display: inline-block; transition: transform 0.15s ease;
  color: var(--text-muted, #5E5C5F); font-size: 11px;
}
.mkt-access[open] .mkt-caret { transform: rotate(90deg); }
/* 折叠/展开角标：按 open 态二选一显示（i18n：apiMarket.expand / collapse） */
.mkt-fold { margin-left: auto; font-size: 11.5px; font-weight: 400; color: var(--accent, #E95420); }
.mkt-fold-collapse { display: none; }
.mkt-access[open] .mkt-fold-expand { display: none; }
.mkt-access[open] .mkt-fold-collapse { display: inline; }
.mkt-access-body {
  display: flex; flex-direction: column; gap: 6px;
  padding: 8px 10px 10px; border-top: 1px dashed var(--border-soft, #EDEDED);
}
/* 密钥/鉴权头/备注键值行：标签 + 行内代码样式值（MD 行内代码观感） */
.mkt-kv { display: flex; align-items: baseline; gap: 8px; font-size: 12px; flex-wrap: wrap; }
.mkt-kv-k { flex-shrink: 0; min-width: 44px; }
.mkt-notes { flex: 1; min-width: 120px; line-height: 1.6; word-break: break-word; }
.mkt-inline-code {
  font-family: var(--mono, 'Ubuntu Mono', Consolas, monospace); font-size: 12px;
  padding: 2px 8px; border-radius: var(--radius-sm, 6px);
  background: #26292F; color: #E8E4E8; word-break: break-all;
}
/* curl 示例代码块——MD 命令行块样式（复用 ob-* 与 acc-* 先例：深色底/等宽/
   横向滚动/右上角悬浮复制按钮） */
.mkt-code { position: relative; }
.mkt-pre {
  margin: 0; padding: 10px 64px 10px 12px; border-radius: var(--radius-sm, 8px);
  background: #26292F; color: #E8E4E8;
  font-family: var(--mono, 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace);
  font-size: 11.5px; line-height: 1.55;
  white-space: pre; overflow-x: auto;
}
.mkt-copy {
  position: absolute; top: 5px; right: 5px; padding: 2px 9px; font-size: 11px;
  background: rgba(255, 255, 255, 0.1); border: 1px solid rgba(255, 255, 255, 0.25);
  color: #E8E4E8;
}
.mkt-copy:hover:not(:disabled) { background: rgba(255, 255, 255, 0.2); }
.mkt-copy.copied { color: #4ade80; border-color: rgba(74, 222, 128, 0.55); background: rgba(74, 222, 128, 0.12); }
/* 占位令牌说明行（脱敏视角/未配 key） */
.mkt-token-note { margin: 0; line-height: 1.6; }
/* 窄屏不塌：搜索框占满整行；硬件覆盖网格 → 单列（规格区已 auto-fill 自适应，
   无需媒体查询干预） */
@media (max-width: 640px) {
  .market-search { width: 100%; flex: 1 1 100%; }
  .market-toolbar .head-actions { flex-wrap: wrap; }
  .mkt-override-grid { grid-template-columns: 1fr; }
}
.market-card-foot {
  display: flex; align-items: center; gap: 10px; flex-wrap: nowrap;
  border-top: 1px dashed var(--border-soft, #EDEDED); padding-top: 8px;
  min-width: 0;
}
/* 底部单行：发布者（超长 ellipsis 吃剩余宽度）+ 打赏 + 下载计数；发布时间右贴齐 */
.market-card-foot > * { flex-shrink: 0; }
.market-pub { min-width: 0; flex-shrink: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.market-time { margin-left: auto; }
/* ============ 总览（默认落地 Tab）：统计注脚 / 快捷入口 / 最近调用 ============ */
.ov-stat-note { margin: 0; }
/* 快捷入口卡（2×2 自适应网格；卡片本身即按钮） */
.quick-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(220px, 100%), 1fr)); gap: 14px; }
.quick-card {
  padding: 14px 16px; display: flex; flex-direction: column; gap: 6px; align-items: flex-start;
  cursor: pointer; font: inherit; text-align: left; transition: border-color 0.15s ease, box-shadow 0.15s ease;
}
.quick-card:hover { border-color: var(--accent, #E95420); }
.quick-title { font-size: 14px; font-weight: 700; color: var(--text, #2B2B2B); }
/* 最近调用摘要（日志前 5 条；一行一条，窄窗自动换行堆叠） */
.recent-card { padding: 6px 14px; }
.recent-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; }
.recent-row {
  display: flex; align-items: center; gap: 12px; flex-wrap: wrap;
  padding: 8px 0; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.recent-list .recent-row:last-child { border-bottom: none; }
.recent-time { flex-shrink: 0; }
.recent-token { min-width: 80px; }
.recent-model { flex: 1; min-width: 140px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* ============ 大厅组 · API 大厅：本地/联邦分段开关（原二级 Tab 功能等价，
   改分段样式避免与组内二级 Tab 视觉堆叠） ============ */
.market-view-switch {
  display: inline-flex; align-items: center; gap: 4px; padding: 3px;
  border: 1px solid var(--border, #D9D9D9); border-radius: var(--radius-pill, 20px);
  background: var(--bg-card, #fff); width: fit-content;
}
.seg-btn {
  padding: 4px 14px; border: none; border-radius: var(--radius-pill, 20px);
  background: transparent; color: var(--text-muted, #5E5C5F);
  font: inherit; font-size: 13px; cursor: pointer;
  transition: background 0.15s ease, color 0.15s ease;
}
.seg-btn:hover { color: var(--text, #2B2B2B); }
.seg-btn.active { background: var(--accent, #E95420); color: #fff; font-weight: 600; }
.seg-btn .fed-icon { color: inherit; }

/* ============ 大厅组 · 我的发布（owner 操作集中地；复用大厅数据/函数） ============ */
.mine-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(min(380px, 100%), 1fr)); gap: 14px; }
.mine-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.mine-head { display: flex; justify-content: space-between; align-items: flex-start; gap: 8px; }
.mine-meta { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.mine-endpoint {
  flex: 1; min-width: 160px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
}
.mine-actions {
  display: flex; align-items: center; gap: 6px; flex-wrap: wrap;
  border-top: 1px dashed var(--border-soft, #EDEDED); padding-top: 8px;
}
/* 窄窗响应式：快捷入口/我的发布网格自然单列（auto-fill minmax 兜底）；
   最近调用行内字段换行堆叠（flex-wrap 已开），元信息区同理 */
@media (max-width: 640px) {
  .market-view-switch { width: 100%; }
  .market-view-switch .seg-btn { flex: 1; text-align: center; }
  .mine-endpoint { flex-basis: 100%; min-width: 0; }
}
/* 实时负载徽章（状态点 + 数值，点击弹详情） */
.load-badge {
  display: inline-flex; align-items: center; gap: 6px; flex-shrink: 0;
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  border-radius: var(--radius-pill, 20px); padding: 3px 10px; cursor: pointer;
  font-family: inherit; font-size: 12px;
}
.load-badge:hover { background: rgba(0, 0, 0, 0.04); }
.dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; flex-shrink: 0; }
/* 负载徽章状态点（2026-09-03 语义）：绿=新鲜心跳≤60s；灰=降级（metrics_url
   代拉到数据但心跳过期）；红=不可达（直连探测无数据源）——仅本地条目使用 */
.dot-green { background: #0e8420; box-shadow: 0 0 0 3px rgba(14, 132, 32, 0.15); }
.dot-gray { background: #9ca3af; }
.dot-red { background: #c7162b; box-shadow: 0 0 0 3px rgba(199, 22, 43, 0.15); }
.dot-pending { background: #d1d5db; animation: mkt-pulse 1.2s ease-in-out infinite; }
/* 联邦条目中继徽章（load-badge 观感但非按钮——常驻展示，不做主动探测） */
.mkt-relay { cursor: default; }
@keyframes mkt-pulse { 50% { opacity: 0.35; } }
/* 发布对话框：身份状态条 + 硬件覆盖 */
.market-publish-modal { width: min(620px, 100%); }
.mkt-identity { font-size: 13px; padding: 8px 12px; border-radius: var(--radius-sm, 8px); line-height: 1.6; }
.mkt-identity code { word-break: break-all; }
.mkt-identity-ok { color: #15803d; background: #f0fdf4; border: 1px solid rgba(21, 128, 61, 0.25); }
.mkt-identity-warn { color: #92400e; background: #fffbeb; border: 1px solid rgba(245, 158, 11, 0.4); }
.mkt-server-cfg { display: flex; flex-direction: column; gap: 8px; }
.mkt-override-grid {
  display: grid; grid-template-columns: 1fr 1fr; gap: 10px 12px;
  border: 1px dashed var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); padding: 12px;
}
/* metrics 详情：元信息行 + 6 指标两列小表 */
.mx-meta { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.mx-table {
  display: grid; grid-template-columns: 1fr 1fr; gap: 8px 16px;
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); padding: 10px 14px;
}
.mx-row { display: flex; justify-content: space-between; gap: 8px; font-size: 13px; }

/* ============ 「接入说明」Tab（2026-08-31 对外接入·AI 交接优先；acc-* 模式
   复用 LlmModels 接入说明面板——本页为 Tab 面板而非弹窗，卡片内四段纵排） ============ */
.acc-card { padding: 4px 20px; max-width: 860px; }
.acc-section {
  display: flex; flex-direction: column; gap: 10px;
  padding: 14px 0 16px; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.acc-section:last-of-type { border-bottom: none; }
/* ② 一键接入块 = 核心区：淡橙底 + 左侧强调条，视觉上压过其余段落 */
.acc-hero {
  padding: 14px 14px 16px; border-radius: var(--radius-sm, 8px);
  background: rgba(233, 84, 32, 0.04); border: 1px solid rgba(233, 84, 32, 0.18);
  border-left: 3px solid var(--accent, #E95420);
}
.acc-sec-title { font-size: 14px; font-weight: 700; color: var(--text, #2B2B2B); }
.acc-kv { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; font-size: 12.5px; }
.acc-k { flex-shrink: 0; min-width: 64px; font-weight: 600; color: var(--text, #2B2B2B); }
.acc-v {
  font-family: var(--mono, 'Ubuntu Mono', Consolas, monospace); font-size: 12px;
  word-break: break-all; padding: 2px 8px; border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa); color: var(--text, #2B2B2B);
}
.acc-note { margin: 0; font-size: 12.5px; line-height: 1.6; color: var(--text-muted, #5E5C5F); }
.acc-note.acc-err { color: #b91c1c; }
.acc-step { margin-top: 2px; font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
/* 有序/无序步骤列表（四计费模式等） */
.acc-steps { margin: 0; padding-left: 20px; display: flex; flex-direction: column; gap: 4px;
  font-size: 12.5px; line-height: 1.6; color: var(--text, #2B2B2B); }
/* 代码块：深色底等宽（右上角悬浮复制按钮；长命令换行） */
.acc-code { position: relative; }
.acc-pre {
  margin: 0; padding: 10px 60px 10px 12px; border-radius: var(--radius-sm, 8px);
  background: #26292f; color: #e8e4e8;
  font-family: var(--mono, 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace);
  font-size: 12px; line-height: 1.55; white-space: pre-wrap; word-break: break-word;
}
.acc-copy {
  position: absolute; top: 5px; right: 5px; padding: 2px 9px; font-size: 11px;
  background: rgba(255, 255, 255, 0.1); border: 1px solid rgba(255, 255, 255, 0.25);
  color: #e8e4e8;
}
.acc-copy:hover { background: rgba(255, 255, 255, 0.2); }
.acc-copy.copied {
  color: #4ade80; border-color: rgba(74, 222, 128, 0.55); background: rgba(74, 222, 128, 0.12);
}
/* 完整接入块：更高（多行 markdown），复制按钮加大加宽（核心 CTA） */
.acc-block-code .acc-pre { max-height: 420px; overflow-y: auto; padding-right: 120px; }
.acc-copy-big { padding: 4px 14px; font-size: 12px; font-weight: 600; }
.acc-copy-inline { padding: 2px 9px; font-size: 11px; }
.acc-copy-inline.copied { color: #15803d; border-color: rgba(21, 128, 61, 0.4); background: #dcfce7; }
/* ① 令牌行：生成按钮 + 当前 key 打码 + 模型计数 */
.acc-token-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.acc-gen-btn { font-weight: 600; }
.acc-key-now { display: inline-flex; align-items: center; gap: 6px; flex-wrap: wrap; }
/* 粘贴 key 输入（password 型不回显，防肩窥） */
.acc-models-form { display: flex; gap: 8px; flex-wrap: wrap; }
.acc-key-input {
  flex: 1; min-width: 260px; padding: 6px 10px; font-size: 12.5px;
  border: 1px solid var(--border, #D9D9D9); border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa); color: var(--text, #2B2B2B);
}
/* 模型清单：每项 = 模型名 chip + 「复制接入」小按钮（复制单模型精简块） */
.acc-model-tags { display: flex; flex-wrap: wrap; gap: 8px; }
.acc-model-item { display: inline-flex; align-items: center; gap: 4px; }
.acc-model-tag {
  padding: 3px 10px; font-size: 12px;
  border: 1px solid rgba(233, 84, 32, 0.25); border-radius: var(--radius-pill, 20px);
  background: rgba(233, 84, 32, 0.06); color: var(--text, #2B2B2B);
}

</style>
