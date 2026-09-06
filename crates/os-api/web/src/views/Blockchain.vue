<script setup lang="ts">
// =============================================================================
// Blockchain.vue —— 区块链管理（RPC 节点 + Blockscout 浏览器）
//
// 3 Tab：RPC 节点 / 区块链浏览器 / 总览
// 后端：/api/v1/blockchain/* （BlockchainRouteHandler，已在线）
//
// 设计：Ubuntu Yaru 风格 .card / .page-head，统计卡 + 表格 + 对话框 + compose 预览。
// 编排层语义：start/stop 真实 spawn docker compose（失败降级 error 不报错弹窗）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';
import { copyText } from '@/utils/clipboard';
import { formatBytes } from '@/utils/format';

// 节点运行 Tab 的文案走 i18n（四语言，键前缀 bcn.*；其余 Tab 保持现有中文硬编码）
const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
interface ChainConfig {
  chain_type: string;
  chain_id: number;
  network: string;
  name: string;
}
interface NodeInstance {
  id: string;
  name: string;
  chain: ChainConfig;
  client: string;
  rpc_port: number;
  ws_port?: number | null;
  data_dir: string;
  sync_mode: string;
  status: string;
  enabled: boolean;
  created_at: string;
  error?: string | null;
  compose_yaml?: string | null;
  start_cmd?: string | null;
}
interface ExplorerInstance {
  id: string;
  name: string;
  node_id: string;
  web_port: number;
  db_port?: number | null;
  status: string;
  url?: string | null;
  created_at: string;
  compose_yaml?: string | null;
  error?: string | null;
}
interface ChainPreset {
  chain_type: string;
  chain_id: number;
  network: string;
  name: string;
  clients: string[];
  default_sync: string;
}
interface ClientInfo {
  client: string;
  name: string;
  description: string;
}
interface BlockchainStats {
  nodes_total: number;
  running: number;
  stopped: number;
  explorers_total: number;
  explorers_running: number;
  supported_chains: number;
}

type NodeStatus = 'stopped' | 'running' | 'syncing' | 'error' | string;

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'nodes' | 'explorers' | 'runtime' | 'overview';
const activeTab = ref<TabKey>('nodes');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'nodes', label: 'RPC 节点' },
  { key: 'explorers', label: '区块链浏览器' },
  { key: 'runtime', label: '' }, // 「节点运行」——i18n 填充（见下方 computed）
  { key: 'overview', label: '总览' },
];
// i18n 标签需响应语言切换：用 computed 渲染而非静态数组写死
const tabLabels = computed<Record<TabKey, string>>(() => ({
  nodes: 'RPC 节点',
  explorers: '区块链浏览器',
  runtime: t('bcn.tab'),
  overview: '总览',
}));

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// =============================================================================
// 数据
// =============================================================================
const nodes = ref<NodeInstance[]>([]);
const explorers = ref<ExplorerInstance[]>([]);
const chainPresets = ref<ChainPreset[]>([]);
const clients = ref<ClientInfo[]>([]);
const stats = ref<BlockchainStats>({
  nodes_total: 0,
  running: 0,
  stopped: 0,
  explorers_total: 0,
  explorers_running: 0,
  supported_chains: 0,
});

const nodesLoading = ref(false);
const nodesError = ref('');
const explorersLoading = ref(false);
const explorersError = ref('');
const acting = ref<string | null>(null); // 正在 start/stop/delete 的节点/浏览器 id

async function loadNodes(): Promise<void> {
  nodesLoading.value = true;
  nodesError.value = '';
  try {
    const raw = await endpoints.blockchainNodes();
    nodes.value = (raw as NodeInstance[]) ?? [];
  } catch (e) {
    nodesError.value = friendlyError(e);
  } finally {
    nodesLoading.value = false;
  }
}

async function loadExplorers(): Promise<void> {
  explorersLoading.value = true;
  explorersError.value = '';
  try {
    const raw = await endpoints.blockchainExplorers();
    explorers.value = (raw as ExplorerInstance[]) ?? [];
  } catch (e) {
    explorersError.value = friendlyError(e);
  } finally {
    explorersLoading.value = false;
  }
}

async function loadChainPresets(): Promise<void> {
  try {
    const raw = await endpoints.blockchainChainPresets();
    chainPresets.value = (raw as ChainPreset[]) ?? [];
  } catch (e) {
    msg.value = { kind: 'err', text: '加载链预设失败：' + friendlyError(e) };
  }
}

async function loadClients(): Promise<void> {
  try {
    const raw = await endpoints.blockchainClients();
    clients.value = (raw as ClientInfo[]) ?? [];
  } catch (e) {
    msg.value = { kind: 'err', text: '加载客户端列表失败：' + friendlyError(e) };
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.blockchainStats();
    stats.value = (raw as BlockchainStats) ?? stats.value;
  } catch {
    // 统计失败不阻塞主流程
  }
}

async function reloadAll(): Promise<void> {
  await Promise.all([loadNodes(), loadExplorers(), loadStats()]);
}

// =============================================================================
// 徽章辅助
// =============================================================================
function chainTypePill(t: string): string {
  switch (t) {
    case 'ethereum':
      return 'pill-blue';
    case 'dev':
      return 'pill-ok';
    case 'l2':
      return 'pill-purple';
    case 'custom':
      return 'pill-orange';
    default:
      return 'pill-muted';
  }
}
function statusPill(s: NodeStatus): string {
  switch (s) {
    case 'running':
      return 'pill-ok';
    case 'error':
      return 'pill-err';
    case 'syncing':
      return 'pill-warning';
    default:
      return 'pill-muted';
  }
}
function statusLabel(s: NodeStatus): string {
  switch (s) {
    case 'running':
      return '运行中';
    case 'error':
      return '错误';
    case 'syncing':
      return '同步中';
    case 'stopped':
      return '已停止';
    default:
      return s;
  }
}

function nodeName(id: string): string {
  return nodes.value.find((n) => n.id === id)?.name ?? id;
}

// =============================================================================
// 节点表格列定义
// =============================================================================
const nodeCols: Column<NodeInstance>[] = [
  { key: 'name', title: '名称', sortable: true },
  { key: 'chain_type', title: '链类型', width: '110px' },
  { key: 'network', title: '网络', width: '120px' },
  { key: 'client', title: '客户端', width: '120px' },
  { key: 'chain_id', title: 'Chain ID', width: '90px', align: 'right' },
  { key: 'rpc_port', title: 'RPC 端口', width: '90px', align: 'right' },
  { key: 'status', title: '状态', width: '100px' },
  { key: 'actions', title: '操作', width: '200px', align: 'right' },
];

const explorerCols: Column<ExplorerInstance>[] = [
  { key: 'name', title: '名称', sortable: true },
  { key: 'node_id', title: '关联节点', width: '160px' },
  { key: 'web_port', title: 'Web 端口', width: '100px', align: 'right' },
  { key: 'status', title: '状态', width: '100px' },
  { key: 'url', title: '访问 URL', width: '220px' },
  { key: 'actions', title: '操作', width: '180px', align: 'right' },
];

