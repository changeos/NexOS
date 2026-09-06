<script setup lang="ts">
// =============================================================================
// ModelHubPanel.vue —— 模型仓库面板（可嵌入组件，模型管理「仓库」分组挂载）
//
// 4 子 Tab：本地模型 / 在线下载（下载任务 + 推荐模型合并）/ 模型大厅 / Spark 专区
// 后端：/api/v1/models/* （ModelHubRouteHandler，契约见 docs/MODELHUB_LOBBY.md §3）
//
// 前身：views/ModelHub.vue（独立「模型仓库」桌面应用，2026-09-03 并入「模型管理」
// (/llm) 一级分组「仓库」；本组件承接其全部逻辑与 i18n modelhub.* 命名空间——
// 照「直播」面板先例（LiveView→LivePanel，现 apps/streaming 应用包）组件化复用，数据/轮询/弹窗自包含，宿主零接线）。
// 本组件管模型**文件下载**；与「推理」分组（vLLM 实例管理）职责互补。
// 设计：Ubuntu Yaru 风格 .card，统计卡 + 模型卡片网格 + 下载表格 + 推荐网格
//       + 大厅卡片流（多源徽章/发布/多源并行下载）+ Spark 专区（NVFP4/SM120 策展卡片，
//       源可用性徽章，一键进既有下载向导）。
// 降级：modelscope 未安装时下载任务 status=failed 不报错；专区探测失败标不可用不剔除。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';
// 统一打赏按钮（docs/TIPS.md：大厅条目打赏，target_kind=lobby_entry，
// ref=model:<name>@<sharer>——与后端 model_lobby 表 id 同构）
import TipButton from '@/components/TipButton.vue';

/** 新增下载源向导 / remote_repo 任务卡片文案走 i18n（zh-CN/zh-TW/en-US/ja-JP
 * 四语全量）；本页既有 Tab 沿用硬编码中文口径，不在此一并迁移（LlmModels 同款）。 */
const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
interface LocalModel {
  id?: string;
  path?: string;
  size_bytes?: number;
  file_count?: number;
  modified_at?: string;
  has_config?: boolean;
  /** 来源徽章：'local'（模型库目录/导入链接）| 'hf_cache'（HF hub 缓存 snapshot）。 */
  source?: string;
  /** 显示名：HF 缓存条目为 org/name；本地条目与 id 相同（缺省回退 id）。 */
  display_name?: string;
  [k: string]: unknown;
}
/** A 面：权重档案文件清单条目（GET /models/:name/detail → files[]）。 */
interface ModelFileEntry {
  name?: string;
  size_bytes?: number;
  modified_at?: string;
  /** safetensors 分片序号（NNNNN-of-NNNNY 解析；非分片文件为 null）。 */
  shard_index?: number | null;
  shard_total?: number | null;
}
/** A 面：架构信息卡（config.json 解析；不存在时为 null）。 */
interface ModelConfigInfo {
  arch?: string;
  num_hidden_layers?: number;
  hidden_size?: number;
  vocab_size?: number;
  max_position_embeddings?: number;
}
/** A 面：GET /api/v1/models/:name/detail 响应。 */
interface ModelDetail {
  name?: string;
  path?: string;
  total_size_bytes?: number;
  file_count?: number;
  complete?: boolean;
  shards?: {
    sharded?: boolean;
    shard_total?: number;
    shard_files?: string[];
    sequence_complete?: boolean;
    missing_shards?: number[];
    index_file_present?: boolean;
  } | null;
  config?: ModelConfigInfo | null;
  files?: ModelFileEntry[];
}
/** B 面：大厅条目的单个分享源（同 name 多发布者合并进 sources[]）。 */
interface LobbySource {
  sharer?: string;
  source_url?: string;
  size_bytes?: number;
  file_count?: number;
  created_at?: string;
}
/** B 面：GET /api/v1/models/lobby 列表条目（同 name 多源合并）。 */
interface LobbyEntry {
  name?: string;
  display_name?: string;
  description?: string;
  tags?: string[];
  arch?: string;
  size_bytes?: number;
  file_count?: number;
  download_count?: number;
  sources?: LobbySource[];
  created_at?: string;
  /** 联邦来源节点（预留：本地条目 'local'/缺省；远程条目=发布节点名，未来复用 NexHub 的 LobbyFedTransport 模式同步）。 */
  source_node?: string;
}
/** C 面：lobby_multi 任务最近文件简报条目。 */
interface RecentFileReport {
  file?: string;
  source?: string;
  bytes?: number;
  status?: string;
  error?: string | null;
}
interface DownloadTask {
  id?: string;
  /** modelscope | lobby_multi | remote_repo（列表混排带 type）。 */
  type?: string;
  model_id?: string;
  name?: string;
  local_dir?: string;
  status?: string;
  progress_pct?: number;
  current_size_bytes?: number;
  estimated_size_bytes?: number;
  pid?: number | null;
  /** lobby_multi 专属。 */
  files_done?: number;
  files_total?: number;
  bytes_done?: number;
  total_bytes?: number;
  active_sources?: string[];
  recent_files?: RecentFileReport[];
  /** remote_repo 专属：'modelscope' | 'hf'（魔搭 / HF 镜像）。 */
  kind?: string;
  repo_id?: string;
  error?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface RecommendedModel {
  model_id?: string;
  name?: string;
  size_gb?: number;
  description?: string;
  tags?: string[];
  category?: string;
  downloaded?: boolean;
  [k: string]: unknown;
}
interface ModelHubStats {
  local_total?: number;
  total_size_bytes?: number;
  downloads_active?: number;
  downloads_completed?: number;
}
/** E 面：Spark 专区条目单源可用性（GET /models/spark-zone → sources[]）。 */
interface SparkSourceStatus {
  kind?: string;
  available?: boolean;
  file_count?: number | null;
  total_size_bytes?: number | null;
  error?: string | null;
}
/** E 面：Spark 专区条目（SM120/NVFP4 策展）。 */
interface SparkZoneItem {
  repo?: string;
  org?: string;
  quant?: string;
  params?: string;
  note?: string;
  downloaded?: boolean;
  sources?: SparkSourceStatus[];
}
/** E 面：GET /api/v1/models/spark-zone 响应。 */
interface SparkZoneResponse {
  ok?: boolean;
  probed?: boolean;
  origin?: string;
  entries?: SparkZoneItem[];
}

// =============================================================================
// 子 Tab 状态（嵌入模型管理「仓库」分组后的二级 Tab；「大厅」Tab 内另有
// 本地大厅/联邦大厅三级 Tab——层级用 sub-tabs 虚线下边线区分）
// =============================================================================
type TabKey = 'local' | 'downloads' | 'lobby' | 'spark';
const activeTab = ref<TabKey>('local');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'local', label: '本地模型' },
  { key: 'downloads', label: t('modelhub.dlTab') },
  { key: 'lobby', label: '大厅' },
  { key: 'spark', label: 'Spark 专区' },
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
const stats = ref<ModelHubStats>({});
async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.modelStats();
    stats.value = (raw as ModelHubStats) ?? {};
  } catch {
    stats.value = {};
  }
}