// =============================================================================
// 创建节点对话框
// =============================================================================
const showNodeDialog = ref(false);
const submitting = ref(false);
const nodeForm = ref<{
  name: string;
  chain_type: string;
  network: string;
  chain_id: number;
  client: string;
  rpc_port: number;
  ws_port: number;
  data_dir: string;
  sync_mode: string;
}>({
  name: '',
  chain_type: 'ethereum',
  network: 'mainnet',
  chain_id: 1,
  client: 'geth',
  rpc_port: 8545,
  ws_port: 8546,
  data_dir: '',
  sync_mode: 'snap',
});

// 按链类型分组的可选预设
const presetsByType = computed(() => {
  const m: Record<string, ChainPreset[]> = {};
  for (const p of chainPresets.value) {
    if (!m[p.chain_type]) m[p.chain_type] = [];
    m[p.chain_type].push(p);
  }
  return m;
});

// 当前链类型下可选的网络预设
const networkOptions = computed<ChainPreset[]>(() => {
  return presetsByType.value[nodeForm.value.chain_type] ?? [];
});

// 当前选中网络对应的预设（用于取 chain_id / clients / default_sync）
const selectedPreset = computed<ChainPreset | null>(() => {
  return networkOptions.value.find((p) => p.network === nodeForm.value.network) ?? null;
});

// 客户端可选列表（根据链预设动态过滤）
const clientOptions = computed<string[]>(() => {
  return selectedPreset.value?.clients ?? [];
});

const isDevChain = computed(() => nodeForm.value.chain_type === 'dev');

function openNodeDialog(): void {
  nodeForm.value = {
    name: '',
    chain_type: 'ethereum',
    network: 'mainnet',
    chain_id: 1,
    client: 'geth',
    rpc_port: 8545,
    ws_port: 8546,
    data_dir: '',
    sync_mode: 'snap',
  };
  msg.value = null;
  showNodeDialog.value = true;
}

// 链类型变化时重置网络为该类型下首个预设
function onChainTypeChange(): void {
  const opts = presetsByType.value[nodeForm.value.chain_type] ?? [];
  const first = opts[0];
  if (first) {
    nodeForm.value.network = first.network;
    onNetworkChange();
  }
}

// 网络变化时同步 chain_id / 推荐客户端 / 默认 sync_mode
function onNetworkChange(): void {
  const p = selectedPreset.value;
  if (p) {
    nodeForm.value.chain_id = p.chain_id;
    nodeForm.value.sync_mode = isDevChain.value ? 'full' : p.default_sync;
    // 若当前客户端不在预设列表内，则切到首个推荐
    if (!p.clients.includes(nodeForm.value.client)) {
      nodeForm.value.client = p.clients[0] ?? 'geth';
    }
  }
}

const createdCompose = ref<string | null>(null);

async function submitNode(): Promise<void> {
  if (!nodeForm.value.name.trim()) {
    msg.value = { kind: 'err', text: '节点名称不可为空' };
    return;
  }
  submitting.value = true;
  msg.value = null;
  try {
    const body = {
      name: nodeForm.value.name.trim(),
      chain_type: nodeForm.value.chain_type,
      network: nodeForm.value.network,
      chain_id: nodeForm.value.chain_id,
      client: nodeForm.value.client,
      rpc_port: nodeForm.value.rpc_port,
      ws_port: nodeForm.value.ws_port,
      data_dir: nodeForm.value.data_dir.trim() || undefined,
      sync_mode: isDevChain.value ? 'full' : nodeForm.value.sync_mode,
    };
    const created = (await endpoints.createBlockchainNode(body)) as NodeInstance;
    createdCompose.value = created.compose_yaml ?? null;
    showNodeDialog.value = false;
    msg.value = { kind: 'ok', text: `节点 ${created.name} 已创建（compose 已生成）` };
    await reloadAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    submitting.value = false;
  }
}

// =============================================================================
// 节点 compose 详情查看
// =============================================================================
const viewingNode = ref<NodeInstance | null>(null);
const viewingExplorer = ref<ExplorerInstance | null>(null);
function viewNode(n: NodeInstance): void {
  viewingNode.value = { ...n };
}
function viewExplorer(e: ExplorerInstance): void {
  viewingExplorer.value = { ...e };
}
async function refreshNode(id: string): Promise<void> {
  try {
    const raw = (await endpoints.getBlockchainNode(id)) as NodeInstance;
    viewingNode.value = raw;
    // 同步表格行
    const idx = nodes.value.findIndex((n) => n.id === id);
    if (idx >= 0) nodes.value[idx] = raw;
  } catch {
    /* ignore */
  }
}

// =============================================================================
// 启动 / 停止 / 删除（真实 spawn docker）
// =============================================================================
async function startNode(n: NodeInstance): Promise<void> {
  acting.value = n.id;
  msg.value = null;
  try {
    await endpoints.startBlockchainNode(n.id);
    msg.value = {
      kind: 'ok',
      text: `节点 ${n.name} 启动指令已发送（docker compose up -d）`,
    };
    await reloadAll();
    if (viewingNode.value?.id === n.id) await refreshNode(n.id);
  } catch (e) {
    msg.value = {
      kind: 'err',
      text: `启动失败：${friendlyError(e)}（docker 未安装/权限不足时请在宿主机手动执行）`,
    };
  } finally {
    acting.value = null;
  }
}

async function stopNode(n: NodeInstance): Promise<void> {
  acting.value = n.id;
  msg.value = null;
  try {
    await endpoints.stopBlockchainNode(n.id);
    msg.value = { kind: 'ok', text: `节点 ${n.name} 停止指令已发送` };
    await reloadAll();
    if (viewingNode.value?.id === n.id) await refreshNode(n.id);
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
  } finally {
    acting.value = null;
  }
}

async function deleteNode(n: NodeInstance): Promise<void> {
  if (!confirm(`确认删除节点 ${n.name}？关联浏览器会一并移除。`)) return;
  acting.value = n.id;
  msg.value = null;
  try {
    await endpoints.deleteBlockchainNode(n.id);
    msg.value = { kind: 'ok', text: `节点 ${n.name} 已删除` };
    if (viewingNode.value?.id === n.id) viewingNode.value = null;
    await reloadAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    acting.value = null;
  }
}

// =============================================================================
// 创建浏览器对话框
// =============================================================================
const showExplorerDialog = ref(false);
const explorerForm = ref<{ name: string; node_id: string; web_port: number }>({
  name: '',
  node_id: '',
  web_port: 4000,
});

function openExplorerDialog(): void {
  explorerForm.value = {
    name: '',
    node_id: nodes.value[0]?.id ?? '',
    web_port: 4000,
  };
  msg.value = null;
  showExplorerDialog.value = true;
}

async function submitExplorer(): Promise<void> {
  if (!explorerForm.value.name.trim()) {
    msg.value = { kind: 'err', text: '浏览器名称不可为空' };
    return;
  }
  if (!explorerForm.value.node_id) {
    msg.value = { kind: 'err', text: '请选择关联节点' };
    return;
  }
  submitting.value = true;
  msg.value = null;
  try {
    const created = (await endpoints.createBlockchainExplorer({
      name: explorerForm.value.name.trim(),
      node_id: explorerForm.value.node_id,
      web_port: explorerForm.value.web_port,
    })) as ExplorerInstance;
    showExplorerDialog.value = false;
    msg.value = {
      kind: 'ok',
      text: `浏览器 ${created.name} 已创建，访问 ${created.url ?? ''}`,
    };
    await reloadAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    submitting.value = false;
  }
}