// =============================================================================
// 工具：格式化
// =============================================================================
function fmtBytes(b?: number): string {
  const n = b ?? 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`;
}
function fmtGb(gb?: number): string {
  const n = gb ?? 0;
  return `≈ ${n} GB`;
}
/** ISO 时间 → "YYYY-MM-DD HH:MM:SS"（去掉时区与毫秒，表格更紧凑）。 */
function fmtTime(iso?: string | null): string {
  if (!iso) return '—';
  const t = iso.replace('T', ' ');
  return t.length > 19 ? t.slice(0, 19) : t;
}
function fmtNum(n?: number): string {
  return (n ?? 0).toLocaleString();
}

// =============================================================================
// Tab1：本地模型
// =============================================================================
const localModels = ref<LocalModel[]>([]);
const localLoading = ref(false);
const localError = ref('');

async function loadLocal(): Promise<void> {
  localLoading.value = true;
  localError.value = '';
  try {
    const raw = await endpoints.modelLocal();
    localModels.value = Array.isArray(raw) ? (raw as LocalModel[]) : [];
  } catch (e) {
    localModels.value = [];
    localError.value = friendlyError(e);
  } finally {
    localLoading.value = false;
  }
}

// —— 两步确认删除（第一步亮红字提示不可恢复，第二步才执行） ——
const busyId = ref<string>('');
const confirmDeleteId = ref<string>('');

async function removeModel(name: string): Promise<void> {
  busyId.value = name;
  msg.value = null;
  try {
    // DELETE /models/:name：真实目录 rm -rf；导入的符号链接只解除链接（action=unlink）
    const res = (await endpoints.deleteModelByName(name)) as { action?: string } | null;
    await loadLocal();
    await loadStats();
    msg.value = {
      kind: 'ok',
      text:
        res?.action === 'unlink'
          ? `已解除导入链接 ${name}（外部源目录原样保留）`
          : `已删除模型 ${name}`,
    };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
    confirmDeleteId.value = '';
  }
}

// —— 明细对话框（文件清单 + 架构信息 + 完整性徽章） ——
const showDetail = ref(false);
const detailLoading = ref(false);
const detailError = ref('');
const detailName = ref('');
const detail = ref<ModelDetail | null>(null);

async function openDetail(name: string): Promise<void> {
  detailName.value = name;
  detail.value = null;
  detailError.value = '';
  detailLoading.value = true;
  showDetail.value = true;
  try {
    detail.value = (await endpoints.modelDetail(name)) as ModelDetail;
  } catch (e) {
    detailError.value = friendlyError(e);
  } finally {
    detailLoading.value = false;
  }
}
function closeDetail(): void {
  showDetail.value = false;
}

const detailFileColumns: Column<ModelFileEntry>[] = [
  { key: 'name', title: '文件名' },
  { key: 'size_bytes', title: '大小', width: '100px', align: 'right' },
  { key: 'modified_at', title: '修改时间', width: '160px' },
  { key: 'shard_index', title: '分片', width: '80px', align: 'center' },
];
function shardLabel(f: ModelFileEntry): string {
  if (f.shard_index == null) return '—';
  return f.shard_total != null ? `${f.shard_index}/${f.shard_total}` : String(f.shard_index);
}

/** 完整性判定文案（complete=false 时给出缺分片/缺 index/缺 config 的具体原因）。 */
const incompleteReason = computed<string>(() => {
  const d = detail.value;
  if (!d || d.complete) return '';
  const s = d.shards;
  if (s?.sharded) {
    const parts: string[] = [];
    if (s.missing_shards?.length) parts.push(`缺 ${s.missing_shards.join('、')} 号分片`);
    if (!s.index_file_present) parts.push('缺 model.safetensors.index.json');
    if (!parts.length) parts.push('分片序列不完整');
    return `权重不完整：${parts.join('；')}`;
  }
  return '权重不完整：单文件模型需同时具备权重文件与 config.json';
});

/** 架构信息卡行（config 为 null 时为空数组 → 模板显示占位文案）。 */
const archRows = computed<{ label: string; value: string }[]>(() => {
  const c = detail.value?.config;
  if (!c) return [];
  return [
    { label: '架构（model_type）', value: c.arch ?? '—' },
    { label: '层数', value: fmtNum(c.num_hidden_layers) },
    { label: '隐层宽度', value: fmtNum(c.hidden_size) },
    { label: '词表大小', value: fmtNum(c.vocab_size) },
    { label: '上下文长度', value: fmtNum(c.max_position_embeddings) },
  ];
});

// —— 导入对话框（库外目录符号链接入库） ——
const showImport = ref(false);
const importing = ref(false);
const importPath = ref('');

function openImport(): void {
  importPath.value = '';
  msg.value = null;
  showImport.value = true;
}
function closeImport(): void {
  if (importing.value) return;
  showImport.value = false;
}
async function submitImport(): Promise<void> {
  const p = importPath.value.trim();
  if (!p) {
    msg.value = { kind: 'err', text: '请填写模型目录的绝对路径' };
    return;
  }
  if (!p.startsWith('/')) {
    msg.value = { kind: 'err', text: '路径须为绝对路径（以 / 开头）' };
    return;
  }
  importing.value = true;
  msg.value = null;
  try {
    const res = (await endpoints.importModel(p)) as {
      name?: string;
      link_path?: string;
    } | null;
    await loadLocal();
    await loadStats();
    msg.value = {
      kind: 'ok',
      text: `已导入 ${res?.name ?? p}（符号链接 → ${res?.link_path ?? '模型库'}，不复制大文件）`,
    };
    showImport.value = false;
  } catch (e) {
    msg.value = { kind: 'err', text: '导入失败：' + friendlyError(e) };
  } finally {
    importing.value = false;
  }
}

// =============================================================================
// 子 Tab2「在线下载」：下载任务（modelscope 表格 + lobby_multi/remote_repo
// 任务卡片）+ 推荐模型一键下载（同 Tab 合并，2026-09-03）
// =============================================================================
const downloads = ref<DownloadTask[]>([]);
const downloadsLoading = ref(false);
const downloadsError = ref('');
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function loadDownloads(): Promise<void> {
  downloadsLoading.value = true;
  downloadsError.value = '';
  try {
    const raw = await endpoints.modelDownloads();
    downloads.value = Array.isArray(raw) ? (raw as DownloadTask[]) : [];
  } catch (e) {
    downloads.value = [];
    downloadsError.value = friendlyError(e);
  } finally {
    downloadsLoading.value = false;
  }
}

/** modelscope CLI 任务（type 缺省按 modelscope 兼容旧响应；remote_repo/lobby_multi 单列）。 */
const msTasks = computed(() =>
  downloads.value.filter(
    (t) => (t.type ?? 'modelscope') !== 'lobby_multi' && t.type !== 'remote_repo',
  ),
);
/** lobby_multi 多源任务。 */
const multiTasks = computed(() => downloads.value.filter((t) => t.type === 'lobby_multi'));
/** remote_repo 在线仓库任务（魔搭 / HF 镜像 HTTP 直连）。 */
const remoteTasks = computed(() => downloads.value.filter((t) => t.type === 'remote_repo'));
/** remote_repo 任务源徽章文案（魔搭 / HF 镜像）。 */
function remoteKindLabel(task: DownloadTask): string {
  return task.kind === 'hf' ? t('modelhub.kindHf') : t('modelhub.kindModelscope');
}

/** 多源任务字节进度（bytes_done / total_bytes）。 */
function multiPct(t: DownloadTask): number {
  const total = t.total_bytes ?? 0;
  if (!total) return 0;
  return Math.max(0, Math.min(100, Math.round(((t.bytes_done ?? 0) / total) * 100)));
}
/** 多源任务最近文件简报文案（done=✓ / failed=✗+错误 / 其他原样）。 */
function recentLabel(rf: RecentFileReport): string {
  if (rf.status === 'done') return `✓ 完成 · ${fmtBytes(rf.bytes ?? 0)}`;
  if (rf.status === 'failed') return `✗ ${rf.error ?? '失败（已换源重试）'}`;
  return `${rf.status ?? '—'} · ${fmtBytes(rf.bytes ?? 0)}`;
}

// 下载中自动 3s 轮询刷新进度（modelscope 与 lobby_multi 同口径，按 status 判断）
function startPolling(): void {
  stopPolling();
  pollTimer = setInterval(async () => {
    const hasActive = downloads.value.some(
      (t) => t.status === 'downloading' || t.status === 'pending',
    );
    if (hasActive) {
      await loadDownloads();
    } else {
      stopPolling();
    }
  }, 3000);
}
function stopPolling(): void {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

const dlColumns: Column<DownloadTask>[] = [
  { key: 'model_id', title: '模型 ID', sortable: true },
  { key: 'status', title: '状态', width: '110px' },
  { key: 'progress_pct', title: '进度', width: '180px' },
  { key: 'current_size_bytes', title: '已下载', width: '110px' },
  { key: 'estimated_size_bytes', title: '预估大小', width: '110px' },
  { key: 'pid', title: 'PID', width: '80px' },
  { key: 'actions', title: '操作', width: '120px', align: 'right' },
];

async function cancelDl(task: DownloadTask): Promise<void> {
  const id = String(task.id ?? '');
  let text: string;
  if (task.type === 'lobby_multi') {
    text = `确定取消多源下载 ${task.name ?? id}？（任务将移除，已下载的 .part 文件保留，重发任务可续传）`;
  } else if (task.type === 'remote_repo') {
    text = t('modelhub.cancelRemote');
  } else {
    text = '确定取消该下载？（将终止 modelscope 进程）';
  }
  if (!window.confirm(text)) return;
  msg.value = null;
  try {
    await endpoints.cancelModelDownload(id);
    await loadDownloads();
    msg.value = { kind: 'ok', text: '已取消下载' };
  } catch (e) {
    msg.value = { kind: 'err', text: '取消失败：' + friendlyError(e) };
  }
}

// —— 新建下载对话框（源选择：魔搭 / HF 镜像 HTTP 直连向导 + ModelScope CLI）——
// 模型大厅下载源三选一：HTTP 直连源走「org/model → 探测 → 文件清单勾选 → 下载」
// 向导（type=remote_repo 任务，Range 断点续传）；CLI 源保持原 modelscope 命令行语义。
type DlSource = 'modelscope' | 'hf' | 'cli';
/** 在线仓库探测响应（GET /models/remote/:kind/:org/:model）。 */
interface RemoteRepoFile {
  name?: string;
  size_bytes?: number;
  default_selected?: boolean;
}
interface RemoteRepoProbe {
  kind?: string;
  repo_id?: string;
  name?: string;
  file_count?: number;
  total_size_bytes?: number;
  files?: RemoteRepoFile[];
}
const showCreate = ref(false);
const submitting = ref(false);
const createModelId = ref('');
const dlSource = ref<DlSource>('modelscope');
const repoId = ref('');
const probing = ref(false);
const probeError = ref('');
const probeResult = ref<RemoteRepoProbe | null>(null);
/** 文件勾选表（key=相对路径；探测后按 default_selected 初始化）。 */
const fileSel = ref<Record<string, boolean>>({});
/** org/model 形态校验（与后端 validate_repo_id 同口径：段内 [A-Za-z0-9._-]）。 */
const REPO_ID_RE = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;
/** HTTP 直连源（向导形态）。 */
const isRemoteSource = computed(() => dlSource.value !== 'cli');

function openCreate(prefill?: string, source?: DlSource): void {
  createModelId.value = prefill ?? '';
  repoId.value = prefill ?? '';
  dlSource.value = source ?? 'modelscope';
  probeResult.value = null;
  probeError.value = '';
  fileSel.value = {};
  msg.value = null;
  showCreate.value = true;
}
function closeCreate(): void {
  if (submitting.value || probing.value) return;
  showCreate.value = false;
}
/** 切换源 / 改输入 → 作废上一次探测结果（kind 变了清单必须重探）。 */
function resetProbe(): void {
  probeResult.value = null;
  probeError.value = '';
  fileSel.value = {};
}
/** 探测仓库：文件清单（名称/大小/默认勾选）拉回本地勾选表。 */
async function probeRepo(): Promise<void> {
  const id = repoId.value.trim();
  if (!REPO_ID_RE.test(id)) {
    probeError.value = t('modelhub.errRepoId');
    return;
  }
  probing.value = true;
  probeError.value = '';
  probeResult.value = null;
  try {
    probeResult.value = (await endpoints.probeModelRepo(
      dlSource.value as 'modelscope' | 'hf',
      id,
    )) as RemoteRepoProbe;
    const sel: Record<string, boolean> = {};
    for (const f of probeResult.value?.files ?? []) {
      sel[String(f.name ?? '')] = f.default_selected === true;
    }
    fileSel.value = sel;
  } catch (e) {
    probeError.value = friendlyError(e);
  } finally {
    probing.value = false;
  }
}
/** 已勾选文件（保持清单顺序稳定）。 */
const selectedFiles = computed<string[]>(() =>
  (probeResult.value?.files ?? [])
    .map((f) => String(f.name ?? ''))
    .filter((n) => n && fileSel.value[n]),
);
const selectedBytes = computed<number>(() =>
  (probeResult.value?.files ?? [])
    .filter((f) => fileSel.value[String(f.name ?? '')])
    .reduce((acc, f) => acc + (f.size_bytes ?? 0), 0),
);
function selectAllFiles(): void {
  for (const f of probeResult.value?.files ?? []) fileSel.value[String(f.name ?? '')] = true;
}
function selectNoFiles(): void {
  for (const f of probeResult.value?.files ?? []) fileSel.value[String(f.name ?? '')] = false;
}
async function submitCreate(): Promise<void> {
  if (isRemoteSource.value) {
    // HTTP 直连源：探测 + 勾选 → POST /models/remote/downloads
    const id = repoId.value.trim();
    if (!REPO_ID_RE.test(id)) {
      msg.value = { kind: 'err', text: t('modelhub.errRepoId') };
      return;
    }
    if (!probeResult.value) {
      await probeRepo();
      if (!probeResult.value) return;
    }
    const files = selectedFiles.value;
    if (!files.length) {
      msg.value = { kind: 'err', text: t('modelhub.errNoFiles') };
      return;
    }
    submitting.value = true;
    msg.value = null;
    try {
      await endpoints.createModelRepoDownload({
        kind: dlSource.value as 'modelscope' | 'hf',
        repo_id: id,
        files,
      });
      await loadDownloads();
      msg.value = {
        kind: 'ok',
        text: t('modelhub.created', { name: id }),
      };
      showCreate.value = false;
      activeTab.value = 'downloads';
      startPolling();
    } catch (e) {
      msg.value = { kind: 'err', text: friendlyError(e) };
    } finally {
      submitting.value = false;
    }
    return;
  }
  // CLI 源：原 modelscope download 语义
  if (!createModelId.value.trim()) {
    msg.value = { kind: 'err', text: 'model_id 不可为空' };
    return;
  }
  submitting.value = true;
  msg.value = null;
  try {
    await endpoints.createModelDownload(createModelId.value.trim());
    await loadDownloads();
    msg.value = {
      kind: 'ok',
      text: '已创建下载任务（modelscope 后台拉取中，进度自动刷新）',
    };
    showCreate.value = false;
    activeTab.value = 'downloads';
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    submitting.value = false;
  }
}

// =============================================================================
// 推荐模型（数据面：并入「在线下载」子 Tab 展示，2026-09-03）
// =============================================================================
const recommended = ref<RecommendedModel[]>([]);
const recommendedLoading = ref(false);
const recommendedError = ref('');
const recBusy = ref<string>('');

async function loadRecommended(): Promise<void> {
  recommendedLoading.value = true;
  recommendedError.value = '';
  try {
    const raw = await endpoints.modelRecommended();
    recommended.value = Array.isArray(raw) ? (raw as RecommendedModel[]) : [];
  } catch (e) {
    recommended.value = [];
    recommendedError.value = friendlyError(e);
  } finally {
    recommendedLoading.value = false;
  }
}

async function downloadRecommended(modelId: string): Promise<void> {
  recBusy.value = modelId;
  msg.value = null;
  try {
    await endpoints.createModelDownload(modelId);
    await loadDownloads();
    msg.value = { kind: 'ok', text: `已开始下载 ${modelId}（进度见上方下载任务）` };
    // 推荐模型与下载任务同在「在线下载」子 Tab：无须切页，任务面板就在上方
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '下载失败：' + friendlyError(e) };
  } finally {
    recBusy.value = '';
  }
}

// =============================================================================
// 子 Tab3：模型大厅（发布/浏览/多源并行下载；内含本地/联邦三级 Tab）
// =============================================================================
const lobbyEntries = ref<LobbyEntry[]>([]);
const lobbyLoading = ref(false);
const lobbyError = ref('');
const lobbyQuery = ref('');
const lobbyBusy = ref<string>('');

async function loadLobby(): Promise<void> {
  lobbyLoading.value = true;
  lobbyError.value = '';
  try {
    const raw = await endpoints.lobbyList({ q: lobbyQuery.value.trim() || undefined });
    lobbyEntries.value = Array.isArray(raw) ? (raw as LobbyEntry[]) : [];
  } catch (e) {
    lobbyEntries.value = [];
    lobbyError.value = friendlyError(e);
  } finally {
    lobbyLoading.value = false;
  }
}

function sharersLabel(e: LobbyEntry): string {
  const names = (e.sources ?? []).map((s) => s.sharer).filter((s): s is string => !!s);
  return names.length ? names.join('、') : '—';
}

/**
 * 打赏目标 ref：`model:<name>@<sharer>`（后端 model_lobby 行 id 即
 * `<name>@<sharer>`，sharer 已是净化后的存储值——前端拼接即精确重建）。
 * 分享者非链上身份（如 admin）时后端 400 带原因提示。见 docs/TIPS.md。
 */
function modelLobbyTipRef(e: LobbyEntry, s: LobbySource): string {
  return `model:${e.name ?? ''}@${s.sharer ?? ''}`;
}

// —— 二级 Tab：本地大厅 / 联邦大厅（大厅 Tab 内；切换只换视图，搜索状态保留）——
// 模型大厅暂无联邦同步（source_node 预留）：本地 Tab = 全部条目（当前行为），
// 联邦 Tab 仅在条目带非 'local' source_node 时出现（现阶段恒空 → 空态提示）。
type LobbyView = 'local' | 'fed';
const lobbyView = ref<LobbyView>('local');
/** 联邦远程条目（source_node 非空且非 'local'）。 */
function isRemoteEntry(e: LobbyEntry): boolean {
  return !!e.source_node && e.source_node !== 'local';
}
const localLobbyEntries = computed(() => lobbyEntries.value.filter((e) => !isRemoteEntry(e)));
const fedLobbyEntries = computed(() => lobbyEntries.value.filter((e) => isRemoteEntry(e)));
/** 当前二级 Tab 显示的条目（本地=默认全量）。 */
const visibleLobbyEntries = computed(() =>
  lobbyView.value === 'fed' ? fedLobbyEntries.value : localLobbyEntries.value,
);

/** 大厅下载：sources>1 先亮多源并行提示，确认后 POST /downloads {name, sources}。 */
async function downloadLobby(e: LobbyEntry): Promise<void> {
  const name = String(e.name ?? '');
  const sources = (e.sources ?? [])
    .map((s) => s.source_url ?? '')
    .filter(Boolean);
  if (!name || !sources.length) {
    msg.value = { kind: 'err', text: '该条目缺少可用的下载源（source_url）' };
    return;
  }
  if (sources.length > 1) {
    const ok = window.confirm(
      `「${e.display_name || name}」有 ${sources.length} 个分享源，` +
        `将从 ${sources.length} 个源并行拉取（文件级轮转分配，失败自动换源续传）。确定开始下载？`,
    );
    if (!ok) return;
  }
  lobbyBusy.value = name;
  msg.value = null;
  try {
    await endpoints.createLobbyDownload(name, sources);
    await loadDownloads();
    msg.value = {
      kind: 'ok',
      text: `已创建多源下载任务：${name}（${sources.length} 个源），下载任务区查看进度`,
    };
    activeTab.value = 'downloads';
    startPolling();
  } catch (e2) {
    msg.value = { kind: 'err', text: '创建下载失败：' + friendlyError(e2) };
  } finally {
    lobbyBusy.value = '';
  }
}

// —— 发布到大厅对话框（new=新发布 / refresh=刷新快照，重发布语义同 name） ——
type PublishMode = 'new' | 'refresh';
const showPublish = ref(false);
const publishing = ref(false);
const publishMode = ref<PublishMode>('new');
const pubForm = ref({ name: '', display_name: '', description: '', tagsText: '' });
const pubPreview = ref<ModelDetail | null>(null);
const pubPreviewLoading = ref(false);
const pubPreviewError = ref('');

async function openPublish(mode: PublishMode, entry?: LobbyEntry): Promise<void> {
  publishMode.value = mode;
  pubPreview.value = null;
  pubPreviewError.value = '';
  if (entry?.name) {
    pubForm.value = {
      name: entry.name,
      display_name: entry.display_name ?? '',
      description: entry.description ?? '',
      tagsText: (entry.tags ?? []).join(', '),
    };
  } else {
    pubForm.value = { name: '', display_name: '', description: '', tagsText: '' };
  }
  msg.value = null;
  if (!localModels.value.length) await loadLocal();
  showPublish.value = true;
}
function closePublish(): void {
  if (publishing.value) return;
  showPublish.value = false;
}

// 选中本地模型后即时拉取权重档案做预览（大小/arch/完整性）
watch(
  () => pubForm.value.name,
  async (n) => {
    pubPreview.value = null;
    pubPreviewError.value = '';
    if (!n) return;
    pubPreviewLoading.value = true;
    try {
      pubPreview.value = (await endpoints.modelDetail(n)) as ModelDetail;
    } catch (e) {
      pubPreviewError.value = friendlyError(e);
    } finally {
      pubPreviewLoading.value = false;
    }
  },
);

/** 发布预览行（arch/大小/文件数）。 */
const pubPreviewRows = computed<{ label: string; value: string }[]>(() => {
  const p = pubPreview.value;
  if (!p) return [];
  return [
    { label: '架构', value: p.config?.arch ?? '未知' },
    { label: '大小', value: fmtBytes(p.total_size_bytes ?? 0) },
    { label: '文件数', value: String(p.file_count ?? 0) },
  ];
});

async function submitPublish(): Promise<void> {
  if (!pubForm.value.name) {
    msg.value = { kind: 'err', text: '请选择要发布的本地模型' };
    return;
  }
  publishing.value = true;
  msg.value = null;
  try {
    const tags = pubForm.value.tagsText
      .split(/[,，\s]+/)
      .map((t) => t.trim())
      .filter(Boolean);
    const res = (await endpoints.lobbyPublish({
      name: pubForm.value.name,
      display_name: pubForm.value.display_name.trim() || undefined,
      description: pubForm.value.description.trim() || undefined,
      tags: tags.length ? tags : undefined,
    })) as { id?: string } | null;
    await loadLobby();
    const target = res?.id ?? pubForm.value.name;
    msg.value = {
      kind: 'ok',
      text:
        publishMode.value === 'refresh'
          ? `已刷新大厅快照（${target}，下载计数保留）`
          : `已发布到大厅（${target}），其他 NexOS 节点即可拉取`,
    };
    showPublish.value = false;
  } catch (e) {
    msg.value = { kind: 'err', text: '发布失败：' + friendlyError(e) };
  } finally {
    publishing.value = false;
  }
}

// =============================================================================
// 子 Tab4：Spark 专区（SM120/NVFP4 策展，E 面）
// =============================================================================
// DGX Spark（GB10，SM120）对 NVFP4 有硬件级加速；专区=精选 NVFP4 清单 + 一键进
// 既有下载向导（预填 repo 与首选源，向导内再探测勾选——不复制下载 UI）。
// 模型对一切 SM120 GPU（RTX 50 系等）通用，非 Spark 专属（头部说明条常驻）。
const sparkZone = ref<SparkZoneResponse | null>(null);
const sparkLoading = ref(false);
const sparkError = ref('');
const sparkLoadedOnce = ref(false);

async function loadSparkZone(probe = true): Promise<void> {
  sparkLoading.value = true;
  sparkError.value = '';
  try {
    sparkZone.value = (await endpoints.sparkZone({ probe })) as SparkZoneResponse;
    sparkLoadedOnce.value = true;
  } catch (e) {
    sparkZone.value = null;
    sparkError.value = friendlyError(e);
  } finally {
    sparkLoading.value = false;
  }
}

// 切到专区 Tab 时惰性加载（首次进入即探测拉取；此后「重新探测」手动刷新）
watch(activeTab, (tab) => {
  if (tab === 'spark' && !sparkLoadedOnce.value && !sparkLoading.value) {
    void loadSparkZone();
  }
});

/** 仓库名短显示（repo 末段）。 */
function sparkShortName(e: SparkZoneItem): string {
  const repo = String(e.repo ?? '');
  return repo.split('/').pop() || repo || '—';
}

/** 条目在指定源上的可用性（sources 恒 [modelscope, hf]，按 kind 取）。 */
function sparkSourceStatus(
  e: SparkZoneItem,
  kind: 'modelscope' | 'hf',
): SparkSourceStatus | undefined {
  return (e.sources ?? []).find((s) => s.kind === kind);
}

/** 源徽章样式：可用（绿）/ 不可用（红）/ 未探测（灰）。 */
function sparkSourcePill(s?: SparkSourceStatus): { cls: string; label: string } {
  if (!s || (s.error ?? '').includes('未探测')) {
    return { cls: 'pill-muted', label: t('modelhub.sparkUnknown') };
  }
  return s.available
    ? { cls: 'pill-ok', label: t('modelhub.sparkAvailable') }
    : { cls: 'pill-err', label: t('modelhub.sparkUnavailable') };
}

/** 源徽章悬停提示：可用=件数+全量大小；不可用=后端错误原文（诚实降级）。 */
function sparkSourceTitle(s?: SparkSourceStatus): string {
  if (!s || (s.error ?? '').includes('未探测')) return t('modelhub.sparkUnknown');
  if (s.available) {
    return t('modelhub.sparkAvailTitle', {
      n: s.file_count ?? 0,
      size: s.total_size_bytes ? fmtBytes(s.total_size_bytes) : '—',
    });
  }
  return s.error ?? t('modelhub.sparkUnavailable');
}

/** 下载首选源：第一个可用源（魔搭优先序=后端 sources 顺序）；全不可用回魔搭（向导里可换）。 */
function sparkPreferredKind(e: SparkZoneItem): 'modelscope' | 'hf' {
  return (e.sources ?? []).some((s) => s.kind === 'hf' && s.available) ? 'hf' : 'modelscope';
}

/** 下载 → 复用既有「新建下载」向导：预填 repo 与源，切到「在线下载」子 Tab
 *  再进探测+清单勾选流程（向导弹窗挂在该子 Tab 的 section 内，切过去才可见）。 */
function downloadSpark(e: SparkZoneItem): void {
  const repo = String(e.repo ?? '');
  if (!repo) return;
  activeTab.value = 'downloads';
  openCreate(repo, sparkPreferredKind(e));
}

// =============================================================================
// 徽章映射
// =============================================================================
function statusClass(s?: string): string {
  switch (s) {
    case 'downloading':
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
    case 'downloading':
      return '下载中';
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
function categoryClass(c?: string): string {
  switch (c) {
    case 'vl':
      return 'pill-purple';
    case 'llm':
      return 'pill-cyan';
    case 'embedding':
      return 'pill-blue';
    default:
      return 'pill-muted';
  }
}
function categoryLabel(c?: string): string {
  switch (c) {
    case 'vl':
      return '视觉多模态';
    case 'llm':
      return '语言模型';
    case 'embedding':
      return '嵌入模型';
    default:
      return c ?? '—';
  }
}
function progressPct(t: DownloadTask): number {
  return Math.max(0, Math.min(100, t.progress_pct ?? 0));
}

const hasActiveDownloads = computed(() =>
  downloads.value.some((t) => t.status === 'downloading' || t.status === 'pending'),
);

// =============================================================================
// 刷新与初始化
// =============================================================================
async function refreshAll(): Promise<void> {
  await Promise.all([loadLocal(), loadDownloads(), loadRecommended(), loadStats(), loadLobby()]);
  if (hasActiveDownloads.value) startPolling();
}

onMounted(() => {
  void refreshAll();
});

onBeforeUnmount(() => {
  stopPolling();
});
</script>

<template>
  <div class="mh-panel">
    <!-- 工具栏：二级 Tab（仓库分组内）+ 面板级刷新（数据自包含，宿主「刷新」只管推理侧） -->
    <div class="mh-toolbar">
      <nav class="tabs sub-tabs" role="tablist" aria-label="模型仓库子页切换">
        <button
          v-for="t in tabs"
          :key="t.key"
          class="tab sub-tab"
          :class="{ active: activeTab === t.key }"
          role="tab"
          :aria-selected="activeTab === t.key"
          @click="activeTab = t.key"
        >{{ t.label }}</button>
      </nav>
      <button
        class="btn btn-small"
        :disabled="localLoading || downloadsLoading || lobbyLoading"
        @click="refreshAll"
      >
        <span
          class="spin"
          :class="{ spinning: localLoading || downloadsLoading || lobbyLoading }"
          aria-hidden="true"
        >↻</span>
        刷新
      </button>
    </div>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== 子 Tab1 本地模型 =================== -->
    <section v-show="activeTab === 'local'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">本地模型数</div>
          <div class="stat-value">{{ stats.local_total ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">总占用</div>
          <div class="stat-value stat-value-sm">{{ fmtBytes(stats.total_size_bytes ?? 0) }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">本地模型列表（模型库 + {{ t('modelhub.hfCacheBadge') }}）</span>
        <span class="muted small">{{ t('modelhub.hfScanHint') }}</span>
        <button class="btn btn-small" @click="openImport">＋ 导入模型</button>
      </div>

      <div v-if="localError" class="error-box">{{ localError }}</div>
      <div v-if="localLoading && !localModels.length" class="card empty-card">加载中…</div>
      <div v-else-if="!localModels.length" class="card empty-card">
        暂无本地模型，去<a class="link" @click="activeTab = 'downloads'">在线下载</a>或
        <a class="link" @click="activeTab = 'lobby'">大厅</a>添加；{{ t('modelhub.hfScanHint') }}，
        也可「导入」库外既有目录。
      </div>
      <div v-else class="model-grid">
        <div v-for="m in localModels" :key="`${m.source ?? 'local'}:${m.id}`" class="card model-card">
          <div class="model-card-head">
            <span class="model-name">{{ m.display_name || m.id || '—' }}</span>
            <span v-if="m.source === 'hf_cache'" class="pill pill-hf" :title="t('modelhub.hfCacheTip')">
              🤗 {{ t('modelhub.hfCacheBadge') }}
            </span>
            <span v-if="m.has_config" class="pill pill-ok">完整模型</span>
            <span v-if="!m.has_config" class="pill pill-muted">下载中</span>
          </div>
          <div class="model-row">
            <span class="muted small">大小</span>
            <span class="mono">{{ fmtBytes(m.size_bytes ?? 0) }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">文件数</span>
            <span class="mono">{{ m.file_count ?? 0 }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">修改时间</span>
            <span class="mono small">{{ fmtTime(m.modified_at) }}</span>
          </div>
          <div class="model-row model-row-path">
            <span class="muted small">路径</span>
            <span class="mono small model-path" :title="m.path ?? ''">{{ m.path ?? '—' }}</span>
          </div>
          <!-- HF 缓存条目：删除由 huggingface 工具链管理（后端 400 拒绝），不给出删除按钮 -->
          <p v-if="m.source === 'hf_cache'" class="muted small hf-manage-note">
            {{ t('modelhub.hfCacheNoDelete') }}
          </p>
          <!-- 两步确认删除：第一次点击亮红字警示，再点「确认删除」才执行 -->
          <p v-if="m.source !== 'hf_cache' && confirmDeleteId === m.id" class="danger-note">
            将递归删除整个模型目录，<strong>此操作不可恢复</strong>！
            （导入的符号链接仅解除链接，外部源目录不受影响）
          </p>
          <div v-if="m.source !== 'hf_cache'" class="model-card-actions">
            <template v-if="confirmDeleteId === m.id">
              <button
                class="btn btn-small"
                :disabled="busyId === m.id"
                @click.stop="confirmDeleteId = ''"
              >再想想</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === m.id"
                @click.stop="removeModel(String(m.id ?? ''))"
              >{{ busyId === m.id ? '删除中…' : '确认删除' }}</button>
            </template>
            <template v-else>
              <button
                class="btn btn-small"
                @click.stop="openDetail(String(m.id ?? ''))"
              >明细</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === m.id"
                @click.stop="confirmDeleteId = String(m.id ?? '')"
              >删除</button>
            </template>
          </div>
          <div v-else class="model-card-actions">
            <button
              class="btn btn-small"
              @click.stop="openDetail(String(m.id ?? ''))"
            >明细</button>
          </div>
        </div>
      </div>

      <!-- 导入外部模型对话框（符号链接入库，不复制大文件） -->
      <div v-if="showImport" class="modal-backdrop" @click.self="closeImport">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="mh-import-title">
          <div class="modal-head">
            <h3 id="mh-import-title">导入外部模型</h3>
            <button class="modal-close" type="button" :disabled="importing" @click="closeImport">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitImport">
            <div class="field">
              <label for="mh-import-path">模型目录绝对路径</label>
              <input
                id="mh-import-path"
                v-model="importPath"
                type="text"
                placeholder="如 /home/nvidia/.cache/huggingface/hub/models--nvidia--Qwen3.6-27B-NVFP4/snapshots/<hash>"
                :disabled="importing"
              />
            </div>
            <p class="muted small">
              以<strong>符号链接</strong>方式导入模型库（不复制大文件，秒级入库），导入后即可发布到大厅——
              HF 缓存若未被自动扫描到（自定义缓存位），可直接粘贴 snapshot 目录路径手动添加。
              目录顶层须含 <code class="mono">config.json</code> 或 <code class="mono">*.safetensors</code>；
              源不存在 404 / 非模型目录 400 / 库内重名 409。
            </p>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="importing" @click="closeImport">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="importing">
                {{ importing ? '导入中…' : '导入' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 模型明细对话框：完整性徽章 + 架构信息卡 + 文件清单表 -->
      <div v-if="showDetail" class="modal-backdrop" @click.self="closeDetail">
        <div class="modal modal-wide" role="dialog" aria-modal="true" aria-labelledby="mh-detail-title">
          <div class="modal-head">
            <h3 id="mh-detail-title">模型明细 — {{ detail?.name ?? detailName }}</h3>
            <button class="modal-close" type="button" @click="closeDetail">×</button>
          </div>
          <div class="modal-body">
            <div v-if="detailLoading" class="muted">权重档案扫描中…</div>
            <div v-else-if="detailError" class="error-box">{{ detailError }}</div>
            <template v-else-if="detail">
              <!-- 概要 + 完整性徽章 -->
              <div class="detail-badges">
                <span
                  class="pill"
                  :class="detail.complete ? 'pill-ok' : 'pill-err'"
                >{{ detail.complete ? '✓ 完整' : '✗ 缺分片' }}</span>
                <span class="pill pill-muted">
                  {{ fmtBytes(detail.total_size_bytes ?? 0) }} · {{ detail.file_count ?? 0 }} 文件
                </span>
                <span v-if="detail.shards?.sharded" class="pill pill-blue">
                  分片 {{ (detail.shards.shard_files ?? []).length }}/{{ detail.shards.shard_total ?? '—' }}
                </span>
                <span v-if="detail.shards?.sharded" class="pill pill-muted">
                  index.json {{ detail.shards.index_file_present ? '在场' : '缺失' }}
                </span>
              </div>
              <p v-if="incompleteReason" :class="['form-msg', 'is-err']">{{ incompleteReason }}</p>
              <div class="model-row model-row-path">
                <span class="muted small">路径</span>
                <span class="mono small model-path" :title="detail.path ?? ''">{{ detail.path ?? '—' }}</span>
              </div>

              <!-- 架构信息卡 -->
              <div class="card arch-card">
                <div class="panel-title">架构信息（config.json）</div>
                <div v-if="archRows.length" class="arch-grid">
                  <div v-for="row in archRows" :key="row.label" class="arch-item">
                    <div class="muted small">{{ row.label }}</div>
                    <div class="mono">{{ row.value }}</div>
                  </div>
                </div>
                <div v-else class="muted small">
                  未找到 config.json（可能仍在下载中，或非 HF 格式模型目录）
                </div>
              </div>

              <!-- 文件清单表（分片高亮） -->
              <div class="panel-title">文件清单（递归，按路径排序）</div>
              <div class="card card-table">
                <DataTable
                  :columns="detailFileColumns"
                  :rows="detail.files ?? []"
                  empty-text="无文件"
                >
                  <template #cell-name="{ row }">
                    <span
                      class="mono small file-name"
                      :class="{ 'shard-file': row.shard_index != null }"
                      :title="row.name ?? ''"
                    >{{ row.name ?? '—' }}</span>
                  </template>
                  <template #cell-size_bytes="{ row }">
                    <span class="mono">{{ fmtBytes(row.size_bytes ?? 0) }}</span>
                  </template>
                  <template #cell-modified_at="{ row }">
                    <span class="mono small">{{ fmtTime(row.modified_at) }}</span>
                  </template>
                  <template #cell-shard_index="{ row }">
                    <span class="mono">{{ shardLabel(row) }}</span>
                  </template>
                </DataTable>
              </div>
              <p class="muted small">
                橙色高亮 = safetensors 分片（<code class="mono">*-0000X-of-0000Y.safetensors</code>）；
                分片列 = 序号/声明总数。
              </p>
            </template>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== 子 Tab2 在线下载（任务 + 推荐） =================== -->
    <section v-show="activeTab === 'downloads'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">下载任务</span>
        <button class="btn btn-small btn-primary" @click="openCreate()">＋ 新建下载</button>
      </div>

      <div v-if="downloadsError" class="error-box">{{ downloadsError }}</div>

      <!-- lobby_multi 多源任务卡片 -->
      <template v-if="multiTasks.length">
        <div class="panel-title">多源下载任务（大厅）</div>
        <div class="multi-grid">
          <div v-for="t in multiTasks" :key="t.id" class="card multi-card">
            <div class="model-card-head">
              <span class="model-name">{{ t.name ?? t.id ?? '—' }}</span>
              <span class="head-pills">
                <span class="pill pill-purple">多源</span>
                <span class="pill" :class="statusClass(t.status)">{{ statusLabel(t.status) }}</span>
              </span>
            </div>
            <div class="model-row">
              <span class="muted small">文件</span>
              <span class="mono">{{ t.files_done ?? 0 }} / {{ t.files_total ?? 0 }}</span>
            </div>
            <div class="prog-wrap">
              <span class="prog-bar">
                <span
                  class="prog-fill"
                  :class="{ 'fill-ok': t.status === 'completed' }"
                  :style="{ width: multiPct(t) + '%' }"
                />
              </span>
              <span class="prog-text">{{ multiPct(t) }}%</span>
            </div>
            <div class="model-row">
              <span class="muted small">已拉取</span>
              <span class="mono small">
                {{ fmtBytes(t.bytes_done ?? 0) }} / {{ fmtBytes(t.total_bytes ?? 0) }}
              </span>
            </div>
            <div class="model-row">
              <span class="muted small">活跃源</span>
              <span
                class="mono"
                :title="(t.active_sources ?? []).join('\n')"
              >{{ (t.active_sources ?? []).length }} 个</span>
            </div>
            <ul v-if="(t.recent_files ?? []).length" class="recent-files">
              <li v-for="(rf, i) in (t.recent_files ?? []).slice(-3).reverse()" :key="i">
                <span class="mono small file-name">{{ rf.file ?? '—' }}</span>
                <span
                  class="small"
                  :class="rf.status === 'failed' ? 'rf-failed' : 'rf-done'"
                >{{ recentLabel(rf) }}</span>
              </li>
            </ul>
            <p v-if="t.error" class="form-msg is-err">{{ t.error }}</p>
            <div class="model-card-actions">
              <button
                class="btn btn-small btn-danger"
                :disabled="t.status !== 'downloading' && t.status !== 'pending'"
                @click.stop="cancelDl(t)"
              >取消</button>
            </div>
          </div>
        </div>
      </template>

      <!-- remote_repo 在线仓库任务卡片（魔搭 / HF 镜像 HTTP 直连） -->
      <template v-if="remoteTasks.length">
        <div class="panel-title">{{ t('modelhub.remoteTasksTitle') }}</div>
        <div class="multi-grid">
          <div v-for="task in remoteTasks" :key="task.id" class="card multi-card">
            <div class="model-card-head">
              <span class="model-name">{{ task.name ?? task.repo_id ?? task.id ?? '—' }}</span>
              <span class="head-pills">
                <span class="pill pill-cyan">{{ remoteKindLabel(task) }}</span>
                <span class="pill" :class="statusClass(task.status)">{{ statusLabel(task.status) }}</span>
              </span>
            </div>
            <div class="model-row model-row-path">
              <span class="muted small">仓库</span>
              <span class="mono small model-path" :title="task.repo_id ?? ''">{{ task.repo_id ?? '—' }}</span>
            </div>
            <div class="model-row">
              <span class="muted small">文件</span>
              <span class="mono">{{ task.files_done ?? 0 }} / {{ task.files_total ?? 0 }}</span>
            </div>
            <div class="prog-wrap">
              <span class="prog-bar">
                <span
                  class="prog-fill"
                  :class="{ 'fill-ok': task.status === 'completed' }"
                  :style="{ width: multiPct(task) + '%' }"
                />
              </span>
              <span class="prog-text">{{ multiPct(task) }}%</span>
            </div>
            <div class="model-row">
              <span class="muted small">已拉取</span>
              <span class="mono small">
                {{ fmtBytes(task.bytes_done ?? 0) }} / {{ fmtBytes(task.total_bytes ?? 0) }}
              </span>
            </div>
            <ul v-if="(task.recent_files ?? []).length" class="recent-files">
              <li v-for="(rf, i) in (task.recent_files ?? []).slice(-3).reverse()" :key="i">
                <span class="mono small file-name">{{ rf.file ?? '—' }}</span>
                <span
                  class="small"
                  :class="rf.status === 'failed' ? 'rf-failed' : 'rf-done'"
                >{{ recentLabel(rf) }}</span>
              </li>
            </ul>
            <p v-if="task.error" class="form-msg is-err">{{ task.error }}</p>
            <div class="model-card-actions">
              <button
                class="btn btn-small btn-danger"
                :disabled="task.status !== 'downloading' && task.status !== 'pending'"
                @click.stop="cancelDl(task)"
              >取消</button>
            </div>
          </div>
        </div>
      </template>

      <!-- modelscope CLI 任务表 -->
      <div class="panel">
        <div class="panel-title">ModelScope 下载任务</div>
        <div class="card card-table">
          <DataTable
            :columns="dlColumns"
            :rows="msTasks"
            :loading="downloadsLoading"
            empty-text="暂无 ModelScope 下载任务，点击右上角「新建下载」。"
          >
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-progress_pct="{ row }">
              <span class="prog-wrap">
                <span class="prog-bar">
                  <span
                    class="prog-fill"
                    :class="{ 'fill-ok': row.status === 'completed' }"
                    :style="{ width: progressPct(row) + '%' }"
                  />
                </span>
                <span class="prog-text">{{ progressPct(row) }}%</span>
              </span>
            </template>
            <template #cell-current_size_bytes="{ row }">
              <span class="mono">{{ fmtBytes(row.current_size_bytes ?? 0) }}</span>
            </template>
            <template #cell-estimated_size_bytes="{ row }">
              <span class="mono">{{ row.estimated_size_bytes ? fmtBytes(row.estimated_size_bytes) : '—' }}</span>
            </template>
            <template #cell-pid="{ row }">
              <span class="mono">{{ row.pid ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                :disabled="row.status !== 'downloading' && row.status !== 'pending'"
                @click.stop="cancelDl(row)"
              >取消</button>
            </template>
          </DataTable>
        </div>
      </div>
      <p v-if="hasActiveDownloads" class="muted small">
        检测到下载中任务，进度每 3 秒自动刷新。
      </p>

      <!-- 推荐模型（2026-09-03 并入「在线下载」子 Tab：一键下载后任务面板就在上方，
           无须切页——原独立「推荐模型」Tab 移除） -->
      <div class="panel-head">
        <span class="panel-title">推荐模型（一键下载）</span>
      </div>

      <div v-if="recommendedError" class="error-box">{{ recommendedError }}</div>
      <div v-if="recommendedLoading && !recommended.length" class="card empty-card">加载中…</div>
      <div v-else-if="!recommended.length" class="card empty-card">暂无推荐模型。</div>
      <div v-else class="rec-grid">
        <div v-for="r in recommended" :key="r.model_id" class="card rec-card">
          <div class="rec-card-head">
            <span class="rec-name">{{ r.name ?? '—' }}</span>
            <span class="pill" :class="categoryClass(r.category)">{{ categoryLabel(r.category) }}</span>
          </div>
          <div class="rec-model-id mono small">{{ r.model_id ?? '—' }}</div>
          <p class="rec-desc">{{ r.description ?? '' }}</p>
          <div class="rec-tags">
            <span v-for="tg in (r.tags ?? [])" :key="tg" class="tag">{{ tg }}</span>
          </div>
          <div class="rec-row">
            <span class="muted small">大小</span>
            <span class="mono">{{ fmtGb(r.size_gb ?? 0) }}</span>
          </div>
          <div class="rec-card-actions">
            <span v-if="r.downloaded" class="pill pill-ok">已下载</span>
            <button
              v-else
              class="btn btn-small btn-primary"
              :disabled="recBusy === r.model_id"
              @click.stop="downloadRecommended(String(r.model_id ?? ''))"
            >
              {{ recBusy === r.model_id ? '创建中…' : '下载' }}
            </button>
          </div>
        </div>
      </div>

      <!-- 新建下载对话框（源选择：魔搭 / HF 镜像 HTTP 直连向导 + ModelScope CLI） -->
      <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
        <div class="modal modal-wide" role="dialog" aria-modal="true" aria-labelledby="mh-create-title">
          <div class="modal-head">
            <h3 id="mh-create-title">{{ t('modelhub.addTitle') }}</h3>
            <button class="modal-close" type="button" :disabled="submitting || probing" @click="closeCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitCreate">
            <!-- 下载源选择（HTTP 直连两源 + 本机 CLI） -->
            <div class="field">
              <label>{{ t('modelhub.sourceLabel') }}</label>
              <div class="src-radios">
                <label class="src-radio">
                  <input
                    v-model="dlSource"
                    type="radio"
                    value="modelscope"
                    :disabled="submitting || probing"
                    @change="resetProbe"
                  />
                  <span>{{ t('modelhub.srcModelscope') }}</span>
                </label>
                <label class="src-radio">
                  <input
                    v-model="dlSource"
                    type="radio"
                    value="hf"
                    :disabled="submitting || probing"
                    @change="resetProbe"
                  />
                  <span>{{ t('modelhub.srcHf') }}</span>
                </label>
                <label class="src-radio">
                  <input
                    v-model="dlSource"
                    type="radio"
                    value="cli"
                    :disabled="submitting || probing"
                    @change="resetProbe"
                  />
                  <span>{{ t('modelhub.srcCli') }}</span>
                </label>
              </div>
            </div>

            <!-- HTTP 直连源：org/model 输入 → 探测 → 文件清单勾选 -->
            <template v-if="isRemoteSource">
              <div class="field">
                <label for="mh-repo-id">{{ t('modelhub.repoIdLabel') }}</label>
                <div class="probe-row">
                  <input
                    id="mh-repo-id"
                    v-model="repoId"
                    type="text"
                    :placeholder="t('modelhub.repoIdPh')"
                    :disabled="submitting || probing"
                    @input="resetProbe"
                  />
                  <button
                    class="btn"
                    type="button"
                    :disabled="submitting || probing || !repoId.trim()"
                    @click="probeRepo"
                  >{{ probing ? t('modelhub.probing') : t('modelhub.probeBtn') }}</button>
                </div>
              </div>
              <p class="muted small">{{ t('modelhub.hintRemote') }}</p>
              <div v-if="probing" class="muted small">{{ t('modelhub.probing') }}</div>
              <div v-else-if="probeError" class="form-msg is-err">{{ probeError }}</div>
              <template v-else-if="probeResult">
                <div class="detail-badges">
                  <span class="pill pill-ok">{{ t('modelhub.probeOk', { n: probeResult.file_count ?? 0, size: fmtBytes(probeResult.total_size_bytes ?? 0) }) }}</span>
                </div>
                <div class="panel-head file-list-head">
                  <span class="muted small">{{ t('modelhub.selectedInfo', { n: selectedFiles.length, size: fmtBytes(selectedBytes) }) }}</span>
                  <span class="head-actions">
                    <button class="btn btn-small" type="button" :disabled="submitting" @click="selectAllFiles">{{ t('modelhub.selectAll') }}</button>
                    <button class="btn btn-small" type="button" :disabled="submitting" @click="selectNoFiles">{{ t('modelhub.selectNone') }}</button>
                  </span>
                </div>
                <div class="card file-check-list">
                  <label v-for="f in probeResult.files ?? []" :key="f.name" class="file-check-row">
                    <input
                      v-model="fileSel[String(f.name ?? '')]"
                      type="checkbox"
                      :disabled="submitting"
                    />
                    <span class="mono small file-name" :title="f.name ?? ''">{{ f.name ?? '—' }}</span>
                    <span class="mono small muted">{{ fmtBytes(f.size_bytes ?? 0) }}</span>
                  </label>
                </div>
              </template>
            </template>

            <!-- CLI 源：原 ModelScope Model ID 表单 -->
            <template v-else>
              <div class="field">
                <label for="mh-model-id">ModelScope Model ID</label>
                <input
                  id="mh-model-id"
                  v-model="createModelId"
                  type="text"
                  placeholder="如 Qwen/Qwen3-VL-8B-Instruct"
                  :disabled="submitting"
                />
              </div>
              <p class="muted small">
                将执行 <code class="mono">modelscope download --model &lt;id&gt; --local_dir /tank/models/&lt;name&gt;</code>
                到 <code class="mono">/tank/models/</code> 目录。{{ t('modelhub.cliOnlyHint') }}
              </p>
            </template>

            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting || probing" @click="closeCreate">取消</button>
              <button
                type="submit"
                class="btn btn-primary"
                :disabled="submitting || probing || (isRemoteSource && !probeResult)"
              >
                {{ submitting ? t('modelhub.creating') : t('modelhub.startBtn') }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== 子 Tab3 模型大厅 =================== -->
    <section v-show="activeTab === 'lobby'" class="tab-panel">
      <!-- 二级 Tab：本地大厅 / 联邦大厅（切换只换视图；搜索状态保留） -->
      <nav class="tabs sub-tabs" role="tablist" aria-label="大厅来源切换">
        <button
          class="tab sub-tab"
          :class="{ active: lobbyView === 'local' }"
          role="tab"
          :aria-selected="lobbyView === 'local'"
          @click="lobbyView = 'local'"
        >🏠 本地大厅 ({{ localLobbyEntries.length }})</button>
        <button
          class="tab sub-tab"
          :class="{ active: lobbyView === 'fed' }"
          role="tab"
          :aria-selected="lobbyView === 'fed'"
          @click="lobbyView = 'fed'"
        ><span class="fed-icon" aria-hidden="true">🌐</span> 联邦大厅 ({{ fedLobbyEntries.length }})</button>
      </nav>

      <div class="panel-head">
        <span class="panel-title">大厅分享（同模型多发布者合并为多源）</span>
        <div class="head-actions">
          <input
            v-model="lobbyQuery"
            class="lobby-search"
            type="text"
            placeholder="搜索名称/描述/标签…"
            @keyup.enter="loadLobby"
          />
          <button class="btn btn-small" :disabled="lobbyLoading" @click="loadLobby">搜索</button>
          <button class="btn btn-small btn-primary" @click="openPublish('new')">＋ 发布到大厅</button>
        </div>
      </div>

      <div v-if="lobbyError" class="error-box">{{ lobbyError }}</div>
      <div v-if="lobbyLoading && !lobbyEntries.length" class="card empty-card">加载中…</div>
      <div v-else-if="!visibleLobbyEntries.length" class="card empty-card">
        <span v-if="lobbyView === 'fed'">
          暂无联邦条目——其他 NexOS 节点发布的项目会自动出现在这里
        </span>
        <span v-else-if="lobbyQuery.trim()">无匹配的分享模型，换个关键词试试。</span>
        <span v-else>大厅暂无分享模型，点右上角「发布到大厅」把本地模型分享给其他 NexOS 节点。</span>
      </div>
      <div v-else class="lobby-grid">
        <div v-for="e in visibleLobbyEntries" :key="e.name" class="card lobby-card">
          <div class="model-card-head">
            <span class="model-name">{{ e.display_name || e.name || '—' }}</span>
            <span
              class="pill"
              :class="(e.sources ?? []).length > 1 ? 'pill-purple' : 'pill-muted'"
              :title="'多人分享同一模型 = 天然多源并行下载'"
            >{{ (e.sources ?? []).length }} 个源{{ (e.sources ?? []).length > 1 ? '·多人分享' : '' }}</span>
            <span
              v-if="isRemoteEntry(e)"
              class="tag tag-fed"
              :title="`联邦远程条目：来自 ${e.source_node} 节点`"
            >🌐 来自 {{ e.source_node }}</span>
          </div>
          <div v-if="e.display_name && e.name" class="mono small muted">{{ e.name }}</div>
          <p class="rec-desc">{{ e.description ?? '' }}</p>
          <div class="rec-tags">
            <span v-for="t in (e.tags ?? [])" :key="t" class="tag">{{ t }}</span>
            <span v-if="e.arch" class="tag tag-arch">{{ e.arch }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">大小</span>
            <span class="mono">{{ fmtBytes(e.size_bytes ?? 0) }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">文件数</span>
            <span class="mono">{{ e.file_count ?? 0 }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">下载次数</span>
            <span class="mono">{{ e.download_count ?? 0 }}</span>
          </div>
          <div class="model-row model-row-path">
            <span class="muted small">分享者</span>
            <span class="mono small model-path">{{ sharersLabel(e) }}</span>
          </div>
          <div class="model-card-actions lobby-actions">
            <button
              class="btn btn-small"
              :disabled="lobbyBusy === e.name"
              title="重新发布同名条目：刷新大小/架构/文件数快照（下载计数保留）"
              @click.stop="openPublish('refresh', e)"
            >刷新快照</button>
            <!-- 打赏分享者：多源条目按源各一枚（ref=model:<name>@<sharer>，
                 每枚累计数独立）；分享者非链上身份时后端 400 提示 -->
            <TipButton
              v-for="s in (e.sources ?? []).filter((x) => x.sharer)"
              :key="`${e.name}@${s.sharer}`"
              target-kind="lobby_entry"
              :target-ref="modelLobbyTipRef(e, s)"
              size="small"
            />
            <button
              class="btn btn-small btn-primary"
              :disabled="lobbyBusy === e.name"
              @click.stop="downloadLobby(e)"
            >
              {{ lobbyBusy === e.name ? '创建中…' : (e.sources ?? []).length > 1 ? `多源下载（${(e.sources ?? []).length} 源）` : '下载' }}
            </button>
          </div>
        </div>
      </div>

      <!-- 发布到大厅 / 刷新快照 对话框 -->
      <div v-if="showPublish" class="modal-backdrop" @click.self="closePublish">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="mh-pub-title">
          <div class="modal-head">
            <h3 id="mh-pub-title">{{ publishMode === 'refresh' ? '刷新大厅快照' : '发布模型到大厅' }}</h3>
            <button class="modal-close" type="button" :disabled="publishing" @click="closePublish">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitPublish">
            <div class="field">
              <label for="mh-pub-model">本地模型</label>
              <select
                id="mh-pub-model"
                v-model="pubForm.name"
                :disabled="publishing || publishMode === 'refresh'"
              >
                <option value="" disabled>选择本地模型…</option>
                <option v-for="m in localModels" :key="m.id" :value="String(m.id ?? '')">
                  {{ m.id }}（{{ fmtBytes(m.size_bytes ?? 0) }}）
                </option>
              </select>
            </div>

            <!-- 选中模型的档案预览（大小/arch/完整性，来自 GET /models/:name/detail） -->
            <div v-if="pubPreviewLoading" class="muted small">读取模型档案中…</div>
            <div v-else-if="pubPreviewError" class="form-msg is-err">{{ pubPreviewError }}</div>
            <div v-else-if="pubPreview" class="pub-preview">
              <span
                class="pill"
                :class="pubPreview.complete ? 'pill-ok' : 'pill-err'"
              >{{ pubPreview.complete ? '✓ 完整' : '✗ 缺分片' }}</span>
              <div v-for="row in pubPreviewRows" :key="row.label" class="model-row">
                <span class="muted small">{{ row.label }}</span>
                <span class="mono">{{ row.value }}</span>
              </div>
            </div>

            <div class="field">
              <label for="mh-pub-display">显示名（可选）</label>
              <input
                id="mh-pub-display"
                v-model="pubForm.display_name"
                type="text"
                placeholder="如 千问3-VL-8B"
                :disabled="publishing"
              />
            </div>
            <div class="field">
              <label for="mh-pub-desc">描述（可选）</label>
              <input
                id="mh-pub-desc"
                v-model="pubForm.description"
                type="text"
                placeholder="一句话介绍该模型"
                :disabled="publishing"
              />
            </div>
            <div class="field">
              <label for="mh-pub-tags">标签（可选，逗号或空格分隔）</label>
              <input
                id="mh-pub-tags"
                v-model="pubForm.tagsText"
                type="text"
                placeholder="如 vl, 8B, qwen"
                :disabled="publishing"
              />
            </div>
            <p class="muted small">
              发布会扫描本地权重档案（arch/大小/文件数）并生成本机 share 下载地址（含 token），
              供其他 NexOS 节点多源拉取。同模型重复发布 = <strong>刷新快照</strong>（下载计数保留）。
            </p>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="publishing" @click="closePublish">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="publishing">
                {{ publishing ? '发布中…' : publishMode === 'refresh' ? '刷新快照' : '发布' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== 子 Tab4 Spark 专区（SM120/NVFP4 策展） =================== -->
    <section v-show="activeTab === 'spark'" class="tab-panel">
      <!-- 醒目说明条：NVFP4/SM120 语义（非 Spark 专属，四语 i18n） -->
      <div class="spark-banner" role="note">
        <span class="spark-banner-icon" aria-hidden="true">⚡</span>
        <p>{{ t('modelhub.sparkBanner') }}</p>
      </div>

      <div class="panel-head">
        <span class="panel-title">{{ t('modelhub.sparkTitle') }}</span>
        <div class="head-actions">
          <button
            class="btn btn-small"
            :disabled="sparkLoading"
            :title="t('modelhub.sparkFastHint')"
            @click="loadSparkZone(false)"
          >{{ t('modelhub.sparkLoadFast') }}</button>
          <button
            class="btn btn-small btn-primary"
            :disabled="sparkLoading"
            @click="loadSparkZone(true)"
          ><span class="spin" :class="{ spinning: sparkLoading }" aria-hidden="true">↻</span>
            {{ t('modelhub.sparkReprobe') }}</button>
        </div>
      </div>

      <div v-if="sparkError" class="error-box">{{ sparkError }}</div>
      <div
        v-if="sparkLoading && !(sparkZone?.entries ?? []).length"
        class="card empty-card"
      >{{ t('modelhub.sparkProbing') }}</div>
      <div v-else-if="!(sparkZone?.entries ?? []).length" class="card empty-card">
        {{ t('modelhub.sparkEmpty') }}
      </div>
      <div v-else class="model-grid">
        <div
          v-for="e in sparkZone?.entries ?? []"
          :key="e.repo"
          class="card model-card spark-card"
        >
          <div class="model-card-head">
            <span class="model-name">{{ sparkShortName(e) }}</span>
            <span class="head-pills">
              <span class="pill pill-purple">{{ e.quant ?? 'NVFP4' }}</span>
              <span v-if="e.downloaded" class="pill pill-ok">{{ t('modelhub.sparkDownloaded') }}</span>
            </span>
          </div>
          <div class="model-row model-row-path">
            <span class="muted small">{{ t('modelhub.sparkRepo') }}</span>
            <span class="mono small model-path" :title="e.repo ?? ''">{{ e.repo ?? '—' }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">{{ t('modelhub.sparkParams') }}</span>
            <span class="mono">{{ e.params ?? '—' }}</span>
          </div>
          <div class="model-row">
            <span class="muted small">{{ t('modelhub.sparkOrg') }}</span>
            <span class="mono small">{{ e.org ?? '—' }}</span>
          </div>
          <!-- 源可用性徽章（魔搭 | HF 镜像；悬停见件数/大小或错误原文） -->
          <div class="model-row model-row-path">
            <span class="muted small">{{ t('modelhub.sparkSources') }}</span>
            <span class="head-pills">
              <span
                v-for="k in (['modelscope', 'hf'] as const)"
                :key="k"
                class="pill"
                :class="sparkSourcePill(sparkSourceStatus(e, k)).cls"
                :title="sparkSourceTitle(sparkSourceStatus(e, k))"
              >{{ (k === 'hf' ? t('modelhub.kindHf') : t('modelhub.kindModelscope'))
                + ' · ' + sparkSourcePill(sparkSourceStatus(e, k)).label }}</span>
            </span>
          </div>
          <p class="rec-desc">{{ e.note ?? '' }}</p>
          <div class="model-card-actions">
            <button
              class="btn btn-small btn-primary"
              :title="t('modelhub.sparkDownloadHint')"
              @click.stop="downloadSpark(e)"
            >{{ t('modelhub.sparkDownload') }}</button>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
/* 嵌入面板本体：模型管理「仓库」分组内容区（页面级 padding/标题由宿主 LlmModels
 * 提供；面板只管自身工具栏 + 子 Tab 内容流）。长内容照其余 Tab 溢出到
 * window-body 滚动（零 vh 公式）。 */
.mh-panel {
  display: flex;
  flex-direction: column;
  gap: 14px;
}
/* 工具栏：二级 sub-tabs + 面板刷新按钮同行 */
.mh-toolbar {
  display: flex; align-items: center; justify-content: space-between;
  gap: 8px; flex-wrap: wrap;
}
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

/* 二级 Tab（大厅内 本地/联邦 切换）：比一级 Tab 小一号，虚线下边线区分层级 */
.sub-tabs { gap: 2px; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.sub-tab { padding: 5px 12px; font-size: 13px; }
.sub-tab .fed-icon { color: var(--accent, #E95420); }

.tab-panel { display: flex; flex-direction: column; gap: 14px; }

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-value-sm { font-size: 18px; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* 模型卡片网格 */
.model-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 14px; }
.model-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.model-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.model-name { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); word-break: break-all; }
.model-row { display: grid; grid-template-columns: 70px 1fr; align-items: center; gap: 10px; font-size: 13px; }
.model-row-path { align-items: start; }
.model-path { word-break: break-all; }
.model-card-actions { display: flex; justify-content: flex-end; align-items: center; gap: 6px; margin-top: 4px; flex-wrap: wrap; }
/* HF 缓存条目管理提示（删除归 huggingface 工具链，卡片不提供删除按钮） */
.hf-manage-note { margin: 2px 0 0; }

/* 两步删除红字警示 */
.danger-note {
  margin: 0; padding: 8px 10px; border-radius: var(--radius-sm, 8px);
  background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.25);
  color: #b91c1c; font-size: 12.5px; line-height: 1.5;
}

/* 明细对话框 */
.modal-wide { width: min(760px, 100%); }
.detail-badges { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.arch-card { padding: 12px 16px; display: flex; flex-direction: column; gap: 10px; background: var(--bg-app, #FAFAFA); }
.arch-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 10px 16px; }
.arch-item { display: flex; flex-direction: column; gap: 2px; }
.file-name { word-break: break-all; }
.shard-file { color: var(--accent, #E95420); font-weight: 600; }

/* 推荐模型网格 */
.rec-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 14px; }
.rec-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.rec-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.rec-name { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); }
.rec-model-id { color: var(--text-muted, #5E5C5F); word-break: break-all; }
.rec-desc { margin: 0; font-size: 13px; line-height: 1.5; color: var(--text, #2B2B2B); min-height: 20px; }
.rec-tags { display: flex; flex-wrap: wrap; gap: 6px; }
.tag {
  display: inline-block; padding: 2px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; font-weight: 500; color: var(--text-muted, #5E5C5F);
  background: var(--border-soft, #F3F4F6);
}
.rec-row { display: grid; grid-template-columns: 70px 1fr; align-items: center; gap: 10px; font-size: 13px; }
.rec-card-actions { display: flex; justify-content: flex-end; margin-top: 4px; }

/* 大厅卡片流 */
.lobby-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; }
.lobby-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.lobby-search {
  padding: 5px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-size: 13px; width: 200px; font-family: inherit; background: var(--bg-card, #fff);
}
.lobby-actions { justify-content: space-between; }
.tag-arch { color: var(--accent, #E95420); background: rgba(233, 84, 32, 0.1); }
/* 联邦远程条目徽章（source_node 非空且非 local）：🌐 来源节点 */
.tag-fed { color: #0e8420; background: rgba(14, 132, 32, 0.1); font-weight: 600; }

/* 发布预览 */
.pub-preview {
  border: 1px dashed var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  padding: 10px 12px; display: flex; flex-direction: column; gap: 6px;
}

/* Spark 专区：头部醒目说明条（NVFP4/SM120 语义——非 Spark 专属） */
.spark-banner {
  display: flex; align-items: flex-start; gap: 10px;
  padding: 12px 16px; border-radius: var(--radius-md, 12px);
  background: rgba(124, 58, 237, 0.07); border: 1px solid rgba(124, 58, 237, 0.25);
}
.spark-banner p { margin: 0; font-size: 13px; line-height: 1.6; color: var(--text, #2B2B2B); }
.spark-banner-icon { font-size: 18px; line-height: 1.3; color: #7c3aed; }
.spark-card .pill-purple { font-weight: 600; letter-spacing: 0.3px; }

/* 多源下载任务卡片 */
.multi-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(340px, 1fr)); gap: 14px; }
.multi-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 8px; }
.head-pills { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.recent-files {
  list-style: none; margin: 0; padding: 8px 10px; background: var(--bg-app, #FAFAFA);
  border-radius: var(--radius-sm, 8px); display: flex; flex-direction: column; gap: 4px;
}
.recent-files li { display: flex; justify-content: space-between; align-items: baseline; gap: 8px; }
.rf-done { color: #15803d; }
.rf-failed { color: #b91c1c; }

/* 进度条 */
.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { display: block; height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-ok { background: #0E8420; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
/* HF hub 缓存来源徽章（🤗 黄底，与本地/导入条目区分） */
.pill-hf { color: #92400e; background: #fef3c7; }
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

.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select, .field textarea {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
}
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

/* 新建下载向导（源选择 + 探测 + 文件清单勾选） */
.src-radios { display: flex; flex-wrap: wrap; gap: 8px; }
.src-radio {
  display: inline-flex; align-items: center; gap: 6px; padding: 6px 12px;
  border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-pill, 20px);
  font-size: 13px; cursor: pointer; background: var(--bg-card, #fff);
}
.src-radio:has(input:checked) { border-color: var(--accent, #E95420); background: rgba(233, 84, 32, 0.06); }
.probe-row { display: flex; gap: 8px; align-items: stretch; }
.probe-row input { flex: 1; }
.file-list-head { margin: 0; padding: 0; }
.file-check-list {
  max-height: 300px; overflow: auto; padding: 6px 10px;
  display: flex; flex-direction: column; gap: 2px;
}
.file-check-row {
  display: grid; grid-template-columns: auto 1fr auto; align-items: center; gap: 8px;
  padding: 3px 4px; border-radius: var(--radius-sm, 8px); cursor: pointer; font-size: 13px;
}
.file-check-row:hover { background: var(--border-soft, #F3F4F6); }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.mono { font-family: var(--mono); }
code.mono { background: var(--border-soft, #F3F4F6); padding: 1px 4px; border-radius: 4px; font-size: 12px; }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(480px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }

@media (max-width: 640px) {
  .model-row, .rec-row { grid-template-columns: 1fr; gap: 2px; }
}
</style>