async function startExplorer(e: ExplorerInstance): Promise<void> {
  acting.value = e.id;
  msg.value = null;
  try {
    await endpoints.startBlockchainExplorer(e.id);
    msg.value = { kind: 'ok', text: `浏览器 ${e.name} 启动指令已发送` };
    await reloadAll();
  } catch (err) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(err) };
  } finally {
    acting.value = null;
  }
}

async function deleteExplorer(e: ExplorerInstance): Promise<void> {
  if (!confirm(`确认删除浏览器 ${e.name}？`)) return;
  acting.value = e.id;
  msg.value = null;
  try {
    await endpoints.deleteBlockchainExplorer(e.id);
    msg.value = { kind: 'ok', text: `浏览器 ${e.name} 已删除` };
    await reloadAll();
  } catch (err) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(err) };
  } finally {
    acting.value = null;
  }
}

// =============================================================================
// 剪贴板
// =============================================================================
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
// Tab3：节点运行（blockchain_nodes 子模块——geth/bitcoind 真实子进程）
// 后端：/api/v1/blockchain/chain-nodes*（创建向导 + 空间预检 + 节点卡 + 日志）
// =============================================================================
interface ChainNodeRecord {
  id: string;
  name: string;
  kind: string;
  network: string;
  client: string;
  mode: string;
  data_dir: string;
  rpc_port: number;
  p2p_port: number;
  txindex: boolean;
  extra_flags: string;
  status: string;
  pid?: number | null;
  error?: string | null;
  created_at: string;
  updated_at: string;
  last_started_at?: string | null;
  last_command?: string | null;
}
interface NodeModeInfo {
  mode: string;
  label: string;
  flags: string;
  estimated_size_gb: number;
  sync_estimate: string;
  note?: string | null;
}
interface NodePreset {
  kind: string;
  network: string;
  name: string;
  default_client: string;
  default_rpc_port: number;
  default_p2p_port: number;
  modes: NodeModeInfo[];
  requires_consensus_client: boolean;
  binary_installed: boolean;
  install_hint: string;
}
interface SpaceCheckResult {
  kind: string;
  network: string;
  mode: string;
  data_dir: string;
  required_bytes: number;
  required_gb: number;
  available_bytes: number;
  sufficient: boolean;
  blocking: boolean;
  filesystem: string;
  error?: string | null;
}

const chainNodes = ref<ChainNodeRecord[]>([]);
const nodePresets = ref<NodePreset[]>([]);
const nodeDataRoot = ref('/tank/blockchain'); // 后端 presets 返回实际解析值
const runtimeLoading = ref(false);
const runtimeError = ref('');
const runtimeActing = ref<string | null>(null);

async function loadChainNodes(): Promise<void> {
  runtimeLoading.value = true;
  runtimeError.value = '';
  try {
    const raw = await endpoints.chainNodes();
    chainNodes.value = (raw as ChainNodeRecord[]) ?? [];
  } catch (e) {
    runtimeError.value = friendlyError(e);
  } finally {
    runtimeLoading.value = false;
  }
}

async function loadNodePresets(): Promise<void> {
  try {
    const raw = (await endpoints.chainNodePresets()) as {
      presets?: NodePreset[];
      default_data_root?: string;
    };
    nodePresets.value = raw?.presets ?? [];
    if (raw?.default_data_root) nodeDataRoot.value = raw.default_data_root;
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.loadPresetsFailed') + friendlyError(e) };
  }
}

function nodeStatusPill(s: string): string {
  switch (s) {
    case 'running':
      return 'pill-ok';
    case 'syncing':
      return 'pill-warning';
    case 'error':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}
function nodeStatusKey(s: string): string {
  switch (s) {
    case 'running':
      return 'bcn.statusRunning';
    case 'syncing':
      return 'bcn.statusSyncing';
    case 'error':
      return 'bcn.statusError';
    default:
      return 'bcn.statusStopped';
  }
}

async function startChainNode(n: ChainNodeRecord): Promise<void> {
  runtimeActing.value = n.id;
  msg.value = null;
  try {
    await endpoints.startChainNode(n.id);
    msg.value = { kind: 'ok', text: t('bcn.startSent', { name: n.name }) };
    await loadChainNodes();
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.startFailed') + friendlyError(e) };
  } finally {
    runtimeActing.value = null;
  }
}

async function stopChainNode(n: ChainNodeRecord): Promise<void> {
  runtimeActing.value = n.id;
  msg.value = null;
  try {
    await endpoints.stopChainNode(n.id);
    msg.value = { kind: 'ok', text: t('bcn.stopSent', { name: n.name }) };
    await loadChainNodes();
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.stopFailed') + friendlyError(e) };
  } finally {
    runtimeActing.value = null;
  }
}

async function deleteChainNode(n: ChainNodeRecord): Promise<void> {
  if (!confirm(t('bcn.confirmDelete', { name: n.name }))) return;
  runtimeActing.value = n.id;
  msg.value = null;
  try {
    await endpoints.deleteChainNode(n.id);
    msg.value = { kind: 'ok', text: t('bcn.deleted', { name: n.name }) };
    await loadChainNodes();
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.deleteFailed') + friendlyError(e) };
  } finally {
    runtimeActing.value = null;
  }
}

// ---- 日志查看 ----
const viewingLogs = ref<{ node: ChainNodeRecord; lines: string[]; path: string; status: string } | null>(null);
const logsLoading = ref(false);

async function openNodeLogs(n: ChainNodeRecord): Promise<void> {
  viewingLogs.value = { node: { ...n }, lines: [], path: '', status: n.status };
  logsLoading.value = true;
  try {
    const raw = (await endpoints.chainNodeLogs(n.id, 200)) as {
      lines?: string[];
      log_path?: string;
      status?: string;
    };
    viewingLogs.value = {
      node: { ...n },
      lines: raw?.lines ?? [],
      path: raw?.log_path ?? '',
      status: raw?.status ?? n.status,
    };
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.logsFailed') + friendlyError(e) };
    viewingLogs.value = null;
  } finally {
    logsLoading.value = false;
  }
}

// ---- 创建向导（链 → 网络 → 模式 → 空间预检实时显示 → 目录/端口）----
const showNodeWizard = ref(false);
const wizardSubmitting = ref(false);
const wizardForm = ref<{
  name: string;
  kind: string;
  network: string;
  mode: string;
  data_dir: string;
  rpc_port: number;
  txindex: boolean;
  extra_flags: string;
}>({
  name: '',
  kind: 'ethereum',
  network: 'mainnet',
  mode: 'fast',
  data_dir: '',
  rpc_port: 8545,
  txindex: false,
  extra_flags: '',
});

const wizardPresetsByKind = computed(() => {
  const m: Record<string, NodePreset[]> = {};
  for (const p of nodePresets.value) {
    if (!m[p.kind]) m[p.kind] = [];
    m[p.kind].push(p);
  }
  return m;
});
const wizardKindOptions = computed(() => Object.keys(wizardPresetsByKind.value));
const wizardNetworkOptions = computed(
  () => wizardPresetsByKind.value[wizardForm.value.kind] ?? [],
);
const wizardSelectedPreset = computed(
  () =>
    wizardNetworkOptions.value.find((p) => p.network === wizardForm.value.network) ?? null,
);
const wizardModeOptions = computed(() => wizardSelectedPreset.value?.modes ?? []);
const wizardNeedsCL = computed(() => wizardSelectedPreset.value?.requires_consensus_client ?? false);
const wizardBinaryInstalled = computed(
  () => wizardSelectedPreset.value?.binary_installed ?? false,
);
const wizardInstallHint = computed(() => wizardSelectedPreset.value?.install_hint ?? '');
const wizardDefaultDir = computed(
  () => `${nodeDataRoot.value.replace(/\/+$/, '')}/${wizardForm.value.network || 'chain'}`,
);
const wizardEffectiveDir = computed(
  () => wizardForm.value.data_dir.trim() || wizardDefaultDir.value,
);

function openNodeWizard(): void {
  const first = nodePresets.value[0];
  wizardForm.value = {
    name: '',
    kind: first?.kind ?? 'ethereum',
    network: first?.network ?? 'mainnet',
    mode: first?.modes[0]?.mode ?? 'fast',
    data_dir: '',
    rpc_port: first?.default_rpc_port ?? 8545,
    txindex: false,
    extra_flags: '',
  };
  msg.value = null;
  spaceCheck.value = null;
  showNodeWizard.value = true;
}

function onWizardKindChange(): void {
  const opts = wizardPresetsByKind.value[wizardForm.value.kind] ?? [];
  const p = opts[0];
  if (p) {
    wizardForm.value.network = p.network;
    onWizardNetworkChange();
  }
}

function onWizardNetworkChange(): void {
  const p = wizardSelectedPreset.value;
  if (p) {
    wizardForm.value.mode = p.modes[0]?.mode ?? 'fast';
    wizardForm.value.rpc_port = p.default_rpc_port;
    wizardForm.value.txindex = false;
    if (!wizardForm.value.data_dir.trim()) {
      wizardForm.value.data_dir = '';
    }
  }
}

// ---- 空间预检（实时；full 不足 → 红色 + 禁止提交）----
const spaceCheck = ref<SpaceCheckResult | null>(null);
const spaceCheckLoading = ref(false);
let spaceCheckTimer: ReturnType<typeof setTimeout> | null = null;

watch(
  () => [
    wizardForm.value.kind,
    wizardForm.value.network,
    wizardForm.value.mode,
    wizardForm.value.data_dir,
    wizardForm.value.txindex,
  ],
  () => {
    if (!showNodeWizard.value) return;
    if (spaceCheckTimer) clearTimeout(spaceCheckTimer);
    spaceCheckTimer = setTimeout(refreshSpaceCheck, 250);
  },
);

async function refreshSpaceCheck(): Promise<void> {
  const { kind, network, mode } = wizardForm.value;
  if (!kind || !network || !mode) return;
  spaceCheckLoading.value = true;
  try {
    const raw = await endpoints.chainNodeSpaceCheck(
      kind,
      network,
      mode,
      wizardEffectiveDir.value,
      wizardForm.value.txindex,
    );
    spaceCheck.value = raw as SpaceCheckResult;
  } catch {
    spaceCheck.value = null; // 预检失败不阻塞表单（提交时后端会硬检查）
  } finally {
    spaceCheckLoading.value = false;
  }
}

const spaceBlocked = computed(
  () => !!spaceCheck.value && spaceCheck.value.blocking && !spaceCheck.value.sufficient,
);

async function submitChainNode(): Promise<void> {
  if (!wizardForm.value.name.trim()) {
    msg.value = { kind: 'err', text: t('bcn.errNameRequired') };
    return;
  }
  if (spaceBlocked.value) {
    msg.value = { kind: 'err', text: t('bcn.errSpaceBlocked') };
    return;
  }
  wizardSubmitting.value = true;
  msg.value = null;
  try {
    const raw = (await endpoints.createChainNode({
      name: wizardForm.value.name.trim(),
      kind: wizardForm.value.kind,
      network: wizardForm.value.network,
      mode: wizardForm.value.mode,
      data_dir: wizardEffectiveDir.value,
      rpc_port: wizardForm.value.rpc_port,
      txindex: wizardForm.value.txindex,
      extra_flags: wizardForm.value.extra_flags.trim() || undefined,
    })) as {
      node?: ChainNodeRecord;
      warnings?: string[];
    };
    showNodeWizard.value = false;
    const warnings = raw?.warnings ?? [];
    msg.value = {
      kind: warnings.length ? 'info' : 'ok',
      text:
        t('bcn.created', { name: raw?.node?.name ?? '' }) +
        (warnings.length ? ' · ' + warnings.join(' · ') : ''),
    };
    await loadChainNodes();
  } catch (e) {
    msg.value = { kind: 'err', text: t('bcn.createFailed') + friendlyError(e) };
  } finally {
    wizardSubmitting.value = false;
  }
}

// =============================================================================
// 生命周期
// ==============================================================================
onMounted(async () => {
  await Promise.all([loadChainPresets(), loadClients(), loadNodePresets(), reloadAll()]);
});
</script>

<template>
  <div class="page">
    <!-- 页头 -->
    <div class="page-head">
      <div>
        <div class="page-title">区块链管理</div>
        <div class="page-sub muted">RPC 节点 · 区块链浏览器 · 一键编排</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" type="button" @click="reloadAll">刷新</button>
        <button
          v-if="activeTab === 'nodes'"
          class="btn btn-primary btn-small"
          type="button"
          @click="openNodeDialog"
        >
          + 创建节点
        </button>
        <button
          v-else-if="activeTab === 'explorers'"
          class="btn btn-primary btn-small"
          type="button"
          @click="openExplorerDialog"
        >
          + 创建浏览器
        </button>
        <button
          v-else-if="activeTab === 'runtime'"
          class="btn btn-primary btn-small"
          type="button"
          @click="openNodeWizard"
        >
          {{ t('bcn.createNode') }}
        </button>
      </div>
    </div>

    <!-- 全局消息 -->
    <div v-if="msg" :class="['form-msg', msg.kind === 'err' ? 'is-err' : msg.kind === 'ok' ? 'is-ok' : 'is-info']">
      {{ msg.text }}
    </div>

    <!-- Tabs -->
    <div class="tabs">
      <button
        v-for="tk in tabs"
        :key="tk.key"
        type="button"
        :class="['tab', { active: activeTab === tk.key }]"
        @click="activeTab = tk.key"
      >
        {{ tabLabels[tk.key] }}
      </button>
    </div>

    <!-- ============ Tab1: RPC 节点 ============ -->
    <div v-if="activeTab === 'nodes'" class="tab-panel">
      <!-- 统计卡 -->
      <div class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">节点总数</div>
          <div class="stat-value">{{ stats.nodes_total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">运行中</div>
          <div class="stat-value">{{ stats.running }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已停止</div>
          <div class="stat-value">{{ stats.stopped }}</div>
        </div>
      </div>

      <div v-if="nodesError" class="error-box">{{ nodesError }}</div>

      <div class="card card-table">
        <div v-if="nodesLoading" class="table-empty">加载中…</div>
        <div v-else-if="!nodes.length" class="table-empty">
          暂无节点，点击右上角"创建节点"添加。
        </div>
        <DataTable v-else :columns="nodeCols" :rows="nodes">
          <template #cell-name="{ row }">
            <span class="strong">{{ row.name }}</span>
            <div class="muted small">{{ row.data_dir }}</div>
          </template>
          <template #cell-chain_type="{ row }">
            <span :class="['pill', chainTypePill(row.chain.chain_type)]">
              {{ row.chain.chain_type }}
            </span>
          </template>
          <template #cell-network="{ row }">{{ row.chain.network }}</template>
          <template #cell-client="{ row }">
            <span class="pill pill-cyan">{{ row.client }}</span>
          </template>
          <template #cell-chain_id="{ row }">{{ row.chain.chain_id }}</template>
          <template #cell-rpc_port="{ row }">{{ row.rpc_port }}</template>
          <template #cell-status="{ row }">
            <span :class="['pill', statusPill(row.status)]">{{ statusLabel(row.status) }}</span>
            <div v-if="row.error" class="muted small err-line">{{ row.error }}</div>
          </template>
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small btn-primary"
              type="button"
              :disabled="acting === row.id || row.status === 'running'"
              @click="startNode(row)"
            >
              启动
            </button>
            <button
              class="btn btn-small btn-warning"
              type="button"
              :disabled="acting === row.id || row.status !== 'running'"
              @click="stopNode(row)"
            >
              停止
            </button>
            <button class="btn btn-small" type="button" @click="viewNode(row)">compose</button>
            <button
              class="btn btn-small btn-danger"
              type="button"
              :disabled="acting === row.id"
              @click="deleteNode(row)"
            >
              删除
            </button>
          </template>
        </DataTable>
      </div>
    </div>

    <!-- ============ Tab2: 区块链浏览器 ============ -->
    <div v-else-if="activeTab === 'explorers'" class="tab-panel">
      <div v-if="explorersError" class="error-box">{{ explorersError }}</div>

      <div class="card card-table">
        <div v-if="explorersLoading" class="table-empty">加载中…</div>
        <div v-else-if="!explorers.length" class="table-empty">
          暂无浏览器，点击右上角"创建浏览器"添加（需先有 RPC 节点）。
        </div>
        <DataTable v-else :columns="explorerCols" :rows="explorers">
          <template #cell-name="{ row }">
            <span class="strong">{{ row.name }}</span>
            <div v-if="row.error" class="muted small err-line">{{ row.error }}</div>
          </template>
          <template #cell-node_id="{ row }">{{ nodeName(row.node_id) }}</template>
          <template #cell-web_port="{ row }">{{ row.web_port }}</template>
          <template #cell-status="{ row }">
            <span :class="['pill', statusPill(row.status)]">{{ statusLabel(row.status) }}</span>
          </template>
          <template #cell-url="{ row }">
            <a v-if="row.url" :href="row.url" target="_blank" rel="noopener">{{ row.url }}</a>
            <span v-else class="muted">—</span>
          </template>
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small btn-primary"
              type="button"
              :disabled="acting === row.id || row.status === 'running'"
              @click="startExplorer(row)"
            >
              启动
            </button>
            <button class="btn btn-small" type="button" @click="viewExplorer(row)">compose</button>
            <button
              class="btn btn-small btn-danger"
              type="button"
              :disabled="acting === row.id"
              @click="deleteExplorer(row)"
            >
              删除
            </button>
          </template>
        </DataTable>
      </div>

      <!-- 浏览器 compose 预览 -->
      <div v-if="viewingExplorer" class="modal-backdrop" @click.self="viewingExplorer = null">
        <div class="modal" role="dialog" aria-modal="true">
          <div class="modal-head">
            <h3>浏览器详情 — {{ viewingExplorer.name }}</h3>
            <button class="modal-close" type="button" @click="viewingExplorer = null">×</button>
          </div>
          <div class="modal-body">
            <div class="detail-grid">
              <div><span class="muted small">关联节点</span> {{ nodeName(viewingExplorer.node_id) }}</div>
              <div><span class="muted small">Web 端口</span> {{ viewingExplorer.web_port }}</div>
              <div><span class="muted small">状态</span> <span :class="['pill', statusPill(viewingExplorer.status)]">{{ statusLabel(viewingExplorer.status) }}</span></div>
              <div>
                <span class="muted small">访问 URL</span>
                <a v-if="viewingExplorer.url" :href="viewingExplorer.url" target="_blank" rel="noopener">{{ viewingExplorer.url }}</a>
                <span v-else class="muted">—</span>
              </div>
            </div>
            <div class="compose-actions">
              <span class="muted small">docker-compose.yml</span>
              <button
                class="btn btn-small"
                type="button"
                @click="copyWithToast(viewingExplorer.compose_yaml ?? '')"
              >
                复制 compose
              </button>
              <button
                class="btn btn-small btn-primary"
                type="button"
                :disabled="acting === viewingExplorer.id || viewingExplorer.status === 'running'"
                @click="startExplorer(viewingExplorer); viewingExplorer = null"
              >
                启动（docker compose up）
              </button>
            </div>
            <pre class="code-block">{{ viewingExplorer.compose_yaml ?? '（无 compose 内容）' }}</pre>
            <div class="compose-note muted small">
              docker 未安装/权限不足时请在宿主机手动执行 docker compose up -d。
            </div>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ Tab3: 节点运行（geth/bitcoind 真实子进程） ============ -->
    <div v-else-if="activeTab === 'runtime'" class="tab-panel">
      <!-- 预设总览（含二进制安装状态 + 诚实备注） -->
      <div v-if="nodePresets.length" class="card runtime-presets">
        <div class="panel-head">
          <div class="panel-title">{{ t('bcn.presetsTitle') }}</div>
          <button class="btn btn-small" type="button" @click="loadNodePresets">
            {{ t('bcn.refreshBinaries') }}
          </button>
        </div>
        <div class="preset-grid">
          <div v-for="p in nodePresets" :key="p.kind + '/' + p.network" class="preset-item">
            <div class="preset-head">
              <span :class="['pill', p.kind === 'ethereum' ? 'pill-blue' : 'pill-orange']">
                {{ p.kind }} · {{ p.network }}
              </span>
              <span class="strong">{{ p.name }}</span>
              <span :class="['pill', p.binary_installed ? 'pill-ok' : 'pill-muted']">
                {{ p.binary_installed ? t('bcn.binaryOk', { c: p.default_client }) : t('bcn.binaryMissing', { c: p.default_client }) }}
              </span>
            </div>
            <div v-for="m in p.modes" :key="m.mode" class="muted small mode-line">
              <span class="strong">{{ m.label }}</span>：
              {{ t('bcn.estSize') }} {{ m.estimated_size_gb }}GB ·
              {{ t('bcn.estSync') }} {{ m.sync_estimate }} · <code class="inline-code">{{ m.flags }}</code>
              <div v-if="m.note" class="mode-note">{{ m.note }}</div>
            </div>
            <div v-if="p.requires_consensus_client" class="form-msg is-info small">
              {{ t('bcn.needsConsensusClient') }}
            </div>
            <div v-if="!p.binary_installed" class="install-hint small">{{ p.install_hint }}</div>
          </div>
        </div>
      </div>

      <div v-if="runtimeError" class="error-box">{{ runtimeError }}</div>

      <!-- 节点卡列表 -->
      <div v-if="runtimeLoading" class="table-empty">{{ t('bcn.loading') }}</div>
      <div v-else-if="!chainNodes.length" class="card table-empty">
        {{ t('bcn.emptyHint') }}
      </div>
      <div v-else class="node-grid">
        <div v-for="n in chainNodes" :key="n.id" class="card node-card">
          <div class="node-card-head">
            <span class="strong">{{ n.name }}</span>
            <span :class="['pill', nodeStatusPill(n.status)]">{{ t(nodeStatusKey(n.status)) }}</span>
          </div>
          <div class="detail-grid node-detail">
            <div>
              <span class="muted small">{{ t('bcn.fChain') }}</span>
              {{ n.kind }} / {{ n.network }}
            </div>
            <div>
              <span class="muted small">{{ t('bcn.fMode') }}</span>
              {{ n.mode }}{{ n.txindex ? ' + txindex' : '' }}
            </div>
            <div>
              <span class="muted small">{{ t('bcn.fClient') }}</span>
              {{ n.client }}
            </div>
            <div>
              <span class="muted small">PID</span>
              {{ n.pid ?? '—' }}
            </div>
            <div class="detail-full">
              <span class="muted small">{{ t('bcn.fRpc') }}</span>
              <code class="inline-code">http://127.0.0.1:{{ n.rpc_port }}</code>
              <button class="btn btn-small" type="button" @click="copyWithToast(`http://127.0.0.1:${n.rpc_port}`)">
                {{ t('bcn.copyRpc') }}
              </button>
            </div>
            <div class="detail-full">
              <span class="muted small">{{ t('bcn.fDataDir') }}</span>
              <code class="inline-code">{{ n.data_dir }}</code>
            </div>
          </div>
          <div v-if="n.error" class="form-msg is-err small node-error">{{ n.error }}</div>
          <div class="node-actions">
            <button
              class="btn btn-small btn-primary"
              type="button"
              :disabled="runtimeActing === n.id || n.status === 'running' || n.status === 'syncing'"
              @click="startChainNode(n)"
            >
              {{ t('bcn.start') }}
            </button>
            <button
              class="btn btn-small btn-warning"
              type="button"
              :disabled="runtimeActing === n.id || (n.status !== 'running' && n.status !== 'syncing')"
              @click="stopChainNode(n)"
            >
              {{ t('bcn.stop') }}
            </button>
            <button class="btn btn-small" type="button" @click="openNodeLogs(n)">
              {{ t('bcn.viewLogs') }}
            </button>
            <button
              class="btn btn-small btn-danger"
              type="button"
              :disabled="runtimeActing === n.id"
              @click="deleteChainNode(n)"
            >
              {{ t('bcn.delete') }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ Tab4: 总览 ============ -->
    <div v-else class="tab-panel">
      <div class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">RPC 节点</div>
          <div class="stat-value">{{ stats.nodes_total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">区块链浏览器</div>
          <div class="stat-value">{{ stats.explorers_total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">支持链数</div>
          <div class="stat-value">{{ stats.supported_chains }}</div>
        </div>
      </div>

      <div class="card overview-card">
        <div class="panel-head">
          <div class="panel-title">链预设</div>
        </div>
        <div class="preset-grid">
          <div v-for="p in chainPresets" :key="p.network" class="preset-item">
            <div class="preset-head">
              <span :class="['pill', chainTypePill(p.chain_type)]">{{ p.chain_type }}</span>
              <span class="strong">{{ p.name }}</span>
            </div>
            <div class="muted small">
              network: {{ p.network }} · chain_id: {{ p.chain_id }} · 默认同步: {{ p.default_sync }}
            </div>
            <div class="client-list muted small">
              推荐客户端：{{ p.clients.join(' / ') }}
            </div>
          </div>
        </div>
      </div>

      <div class="card overview-card">
        <div class="panel-head">
          <div class="panel-title">客户端说明</div>
        </div>
        <div class="client-grid">
          <div v-for="c in clients" :key="c.client" class="client-item">
            <div class="client-head">
              <span class="pill pill-cyan">{{ c.client }}</span>
              <span class="strong">{{ c.name }}</span>
            </div>
            <div class="muted small">{{ c.description }}</div>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 创建节点对话框 ============ -->
    <div v-if="showNodeDialog" class="modal-backdrop" @click.self="showNodeDialog = false">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bc-node-title">
        <div class="modal-head">
          <h3 id="bc-node-title">创建 RPC 节点</h3>
          <button
            class="modal-close"
            type="button"
            :disabled="submitting"
            @click="showNodeDialog = false"
          >
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitNode">
          <div class="field">
            <label for="bc-name">节点名称</label>
            <input
              id="bc-name"
              v-model="nodeForm.name"
              type="text"
              placeholder="如 ETH主网节点 / 本地开发链"
            />
          </div>

          <div class="form-row">
            <div class="field">
              <label for="bc-type">链类型</label>
              <select id="bc-type" v-model="nodeForm.chain_type" @change="onChainTypeChange">
                <option value="ethereum">ethereum（主网/测试网）</option>
                <option value="dev">dev（本地开发链）</option>
                <option value="l2">l2（Optimism/Arbitrum/Base）</option>
                <option value="custom">custom（自定义私有链）</option>
              </select>
            </div>
            <div class="field">
              <label for="bc-net">网络（链预设）</label>
              <select id="bc-net" v-model="nodeForm.network" @change="onNetworkChange">
                <option v-for="p in networkOptions" :key="p.network" :value="p.network">
                  {{ p.name }}（{{ p.network }}）
                </option>
              </select>
            </div>
          </div>

          <div class="form-row">
            <div class="field">
              <label for="bc-client">客户端</label>
              <select id="bc-client" v-model="nodeForm.client">
                <option v-for="c in clientOptions" :key="c" :value="c">{{ c }}</option>
              </select>
            </div>
            <div class="field">
              <label for="bc-cid">Chain ID</label>
              <input id="bc-cid" v-model.number="nodeForm.chain_id" type="number" min="0" />
            </div>
          </div>

          <div class="form-row">
            <div class="field">
              <label for="bc-rpc">RPC 端口</label>
              <input id="bc-rpc" v-model.number="nodeForm.rpc_port" type="number" min="1" max="65535" />
            </div>
            <div class="field">
              <label for="bc-ws">WS 端口</label>
              <input id="bc-ws" v-model.number="nodeForm.ws_port" type="number" min="1" max="65535" />
            </div>
          </div>

          <div class="field">
            <label for="bc-dir">数据目录</label>
            <input
              id="bc-dir"
              v-model="nodeForm.data_dir"
              type="text"
              placeholder="留空则默认 /tank/blockchain/<id>"
            />
          </div>

          <div v-if="!isDevChain" class="field">
            <label for="bc-sync">同步模式</label>
            <select id="bc-sync" v-model="nodeForm.sync_mode">
              <option value="snap">snap（快照同步，推荐）</option>
              <option value="full">full（全节点）</option>
              <option value="archive">archive（存档节点）</option>
            </select>
          </div>
          <div v-else class="form-msg is-info small">dev 链固定 full 同步模式。</div>

          <div class="form-actions">
            <button class="btn" type="button" :disabled="submitting" @click="showNodeDialog = false">
              取消
            </button>
            <button class="btn btn-primary" type="submit" :disabled="submitting">
              {{ submitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 创建浏览器对话框 ============ -->
    <div v-if="showExplorerDialog" class="modal-backdrop" @click.self="showExplorerDialog = false">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bc-exp-title">
        <div class="modal-head">
          <h3 id="bc-exp-title">创建区块链浏览器（Blockscout）</h3>
          <button
            class="modal-close"
            type="button"
            :disabled="submitting"
            @click="showExplorerDialog = false"
          >
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitExplorer">
          <div class="field">
            <label for="bc-exp-name">浏览器名称</label>
            <input id="bc-exp-name" v-model="explorerForm.name" type="text" placeholder="如 主网浏览器" />
          </div>
          <div class="field">
            <label for="bc-exp-node">关联节点</label>
            <select id="bc-exp-node" v-model="explorerForm.node_id">
              <option v-for="n in nodes" :key="n.id" :value="n.id">
                {{ n.name }}（{{ n.chain.network }}）
              </option>
            </select>
          </div>
          <div class="field">
            <label for="bc-exp-port">Web 端口</label>
            <input
              id="bc-exp-port"
              v-model.number="explorerForm.web_port"
              type="number"
              min="1"
              max="65535"
            />
          </div>
          <div v-if="!nodes.length" class="form-msg is-err">
            尚无 RPC 节点，请先在"RPC 节点" Tab 创建。
          </div>
          <div class="form-actions">
            <button class="btn" type="button" :disabled="submitting" @click="showExplorerDialog = false">
              取消
            </button>
            <button class="btn btn-primary" type="submit" :disabled="submitting || !nodes.length">
              {{ submitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 节点 compose 详情模态 ============ -->
    <div v-if="viewingNode && 'chain' in viewingNode" class="modal-backdrop" @click.self="viewingNode = null">
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal-head">
          <h3>节点详情 — {{ viewingNode.name }}</h3>
          <button class="modal-close" type="button" @click="viewingNode = null">×</button>
        </div>
        <div class="modal-body">
          <div class="detail-grid">
            <div><span class="muted small">链类型</span> <span :class="['pill', chainTypePill(viewingNode.chain.chain_type)]">{{ viewingNode.chain.chain_type }}</span></div>
            <div><span class="muted small">网络</span> {{ viewingNode.chain.network }}</div>
            <div><span class="muted small">Chain ID</span> {{ viewingNode.chain.chain_id }}</div>
            <div><span class="muted small">客户端</span> <span class="pill pill-cyan">{{ viewingNode.client }}</span></div>
            <div><span class="muted small">RPC 端口</span> {{ viewingNode.rpc_port }}</div>
            <div><span class="muted small">WS 端口</span> {{ viewingNode.ws_port ?? '—' }}</div>
            <div><span class="muted small">同步模式</span> {{ viewingNode.sync_mode }}</div>
            <div><span class="muted small">状态</span> <span :class="['pill', statusPill(viewingNode.status)]">{{ statusLabel(viewingNode.status) }}</span></div>
            <div class="detail-full"><span class="muted small">数据目录</span> {{ viewingNode.data_dir }}</div>
            <div v-if="viewingNode.start_cmd" class="detail-full">
              <span class="muted small">启动命令</span>
              <code class="inline-code">{{ viewingNode.start_cmd }}</code>
            </div>
          </div>

          <div class="compose-actions">
            <span class="muted small">docker-compose.yml</span>
            <button
              class="btn btn-small"
              type="button"
              @click="copyWithToast(viewingNode.compose_yaml ?? '')"
            >
              复制 compose
            </button>
            <button
              class="btn btn-small"
              type="button"
              :disabled="acting === viewingNode.id"
              @click="startNode(viewingNode)"
            >
              启动（docker compose up）
            </button>
          </div>
          <pre class="code-block">{{ viewingNode.compose_yaml ?? '（无 compose 内容）' }}</pre>
          <div class="compose-note muted small">
            docker 未安装/权限不足时请在宿主机手动执行 start_cmd；失败时节点会标记为 error 状态。
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 创建节点向导（节点运行 Tab） ============ -->
    <div v-if="showNodeWizard" class="modal-backdrop" @click.self="showNodeWizard = false">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bcn-wizard-title">
        <div class="modal-head">
          <h3 id="bcn-wizard-title">{{ t('bcn.wizardTitle') }}</h3>
          <button
            class="modal-close"
            type="button"
            :disabled="wizardSubmitting"
            @click="showNodeWizard = false"
          >
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitChainNode">
          <div class="field">
            <label for="bcn-name">{{ t('bcn.fName') }}</label>
            <input id="bcn-name" v-model="wizardForm.name" type="text" :placeholder="t('bcn.fNamePh')" />
          </div>

          <div class="form-row">
            <div class="field">
              <label for="bcn-kind">{{ t('bcn.fKind') }}</label>
              <select id="bcn-kind" v-model="wizardForm.kind" @change="onWizardKindChange">
                <option v-for="k in wizardKindOptions" :key="k" :value="k">{{ k }}</option>
              </select>
            </div>
            <div class="field">
              <label for="bcn-net">{{ t('bcn.fNetwork') }}</label>
              <select id="bcn-net" v-model="wizardForm.network" @change="onWizardNetworkChange">
                <option v-for="p in wizardNetworkOptions" :key="p.network" :value="p.network">
                  {{ p.name }}（{{ p.network }}）
                </option>
              </select>
            </div>
          </div>

          <div class="field">
            <label>{{ t('bcn.fMode') }}</label>
            <div v-for="m in wizardModeOptions" :key="m.mode" class="mode-option">
              <label class="mode-radio">
                <input v-model="wizardForm.mode" type="radio" :value="m.mode" />
                <span class="strong">{{ m.label }}</span>
                <span class="muted small">
                  {{ t('bcn.estSize') }} {{ m.estimated_size_gb }}GB · {{ m.sync_estimate }}
                </span>
              </label>
              <div v-if="m.note" class="muted small mode-note">{{ m.note }}</div>
            </div>
            <div v-if="wizardForm.mode === 'full' && wizardForm.kind === 'bitcoin'" class="mode-option">
              <label class="mode-radio">
                <input v-model="wizardForm.txindex" type="checkbox" />
                <span class="strong">txindex</span>
                <span class="muted small">{{ t('bcn.txindexHint') }}</span>
              </label>
            </div>
          </div>

          <!-- 空间预检（实时；full 不足 → 红色阻断） -->
          <div class="space-check-box" :class="{ 'is-blocked': spaceBlocked, 'is-ok': spaceCheck?.sufficient && !spaceBlocked }">
            <div v-if="spaceCheckLoading">{{ t('bcn.spaceChecking') }}</div>
            <template v-else-if="spaceCheck">
              <div class="space-line">
                {{ t('bcn.spaceRequired') }}
                <span class="strong">{{ spaceCheck.required_gb }}GB</span>
                · {{ t('bcn.spaceAvailable') }}
                <span class="strong">{{ formatBytes(spaceCheck.available_bytes) }}</span>
                <span class="muted small">（{{ spaceCheck.filesystem }}）</span>
              </div>
              <div v-if="spaceCheck.sufficient" class="small">{{ t('bcn.spaceOk') }}</div>
              <div v-else class="space-verdict">
                {{
                  spaceCheck.blocking
                    ? t('bcn.spaceBlockedFull')
                    : t('bcn.spaceLowFast')
                }}
              </div>
            </template>
            <div v-else class="muted small">{{ t('bcn.spacePending') }}</div>
          </div>

          <div class="form-row">
            <div class="field">
              <label for="bcn-dir">{{ t('bcn.fDataDir') }}</label>
              <input
                id="bcn-dir"
                v-model="wizardForm.data_dir"
                type="text"
                :placeholder="t('bcn.fDataDirPh', { dir: wizardDefaultDir })"
              />
            </div>
            <div class="field">
              <label for="bcn-rpcp">{{ t('bcn.fRpcPort') }}</label>
              <input id="bcn-rpcp" v-model.number="wizardForm.rpc_port" type="number" min="1024" max="65535" />
            </div>
          </div>

          <div class="field">
            <label for="bcn-extra">{{ t('bcn.fExtraFlags') }}</label>
            <input id="bcn-extra" v-model="wizardForm.extra_flags" type="text" :placeholder="t('bcn.fExtraFlagsPh')" />
          </div>

          <div v-if="wizardNeedsCL" class="form-msg is-info small">{{ t('bcn.needsConsensusClient') }}</div>
          <div v-if="!wizardBinaryInstalled" class="form-msg is-err small">{{ wizardInstallHint }}</div>

          <div class="form-actions">
            <button class="btn" type="button" :disabled="wizardSubmitting" @click="showNodeWizard = false">
              {{ t('bcn.cancel') }}
            </button>
            <button class="btn btn-primary" type="submit" :disabled="wizardSubmitting || spaceBlocked">
              {{ wizardSubmitting ? t('bcn.creating') : t('bcn.create') }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 节点日志查看（节点运行 Tab） ============ -->
    <div v-if="viewingLogs" class="modal-backdrop" @click.self="viewingLogs = null">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bcn-logs-title">
        <div class="modal-head">
          <h3 id="bcn-logs-title">{{ t('bcn.logsTitle', { name: viewingLogs.node.name }) }}</h3>
          <button class="modal-close" type="button" @click="viewingLogs = null">×</button>
        </div>
        <div class="modal-body">
          <div class="compose-actions">
            <span class="muted small">
              {{ t(nodeStatusKey(viewingLogs.status)) }} · {{ viewingLogs.path }}
            </span>
            <button
              class="btn btn-small"
              type="button"
              :disabled="logsLoading"
              @click="openNodeLogs(viewingLogs.node)"
            >
              {{ t('bcn.refreshLogs') }}
            </button>
          </div>
          <div v-if="logsLoading" class="table-empty">{{ t('bcn.loading') }}</div>
          <pre v-else class="code-block">{{ viewingLogs.lines.join('\n') || t('bcn.noLogsYet') }}</pre>
        </div>
      </div>
    </div>

    <!-- ============ 创建后 compose 预览 ============ -->
    <div v-if="createdCompose" class="modal-backdrop" @click.self="createdCompose = null">
      <div class="modal" role="dialog" aria-modal="true">
        <div class="modal-head">
          <h3>已生成 docker-compose.yml</h3>
          <button class="modal-close" type="button" @click="createdCompose = null">×</button>
        </div>
        <div class="modal-body">
          <div class="compose-actions">
            <button class="btn btn-small" type="button" @click="copyWithToast(createdCompose)">复制</button>
          </div>
          <pre class="code-block">{{ createdCompose }}</pre>
          <div class="compose-note muted small">
            docker 未安装时请在宿主机手动执行启动命令；启动后可点击表格"启动"按钮触发 docker compose up -d。
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.page {
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
.table-empty { padding: 28px 18px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 13px; }
.err-line { color: #b91c1c; margin-top: 2px; }

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
.form-row { display: flex; gap: 12px; flex-wrap: wrap; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 8px; }

/* Modal */
.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(620px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 16px 20px; display: flex; flex-direction: column; gap: 12px; }

/* compose / detail */
.detail-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 8px 16px; }
.detail-full { grid-column: 1 / -1; }
.inline-code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; background: var(--border-soft, #f3f4f6); padding: 2px 6px; border-radius: 4px; margin-left: 6px; }
.compose-actions { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.code-block {
  background: #1e1e2e; color: #cdd6f4; padding: 14px 16px; border-radius: var(--radius-sm, 8px);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; line-height: 1.5;
  overflow: auto; max-height: 340px; white-space: pre-wrap; word-break: break-all;
}
.compose-note { margin-top: 6px; }

/* overview */
.overview-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.preset-grid, .client-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(260px, 1fr)); gap: 12px; }
.preset-item, .client-item { padding: 12px 14px; border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); display: flex; flex-direction: column; gap: 6px; }
.preset-head, .client-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.client-list { line-height: 1.5; }

/* 节点运行 Tab（blockchain_nodes） */
.runtime-presets { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; }
.mode-line { line-height: 1.6; }
.mode-note { padding: 4px 8px; border-left: 2px solid var(--border, #d1d5db); color: var(--text-muted, #6b7280); margin: 2px 0 4px; }
.install-hint { white-space: pre-line; background: var(--border-soft, #f3f4f6); border-radius: var(--radius-sm, 8px); padding: 8px 10px; color: var(--text-muted, #5E5C5F); }
.node-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 14px; }
.node-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.node-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.node-detail { font-size: 13px; gap: 6px 12px; }
.node-error { white-space: pre-wrap; max-height: 90px; overflow: auto; }
.node-actions { display: flex; gap: 6px; flex-wrap: wrap; border-top: 1px solid var(--border-soft, #EDEDED); padding-top: 10px; }
.mode-option { padding: 4px 0; }
.mode-radio { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; cursor: pointer; font-size: 14px; }
.mode-radio input { accent-color: var(--accent, #E95420); }
.space-check-box { border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); padding: 10px 12px; font-size: 13px; display: flex; flex-direction: column; gap: 4px; }
.space-check-box.is-ok { border-color: rgba(21, 128, 61, 0.35); background: #f0fdf4; }
.space-check-box.is-blocked { border-color: rgba(185, 28, 28, 0.45); background: #fef2f2; }
.space-verdict { font-weight: 600; color: #b91c1c; }
.space-check-box.is-ok .space-verdict { color: #15803d; }
.space-line { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
</style>
