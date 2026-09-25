<script setup lang="ts">
// =============================================================================
// LlmModels.vue —— 模型管理（vLLM 推理服务 + 媒体生成 + 模型仓库）
//
// 顶部两级 Tab（2026-09-03 重构）：一级组「推理｜仓库｜诊断」——
//   推理（6 子 Tab）：实例管理 / 推理环境 / 配方库 / 生成 / 对话 / 外部 API
//   仓库（4 子 Tab，ModelHubPanel 组件承载）：本地模型 / 在线下载 / 模型大厅 /
//     Spark 专区——原独立「模型仓库」桌面应用（/modelhub）并入本页（照
//     直播面板（LiveView→LivePanel，现 apps/streaming 包）/ modelchat 并入先例；旧路由重定向 /llm?tab=repo）
//   诊断（3 子 Tab）：实例监控 / GPU 监控 / 参数说明
// 后端：/api/v1/llm/* （LlmRouteHandler，已在线）
//       /api/v1/media/image|video* （MediaGenRouteHandler：sd-turbo 文生图
//       + 视频任务框架；写需系统 admin token，读公开）
//       /api/v1/models/* （ModelHubPanel 内，模型文件下载面）
//
// 「推理环境」Tab（2026-08-31 建）：vLLM Python venv 的创建/更新/默认切换
// （后端 handlers/llm_envs.rs，uv 管理 ~/llm-envs/<name>/，202 异步任务 +
// 2s 轮询日志尾）；创建/更新为分钟级长任务，环境卡列表 + 任务面板全部真实
// 注册表/任务数据。本 Tab 文案走 i18n（四语全量）；实例创建对话框新增可选
// 「推理环境」下拉（env_name）。
// 2026-09-02 新增渠道（channel）：创建/更新表单可选 stable（默认，零变化）或
// nightly（预置示例 uv pip install -U vllm --torch-backend=auto
// --extra-index-url https://wheels.vllm.ai/nightly，恒最新不钉版本——选
// nightly 时版本输入禁用）；表单下方与更新对话框均展示与后端 argv 同构的
// 命令预览；环境卡 nightly 蓝色渠道徽章。
//
// 「对话」Tab（2026-09-02 统一化重构，合并旧「对话」+「推理测试」两个 Tab）：
// 顶部**目标选择器**三组——①本地实例（直连实例端口 /v1/chat/completions
// SSE 流式，host 用 window.location.hostname）②外部 API（llm_external_apis
// 登记下拉，POST /llm/external-apis/:id/chat SSE 流式透传；下拉末项
// 「＋ 手动输入」展开行内表单——「临时对话」内存目标浏览器直连该 URL 的
// /chat/completions SSE（同实例直连模式，跨网不可达报错引导联邦导入/中继），
// 「登记并对话」POST external-apis 后切换（via_node 留空=直连语义）；
// 2026-09-03 手动输入上线）③联邦大厅
// （GET /api/v1/api-market?scope=fed 条目下拉，选中即一键导入为外部 API
// 登记——api_key 仅明文视角可带，脱敏态提示手动补填——成功后自动切换）。
// 统一气泡流式渲染（parseDeltaFields：content + reasoning 思考段折叠，
// vLLM 0.28 `reasoning` / 0.27 `reasoning_content` 双键兼容）、max_tokens
// 可调（默认 4096，Qwen 思考模型提示保留）、历史按目标存 localStorage。
// 布局照 IM Chat.vue 成熟模式：面板 flex 填满 + 消息区滚动 + composer 钉底
// （shrink 0，无 100vh 公式）。旧「推理测试」Tab（POST /llm/instances/:id/chat
// 非流式）整体移除；外部 API Tab 的精简聊天窗同日移除（统一对话已覆盖）。
//
// 「配方库」Tab（2026-08-29 建，2026-08-30 改树状视图，2026-09-02 缓存改
// 手动刷新）：从 vLLM 官方配方站 recipes.vllm.ai 导入部署配方（外网经服务端
// 烘焙代理，浏览器不直连）；目录为官方同款树——左侧「提供方（可折叠父节点，
// 带数量徽章）→ 模型（子节点）」+ 搜索框过滤树（命中组自动展开），点击模型
// 子节点右侧速览并弹出完整配方详情；「存为本地配方」暂存 localStorage。
// 后端**常驻缓存（无 TTL）**：打开 Tab 只读缓存秒回零外呼，「刷新目录」
// 按钮 ?refresh=1 强制重拉并更新缓存（响应信封带 cached_at 上次刷新时间）。
//
// 「实例监控」Tab（2026-08-30 抽组件）：实例级 vLLM /metrics + 网关可路由模型
// 聚合（GET /api/v1/llm/gateway/models），UI/逻辑已抽成可复用组件
// components/InstanceMonitor.vue——本页原位引用（v-if 挂载，轮询随 Tab 激活），
// ApiGateway.vue「实例监控」Tab 复用同一组件（数据自包含，宿主零接线）。
// 「GPU 监控」Tab 与其数据不重叠：GPU 监控 = 设备级显存/使用率（/api/v1/llm/gpu），
// 实例监控 = 实例级 vLLM 指标 + 可路由模型。
//
// 设计：Ubuntu Yaru 风格 .card / .page-head，统计卡 + GPU 卡 + 表格 + 对话框，三态加载。
// 通用化：GPU 信息动态探测，无 GPU 时友好降级，vllm 未安装时 error 状态不报错。
// =============================================================================
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue';
import { marked } from 'marked';
import { useI18n } from 'vue-i18n';
import { useRoute } from 'vue-router';
import AppIcon from '@/components/AppIcon.vue';
import DataTable from '@/components/DataTable.vue';
import InstanceMonitor from '@/components/InstanceMonitor.vue';
import ModelHubPanel from '@/components/ModelHubPanel.vue';
import type { Column } from '@/components/data-table';
import { endpoints, getApiToken } from '@/api/client';
import type {
  LlmEnvRow,
  LlmEnvTask,
  LlmExternalApi,
  LlmExternalTestResult,
  LlmInstanceLog,
  LlmRecipeCatalogItem,
  LlmRecipeDetail,
  LlmRecipeSaved,
} from '@/api/client';
import { copyText } from '@/utils/clipboard';
import { fedUpgradeDecision } from '@/utils/fedImport';

/** 「推理环境」Tab 文案走 i18n（zh-CN/zh-TW/en-US/ja-JP 四语全量）；本页其余
 * Tab 沿用既有硬编码中文口径，不在此一并迁移。 */
const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
type InstanceStatus = 'stopped' | 'starting' | 'running' | 'error' | string;

interface VllmConfig {
  host?: string;
  port?: number;
  tensor_parallel_size?: number;
  gpu_memory_utilization?: number;
  max_model_len?: number;
  quantization?: string | null;
  dtype?: string;
  served_model_name?: string | null;
  trust_remote_code?: boolean;
  extra_args?: string[];
  [k: string]: unknown;
}
interface HealthInfo {
  alive?: boolean;
  model_loaded?: boolean;
  models?: string[];
  checked_at?: string;
}
interface ModelInstance {
  id?: string;
  name?: string;
  model?: string;
  source_type?: 'huggingface' | 'local' | string;
  port?: number;
  status?: InstanceStatus;
  pid?: number | null;
  config?: VllmConfig;
  health?: HealthInfo | null;
  created_at?: string;
  error?: string | null;
  /** 有效启动命令（后端注入恒有值：真实 argv / 按 config 构造；接入说明「启动参数」块）。 */
  launch_command?: string | null;
  [k: string]: unknown;
}
interface GpuDevice {
  index?: number;
  name?: string;
  /** 独立显存 MiB；统一内存架构（GB10/Jetson，nvidia-smi 报 [N/A]）→ null。 */
  memory_total_mib?: number | null;
  memory_used_mib?: number | null;
  memory_free_mib?: number | null;
  /** 统一内存架构标记（CPU/GPU 共享 LPDDR5x 池，显存字段 N/A 时 true）。 */
  unified_memory?: boolean;
  /** 统一内存池总量 MiB（/proc/meminfo MemTotal；unified 时后端填）。 */
  unified_memory_total_mib?: number | null;
  unified_memory_used_mib?: number | null;
  unified_memory_free_mib?: number | null;
  utilization_pct?: number | null;
}
interface GpuInfo {
  available?: boolean;
  backend?: string;
  devices?: GpuDevice[];
}
interface LlmStats {
  instances_total?: number;
  running?: number;
  stopped?: number;
  gpu_available?: boolean;
  gpu_devices?: number;
}
/** POST /api/v1/media/image 成功响应（png_base64 直接拼 data URL 渲染/下载）。 */
interface ImageGenResult {
  id: string;
  png_base64: string;
  width: number;
  height: number;
  elapsed_ms: number;
  file_path: string;
}
/** GET /api/v1/media/image/recent 元素（无图）。 */
interface ImageRecentItem {
  id: string;
  prompt_summary: string;
  width: number;
  height: number;
  steps: number;
  elapsed_ms: number;
  created_at: string;
}
/** 视频生成任务（queued→processing→completed|failed；当前后端创建即 failed 附指引）。 */
interface VideoTask {
  id: string;
  prompt: string;
  duration_secs: number;
  backend: string;
  status: string;
  video_url?: string | null;
  error?: string | null;
  created_at: string;
}

// —— 「实例监控」Tab ——（UI/逻辑已抽至 components/InstanceMonitor.vue，
// 本文件不再持有 metrics 类型与轮询状态）

// —— 「对话」Tab（统一目标选择器：本地实例 / 外部 API / 联邦大厅导入）——
/** OpenAI 兼容 messages 元素。 */
interface McChatMessage {
  role: 'user' | 'assistant' | 'system';
  content: string;
}
/** 对话气泡：UI 渲染用（含流式/思考段/错误标记）。 */
interface McBubble {
  id: string;
  role: 'user' | 'assistant';
  content: string;
  /** assistant 思考段（vLLM `reasoning` / `reasoning_content` 流式增量累计；
   *  折叠展示，Qwen 等思考模型非空）。 */
  reasoning?: string;
  /** assistant 流式生成中 */
  streaming?: boolean;
  /** 该气泡是否为错误降级提示 */
  error?: boolean;
}

// =============================================================================
// Tab 状态（2026-09-03 两级化：一级组「推理｜仓库｜诊断」——14 个平铺 Tab
// 挤一行的问题用分组收拢；「仓库」组由 ModelHubPanel 组件自带二级 Tab
// （本地模型/在线下载/模型大厅/Spark 专区），推理/诊断组的二级 Tab 本页渲染）
// =============================================================================
type TabGroup = 'inference' | 'repo' | 'diag';
type TabKey =
  | 'instances'
  | 'envs'
  | 'recipes'
  | 'generate'
  | 'modelchat'
  | 'metrics'
  | 'gpu'
  | 'external'
  | 'params';
/** 一级组（label 走 i18n——新增 UI 文案四语全量）。 */
const tabGroups: { key: TabGroup; label: string }[] = [
  { key: 'inference', label: t('llmTab.groupInference') },
  { key: 'repo', label: t('llmTab.groupRepo') },
  { key: 'diag', label: t('llmTab.groupDiag') },
];
/** 二级 Tab 定义（推理/诊断两组；仓库组的二级 Tab 在 ModelHubPanel 内）。 */
const groupTabs: Record<Exclude<TabGroup, 'repo'>, { key: TabKey; label: string }[]> = {
  inference: [
    { key: 'instances', label: '实例管理' },
    { key: 'envs', label: t('llmEnv.tab') },
    { key: 'recipes', label: '配方库' },
    { key: 'generate', label: '生成' },
    { key: 'modelchat', label: t('llmChat.tab') },
    { key: 'external', label: t('llmExt.tab') },
  ],
  diag: [
    { key: 'metrics', label: '实例监控' },
    { key: 'gpu', label: 'GPU 监控' },
    { key: 'params', label: '参数说明' },
  ],
};
/** TabKey → 所属组（深链 ?tab=<key> 反查组用）。 */
const TAB_GROUP_OF: Record<TabKey, TabGroup> = {
  instances: 'inference',
  envs: 'inference',
  recipes: 'inference',
  generate: 'inference',
  modelchat: 'inference',
  external: 'inference',
  metrics: 'diag',
  gpu: 'diag',
  params: 'diag',
};

// 深链支持（?tab= 先例：原流媒体中心，现 apps/streaming 应用包 ?tab=live）：?tab=repo 直达仓库分组
// （旧 /modelhub 路由重定向到这里）；?tab=<TabKey> 直达对应子 Tab；非法/缺省
// 回落默认（实例管理）。仅首载读一次，后续切换不再回写 query。
const route = useRoute();
const initialTab = (route.query.tab as string) || '';
const initialTabGroup: TabGroup | undefined = TAB_GROUP_OF[initialTab as TabKey];
const activeGroup = ref<TabGroup>(initialTab === 'repo' ? 'repo' : (initialTabGroup ?? 'inference'));
const activeTab = ref<TabKey>(
  initialTabGroup && initialTab !== 'repo' ? (initialTab as TabKey) : 'instances',
);

/** 一级组切换：推理/诊断落到该组首个子 Tab；仓库组子 Tab 由 ModelHubPanel 自理。 */
function switchGroup(group: TabGroup): void {
  if (activeGroup.value === group) return;
  activeGroup.value = group;
  if (group !== 'repo') {
    activeTab.value = groupTabs[group][0].key;
  }
}

/** 当前组的二级 Tab 列表（仓库组为空数组——其二级 Tab 在 ModelHubPanel 内）。 */
const activeGroupTabs = computed(() =>
  activeGroup.value === 'repo' ? [] : groupTabs[activeGroup.value],
);

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/502/.test(m)) return '推理失败：实例未运行或 vllm 未安装';
  return m;
}

// =============================================================================
// GPU 信息
// =============================================================================
const gpu = ref<GpuInfo>({});
const gpuLoading = ref(false);
const gpuError = ref('');

async function loadGpu(): Promise<void> {
  gpuLoading.value = true;
  gpuError.value = '';
  try {
    const raw = await endpoints.llmGpu();
    gpu.value = (raw as GpuInfo) ?? {};
  } catch (e) {
    gpu.value = {};
    gpuError.value = friendlyError(e);
  } finally {
    gpuLoading.value = false;
  }
}

// =============================================================================
// 统计
// =============================================================================
const stats = ref<LlmStats>({});
const statsLoading = ref(false);

async function loadStats(): Promise<void> {
  statsLoading.value = true;
  try {
    const raw = await endpoints.llmStats();
    stats.value = (raw as LlmStats) ?? {};
  } catch {
    stats.value = {};
  } finally {
    statsLoading.value = false;
  }
}

// =============================================================================
// Tab「推理环境」：vLLM Python venv 的创建/更新/默认切换（2026-08-31）
//
// 数据面（全部真实注册表/任务数据，无演示态）：
//   GET  /api/v1/llm/environments            环境列表 + default_name
//   POST /api/v1/llm/environments            创建 → 202 {task_id}（后台 uv 任务）
//   POST /api/v1/llm/environments/:name/update | /default、DELETE /:name
//   GET  /api/v1/llm/environments/tasks[/:id] 任务列表/详情（日志尾）
// 创建/更新为分钟级长任务：提交后轮询任务详情 2s，显示日志尾自动滚动；
// 任务完成/失败即刷新环境列表。与 NEXOS_LLM_METRICS_SIMULATE 模拟链路完全无关。
// =============================================================================
const envs = ref<LlmEnvRow[]>([]);
const envsDefaultName = ref<string | null>(null);
const envsLoading = ref(false);
const envsError = ref('');
/** 懒加载标记：首次切入本 Tab 才拉列表。 */
let envsLoaded = false;

// —— 创建表单 ——
const envForm = ref({
  name: '',
  python_version: '3.12',
  vllm_version: '',
  channel: 'stable',
});
const envCreating = ref(false);
/** Python 版本下拉选项（uv 会自动下载对应 CPython）。 */
const envPythonOptions = ['3.10', '3.11', '3.12', '3.13'];
/** vLLM nightly 轮子源（渠道=nightly 时的主源；与后端 llm_envs.rs 常量一致）。 */
const VLLM_NIGHTLY_INDEX = 'https://wheels.vllm.ai/nightly';
/** 创建表单是否选了 nightly 渠道（版本输入禁用 + 命令预览换形态）。 */
const envCreateNightly = computed(() => envForm.value.channel === 'nightly');

// —— 更新对话框 ——
const showEnvUpdate = ref(false);
const envUpdateTarget = ref<LlmEnvRow | null>(null);
const envUpdateVersion = ref('');
/** 更新对话框渠道（默认带出该行当前渠道；可 nightly↔stable 切换重装）。 */
const envUpdateChannel = ref('stable');
const envUpdating = ref(false);
/** 更新对话框是否 nightly。 */
const envUpdateNightly = computed(() => envUpdateChannel.value === 'nightly');

// —— 单环境操作 busy 标记（按环境名）——
const envBusyName = ref<string>('');

// —— 任务面板：轮询进行中任务 + 日志尾 ——
const envTasks = ref<LlmEnvTask[]>([]);
/** 当前盯梢的任务详情（含日志；优先最新的 running 任务）。 */
const envActiveTask = ref<LlmEnvTask | null>(null);
const envLogBox = ref<HTMLElement | null>(null);
let envPollTimer: ReturnType<typeof setInterval> | null = null;

/** 任务是否仍在执行（running）。 */
const envHasRunningTask = computed(() => envTasks.value.some((t) => t.status === 'running'));

async function loadEnvs(): Promise<void> {
  envsLoading.value = true;
  envsError.value = '';
  try {
    const raw = await endpoints.llmEnvironments();
    envs.value = Array.isArray(raw.environments) ? raw.environments : [];
    envsDefaultName.value = raw.default_name ?? null;
  } catch (e) {
    envs.value = [];
    envsDefaultName.value = null;
    envsError.value = friendlyError(e);
  } finally {
    envsLoading.value = false;
  }
}

/** 拉任务列表；盯梢最新 running 任务（无 running 时保留最近一个的终态）。 */
async function refreshEnvTasks(): Promise<void> {
  try {
    const raw = await endpoints.llmEnvTasks();
    envTasks.value = Array.isArray(raw.tasks) ? raw.tasks : [];
    const target =
      envTasks.value.find((t) => t.status === 'running') ?? envTasks.value[0] ?? null;
    if (target) {
      await loadEnvTaskDetail(target.id);
    } else {
      envActiveTask.value = null;
    }
  } catch {
    // 任务面板拉失败不打断环境列表（面板显示空即可）
    envTasks.value = [];
  }
}

/** 拉单任务详情（含日志尾）并滚到底。 */
async function loadEnvTaskDetail(taskId: string): Promise<void> {
  try {
    const detail = await endpoints.llmEnvTask(taskId);
    envActiveTask.value = detail;
    await nextTick();
    if (envLogBox.value) envLogBox.value.scrollTop = envLogBox.value.scrollHeight;
  } catch {
    // 单任务拉失败保留旧内容（下轮轮询重试）
  }
}

/** 启停轮询：有 running 任务（或强制）才开 2s 定时器；Tab 离开/卸载即停。 */
function startEnvPolling(force = false): void {
  if (envPollTimer != null) return;
  if (!force && !envHasRunningTask.value) return;
  void refreshEnvTasks();
  envPollTimer = setInterval(() => {
    if (!envHasRunningTask.value) {
      // 全部任务终态：停轮询 + 刷新环境列表（状态已落库）
      stopEnvPolling();
      void loadEnvs();
      return;
    }
    void refreshEnvTasks();
  }, 2000);
}

function stopEnvPolling(): void {
  if (envPollTimer != null) {
    clearInterval(envPollTimer);
    envPollTimer = null;
  }
}

/** 提交创建：202 后任务即入面板轮询。nightly 渠道不发版本（后端恒装最新）。 */
async function submitEnvCreate(): Promise<void> {
  const name = envForm.value.name.trim();
  if (!name) {
    msg.value = { kind: 'err', text: t('llmEnv.errNameRequired') };
    return;
  }
  envCreating.value = true;
  msg.value = null;
  try {
    const nightly = envForm.value.channel === 'nightly';
    const vllm = envForm.value.vllm_version.trim();
    await endpoints.llmEnvCreate({
      name,
      python_version: envForm.value.python_version,
      vllm_version: nightly ? undefined : vllm || undefined,
      channel: envForm.value.channel,
    });
    envForm.value = { name: '', python_version: '3.12', vllm_version: '', channel: 'stable' };
    await Promise.all([loadEnvs(), refreshEnvTasks()]);
    startEnvPolling(true);
    msg.value = { kind: 'ok', text: t('llmEnv.createSubmitted') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmEnv.createFailed') + friendlyError(e) };
  } finally {
    envCreating.value = false;
  }
}

/** venv 根目录（从任一环境行 path 剥掉 /<name> 得真实根；无行时按默认 ~/llm-envs）。 */
const envRootDir = computed(() => {
  const first = envs.value[0];
  if (first?.path && first.name) {
    const idx = first.path.lastIndexOf('/' + first.name);
    if (idx > 0) return first.path.slice(0, idx);
  }
  return '~/llm-envs';
});

/** 安装命令预览（与后端 llm_envs.rs pip_install_argv 同构——所见即将执行）。
 * nightly：用户点名示例 uv pip install -U vllm --torch-backend=auto
 *   --extra-index-url https://wheels.vllm.ai/nightly（外加 --python 定位环境）；
 * stable：uv pip install --python <py> vllm[==<ver>]。 */
function envInstallPreview(channel: string, name: string, version: string): string {
  const py = `${envRootDir.value}/${name || '<name>'}/bin/python`;
  if (channel === 'nightly') {
    return `uv pip install --python ${py} -U vllm --torch-backend=auto --extra-index-url ${VLLM_NIGHTLY_INDEX}`;
  }
  const v = version.trim();
  return `uv pip install --python ${py} ${!v || v === 'latest' ? 'vllm' : `vllm==${v}`}`;
}

function openEnvUpdate(row: LlmEnvRow): void {
  envUpdateTarget.value = row;
  envUpdateVersion.value = '';
  envUpdateChannel.value = row.channel === 'nightly' ? 'nightly' : 'stable';
  msg.value = null;
  showEnvUpdate.value = true;
}

/** 更新对话框命令预览（渠道/版本联动）。 */
const envUpdatePreview = computed(() =>
  envInstallPreview(
    envUpdateChannel.value,
    envUpdateTarget.value?.name ?? '',
    envUpdateVersion.value,
  ),
);

async function submitEnvUpdate(): Promise<void> {
  const target = envUpdateTarget.value;
  if (!target) return;
  envUpdating.value = true;
  try {
    const v = envUpdateVersion.value.trim();
    const nightly = envUpdateChannel.value === 'nightly';
    await endpoints.llmEnvUpdate(target.name, nightly ? 'latest' : v || 'latest', envUpdateChannel.value);
    showEnvUpdate.value = false;
    await Promise.all([loadEnvs(), refreshEnvTasks()]);
    startEnvPolling(true);
    msg.value = { kind: 'ok', text: t('llmEnv.updateSubmitted', { name: target.name }) };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmEnv.updateFailed') + friendlyError(e) };
  } finally {
    envUpdating.value = false;
  }
}

async function setEnvDefault(row: LlmEnvRow): Promise<void> {
  envBusyName.value = row.name;
  msg.value = null;
  try {
    await endpoints.llmEnvSetDefault(row.name);
    await loadEnvs();
    msg.value = { kind: 'ok', text: t('llmEnv.defaultDone', { name: row.name }) };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmEnv.defaultFailed') + friendlyError(e) };
  } finally {
    envBusyName.value = '';
  }
}

async function removeEnv(row: LlmEnvRow): Promise<void> {
  if (!window.confirm(t('llmEnv.deleteConfirm', { name: row.name }))) return;
  envBusyName.value = row.name;
  msg.value = null;
  try {
    await endpoints.llmEnvDelete(row.name);
    await loadEnvs();
    msg.value = { kind: 'ok', text: t('llmEnv.deleteDone', { name: row.name }) };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmEnv.deleteFailed') + friendlyError(e) };
  } finally {
    envBusyName.value = '';
  }
}

/** 环境状态 → 徽标样式（ready 绿 / creating|updating 蓝 / error 红）。 */
function envStatusClass(s: string): string {
  switch (s) {
    case 'ready':
      return 'pill-ok';
    case 'creating':
    case 'updating':
      return 'pill-blue';
    case 'error':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}
function envStatusLabel(s: string): string {
  switch (s) {
    case 'ready':
      return t('llmEnv.statusReady');
    case 'creating':
      return t('llmEnv.statusCreating');
    case 'updating':
      return t('llmEnv.statusUpdating');
    case 'error':
      return t('llmEnv.statusError');
    default:
      return s || '—';
  }
}
/** 环境任务状态 → 文案。 */
function envTaskStatusLabel(s: string): string {
  switch (s) {
    case 'running':
      return t('llmEnv.taskRunning');
    case 'done':
      return t('llmEnv.taskDone');
    case 'error':
      return t('llmEnv.taskError');
    default:
      return s || '—';
  }
}
/** 目标版本与已装版本是否不一致（latest 目标不比对——始终追新）。 */
function envVersionMismatch(row: LlmEnvRow): boolean {
  const req = (row.vllm_version_requested ?? '').trim();
  const got = (row.vllm_version_installed ?? '').trim();
  if (!req || !got || req === 'latest') return false;
  return req !== got;
}
/** 字节数 → 人类可读。 */
function fmtBytes(n?: number): string {
  if (n == null || n <= 0) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
  return `${(n / 1024 ** 3).toFixed(2)} GiB`;
}
/** Unix epoch 秒 → 本地时间串。 */
function fmtEpoch(sec?: number | null): string {
  if (!sec) return '—';
  try {
    return new Date(sec * 1000).toLocaleString('zh-CN');
  } catch {
    return String(sec);
  }
}

// =============================================================================
// Tab1：实例管理
// =============================================================================
const instances = ref<ModelInstance[]>([]);
const instancesLoading = ref(false);
const instancesError = ref('');

async function loadInstances(): Promise<void> {
  instancesLoading.value = true;
  instancesError.value = '';
  try {
    const raw = await endpoints.llmInstances();
    instances.value = Array.isArray(raw) ? (raw as ModelInstance[]) : [];
  } catch (e) {
    instances.value = [];
    instancesError.value = friendlyError(e);
  } finally {
    instancesLoading.value = false;
  }
}

const instColumns: Column<ModelInstance>[] = [
  { key: 'name', title: '名称', sortable: true },
  { key: 'model', title: '模型' },
  { key: 'source_type', title: '来源', width: '110px' },
  { key: 'port', title: '端口', width: '80px' },
  { key: 'status', title: '状态', width: '100px' },
  { key: 'pid', title: 'PID', width: '80px' },
  { key: 'actions', title: '操作', width: '400px', align: 'right' },
];

const busyId = ref<string>('');

async function startInst(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.startLlmInstance(id);
    await loadInstances();
    msg.value = { kind: 'ok', text: '已发出启动命令（vllm 后台拉起中）' };
  } catch (e) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function stopInst(id: string): Promise<void> {
  if (!window.confirm('确定停止该实例？（将 kill 进程）')) return;
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.stopLlmInstance(id);
    await loadInstances();
    msg.value = { kind: 'ok', text: '已停止' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function healthInst(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    const raw = await endpoints.checkLlmHealth(id);
    const h = raw as HealthInfo;
    await loadInstances();
    if (h.alive) {
      msg.value = { kind: 'ok', text: '健康探测：存活，已加载模型 ' + (h.models?.length ?? 0) + ' 个' };
    } else {
      msg.value = { kind: 'err', text: '健康探测：实例未存活（vllm 未运行或仍在加载）' };
    }
  } catch (e) {
    msg.value = { kind: 'err', text: '健康探测失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function removeInst(id: string): Promise<void> {
  if (!window.confirm('确定删除该实例？')) return;
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteLlmInstance(id);
    await loadInstances();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 实例拉起日志抽屉（2026-08-31）：GET /api/v1/llm/instances/:id/log
//
// 每实例一个日志文件（<NEXOS_LLM_SPAWN_DIR>/llm-vllm-<id>.log）；抽屉 2s 轮询
// 拉尾 N 行（拉取式 follow——后端 follow 参数当前同构响应），自动滚底；
// 「暂停跟随」停轮询+停滚底（翻历史不被拽走），「清空」只清屏（不动文件）。
// starting 状态时实例行「日志」按钮高亮（看启动进度）。
// =============================================================================
const logDrawerOpen = ref(false);
const logInstanceId = ref('');
const logLines = ref<string[]>([]);
const logFile = ref('');
const logStatus = ref('');
const logError = ref('');
/** 是否跟随（true = 2s 轮询 + 自动滚底）。 */
const logFollow = ref(true);
const logBox = ref<HTMLElement | null>(null);
let logTimer: ReturnType<typeof setInterval> | null = null;
/** 单次拉回行数（后端默认 200、上限 1000；抽屉取 300 兼顾上下文）。 */
const LOG_TAIL_LINES = 300;

/** 拉一次日志尾；跟随时滚到底。 */
async function fetchInstLog(): Promise<void> {
  if (!logInstanceId.value) return;
  try {
    const res: LlmInstanceLog = await endpoints.llmInstanceLog(logInstanceId.value, {
      tail: LOG_TAIL_LINES,
      follow: logFollow.value,
    });
    logLines.value = res.lines ?? [];
    logFile.value = res.file ?? '';
    logStatus.value = res.status ?? '';
    logError.value = '';
    if (logFollow.value) {
      await nextTick();
      if (logBox.value) logBox.value.scrollTop = logBox.value.scrollHeight;
    }
  } catch (e) {
    // 404（未拉起过/实例没了）等：显示原因，保留旧内容
    logError.value = friendlyError(e);
  }
}

function startLogPolling(): void {
  if (logTimer != null) return;
  logTimer = setInterval(() => {
    if (!logFollow.value || !logDrawerOpen.value) return;
    void fetchInstLog();
  }, 2000);
}

function stopLogPolling(): void {
  if (logTimer != null) {
    clearInterval(logTimer);
    logTimer = null;
  }
}

function openInstLog(id: string): void {
  logInstanceId.value = id;
  logLines.value = [];
  logFile.value = '';
  logStatus.value = '';
  logError.value = '';
  logFollow.value = true;
  logDrawerOpen.value = true;
  void fetchInstLog();
  startLogPolling();
}

function closeInstLog(): void {
  logDrawerOpen.value = false;
  logInstanceId.value = '';
  stopLogPolling();
}

/** 清屏：只清本地显示，不动服务端日志文件。 */
function clearInstLog(): void {
  logLines.value = [];
}

/** 抽屉内状态徽标（starting 时高亮——正在加载模型，看日志最常用）。 */
function logStatusClass(s: string): string {
  switch (s) {
    case 'running':
      return 'pill-ok';
    case 'starting':
      return 'pill-blue';
    case 'error':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}

// =============================================================================
// 「接入说明」弹窗（2026-08-31）：实例级接入速查
//
// 用户原话需求："实例管理里没有接入说明，比如模型的名，模型的上下文等"。
// 三段式：① 直连 vLLM（OpenAI 兼容）/ ② 经网关调用 / ③ 实例参数速览——
// 全部内容按实例真实数据动态渲染（served_model_name / max_model_len / port…），
// 零硬编码模型名端口；复制按钮复用 utils/clipboard 的 copyText（含 HTTP 非安全
// 上下文 execCommand 降级）。历史坑显式警示：直连调用 model 参数必须用
// served_model_name（vLLM 设了 --served-model-name 后只认它——曾因传模型
// 路径 404）。
// =============================================================================
const showAccess = ref(false);
/** 弹窗绑定实例（打开时快照；实例列表刷新不影响已打开的面板内容）。 */
const accessInst = ref<ModelInstance | null>(null);
/** 刚复制成功的块 key（对应按钮 ✓ 反馈 1.5s；'' = 无）。 */
const accessCopied = ref('');
let accessCopyTimer: ReturnType<typeof setTimeout> | undefined;

function openAccess(inst: ModelInstance): void {
  accessInst.value = inst;
  accessCopied.value = '';
  showAccess.value = true;
}

function closeAccess(): void {
  showAccess.value = false;
}

/** 直连 Base URL：http://<当前访问主机名>:<实例port>/v1。
 *  host 用浏览器当前访问的主机名动态拼出——本机访问时可手写 127.0.0.1，
 *  跨机调用即当前访问所用节点 IP（实例监听 0.0.0.0，可达）。 */
const accessBaseUrl = computed(() => {
  const port = accessInst.value?.port;
  if (!port) return '';
  return `http://${window.location.hostname}:${port}/v1`;
});

/** 调用用的模型名：served_model_name 优先（设了 --served-model-name 时 vLLM
 *  只认它）；未设时回退模型路径（vLLM 此时接受路径）。 */
const accessModelName = computed(() => {
  const smn = accessInst.value?.config?.served_model_name;
  if (typeof smn === 'string' && smn.trim()) return smn;
  return accessInst.value?.model ?? '';
});

/** 是否以 served_model_name 命名（false = 回退模型路径，警示文案切换）。 */
const accessHasServedName = computed(() => {
  const smn = accessInst.value?.config?.served_model_name;
  return typeof smn === 'string' && !!smn.trim();
});

/** 实例鉴权密钥（按数据分支，不硬编码猜）：config.api_key（后端暂无此字段，
 *  将来透出即自动生效）→ extra_args 里的 --api-key <值>（当前唯一启用途径，
 *  见 build_vllm_serve_cmd：spawn 不主动加 --api-key）→ null = 未启用。 */
const accessApiKey = computed<string | null>(() => {
  const cfg = accessInst.value?.config;
  if (!cfg) return null;
  const k = (cfg as { api_key?: unknown }).api_key;
  if (typeof k === 'string' && k.trim()) return k;
  const args = cfg.extra_args ?? [];
  const i = args.findIndex((a) => a === '--api-key');
  if (i >= 0 && args[i + 1] && !args[i + 1].startsWith('--')) return args[i + 1];
  return null;
});

/** ① 直连完整 curl 示例（model 用实例真实模型名；启用鉴权时加 Bearer 头行）。 */
const accessCurl = computed(() => {
  const base = accessBaseUrl.value;
  const model = accessModelName.value;
  if (!base || !model) return '';
  const lines = [`curl ${base}/chat/completions \\`];
  if (accessApiKey.value) {
    lines.push(`  -H 'Authorization: Bearer ${accessApiKey.value}' \\`);
  }
  lines.push(`  -H 'Content-Type: application/json' \\`);
  lines.push(`  -d '{`);
  lines.push(`    "model": "${model}",`);
  lines.push(`    "messages": [{"role": "user", "content": "你好"}]`);
  lines.push(`  }'`);
  return lines.join('\n');
});

/** ② 网关调用地址：POST <当前 origin>/api/v1/llm/instances/<id>/chat。
 *  端口随浏览器实际访问 origin 动态得出（Web UI 与 API 同源，默认 8558）。 */
const accessGatewayUrl = computed(() => {
  const id = accessInst.value?.id;
  if (!id) return '';
  return `${window.location.origin}/api/v1/llm/instances/${id}/chat`;
});

/** ② 网关请求 curl 示例（鉴权说明见面板文案：测试期免头，生产需 Bearer）。 */
const accessGatewayCurl = computed(() => {
  const url = accessGatewayUrl.value;
  if (!url) return '';
  return [
    `curl -X POST ${url} \\`,
    `  -H 'Content-Type: application/json' \\`,
    `  -d '{`,
    `    "messages": [{"role": "user", "content": "你好"}],`,
    `    "max_tokens": 256,`,
    `    "temperature": 0.7`,
    `  }'`,
  ].join('\n');
});

/** ② 网关响应示例（ChatOutcome 契约：content/reasoning/finish_reason/
 *  total_tokens——reasoning 双键兼容已在服务端归一）。 */
const accessGatewayResp = computed(() =>
  [
    `{`,
    `  "content": "${t('llmAccess.respContentSample')}",`,
    `  "reasoning": "${t('llmAccess.respReasoningSample')}",`,
    `  "finish_reason": "stop",`,
    `  "total_tokens": 128`,
    `}`,
  ].join('\n'),
);

/** ④ 有效启动命令（后端 `launch_command` 字段透出真实值：曾拉起 = 最近一次
 *  真实 argv（含推理环境二进制路径）；未拉起 = 服务端按当前 config 用
 *  build_vllm_serve_cmd 构造）。字段缺失（旧后端）时前端按 config 忠实重建，
 *  重建规则与 build_vllm_serve_cmd 一一对应：
 *  `vllm serve <model> --host <h> --port <p> --tensor-parallel-size <tp>
 *   --gpu-memory-utilization <两位小数去尾零> --max-model-len <mml> --dtype <d>
 *   [--quantization <q>] [--served-model-name <s>] [--trust-remote-code] [extra_args 原样]`。 */
const accessLaunchCommand = computed(() => {
  const inst = accessInst.value;
  if (!inst) return '';
  const lc = typeof inst.launch_command === 'string' ? inst.launch_command.trim() : '';
  if (lc) return lc;
  const cfg = inst.config ?? {};
  const args: string[] = ['vllm', 'serve', inst.model ?? ''];
  args.push('--host', cfg.host || '0.0.0.0');
  args.push('--port', String(inst.port ?? cfg.port ?? ''));
  args.push('--tensor-parallel-size', String(cfg.tensor_parallel_size ?? 1));
  // 与后端 format_gpu_mem 同口径：最多两位小数、去尾零（0.9499999 → 0.95）
  const gmu = cfg.gpu_memory_utilization ?? 0.9;
  args.push('--gpu-memory-utilization', String(Math.round(gmu * 100) / 100));
  args.push('--max-model-len', String(cfg.max_model_len ?? 8192));
  args.push('--dtype', cfg.dtype || 'auto');
  if (cfg.quantization && String(cfg.quantization).trim()) {
    args.push('--quantization', String(cfg.quantization).trim());
  }
  if (cfg.served_model_name && String(cfg.served_model_name).trim()) {
    args.push('--served-model-name', String(cfg.served_model_name).trim());
  }
  if (cfg.trust_remote_code) args.push('--trust-remote-code');
  args.push(...(cfg.extra_args ?? []));
  return args.join(' ');
});

/** 复制接入面板的一个块（复用 copyText 剪贴板工具；✓ 反馈 1.5s，失败走全局 msg）。 */
async function copyAccess(key: string, text: string): Promise<void> {
  if (!text) return;
  if (await copyText(text)) {
    accessCopied.value = key;
    clearTimeout(accessCopyTimer);
    accessCopyTimer = setTimeout(() => {
      accessCopied.value = '';
    }, 1500);
  } else {
    msg.value = { kind: 'err', text: t('llmAccess.copyFail') };
  }
}

// =============================================================================
// Tab「外部 API」（2026-08-31）：接入其它节点/服务商的 OpenAI 兼容端点
//
// 「我要用别家的模型」：登记 base_url + key（如 106 节点网关的
// qwen3.5-9b），连通测试拿真实模型清单（GET <base>/models，mock 零编造）。
// 对话能力 2026-09-02 起并入统一「对话」Tab（本 Tab 原精简聊天窗移除——
// 减少两份聊天 UI；「对话」Tab 的外部 API 目标即本表登记）。与网关渠道
// （「我要卖」）边界见 docs/LLM_EXTERNAL_APIS.md。文案走 i18n（llmExt
// 命名空间，四语全量）。
// =============================================================================
const extApis = ref<LlmExternalApi[]>([]);
const extLoading = ref(false);
const extError = ref('');
/** 懒加载标记：首次切入本 Tab 或「对话」Tab 目标切到外部 API 时拉列表。 */
let extLoaded = false;

// —— 登记表单 ——
const extForm = ref({ name: '', base_url: '', api_key: '', models_text: '', notes: '' });
const extCreating = ref(false);

// —— 单行操作 busy（测试/删除按行禁用）——
const extBusyId = ref('');

// —— 连通测试结果面板（真实 models 清单 + 延迟；null = 未测）——
const extTest = ref<(LlmExternalTestResult & { name: string }) | null>(null);

// —— via_node 条目内网 IP 遮蔽（2026-09-03）——
// 联邦导入的登记 base_url 是源节点内网地址（消费侧不可达也无须知道）；卡片详情
// 默认只显示「经源节点中继」占位，配置了 API token（本控制台的 admin 凭据）才
// 可逐条展开看真实值——普通访客视角不泄露内网拓扑。
const extUrlRevealed = ref<Record<string, boolean>>({});
/** 本会话是否持有 API token（本 UI 的 admin 凭据；空 = 匿名只读视角）。 */
const hasApiToken = computed(() => getApiToken().trim().length > 0);
/** 切换某行真实地址的显示/遮蔽（仅 admin 可触发，模板里按钮按此禁用）。 */
function toggleExtUrlReveal(id: unknown): void {
  const key = String(id ?? '');
  extUrlRevealed.value[key] = !extUrlRevealed.value[key];
}

async function loadExtApis(): Promise<void> {
  extLoading.value = true;
  extError.value = '';
  try {
    const raw = await endpoints.llmExternalApis();
    extApis.value = Array.isArray(raw.apis) ? raw.apis : [];
  } catch (e) {
    extApis.value = [];
    extError.value = friendlyError(e);
  } finally {
    extLoading.value = false;
  }
}

/** 登记一条外部 API（models 留空 → 由连通测试回填）。 */
async function submitExtCreate(): Promise<void> {
  const name = extForm.value.name.trim();
  const base_url = extForm.value.base_url.trim();
  if (!name) {
    msg.value = { kind: 'err', text: t('llmExt.errNameRequired') };
    return;
  }
  if (!base_url) {
    msg.value = { kind: 'err', text: t('llmExt.errBaseUrlRequired') };
    return;
  }
  extCreating.value = true;
  msg.value = null;
  try {
    await endpoints.llmExternalApiCreate({
      name,
      base_url,
      api_key: extForm.value.api_key.trim() || undefined,
      models: extForm.value.models_text
        .split(/[\n,]/)
        .map((m) => m.trim())
        .filter((m) => m.length > 0),
      notes: extForm.value.notes.trim() || undefined,
    });
    extForm.value = { name: '', base_url: '', api_key: '', models_text: '', notes: '' };
    await loadExtApis();
    msg.value = { kind: 'ok', text: t('llmExt.created') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmExt.createFailed') + friendlyError(e) };
  } finally {
    extCreating.value = false;
  }
}

/** 连通测试：真实 GET <base>/models（服务端带鉴权头），结果面板 + 状态/回填落行。 */
async function testExtApi(row: LlmExternalApi): Promise<void> {
  extBusyId.value = row.id;
  extTest.value = null;
  msg.value = null;
  try {
    const result = await endpoints.llmExternalApiTest(row.id);
    extTest.value = { ...result, name: row.name };
    // 状态/回填已落行，刷新列表展示新 status/models
    await loadExtApis();
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmExt.testFailed') + friendlyError(e) };
  } finally {
    extBusyId.value = '';
  }
}

async function removeExtApi(row: LlmExternalApi): Promise<void> {
  if (!window.confirm(t('llmExt.deleteConfirm', { name: row.name }))) return;
  extBusyId.value = row.id;
  msg.value = null;
  try {
    await endpoints.llmExternalApiDelete(row.id);
    // 统一「对话」Tab 正瞄准该登记时同步清掉目标（防悬空 id）
    if (mcExtApiId.value === row.id) {
      mcExtApiId.value = '';
      mcModel.value = '';
    }
    if (extTest.value && (extTest.value as { id?: string }).id === row.id) extTest.value = null;
    await loadExtApis();
    msg.value = { kind: 'ok', text: t('llmExt.deleted', { name: row.name }) };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmExt.deleteFailed') + friendlyError(e) };
  } finally {
    extBusyId.value = '';
  }
}

// —— 发布到网关（2026-09-03 联邦中继双向打通）：把该登记一键导入为网关渠道
// （POST /api/v1/gateway/channels {from_external_api}——后端复制
// name/base_url/api_key/models/via_node，models 空则先探回填）。发布后本
// 局域网 AI 可经本节点网关 + sk-os- 令牌调到该 API（via_node 非空即中继渠道，
// 经 overlay 定向源节点代发）。
const extPublishingId = ref('');

/** 一键发布：登记 → 网关渠道（渠道页可后续调优先级/权重/计费）。 */
async function publishExtToGateway(row: LlmExternalApi): Promise<void> {
  extPublishingId.value = row.id;
  msg.value = null;
  try {
    const created = (await endpoints.createGatewayChannel({
      from_external_api: row.id,
    })) as { id?: string; models?: string[]; warning?: string | null; via_node?: string };
    const relayed = Boolean((created as { via_node?: string }).via_node);
    const n = (created.models ?? []).length;
    msg.value = {
      kind: 'ok',
      text:
        t('llmExt.published', { name: row.name }) +
        (relayed ? `（${t('llmExt.publishedRelay')}）` : '') +
        (n > 0 ? ` · ${t('llmExt.publishedModels', { n })}` : '') +
        (created.warning ? ` · ${created.warning}` : ''),
    };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmExt.publishFailed') + friendlyError(e) };
  } finally {
    extPublishingId.value = '';
  }
}

// —— 编辑弹窗（复用登记表单字段；PUT 部分更新：api_key 留空 = 保留原 key）——
const extEditShow = ref(false);
const extEditing = ref(false);
const extEditId = ref('');
const extEditKeyMasked = ref('');
const extEditForm = ref({ name: '', base_url: '', api_key: '', models_text: '', notes: '' });

/** 打开编辑弹窗：预填行内现值（api_key 只显示脱敏占位，不回填明文）。 */
function openExtEdit(row: LlmExternalApi): void {
  extEditId.value = row.id;
  extEditKeyMasked.value = row.api_key_masked || '';
  extEditForm.value = {
    name: row.name,
    base_url: row.base_url,
    api_key: '',
    models_text: (row.models ?? []).join('\n'),
    notes: row.notes ?? '',
  };
  extEditShow.value = true;
}

/** 提交编辑：全字段发送（api_key 空串不发送 → 服务端保留原值）。 */
async function submitExtEdit(): Promise<void> {
  const name = extEditForm.value.name.trim();
  const base_url = extEditForm.value.base_url.trim();
  if (!name) {
    msg.value = { kind: 'err', text: t('llmExt.errNameRequired') };
    return;
  }
  if (!base_url) {
    msg.value = { kind: 'err', text: t('llmExt.errBaseUrlRequired') };
    return;
  }
  extEditing.value = true;
  msg.value = null;
  try {
    await endpoints.llmExternalApiUpdate(extEditId.value, {
      name,
      base_url,
      // 留空 = 不发送该字段（服务端保留原 key），与登记表单同口径
      api_key: extEditForm.value.api_key.trim() || undefined,
      models: extEditForm.value.models_text
        .split(/[\n,]/)
        .map((m) => m.trim())
        .filter((m) => m.length > 0),
      // models/notes 已预填现值——清空即表示清除（后端空串=清备注，空数组=清清单）
      notes: extEditForm.value.notes.trim(),
    });
    extEditShow.value = false;
    await loadExtApis();
    msg.value = { kind: 'ok', text: t('llmExt.edited') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmExt.editFailed') + friendlyError(e) };
  } finally {
    extEditing.value = false;
  }
}

/** 状态 → 徽标样式（ok 绿 / error 红 / unknown 灰）。 */
function extStatusClass(s: string): string {
  switch (s) {
    case 'ok':
      return 'pill-ok';
    case 'error':
      return 'pill-err';
    default:
      return 'muted';
  }
}


// —— 创建实例对话框 ——
const showCreate = ref(false);
const submitting = ref(false);
const form = ref({
  name: '',
  model: '',
  source_type: 'huggingface',
  host: '0.0.0.0',
  /** 监听端口文本（空 = 后端自动选口：实例表去重 + 真实试绑，8123 起）。 */
  port_text: '',
  tensor_parallel_size: 1,
  gpu_memory_utilization: 0.9,
  max_model_len: 8192,
  quantization: '',
  dtype: 'auto',
  served_model_name: '',
  trust_remote_code: false,
  /** extra_args 文本域内容（每行一个参数，原样追加到 vllm serve 命令）。 */
  extra_args_text: '',
  /** 创建后立即 spawn vllm（后端默认 false；模板场景默认勾选）。 */
  autostart: false,
  /** 指定推理环境（空 = 默认环境；后端 spawn 时解析对应 venv 的 bin/vllm）。 */
  env_name: '',
});

// —— 模型路径建议（datalist）：本地模型库 + HF hub 缓存合并清单的 path ——
// GET /models/local 现并入 HF 缓存条目（source=hf_cache，path=snapshot 真实目录，
// vLLM 直接可吃）；挂进输入框 datalist，建实例时一键选路径，不用手敲。
const modelPathOptions = ref<string[]>([]);
async function loadModelPathOptions(): Promise<void> {
  try {
    const raw = await endpoints.modelLocal();
    if (Array.isArray(raw)) {
      modelPathOptions.value = (raw as { path?: string }[])
        .map((m) => (typeof m.path === 'string' ? m.path : ''))
        .filter((p) => p.length > 0)
        .slice(0, 50);
    }
  } catch {
    modelPathOptions.value = []; // 拉不到就空建议，不影响手输
  }
}

// —— Qwen 系列推荐配置（一键填充模板，实测可用参数）——
interface QwenPreset {
  key: string;
  label: string;
  /** 建议实例名。 */
  name: string;
  model: string;
  served_model_name: string;
  max_model_len: number;
  gpu_memory_utilization: number;
  /** 原样透传的 vllm serve 参数（flag 与取值各占一项）。 */
  extra_args: string[];
}
const qwenPresets: QwenPreset[] = [
  {
    key: 'qwen35-9b',
    label: 'Qwen3.5-9B（推荐实测）',
    name: 'Qwen3.5-9B',
    model: '/tank/models/Qwen3.5-9B',
    served_model_name: 'qwen3.5-9b',
    max_model_len: 8192,
    gpu_memory_utilization: 0.92,
    extra_args: [
      '--max-num-seqs',
      '24',
      '--enable-auto-tool-choice',
      '--tool-call-parser',
      'qwen3_coder',
      '--reasoning-parser',
      'qwen3',
      '--mm-encoder-tp-mode',
      'data',
      '--speculative-config',
      '{"method":"mtp","num_speculative_tokens":3}',
    ],
  },
  {
    key: 'qwen3-vl-8b',
    label: 'Qwen3-VL-8B',
    name: 'Qwen3-VL-8B',
    model: '/tank/models/Qwen3-VL-8B-Instruct',
    served_model_name: 'qwen3-vl-8b',
    max_model_len: 8192,
    gpu_memory_utilization: 0.85,
    extra_args: [],
  },
];

/** 当前应用的模板 key（''=未选，用于按钮高亮）。 */
const activePreset = ref('');
/** extra_args 文本域是否展开（可折叠；应用模板后自动展开）。 */
const showExtraArgs = ref(false);

/** extra_args 文本域 → 参数数组（按行拆分、去空白行；行内空格不拆，保 JSON 值完整）。 */
const extraArgsList = computed(() =>
  form.value.extra_args_text
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l.length > 0),
);

/** 应用 Qwen 模板：一键填充全部表单字段（autostart 默认勾选）。 */
function applyPreset(p: QwenPreset): void {
  form.value.name = p.name;
  form.value.model = p.model;
  form.value.source_type = 'local';
  form.value.host = '0.0.0.0';
  form.value.tensor_parallel_size = 1;
  form.value.gpu_memory_utilization = p.gpu_memory_utilization;
  form.value.max_model_len = p.max_model_len;
  form.value.quantization = '';
  form.value.dtype = 'auto';
  form.value.served_model_name = p.served_model_name;
  form.value.trust_remote_code = true;
  form.value.extra_args_text = p.extra_args.join('\n');
  form.value.autostart = true;
  activePreset.value = p.key;
  showExtraArgs.value = p.extra_args.length > 0; // 无参数时无需展开
}

/** 清空：重置为默认空表单（等同重新打开）。 */
function clearPresetForm(): void {
  resetForm();
  activePreset.value = '';
  showExtraArgs.value = false;
}

function resetForm(): void {
  form.value = {
    name: '',
    model: '',
    source_type: 'huggingface',
    host: '0.0.0.0',
    port_text: '',
    tensor_parallel_size: 1,
    gpu_memory_utilization: 0.9,
    max_model_len: 8192,
    quantization: '',
    dtype: 'auto',
    served_model_name: '',
    trust_remote_code: false,
    extra_args_text: '',
    autostart: false,
    env_name: '',
  };
}

function openCreate(): void {
  resetForm();
  activePreset.value = '';
  showExtraArgs.value = false;
  msg.value = null;
  showCreate.value = true;
  // 环境下拉取最新注册表（首次打开即拉，之后顺带刷新）
  void loadEnvs();
}
function closeCreate(): void {
  if (submitting.value) return;
  showCreate.value = false;
}
async function submitCreate(): Promise<void> {
  if (!form.value.name.trim()) {
    msg.value = { kind: 'err', text: '名称不可为空' };
    return;
  }
  if (!form.value.model.trim()) {
    msg.value = { kind: 'err', text: '模型不可为空' };
    return;
  }
  // 手动端口：可空 = 后端自动选（实例表去重 + 真实试绑）；填了则本地先校验
  // 范围（1024-65535），冲突/被占由后端 409 带原因
  const portText = form.value.port_text.trim();
  let manualPort: number | undefined;
  if (portText) {
    const p = Number(portText);
    if (!Number.isInteger(p) || p < 1024 || p > 65535) {
      msg.value = { kind: 'err', text: t('llmLog.portInvalid') };
      return;
    }
    manualPort = p;
  }
  submitting.value = true;
  msg.value = null;
  try {
    // 完整 VllmConfig：后端 Rust 端无 serde default，缺字段（如 port/extra_args）
    // 会导致反序列化 500，故字段全部显式给出。config.port 为占位值——后端以
    // 请求体顶层 port（手动）或 pick_free_port（自动）为准并回写两处。
    const config: Record<string, unknown> = {
      host: form.value.host.trim() || '0.0.0.0',
      port: 8000,
      tensor_parallel_size: form.value.tensor_parallel_size,
      gpu_memory_utilization: form.value.gpu_memory_utilization,
      max_model_len: form.value.max_model_len,
      quantization: form.value.quantization || null,
      dtype: form.value.dtype,
      served_model_name: form.value.served_model_name.trim() || null,
      trust_remote_code: form.value.trust_remote_code,
      extra_args: extraArgsList.value,
    };
    const created = (await endpoints.createLlmInstance({
      name: form.value.name,
      model: form.value.model,
      source_type: form.value.source_type,
      config,
      port: manualPort,
      autostart: form.value.autostart,
      env_name: form.value.env_name || undefined,
    })) as ModelInstance;
    await loadInstances();
    const finalPort = created?.port ?? manualPort;
    msg.value = form.value.autostart
      ? {
          kind: 'ok',
          text: `已创建实例并自动启动（端口 ${finalPort}，vllm 后台拉起中——实例行「日志」按钮看进度）`,
        }
      : { kind: 'ok', text: '已创建实例（默认停止状态，点击「启动」拉起 vllm）' };
    showCreate.value = false;
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    submitting.value = false;
  }
}

// =============================================================================
// Tab「配方库」：vLLM Recipes 官方部署配方导入
//
// 数据面：GET /api/v1/llm/recipes/catalog（服务端代理 + 1h 缓存）/
//        recipe?hf_id=（原样透传 + 60s 缓存）——浏览器直连外网被 CORS 挡，
//        外网请求只在服务端做。「存为本地配方」暂存 localStorage
//        （llm-recipes-saved，轻量优先），后续可接节点执行。
// =============================================================================
/** vLLM Recipes 官方站点（「查看网站」外链目标）。 */
const RECIPES_SITE = 'https://recipes.vllm.ai';
/** 本地保存配方的 localStorage key。 */
const RECIPES_SAVED_KEY = 'llm-recipes-saved';

// marked：GFM 同步渲染（上游官方站内容，信任模型同 DevDocs.vue）
marked.setOptions({ gfm: true, breaks: false, async: false });

// —— 目录 ——
const recipes = ref<LlmRecipeCatalogItem[]>([]);
const recipesLoading = ref(false);
const recipesError = ref('');
const recipeSearch = ref('');
/** 上次刷新时刻（后端缓存信封 cached_at，RFC3339；null = 尚未拉取过）。 */
const recipesCachedAt = ref<string | null>(null);
/** 懒加载标记：首次切入本 Tab 才拉目录（服务端常驻缓存命中即秒回零外呼；
 * 「刷新目录」按钮带 refresh=1 强制重拉——自动打开 Tab 永不触发外呼重拉）。 */
let recipesLoaded = false;

/** 树视图（2026-08-30 官方同款重构）：展开的提供方集合（空 = 全部折叠）。 */
const expandedProviders = ref<Set<string>>(new Set());
/** 当前选中的模型节点（右侧速览卡数据源；null = 未选）。 */
const selectedRecipe = ref<LlmRecipeCatalogItem | null>(null);

/** 搜索过滤（标题 / 提供方 / HF ID 子串，大小写不敏感）——树节点过滤的唯一依据。 */
const filteredRecipes = computed(() => {
  const q = recipeSearch.value.trim().toLowerCase();
  if (!q) return recipes.value;
  return recipes.value.filter(
    (r) =>
      (r.title ?? '').toLowerCase().includes(q) ||
      (r.hf_id ?? '').toLowerCase().includes(q) ||
      (r.provider ?? '').toLowerCase().includes(q),
  );
});

/** 提供方聚合（数量降序、同数量按名称字典序）——树父节点排序 + 徽章计数。 */
const recipeProviders = computed<{ name: string; count: number }[]>(() => {
  const counts = new Map<string, number>();
  for (const r of recipes.value) {
    const p = r.provider ?? '';
    counts.set(p, (counts.get(p) ?? 0) + 1);
  }
  return [...counts.entries()]
    .map(([name, count]) => ({ name, count }))
    .sort((a, b) => b.count - a.count || a.name.localeCompare(b.name));
});

/** 提供方 → 树展示序（数量大的大厂排前，与父节点折叠列表一致）。 */
const recipeProviderOrder = computed(() => {
  const m = new Map<string, number>();
  recipeProviders.value.forEach((p, i) => m.set(p.name, i));
  return m;
});

/** 树数据：搜索命中 → 按提供方分组（组序同父节点序；组内按标题字典序）。
 *  官方站同款两极结构——提供方为父节点（带数量徽章）、模型为子节点。 */
const recipeTree = computed<{ provider: string; items: LlmRecipeCatalogItem[] }[]>(() => {
  const groups = new Map<string, LlmRecipeCatalogItem[]>();
  for (const r of filteredRecipes.value) {
    const p = r.provider ?? '';
    const list = groups.get(p);
    if (list) list.push(r);
    else groups.set(p, [r]);
  }
  const order = recipeProviderOrder.value;
  return [...groups.entries()]
    .sort((a, b) => (order.get(a[0]) ?? 0) - (order.get(b[0]) ?? 0))
    .map(([provider, items]) => ({
      provider,
      items: [...items].sort(
        (a, b) => (a.title ?? '').localeCompare(b.title ?? '') || a.hf_id.localeCompare(b.hf_id),
      ),
    }));
});

/** 提供方是否展开（搜索时命中组自动展开——否则匹配的子节点被折叠藏住）。 */
function providerExpanded(name: string): boolean {
  if (recipeSearch.value.trim()) return true;
  return expandedProviders.value.has(name);
}

/** 折叠/展开一个提供方父节点（搜索态下点击无效——保持自动展开）。 */
function toggleProvider(name: string): void {
  if (recipeSearch.value.trim()) return;
  const next = new Set(expandedProviders.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  expandedProviders.value = next;
}

/** 点击模型子节点：置为选中（右侧速览）+ 打开配方详情模态（现有逻辑复用）。 */
function selectRecipe(row: LlmRecipeCatalogItem): void {
  selectedRecipe.value = row;
  void openRecipe(row.hf_id);
}

/** 目录空态文案（区分「还没拉取」与「过滤/搜索后无匹配」）。 */
const recipeEmptyText = computed(() =>
  recipes.value.length === 0
    ? t('llmRecipes.emptyNotFetched', { btn: t('llmRecipes.refreshBtn') })
    : t('llmRecipes.emptyNoMatch'),
);

/** 拉目录（refresh=false 读服务端常驻缓存秒回；true 强制重拉上游并更新缓存）。 */
async function loadRecipes(refresh = false): Promise<void> {
  recipesLoading.value = true;
  recipesError.value = '';
  try {
    const raw = await endpoints.llmRecipesCatalog({ refresh });
    recipes.value = Array.isArray(raw.items) ? raw.items : [];
    recipesCachedAt.value = raw.cached_at ?? null;
  } catch (e) {
    recipesError.value = friendlyError(e);
  } finally {
    recipesLoading.value = false;
  }
}

/** 配方详情页外链（上游 url 字段恒等于 /{hf_id}）。 */
function recipeSiteUrl(hfId: string): string {
  return `${RECIPES_SITE}/${hfId}`;
}

// —— 详情（模态）——
const showRecipeDetail = ref(false);
const recipeDetail = ref<LlmRecipeDetail | null>(null);
const recipeDetailLoading = ref(false);
const recipeDetailError = ref('');
const recipeDetailHfId = ref('');
/** 「复制启动命令」成功后的短暂反馈（2s）。 */
const recipeCopied = ref(false);

const recipeDetailTitle = computed(
  () => recipeDetail.value?.meta?.title ?? recipeDetailHfId.value,
);

/** variants 对象 → 表格行（键名即变体名）。 */
const recipeVariants = computed(() => {
  const v = recipeDetail.value?.variants;
  if (!v) return [];
  return Object.entries(v).map(([name, val]) => ({
    name,
    precision: val?.precision,
    vram_minimum_gb: val?.vram_minimum_gb,
    description: val?.description,
  }));
});

/** guide Markdown → HTML（空/缺省返回空串，模板用 v-if 跳过）。 */
const recipeGuideHtml = computed(() => {
  const g = recipeDetail.value?.guide;
  if (!g || !g.trim()) return '';
  return marked.parse(g) as string;
});

async function openRecipe(hfId: string): Promise<void> {
  recipeDetailHfId.value = hfId;
  recipeDetail.value = null;
  recipeDetailError.value = '';
  recipeCopied.value = false;
  showRecipeDetail.value = true;
  recipeDetailLoading.value = true;
  try {
    recipeDetail.value = await endpoints.llmRecipe(hfId);
  } catch (e) {
    recipeDetailError.value = friendlyError(e);
  } finally {
    recipeDetailLoading.value = false;
  }
}

function closeRecipe(): void {
  showRecipeDetail.value = false;
}

/** 复制文本到剪贴板：统一走 utils/clipboard 的 copyText（安全上下文 Clipboard
 *  API，HTTP 非安全上下文回退临时 textarea + execCommand('copy')——2026-08-31
 *  起本页删除本地重复实现，与「接入说明」弹窗等共用同一工具）。 */

async function copyRecipeCommand(): Promise<void> {
  const cmd = recipeDetail.value?.recommended_command?.command ?? '';
  if (!cmd) {
    msg.value = { kind: 'err', text: '该配方没有推荐启动命令' };
    return;
  }
  if (await copyText(cmd)) {
    recipeCopied.value = true;
    setTimeout(() => {
      recipeCopied.value = false;
    }, 2000);
  } else {
    msg.value = { kind: 'err', text: '复制失败（浏览器剪贴板不可用）' };
  }
}

async function copySavedCommand(cmd: string): Promise<void> {
  msg.value = (await copyText(cmd))
    ? { kind: 'ok', text: '启动命令已复制到剪贴板' }
    : { kind: 'err', text: '复制失败（浏览器剪贴板不可用）' };
}

// —— 存为本地配方（localStorage，轻量优先；后续可接节点执行）——
const savedRecipes = ref<LlmRecipeSaved[]>([]);

function loadSavedRecipes(): void {
  try {
    const raw = localStorage.getItem(RECIPES_SAVED_KEY);
    const arr = raw ? (JSON.parse(raw) as LlmRecipeSaved[]) : [];
    savedRecipes.value = Array.isArray(arr) ? arr : [];
  } catch {
    savedRecipes.value = [];
  }
}

function persistSavedRecipes(): void {
  try {
    localStorage.setItem(RECIPES_SAVED_KEY, JSON.stringify(savedRecipes.value));
  } catch {
    /* 配额满等写入失败忽略（仅影响持久化，当次会话内仍可用） */
  }
}

function saveRecipeToLocal(): void {
  const d = recipeDetail.value;
  const hfId = d?.hf_id ?? recipeDetailHfId.value;
  if (!hfId) return;
  if (savedRecipes.value.some((s) => s.hf_id === hfId)) {
    msg.value = { kind: 'info', text: '该配方已在本地保存列表中' };
    return;
  }
  savedRecipes.value.unshift({
    hf_id: hfId,
    title: d?.meta?.title ?? hfId,
    provider: d?.meta?.provider ?? '',
    command: d?.recommended_command?.command ?? '',
    docker_command: d?.recommended_command?.docker_command ?? '',
    hardware: d?.recommended_command?.hardware ?? '',
    variants: d?.variants,
    saved_at: new Date().toISOString(),
  });
  persistSavedRecipes();
  msg.value = {
    kind: 'ok',
    text: `已存为本地配方：${hfId}（暂存本浏览器，后续可接节点执行）`,
  };
}

function removeSavedRecipe(hfId: string): void {
  savedRecipes.value = savedRecipes.value.filter((s) => s.hf_id !== hfId);
  persistSavedRecipes();
}

/** 上游 difficulty → 中文标签。 */
function difficultyLabel(d?: string): string {
  switch (d) {
    case 'beginner':
      return '入门';
    case 'intermediate':
      return '进阶';
    case 'advanced':
      return '高级';
    default:
      return d ?? '—';
  }
}

// =============================================================================
// Tab「对话」（2026-09-02 统一化重构：合并旧「对话」+「推理测试」）
//
// 顶部目标选择器三组：
//   a. 本地实例   —— 直连 http://<window.location.hostname>:<port>/v1/chat/completions
//                   SSE 流式（实例监听 0.0.0.0，浏览器与本页面同机/同网可达；
//                   0.1.10 时代硬编码 192.0.2.106 已修）
//   b. 外部 API   —— POST /api/v1/llm/external-apis/:id/chat（stream:true，
//                   服务端 SSE 逐块透传，key 不出服务端）
//   c. 联邦大厅   —— GET /api/v1/api-market?scope=fed 条目下拉；选中即一键
//                   导入为外部 API 登记（同名已存在则直接复用不重复建），
//                   导入成功自动切到该外部 API 目标开聊
// 流式解析 parseDeltaFields：content + reasoning（vLLM 0.28 `reasoning` /
// 0.27 `reasoning_content` 双键兼容），思考段折叠展示；content 空且思考段
// 非空时给 max_tokens 调大提示（Qwen 思考模型提示保留）。max_tokens 可调
// （默认 4096）。历史按目标隔离存 localStorage。
// =============================================================================
/** 历史前缀（按目标拼 key：os-llm-chat-<instance|ext>-<id>，目标间互不串台）。 */
const MC_LS_PREFIX = 'os-llm-chat';
/** 直连实例用的主机名：与页面同源（实例监听 0.0.0.0，本机/同网浏览器可达）。 */
const MC_INSTANCE_HOST = window.location.hostname || '127.0.0.1';

// —— 目标选择（三组）——
type McTargetKind = 'instance' | 'extapi' | 'fed';
const mcKind = ref<McTargetKind>('instance');
/** 本地实例目标 id。 */
const mcInstanceId = ref<string>('');
/** 外部 API 目标 id（联邦导入成功后也落到这里；__manual__ = 手动临时目标）。 */
const mcExtApiId = ref<string>('');
/** 联邦大厅条目目标 id（选中即触发导入）。 */
const mcFedListingId = ref<string>('');
/** 联邦导入进行中（下拉禁用防连点）。 */
const mcFedImporting = ref(false);

// —— 手动输入目标（2026-09-03）：外部 API 组下拉末项「＋ 手动输入」——
// 用户原话「外部api 应该是可以手动写ip的，这个没有，只有选列表内的」。两个动作：
//   临时对话   不落库，本会话内存目标直接聊——浏览器直连该 URL 的
//              /chat/completions SSE（照本地实例直连模式；跨网不可达时报错
//              文案引导「联邦大厅导入或经中继」）
//   登记并对话 POST /llm/external-apis 落库后切换（via_node 留空 = 直连语义，
//              与联邦导入的经源节点中继行相对）
// 历史存储：临时目标用 URL hash 做 key（localStorage 同规则，目标间不串台）。
/** 手动临时目标在外部 API 下拉中的 sentinel 值。 */
const MC_MANUAL_ID = '__manual__';
/** 行内小表单展开（选中「＋ 手动输入」或生效横幅点「编辑」）。 */
const mcManualOpen = ref(false);
/** 表单三件套：Base URL（必填 http(s)）/ API Key（可选）/ 模型名（必填）。 */
const mcManualForm = ref({ base_url: '', api_key: '', model: '' });
/** 「登记并对话」进行中（双按钮防连点）。 */
const mcManualRegistering = ref(false);
/** 表单校验错误（行内展示，空串 = 通过）。 */
const mcManualError = ref('');
/** 临时目标已应用（点过「临时对话」；「取消」只收表单不退出已生效目标）。 */
const mcManualApplied = ref(false);
/** 手动临时目标当前生效（外部 API 组选中 __manual__ 项）。 */
const mcManualActive = computed(
  () => mcKind.value === 'extapi' && mcExtApiId.value === MC_MANUAL_ID,
);

// —— 聊天通用状态 ——
/** 输入框文本。 */
const mcInput = ref('');
/** 是否正在等待/接收流式回复。 */
const mcSending = ref(false);
/** 气泡列表。 */
const mcBubbles = ref<McBubble[]>([]);
/** 滚动容器引用。 */
const mcScrollEl = ref<HTMLDivElement | null>(null);
/** max_tokens（默认 4096——用户要求 4k 起；思考模型小额度会吃满只剩思考段）。 */
const mcMaxTokens = ref(4096);
/** 外部 API 目标的模型选择（实例目标模型自动取 served_model_name 无需选）。 */
const mcModel = ref('');

// —— 联邦大厅条目（scope=fed 下拉数据，懒加载）——
/** 联邦条目最小形状（ApiGateway.vue MarketListing 的导入所需子集）。 */
interface McFedListing {
  id?: string;
  api_name?: string;
  endpoint_url?: string;
  source_node?: string;
  /** 来源 NodeID（0x+66hex，源端验签落列；一键导入作 via_node → chat/test
   * 经 overlay 定向源节点代发，跨网可达发布者内网 endpoint）。 */
  source_node_id?: string;
  server_config?: { model_name?: string | null } | null;
  access_info?: { api_key?: string } | null;
  [k: string]: unknown;
}
const mcFedListings = ref<McFedListing[]>([]);
const mcFedLoading = ref(false);
const mcFedError = ref('');
let mcFedLoaded = false;

async function loadFedListings(): Promise<void> {
  mcFedLoading.value = true;
  mcFedError.value = '';
  try {
    const raw = await endpoints.apiMarketList({ scope: 'fed' });
    mcFedListings.value = Array.isArray(raw) ? (raw as McFedListing[]) : [];
  } catch (e) {
    mcFedListings.value = [];
    mcFedError.value = friendlyError(e);
  } finally {
    mcFedLoading.value = false;
  }
}

/** 当前选中的本地实例对象（计算）。 */
const mcSelected = computed<ModelInstance | undefined>(() =>
  instances.value.find((i) => String(i.id ?? '') === mcInstanceId.value),
);

/** 当前选中的外部 API 登记对象（计算；__manual__ → 手动临时目标合成行——
 *  形状对齐 LlmExternalApi，下游 mcModelOptions/mcReady 等零改动复用）。 */
const mcExtTarget = computed<LlmExternalApi | undefined>(() => {
  if (mcExtApiId.value === MC_MANUAL_ID) {
    const baseUrl = mcManualForm.value.base_url.trim();
    const model = mcManualForm.value.model.trim();
    return {
      id: MC_MANUAL_ID,
      name: t('llmChat.manualName'),
      base_url: baseUrl,
      api_key_masked: '',
      has_api_key: mcManualForm.value.api_key.trim().length > 0,
      models: model ? [model] : [],
      status: 'unknown',
      created_at: '',
    };
  }
  return extApis.value.find((a) => a.id === mcExtApiId.value);
});

/** 当前选中的联邦条目（计算）。 */
const mcFedSelected = computed<McFedListing | undefined>(() =>
  mcFedListings.value.find((l) => String(l.id ?? '') === mcFedListingId.value),
);

/** running 实例才可选（其它状态在「实例管理」启动）。 */
const mcRunningInstances = computed(() =>
  instances.value.filter((i) => i.status === 'running'),
);

/** 外部 API 目标的模型下拉选项（登记 models / 连通测试回填）。 */
const mcModelOptions = computed(() => mcExtTarget.value?.models ?? []);

/** 选中实例的 served_model_name（缺失时回退 model 字段）。 */
const mcServedName = computed(() => {
  const inst = mcSelected.value;
  if (!inst) return '';
  const smn = inst.config?.served_model_name;
  return (typeof smn === 'string' && smn.trim()) || inst.model || '';
});

/** 当前目标是否就绪可发（实例 running + 模型名就绪 / 外部 API + 模型选中）。 */
const mcReady = computed(() => {
  if (mcKind.value === 'instance') {
    return !!mcSelected.value && mcSelected.value.status === 'running' && !!mcServedName.value;
  }
  if (mcKind.value === 'extapi') return !!mcExtTarget.value && !!mcModel.value;
  return false; // 联邦条目选中即导入并切到 extapi，不直接停在 fed 目标上发
});

/** 目标切换 → 换历史 + 模型默认值（extapi 默认取第一个模型）。 */
watch([mcKind, mcInstanceId, mcExtApiId], () => {
  if (mcKind.value === 'extapi') {
    mcModel.value = mcModelOptions.value[0] ?? '';
  }
  mcLoadHistory();
  void mcScrollToEnd();
});

/** 外部 API 目标的模型清单后到（连通测试回填）时补默认选择。 */
watch(mcModelOptions, (opts) => {
  if (mcKind.value === 'extapi' && !mcModel.value) mcModel.value = opts[0] ?? '';
});

// —— 持久化（localStorage，按目标隔离）——
/** URL → 稳定短 hash（FNV-1a 32bit hex；手动临时目标历史 key 用——URL 归一
 *  小写去尾斜杠后哈希，同地址不同写法共享同一份历史）。 */
function mcUrlHash(url: string): string {
  const normalized = url.trim().toLowerCase().replace(/\/+$/, '');
  let h = 0x811c9dc5;
  for (let i = 0; i < normalized.length; i++) {
    h ^= normalized.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h.toString(16);
}

function mcHistoryKey(): string {
  if (mcKind.value === 'instance') return `${MC_LS_PREFIX}-instance-${mcInstanceId.value}`;
  if (mcExtApiId.value === MC_MANUAL_ID) {
    return `${MC_LS_PREFIX}-manual-${mcUrlHash(mcManualForm.value.base_url)}`;
  }
  return `${MC_LS_PREFIX}-ext-${mcExtApiId.value}`;
}

function mcLoadHistory(): void {
  try {
    const raw = localStorage.getItem(mcHistoryKey());
    if (!raw) {
      mcBubbles.value = [];
      return;
    }
    const arr = JSON.parse(raw) as McBubble[];
    if (Array.isArray(arr)) {
      // 落地时若处于 streaming 状态，恢复为已完成
      mcBubbles.value = arr.map((b) => ({ ...b, streaming: false }));
    }
  } catch {
    mcBubbles.value = [];
  }
}

function mcSaveHistory(): void {
  try {
    localStorage.setItem(mcHistoryKey(), JSON.stringify(mcBubbles.value));
  } catch {
    /* 配额满等错误忽略 */
  }
}

/** 实例列表就绪后默认选第一个 running 实例（原 ModelChat 逻辑）。 */
watch(instances, () => {
  if (mcKind.value !== 'instance' || mcInstanceId.value) return;
  const first = mcRunningInstances.value[0];
  if (first) mcInstanceId.value = String(first.id ?? '');
});

/** 目标种类切换：fed 组懒加载联邦条目；extapi 组懒加载登记列表。 */
watch(mcKind, (kind) => {
  if (kind === 'fed' && !mcFedLoaded) {
    mcFedLoaded = true;
    void loadFedListings();
  }
  if (kind === 'extapi' && !extLoaded) {
    extLoaded = true;
    void loadExtApis();
  }
});

/** 把当前气泡转成 OpenAI messages 历史（仅成功完成的 user/assistant）。 */
function mcToHistoryMessages(): McChatMessage[] {
  return mcBubbles.value
    .filter((b) => !b.error && b.content.trim())
    .map((b) => ({ role: b.role, content: b.content }));
}

/** 滚到底。 */
async function mcScrollToEnd(): Promise<void> {
  await nextTick();
  const el = mcScrollEl.value;
  if (el) el.scrollTop = el.scrollHeight;
}

/** 解析 vLLM/OpenAI SSE chunk：提取 choices[0].delta 的正文与思考段增量
 *  （`reasoning` vLLM 0.28 / `reasoning_content` 0.27 双键兼容）。 */
function parseDeltaFields(line: string): { content: string; reasoning: string } {
  const trimmed = line.trim();
  if (!trimmed.startsWith('data:')) return { content: '', reasoning: '' };
  const payload = trimmed.slice(5).trim();
  if (!payload || payload === '[DONE]') return { content: '', reasoning: '' };
  try {
    const obj = JSON.parse(payload) as {
      choices?: Array<{
        delta?: { content?: string; reasoning?: string; reasoning_content?: string };
      }>;
    };
    const delta = obj.choices?.[0]?.delta;
    if (!delta) return { content: '', reasoning: '' };
    return {
      content: typeof delta.content === 'string' ? delta.content : '',
      reasoning:
        (typeof delta.reasoning === 'string' && delta.reasoning) ||
        (typeof delta.reasoning_content === 'string' ? delta.reasoning_content : ''),
    };
  } catch {
    return { content: '', reasoning: '' };
  }
}

/**
 * 联邦条目一键导入：同名登记已存在 → 先用条目携带的新凭据 PUT 升级旧行
 * （via_node=source_node_id、api_key 仅明文视角、models=model_name；脱敏/
 * 缺失字段不覆盖——决策见 utils/fedImport.ts fedUpgradeDecision）再切换
 * （0.1.16 前导入的旧行是直连语义，只切换会让对话继续直连报错）；
 * 否则 POST /llm/external-apis 建登记（name=条目名、base_url=endpoint_url、
 * api_key=access_info.api_key 仅明文视角带——脱敏态提示手动补填、
 * models=server_config.model_name）。成功后自动切到该外部 API 目标并清掉
 * fed 选中（防重复导入循环）。
 */
async function mcImportFed(listing: McFedListing): Promise<void> {
  const name = (listing.api_name ?? '').trim();
  const baseUrl = (listing.endpoint_url ?? '').trim();
  if (!name || !baseUrl) {
    mcFedListingId.value = '';
    return;
  }
  mcFedImporting.value = true;
  msg.value = null;
  try {
    // 登记列表先就绪（fed 下拉入口可能从未加载 extapi 组）——否则同名检查
    // 扑空会走 POST 造出重复行。
    if (!extLoaded) {
      extLoaded = true;
    }
    await loadExtApis();
    // 同名登记已存在 → 升级（凭据有增量）后切换；无增量只切换
    const existing = extApis.value.find((a) => a.name === name);
    if (existing) {
      const decision = fedUpgradeDecision(existing, listing);
      if (decision.upgraded) {
        await endpoints.llmExternalApiUpdate(existing.id, decision.patch);
        await loadExtApis();
      }
      mcKind.value = 'extapi';
      mcExtApiId.value = existing.id;
      let text = decision.upgraded
        ? t('llmChat.fedExistsUpgraded', { name })
        : t('llmChat.fedExists', { name });
      // 脱敏且旧行无 key → 附手动补填指引（与新建导入同口径）
      if (decision.needsManualKey) text += `（${t('llmChat.fedKeyMasked')}）`;
      msg.value = { kind: decision.upgraded ? 'ok' : 'info', text };
      return;
    }
    // key 仅明文视角可带（脱敏态 `前4***后4` 含 ***；短 key 全掩 ****）
    const rawKey = (listing.access_info?.api_key ?? '').trim();
    const keyMasked = rawKey.includes('*');
    const modelName = (listing.server_config?.model_name ?? '').trim();
    // via_node = 来源 NodeID（2026-09-02 跨网中继）：非空写入 → 该登记的
    // chat/test 经 overlay 定向源节点代发（不直连发布者内网 endpoint）。
    const viaNode = (listing.source_node_id ?? '').trim();
    await endpoints.llmExternalApiCreate({
      name,
      base_url: baseUrl,
      api_key: !rawKey || keyMasked ? undefined : rawKey,
      models: modelName ? [modelName] : [],
      notes: t('llmChat.fedNotes', { node: listing.source_node || '—' }),
      via_node: viaNode || undefined,
    });
    if (!extLoaded) {
      extLoaded = true;
    }
    await loadExtApis();
    const created = extApis.value.find((a) => a.name === name);
    mcKind.value = 'extapi';
    mcExtApiId.value = created?.id ?? '';
    // 成功消息附带脱敏提示（否则会被 ok 覆盖，用户错过补填指引）
    msg.value = {
      kind: 'ok',
      text: keyMasked
        ? `${t('llmChat.fedImported', { name })}（${t('llmChat.fedKeyMasked')}）`
        : t('llmChat.fedImported', { name }),
    };
  } catch (e) {
    msg.value = { kind: 'err', text: t('llmChat.fedImportFailed') + friendlyError(e) };
  } finally {
    mcFedImporting.value = false;
    mcFedListingId.value = '';
  }
}

/** 联邦下拉选中 → 触发一键导入（watch 而非 @change：编程赋值同样生效）。 */
watch(mcFedListingId, (id) => {
  if (!id) return;
  const listing = mcFedSelected.value;
  if (listing) void mcImportFed(listing);
});

// —— 手动输入目标：动作与校验 ——
/** 外部 API 下拉选中「＋ 手动输入」→ 展开行内表单（watch 而非 @change：
 *  编程赋值同样生效——与 fed 导入同款）。 */
watch(mcExtApiId, (id) => {
  if (id === MC_MANUAL_ID) mcManualOpen.value = true;
});

/** 校验手动表单：Base URL 须 http(s)、模型名非空（返回错误文案，空串 = 通过）。 */
function mcManualValidate(): string {
  if (!/^https?:\/\//i.test(mcManualForm.value.base_url.trim())) {
    return t('llmChat.manualErrBaseUrl');
  }
  if (!mcManualForm.value.model.trim()) {
    return t('llmChat.manualErrModel');
  }
  return '';
}

/** 「临时对话」：表单即刻生效为内存目标（不落库），收起表单开聊——历史按
 *  URL hash 换 key 装载（同地址旧对话恢复；首次选中空表单时装载的是空历史）。 */
function mcManualTemp(): void {
  const err = mcManualValidate();
  if (err) {
    mcManualError.value = err;
    return;
  }
  mcManualError.value = '';
  mcManualApplied.value = true;
  mcModel.value = mcManualForm.value.model.trim();
  mcManualOpen.value = false;
  mcLoadHistory();
  void mcScrollToEnd();
}

/** 「登记并对话」：POST external-apis 落库后切换到该登记开聊。via_node 留空 =
 *  直连语义（不经 overlay 中继）；登记名取 URL host，与既有登记同名时叠加
 *  模型名/序号去重（后端不强制唯一，前端去重避免下拉两行同名）。 */
async function mcManualRegister(): Promise<void> {
  const err = mcManualValidate();
  if (err) {
    mcManualError.value = err;
    return;
  }
  const baseUrl = mcManualForm.value.base_url.trim();
  const modelName = mcManualForm.value.model.trim();
  mcManualRegistering.value = true;
  mcManualError.value = '';
  msg.value = null;
  try {
    let host = baseUrl;
    try {
      host = new URL(baseUrl).host;
    } catch {
      /* URL 解析失败保持原串（校验已保证 http(s) 前缀，理论不至此） */
    }
    const names = new Set(extApis.value.map((a) => a.name));
    let name = host;
    if (names.has(name)) name = `${host} · ${modelName}`;
    for (let i = 2; names.has(name); i++) name = `${host} · ${modelName} ${i}`;
    const created = await endpoints.llmExternalApiCreate({
      name,
      base_url: baseUrl,
      api_key: mcManualForm.value.api_key.trim() || undefined,
      models: [modelName],
      notes: t('llmChat.manualNotes'),
    });
    if (!extLoaded) {
      extLoaded = true;
    }
    await loadExtApis();
    const row = created?.id ? created : extApis.value.find((a) => a.name === name);
    mcManualOpen.value = false;
    mcManualApplied.value = false;
    mcExtApiId.value = row?.id ?? '';
    mcModel.value = modelName;
    msg.value = { kind: 'ok', text: t('llmChat.manualRegistered', { name }) };
  } catch (e) {
    mcManualError.value = t('llmChat.manualRegisterFailed') + friendlyError(e);
  } finally {
    mcManualRegistering.value = false;
  }
}

/** 取消：只收起表单；临时目标从未应用时顺带把下拉复位回空占位（已应用的目标
 *  保留——退出走横幅上的「退出临时目标」）。 */
function mcManualClose(): void {
  mcManualOpen.value = false;
  mcManualError.value = '';
  if (mcExtApiId.value === MC_MANUAL_ID && !mcManualApplied.value) {
    mcExtApiId.value = '';
  }
}

/** 退出临时目标（横幅按钮）：清选择回占位；表单值保留在内存，再选「＋ 手动
 *  输入」可带回继续用。 */
function mcManualExit(): void {
  mcManualOpen.value = false;
  mcManualApplied.value = false;
  if (mcExtApiId.value === MC_MANUAL_ID) {
    mcExtApiId.value = '';
  }
}

/** 手动目标 chat 地址：<base_url 去尾斜杠>/chat/completions（base_url 为
 *  OpenAI 兼容根地址口径，含 /v1——与登记行一致）。 */
function mcManualChatUrl(baseUrl: string): string {
  return `${baseUrl.replace(/\/+$/, '')}/chat/completions`;
}

/** 发送一条消息（按目标种类分流：实例直连 SSE / 手动目标浏览器直连 SSE /
 *  外部 API 登记服务端透传 SSE）。 */
async function mcSend(): Promise<void> {
  const text = mcInput.value.trim();
  if (!text || mcSending.value) return;

  // —— 目标解析（url / model / headers 三件套按种类装配）——
  let url = '';
  let model = '';
  const headers: Record<string, string> = { 'Content-Type': 'application/json' };
  if (mcKind.value === 'instance') {
    const inst = mcSelected.value;
    if (!inst || !inst.port || inst.status !== 'running') {
      mcPushErrorBubble(t('llmChat.instanceNotReady'));
      return;
    }
    model = mcServedName.value;
    if (!model) {
      mcPushErrorBubble(t('llmChat.errNoModel'));
      return;
    }
    url = `http://${MC_INSTANCE_HOST}:${inst.port}/v1/chat/completions`;
  } else {
    const target = mcExtTarget.value;
    if (!target) {
      mcPushErrorBubble(t('llmChat.errNoTarget'));
      return;
    }
    if (!mcModel.value) {
      mcPushErrorBubble(t('llmChat.errModelRequired'));
      return;
    }
    model = mcModel.value;
    if (target.id === MC_MANUAL_ID) {
      // 手动临时目标：浏览器直连该 URL 的 /chat/completions SSE（照本地实例
      // 直连模式——不落库、key 只在本会话内存，不经服务端转发）
      url = mcManualChatUrl(target.base_url);
      const manualKey = mcManualForm.value.api_key.trim();
      if (manualKey) headers['Authorization'] = `Bearer ${manualKey}`;
    } else {
      url = `/api/v1/llm/external-apis/${encodeURIComponent(target.id)}/chat`;
      // 服务端启用了 admin token 时带上（与 client.ts request 同口径：空串不发头）
      const token = getApiToken();
      if (token.trim()) headers['Authorization'] = `Bearer ${token}`;
    }
  }

  // 推入用户气泡 + 空的 assistant 气泡（流式追加）
  const aiBubble: McBubble = {
    id: `a-${Date.now()}`,
    role: 'assistant',
    content: '',
    streaming: true,
  };
  mcBubbles.value.push(
    { id: `u-${Date.now()}`, role: 'user', content: text },
    aiBubble,
  );
  mcInput.value = '';
  mcSending.value = true;
  mcSaveHistory();
  await mcScrollToEnd();

  // 构造 OpenAI 兼容请求体（含历史；max_tokens 可调，默认 4096）
  const messages = [
    ...mcToHistoryMessages().filter((m) => m.content !== text),
    { role: 'user' as const, content: text },
  ];
  const maxTokens = Math.max(1, Math.floor(Number(mcMaxTokens.value) || 4096));
  const body = { model, messages, stream: true, max_tokens: maxTokens };

  try {
    const resp = await fetch(url, { method: 'POST', headers, body: JSON.stringify(body) });
    if (!resp.ok || !resp.body) {
      // 非 2xx：尝试读 error
      let detail = `${resp.status} ${resp.statusText}`;
      try {
        const j = await resp.json();
        if (j && (j.error?.message || j.error || j.message)) {
          detail = String(j.error?.message ?? j.error ?? j.message);
        }
      } catch {
        /* ignore */
      }
      aiBubble.content = `${t('llmChat.requestFailed')}${detail}`;
      aiBubble.error = true;
      aiBubble.streaming = false;
      mcSaveHistory();
      return;
    }
    // 读 SSE 流（ReadableStream + TextDecoder，逐 data: 行解析）
    const reader = resp.body.getReader();
    const decoder = new TextDecoder('utf-8');
    let buffer = '';
    let received = false;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });
      // 按行切（保留最后不完整行在 buffer）
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';
      for (const line of lines) {
        const delta = parseDeltaFields(line);
        if (delta.content) {
          aiBubble.content += delta.content;
          received = true;
        }
        if (delta.reasoning) {
          aiBubble.reasoning = (aiBubble.reasoning ?? '') + delta.reasoning;
          received = true;
        }
        if (delta.content || delta.reasoning) {
          // 节流滚到底（每个 chunk 后滚）
          await mcScrollToEnd();
        }
      }
    }
    // 处理 buffer 剩余
    if (buffer.trim()) {
      const delta = parseDeltaFields(buffer);
      if (delta.content) aiBubble.content += delta.content;
      if (delta.reasoning) aiBubble.reasoning = (aiBubble.reasoning ?? '') + delta.reasoning;
    }
    if (!received && !aiBubble.content) {
      // 流正常结束但无内容（例如只有 finish_reason）
      aiBubble.content = t('llmChat.emptyReply');
    }
  } catch (e) {
    // 网络错误 / vLLM 不可达 → 降级提示，不 panic
    if (mcKind.value === 'instance' && mcSelected.value?.port) {
      aiBubble.content = t('llmChat.instanceUnreachable', {
        host: MC_INSTANCE_HOST,
        port: mcSelected.value.port,
      });
    } else if (mcManualActive.value) {
      // 手动临时目标直连失败：浏览器须本机可达该地址（CORS/跨网均会落这里）
      // ——文案引导改走联邦大厅导入或经源节点中继
      aiBubble.content = t('llmChat.manualUnreachable', {
        url: mcManualChatUrl(mcManualForm.value.base_url.trim()),
      });
    } else {
      aiBubble.content = `${t('llmChat.requestFailed')}${e instanceof Error ? e.message : String(e)}`;
    }
    aiBubble.error = true;
    aiBubble.content += `\n[detail: ${e instanceof Error ? e.message : String(e)}]`;
  } finally {
    aiBubble.streaming = false;
    mcSending.value = false;
    mcSaveHistory();
    await mcScrollToEnd();
  }
}

function mcPushErrorBubble(msg: string): void {
  mcBubbles.value.push({
    id: `e-${Date.now()}`,
    role: 'assistant',
    content: msg,
    error: true,
  });
  mcSaveHistory();
  void mcScrollToEnd();
}

/** 清空对话（含 localStorage）。 */
function mcClearChat(): void {
  if (mcSending.value) return;
  if (mcBubbles.value.length && !window.confirm(t('llmChat.clearConfirm'))) return;
  mcBubbles.value = [];
  mcSaveHistory();
}

function mcOnKeydown(e: KeyboardEvent): void {
  // Ctrl+Enter / Cmd+Enter 发送
  if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
    e.preventDefault();
    void mcSend();
  }
}

// =============================================================================
// Tab「生成」：图片生成（sd-turbo）+ 最近生成 + 视频任务（/api/v1/media/*）
// =============================================================================
// —— 图片生成 ——
/** 宽高/步数输入保持字符串：留空 = 不传（走服务端默认 768×432 / 4 步）。 */
const imgForm = ref({ prompt: '', width: '', height: '', steps: '' });
const imgGenerating = ref(false);
const imgResult = ref<ImageGenResult | null>(null);
/** 503（显存不足）/502（生成失败）错误横幅——文案原样展示（error 已含指引）。 */
const imgError = ref('');

/** 结果 data URL（<img> 渲染 + 下载共用）。 */
const imgResultUrl = computed(() =>
  imgResult.value ? `data:image/png;base64,${imgResult.value.png_base64}` : '',
);

/** 解析可选数字输入：留空/非法 → undefined（省略字段走服务端默认）。 */
function parseOptNum(v: string): number | undefined {
  const t = v.trim();
  if (!t) return undefined;
  const n = Number(t);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : undefined;
}

async function generateImage(): Promise<void> {
  const prompt = imgForm.value.prompt.trim();
  if (!prompt) {
    imgError.value = 'prompt 不可为空';
    return;
  }
  imgGenerating.value = true;
  imgError.value = '';
  imgResult.value = null;
  try {
    imgResult.value = await endpoints.mediaImageGenerate({
      prompt,
      width: parseOptNum(imgForm.value.width),
      height: parseOptNum(imgForm.value.height),
      steps: parseOptNum(imgForm.value.steps),
    });
    await loadRecent();
  } catch (e) {
    // 503/502 服务端 error 文案已含操作指引（如"先停 LLM 实例再生成"），原样展示
    imgError.value = e instanceof Error ? e.message : String(e);
  } finally {
    imgGenerating.value = false;
  }
}

// —— 最近生成（recent，无图，仅信息列表）——
const recentItems = ref<ImageRecentItem[]>([]);
const recentLoading = ref(false);

async function loadRecent(): Promise<void> {
  recentLoading.value = true;
  try {
    const raw = await endpoints.mediaImageRecent();
    recentItems.value = Array.isArray(raw) ? raw : [];
  } catch {
    recentItems.value = [];
  } finally {
    recentLoading.value = false;
  }
}

const recentColumns: Column<ImageRecentItem>[] = [
  { key: 'created_at', title: '时间', width: '170px' },
  { key: 'prompt_summary', title: 'Prompt' },
  { key: 'width', title: '尺寸', width: '100px' },
  { key: 'steps', title: '步数', width: '70px' },
  { key: 'elapsed_ms', title: '耗时', width: '90px', align: 'right' },
];

// —— 视频生成任务（任务框架；当前后端创建即 failed，原因原样展示）——
const videoForm = ref({ prompt: '', duration_secs: '5', backend: 'external' });
const videoCreating = ref(false);
const videoTasks = ref<VideoTask[]>([]);
const videoLoading = ref(false);
const videoError = ref('');

async function loadVideoTasks(): Promise<void> {
  videoLoading.value = true;
  try {
    const raw = await endpoints.mediaVideoTasks();
    videoTasks.value = Array.isArray(raw) ? raw : [];
  } catch {
    videoTasks.value = [];
  } finally {
    videoLoading.value = false;
  }
}

async function createVideoTask(): Promise<void> {
  const prompt = videoForm.value.prompt.trim();
  if (!prompt) {
    videoError.value = 'prompt 不可为空';
    return;
  }
  videoCreating.value = true;
  videoError.value = '';
  try {
    const duration = parseOptNum(videoForm.value.duration_secs);
    await endpoints.mediaVideoCreate({
      prompt,
      duration_secs: duration && duration > 0 ? duration : undefined,
      backend: videoForm.value.backend === 'local' ? 'local' : 'external',
    });
    // 202：任务已创建（当前必 failed，原因在任务列表里展示）
    videoForm.value.prompt = '';
    await loadVideoTasks();
  } catch (e) {
    videoError.value = e instanceof Error ? e.message : String(e);
  } finally {
    videoCreating.value = false;
  }
}

/** 视频任务状态 → 徽章样式/文案。 */
function videoStatusClass(s: string): string {
  switch (s) {
    case 'completed':
      return 'pill-ok';
    case 'processing':
      return 'pill-blue';
    case 'queued':
      return 'pill-muted';
    case 'failed':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}
function videoStatusLabel(s: string): string {
  switch (s) {
    case 'completed':
      return '已完成';
    case 'processing':
      return '生成中';
    case 'queued':
      return '排队中';
    case 'failed':
      return '失败';
    default:
      return s;
  }
}

/** 「生成」Tab 首次切入时懒加载（recent + 视频任务列表都是公开 GET，按需拉取）。 */
let generateLoaded = false;
watch(
  activeTab,
  (tab) => {
    if (tab === 'generate' && !generateLoaded) {
      generateLoaded = true;
      void loadRecent();
      void loadVideoTasks();
    }
    // 「配方库」Tab 首次切入懒加载目录（服务端常驻缓存命中秒回，冷进程仅此
    // 一次外呼）+ 本地保存列表；再次切入不重复拉（刷新只走「刷新目录」按钮）
    if (tab === 'recipes' && !recipesLoaded) {
      recipesLoaded = true;
      void loadRecipes();
      loadSavedRecipes();
    }
    // 「推理环境」Tab 首次切入懒加载；离开即停任务轮询（回来时有 running 任务
    // 会由 loadEnvs → refreshEnvTasks 重新启动）
    if (tab === 'envs') {
      if (!envsLoaded) {
        envsLoaded = true;
        void loadEnvs().then(() => startEnvPolling());
      } else {
        void Promise.all([loadEnvs(), refreshEnvTasks()]).then(() => startEnvPolling());
      }
    } else {
      stopEnvPolling();
    }
    // 「对话」Tab 切入：滚到最新消息 + 预热外部 API 登记列表（目标下拉要用；
    // 联邦条目按需在切到 fed 组时再拉）
    if (tab === 'modelchat') {
      void mcScrollToEnd();
      if (!extLoaded) {
        extLoaded = true;
        void loadExtApis();
      }
    }
    // 「外部 API」Tab 首次切入懒加载登记列表
    if (tab === 'external' && !extLoaded) {
      extLoaded = true;
      void loadExtApis();
    }
    // 「实例监控」Tab 已抽成 <InstanceMonitor /> 组件（轮询/拉取自包含，
    // v-if 挂载随 Tab 激活、卸载即停，无需宿主接线）
  },
  // immediate：深链（?tab=<key>，如旧 /modelhub → ?tab=repo、直链 modelchat）
  // 首载即落在非默认 Tab 时也要触发对应懒加载（否则 watch 不点火、列表空白）
  { immediate: true },
);

// =============================================================================
// 徽章映射
// =============================================================================
function statusClass(s?: string): string {
  switch (s) {
    case 'running':
    case 'starting':
      return 'pill-ok';
    case 'error':
      return 'pill-err';
    case 'stopped':
      return 'pill-muted';
    default:
      return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'running':
      return '运行中';
    case 'starting':
      return '启动中';
    case 'error':
      return '错误';
    case 'stopped':
      return '已停止';
    default:
      return s ?? '—';
  }
}
function sourceClass(s?: string): string {
  return s === 'local' ? 'pill-cyan' : 'pill-blue';
}
/** ISO 时间 → 本地时间串（recent 列表 / 视频任务用；非法原样返回）。 */
function fmtDateTime(s?: string): string {
  if (!s) return '—';
  try {
    return new Date(s).toLocaleString('zh-CN');
  } catch {
    return s;
  }
}
/** 毫秒耗时 → 人类可读（≥1s 用秒）。 */
function fmtMs(ms?: number): string {
  if (ms == null) return '—';
  return ms >= 1000 ? `${(ms / 1000).toFixed(1)} s` : `${ms} ms`;
}
function gpuPct(pct?: number | null): number {
  if (pct == null) return 0;
  return Math.max(0, Math.min(100, pct));
}
/** 显存占用%：统一内存卡（GB10）用 unified 池口径，独立显存卡用 memory_*。 */
function memPct(d: GpuDevice): number {
  const t = d.unified_memory ? d.unified_memory_total_mib : d.memory_total_mib;
  const u = d.unified_memory ? d.unified_memory_used_mib : d.memory_used_mib;
  if (!t) return 0;
  return Math.max(0, Math.min(100, ((u ?? 0) / t) * 100));
}
/** MiB → 「N GB」（1024 进制保留一位小数去尾零，与 API 大厅 vramLabel 同口径：
 *  GB10 统一内存 124609 MiB → 121.7 GB）。 */
function mibToGB(mib?: number | null): string {
  if (!mib) return '';
  const gb = mib / 1024;
  return `${gb % 1 === 0 ? gb : gb.toFixed(1)} GB`;
}
/** 卡的容量徽标：统一内存 → 「统一内存 121.7 GB」；独立显存 → 「0 / 24576 MiB」。 */
function gpuMemBadge(d: GpuDevice): string {
  if (d.unified_memory) {
    const gb = mibToGB(d.unified_memory_total_mib);
    return gb ? `统一内存 ${gb}` : '统一内存（容量未知）';
  }
  return `${d.memory_used_mib ?? 0} / ${d.memory_total_mib ?? 0} MiB`;
}

// =============================================================================
// 刷新与初始化
// =============================================================================
async function refreshAll(): Promise<void> {
  await Promise.all([loadGpu(), loadInstances(), loadStats()]);
}

onMounted(() => {
  mcLoadHistory();
  void refreshAll();
  void loadModelPathOptions();
});

onUnmounted(() => {
  // 组件卸载停任务轮询（防后台定时器泄漏）
  stopEnvPolling();
  stopLogPolling();
});
</script>

<template>
  <div class="llm-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">模型管理</h2>
        <div class="page-sub muted">vLLM 推理 · 模型仓库 · 对话</div>
      </div>
      <div class="head-actions">
        <button
          class="btn btn-small"
          :disabled="instancesLoading || gpuLoading"
          @click="refreshAll"
        >
          <span
            class="spin"
            :class="{ spinning: instancesLoading || gpuLoading }"
            aria-hidden="true"
          >↻</span>
          刷新
        </button>
      </div>
    </div>

    <!-- Tab 切换（两级）：一级组 推理｜仓库｜诊断 + 组内二级 Tab -->
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

    <!-- 二级 Tab：推理/诊断两组本页渲染；仓库组由 ModelHubPanel 自带（v-if 挂载
         随组激活、卸载即停轮询，照 InstanceMonitor 先例） -->
    <nav
      v-if="activeGroup !== 'repo'"
      class="tabs sub-tabs"
      role="tablist"
      aria-label="组内子页切换"
    >
      <button
        v-for="st in activeGroupTabs"
        :key="st.key"
        class="tab sub-tab"
        :class="{ active: activeTab === st.key }"
        role="tab"
        :aria-selected="activeTab === st.key"
        @click="activeTab = st.key"
      >{{ st.label }}</button>
    </nav>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 「仓库」分组：模型仓库面板（本地模型 / 在线下载 / 模型大厅 / Spark 专区） -->
    <section v-if="activeGroup === 'repo'" class="tab-panel">
      <ModelHubPanel />
    </section>

    <!-- 推理/诊断两组的子 Tab 面板：组不活跃时整体卸载（v-if 包装防仓库组下
         旧面板漏显——activeTab 仅在组内有效；卸载同步停 InstanceMonitor 轮询） -->
    <template v-if="activeGroup !== 'repo'">
    <!-- =================== Tab1 实例管理 =================== -->
    <section v-show="activeTab === 'instances'" class="tab-panel">
      <!-- GPU 信息卡（无 GPU 时友好提示）-->
      <div class="card gpu-summary-card">
        <div class="gpu-summary-head">
          <span class="panel-title">GPU</span>
          <span v-if="gpu.available" class="pill" :class="gpu.backend === 'rocm' ? 'pill-purple' : 'pill-ok'">
            {{ gpu.backend }}
          </span>
          <span v-else class="pill pill-muted">未检测到 GPU</span>
        </div>
        <div v-if="gpu.available && (gpu.devices?.length ?? 0) > 0" class="gpu-mini-list">
          <div v-for="d in gpu.devices" :key="d.index" class="gpu-mini-item">
            <span class="gpu-mini-name">{{ d.name }}</span>
            <span v-if="d.unified_memory" class="pill pill-purple">统一内存</span>
            <span class="gpu-mini-mem mono">{{ gpuMemBadge(d) }}</span>
            <span class="prog-wrap">
              <span class="prog-bar"><span class="prog-fill" :style="{ width: memPct(d) + '%' }" /></span>
              <span class="prog-text">{{ Math.round(memPct(d)) }}%</span>
            </span>
          </div>
        </div>
        <div v-else class="empty-inline muted small">
          无可用 GPU（CPU 模式或未安装驱动）。vLLM 需要 GPU 才能运行，但实例仍可创建并尝试启动。
        </div>
      </div>

      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">实例总数</div>
          <div class="stat-value">{{ stats.instances_total ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">运行中</div>
          <div class="stat-value">{{ stats.running ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已停止</div>
          <div class="stat-value">{{ stats.stopped ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">GPU 设备</div>
          <div class="stat-value">{{ stats.gpu_devices ?? 0 }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">推理实例列表</span>
        <button class="btn btn-small btn-primary" @click="openCreate">＋ 创建实例</button>
      </div>

      <div v-if="instancesError" class="error-box">{{ instancesError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="instColumns"
            :rows="instances"
            :loading="instancesLoading"
            empty-text="暂无实例，点击右上角「创建实例」。"
          >
            <template #cell-source_type="{ row }">
              <span class="pill" :class="sourceClass(row.source_type)">{{ row.source_type ?? '—' }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-pid="{ row }">
              <span class="mono">{{ row.pid ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="busyId === row.id || row.status === 'running' || row.status === 'starting'"
                @click.stop="startInst(String(row.id ?? ''))"
              >启动</button>
              <button
                class="btn btn-small btn-warning"
                :disabled="busyId === row.id || row.status === 'stopped'"
                @click.stop="stopInst(String(row.id ?? ''))"
              >停止</button>
              <button
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="healthInst(String(row.id ?? ''))"
              >探测</button>
              <!-- 日志：按实例日志文件尾（starting 时高亮——看模型加载进度） -->
              <button
                class="btn btn-small log-btn"
                :class="{ 'log-btn-starting': row.status === 'starting' }"
                :title="row.status === 'starting' ? t('llmLog.btnTitleStarting') : t('llmLog.btnTitle')"
                @click.stop="openInstLog(String(row.id ?? ''))"
              >{{ t('llmLog.btn') }}</button>
              <!-- 接入说明：实例级接入速查（模型名/上下文/URL/curl 示例；
                   running 才可看——未运行的实例端口未监听，接入信息无意义） -->
              <button
                class="btn btn-small"
                :disabled="row.status !== 'running'"
                :title="row.status === 'running' ? t('llmAccess.btnTitle') : t('llmAccess.btnDisabledTitle')"
                @click.stop="openAccess(row)"
              >{{ t('llmAccess.btn') }}</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === row.id"
                @click.stop="removeInst(String(row.id ?? ''))"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 创建实例对话框 -->
      <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="llm-create-title">
          <div class="modal-head">
            <h3 id="llm-create-title">创建推理实例</h3>
            <button class="modal-close" type="button" :disabled="submitting" @click="closeCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitCreate">
            <!-- Qwen 系列推荐配置：一键填充全部字段（模板场景 autostart 默认勾选） -->
            <div class="preset-box">
              <div class="preset-row">
                <span class="preset-label">Qwen 系列推荐配置</span>
                <div class="preset-btns">
                  <button
                    v-for="p in qwenPresets"
                    :key="p.key"
                    type="button"
                    class="btn btn-small preset-btn"
                    :class="{ 'preset-active': activePreset === p.key }"
                    :disabled="submitting"
                    @click="applyPreset(p)"
                  >{{ p.label }}</button>
                  <button
                    type="button"
                    class="btn btn-small preset-btn"
                    :disabled="submitting"
                    @click="clearPresetForm"
                  >清空</button>
                </div>
              </div>
              <p class="preset-hint muted small">
                Qwen3.5：max_num_seqs≤31（GDN 缓存块限制）；端口留空自动选（8123 起）；日志按实例 /tmp/llm-vllm-&lt;id&gt;.log
              </p>
            </div>
            <div class="field">
              <label for="llm-name">实例名称</label>
              <input id="llm-name" v-model="form.name" type="text" placeholder="Qwen2.5-7B 对话" :disabled="submitting" />
            </div>
            <div class="field">
              <label for="llm-model">模型名或路径</label>
              <input
                id="llm-model"
                v-model="form.model"
                type="text"
                list="llm-model-path-options"
                placeholder="Qwen/Qwen2.5-7B-Instruct 或 /tank/models/xxx"
                :disabled="submitting"
              />
              <!-- 本地模型库 + HF 缓存路径建议（datalist；不选也可自由手输） -->
              <datalist id="llm-model-path-options">
                <option v-for="p in modelPathOptions" :key="p" :value="p" />
              </datalist>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="llm-src">来源</label>
                <select id="llm-src" v-model="form.source_type" :disabled="submitting">
                  <option value="huggingface">huggingface（自动拉取）</option>
                  <option value="local">local（本地路径）</option>
                </select>
              </div>
              <div class="field">
                <label for="llm-host">host</label>
                <input id="llm-host" v-model="form.host" type="text" :disabled="submitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <!-- 端口：空 = 自动（后端实例表去重 + 真实试绑，8123 起） -->
                <label for="llm-port">{{ t('llmLog.portLabel') }}</label>
                <input
                  id="llm-port"
                  v-model="form.port_text"
                  type="number"
                  min="1024"
                  max="65535"
                  :placeholder="t('llmLog.portPh')"
                  :disabled="submitting"
                />
                <span class="muted small">{{ t('llmLog.portHint') }}</span>
              </div>
              <div class="field">
                <label>自动启动</label>
                <label class="switch">
                  <input v-model="form.autostart" type="checkbox" :disabled="submitting" />
                  autostart（创建后立即拉起 vllm）
                </label>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="llm-tp">tensor_parallel_size</label>
                <input id="llm-tp" v-model.number="form.tensor_parallel_size" type="number" min="1" :disabled="submitting" />
              </div>
              <div class="field">
                <label for="llm-gmu">gpu_memory_utilization</label>
                <input id="llm-gmu" v-model.number="form.gpu_memory_utilization" type="number" min="0" max="1" step="0.05" :disabled="submitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="llm-mml">max_model_len</label>
                <input id="llm-mml" v-model.number="form.max_model_len" type="number" min="1" :disabled="submitting" />
              </div>
              <div class="field">
                <label for="llm-q">quantization</label>
                <select id="llm-q" v-model="form.quantization" :disabled="submitting">
                  <option value="">None</option>
                  <option value="awq">awq</option>
                  <option value="gptq">gptq</option>
                </select>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="llm-dt">dtype</label>
                <select id="llm-dt" v-model="form.dtype" :disabled="submitting">
                  <option value="auto">auto</option>
                  <option value="float16">float16</option>
                  <option value="bfloat16">bfloat16</option>
                </select>
              </div>
              <div class="field">
                <label>信任远程代码</label>
                <label class="switch">
                  <input v-model="form.trust_remote_code" type="checkbox" :disabled="submitting" />
                  trust_remote_code
                </label>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="llm-smn">served_model_name</label>
                <input id="llm-smn" v-model="form.served_model_name" type="text" placeholder="API 对外模型名（默认同模型名）" :disabled="submitting" />
              </div>
              <!-- 推理环境：可选指定拉起用 venv（空 = 默认环境；「推理环境」Tab 管理） -->
              <div class="field">
                <label for="llm-env">{{ t('llmEnv.instanceEnvLabel') }}</label>
                <select id="llm-env" v-model="form.env_name" :disabled="submitting">
                  <option value="">{{ t('llmEnv.instanceDefaultEnv') }}</option>
                  <option v-for="e in envs" :key="e.name" :value="e.name">
                    {{ e.name }}（{{ envStatusLabel(e.status) }}{{ e.is_default ? ' · 默认' : '' }}）
                  </option>
                </select>
              </div>
            </div>
            <!-- extra_args：可折叠文本域，每行一个参数（flag 与取值各占一行） -->
            <div class="field">
              <button
                type="button"
                class="btn btn-small extra-toggle"
                :disabled="submitting"
                @click="showExtraArgs = !showExtraArgs"
              >
                {{ showExtraArgs ? '收起' : '展开' }} extra_args{{ extraArgsList.length ? `（${extraArgsList.length} 项）` : '' }} {{ showExtraArgs ? '▴' : '▾' }}
              </button>
              <textarea
                v-if="showExtraArgs"
                id="llm-extra"
                v-model="form.extra_args_text"
                class="extra-args-input mono"
                rows="6"
                placeholder="--enable-prefix-caching&#10;--enforce-eager"
                :disabled="submitting"
              />
              <span v-if="showExtraArgs" class="muted small">每行一个参数，按原样追加到 vllm serve 命令（带空格的取值如 JSON 整行放一行即可）。</span>
            </div>
            <p class="muted small">{{ t('llmLog.portFooterHint') }}</p>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="submitting" @click="closeCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="submitting">
                {{ submitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab 推理环境（vLLM Python venv 管理） =================== -->
    <section v-show="activeTab === 'envs'" class="tab-panel">
      <!-- 创建表单卡 -->
      <div class="card env-create-card">
        <div class="panel-title">{{ t('llmEnv.createTitle') }}</div>
        <p class="muted small">{{ t('llmEnv.createHint') }}</p>
        <form class="env-create-form" @submit.prevent="submitEnvCreate">
          <div class="field">
            <label for="env-name">{{ t('llmEnv.fieldName') }}</label>
            <input
              id="env-name"
              v-model="envForm.name"
              type="text"
              class="mono"
              :placeholder="t('llmEnv.fieldNamePh')"
              :disabled="envCreating"
            />
          </div>
          <div class="field">
            <label for="env-py">{{ t('llmEnv.fieldPython') }}</label>
            <select id="env-py" v-model="envForm.python_version" :disabled="envCreating">
              <option v-for="p in envPythonOptions" :key="p" :value="p">{{ p }}</option>
            </select>
          </div>
          <div class="field">
            <label for="env-channel">{{ t('llmEnv.channel') }}</label>
            <select id="env-channel" v-model="envForm.channel" :disabled="envCreating">
              <option value="stable">{{ t('llmEnv.channelStable') }}</option>
              <option value="nightly">{{ t('llmEnv.channelNightly') }}</option>
            </select>
          </div>
          <div class="field">
            <label for="env-vllm">{{ t('llmEnv.fieldVllm') }}</label>
            <input
              id="env-vllm"
              v-model="envForm.vllm_version"
              type="text"
              class="mono"
              placeholder="latest"
              :disabled="envCreating || envCreateNightly"
            />
            <span v-if="envCreateNightly" class="muted small">{{ t('llmEnv.channelNightlyHint') }}</span>
          </div>
          <div class="field env-create-submit">
            <button type="submit" class="btn btn-primary" :disabled="envCreating">
              <span v-if="envCreating" class="spin spinning" aria-hidden="true">↻</span>
              {{ envCreating ? t('llmEnv.createSubmitting') : t('llmEnv.createSubmit') }}
            </button>
          </div>
        </form>
        <!-- 安装命令预览：与后端实际执行的 argv 同构（nightly 为预置示例命令） -->
        <div class="env-cmd-preview">
          <div class="muted small">{{ t('llmEnv.cmdPreview') }}</div>
          <code class="mono">{{ envInstallPreview(envForm.channel, envForm.name.trim(), envForm.vllm_version) }}</code>
        </div>
      </div>

      <div v-if="envsError" class="error-box">{{ t('llmEnv.loadFailed') }}：{{ envsError }}</div>

      <!-- 环境卡列表 -->
      <div class="panel-head">
        <span class="panel-title">{{ t('llmEnv.listTitle') }}</span>
        <span class="muted small">
          {{ t('llmEnv.defaultIs') }}
          <span class="mono">{{ envsDefaultName || t('llmEnv.defaultNone') }}</span>
        </span>
      </div>
      <div v-if="envsLoading && envs.length === 0" class="card empty-card">{{ t('llmEnv.loading') }}</div>
      <div v-else-if="envs.length === 0" class="card empty-card">{{ t('llmEnv.empty') }}</div>
      <div v-else class="env-grid">
        <div v-for="e in envs" :key="e.name" class="card env-card">
          <div class="env-card-head">
            <span class="env-name mono">{{ e.name }}</span>
            <span v-if="e.is_default" class="pill pill-purple">{{ t('llmEnv.defaultBadge') }}</span>
            <span v-if="e.channel === 'nightly'" class="pill pill-blue">{{ t('llmEnv.channelNightlyBadge') }}</span>
            <span class="pill" :class="envStatusClass(e.status)">
              <span
                v-if="e.status === 'creating' || e.status === 'updating'"
                class="spin spinning env-status-spin"
                aria-hidden="true"
              >↻</span>
              {{ envStatusLabel(e.status) }}
            </span>
          </div>
          <div class="env-meta">
            <div class="env-meta-row">
              <span class="env-meta-label">vLLM</span>
              <span class="mono">
                {{ e.vllm_version_installed || '—' }}
                <span
                  v-if="envVersionMismatch(e)"
                  class="env-mismatch"
                  :title="t('llmEnv.mismatchTitle')"
                >⚠ {{ t('llmEnv.mismatch', { wanted: e.vllm_version_requested ?? '' }) }}</span>
              </span>
            </div>
            <div class="env-meta-row">
              <span class="env-meta-label">{{ t('llmEnv.metaPython') }}</span>
              <span class="mono">{{ e.python_version || '—' }}</span>
            </div>
            <div class="env-meta-row">
              <span class="env-meta-label">{{ t('llmEnv.metaSize') }}</span>
              <span class="mono">{{ fmtBytes(e.size_bytes) }}</span>
            </div>
            <div class="env-meta-row">
              <span class="env-meta-label">{{ t('llmEnv.metaUpdated') }}</span>
              <span>{{ fmtEpoch(e.updated_at) }}</span>
            </div>
            <div class="env-meta-row env-path-row">
              <span class="env-meta-label">{{ t('llmEnv.metaPath') }}</span>
              <span class="mono muted small">{{ e.path }}</span>
            </div>
          </div>
          <div v-if="e.status === 'error' && e.last_error" class="error-box env-error-box">
            {{ e.last_error }}
          </div>
          <div class="env-actions">
            <button
              class="btn btn-small"
              :disabled="envBusyName === e.name || e.status === 'creating' || e.status === 'updating'"
              @click="openEnvUpdate(e)"
            >{{ t('llmEnv.actionUpdate') }}</button>
            <button
              class="btn btn-small"
              :disabled="envBusyName === e.name || e.is_default"
              :title="e.is_default ? t('llmEnv.alreadyDefault') : ''"
              @click="setEnvDefault(e)"
            >{{ t('llmEnv.actionSetDefault') }}</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="envBusyName === e.name || e.is_default"
              :title="e.is_default ? t('llmEnv.defaultDeleteHint') : ''"
              @click="removeEnv(e)"
            >{{ t('llmEnv.actionDelete') }}</button>
          </div>
        </div>
      </div>

      <!-- 任务面板：进行中任务轮询 2s，日志尾自动滚动 -->
      <div class="panel-head">
        <span class="panel-title">{{ t('llmEnv.taskPanel') }}</span>
        <span class="muted small">
          <span v-if="envHasRunningTask" class="spin spinning" aria-hidden="true">↻</span>
          {{ envHasRunningTask ? t('llmEnv.taskPolling') : t('llmEnv.taskIdle') }}
        </span>
      </div>
      <div v-if="envTasks.length === 0" class="card empty-card">{{ t('llmEnv.taskEmpty') }}</div>
      <div v-else class="card env-task-card">
        <div class="env-task-list">
          <button
            v-for="task in envTasks"
            :key="task.id"
            class="env-task-item"
            :class="{ active: envActiveTask?.id === task.id }"
            type="button"
            @click="loadEnvTaskDetail(task.id)"
          >
            <span class="mono">{{ task.id }}</span>
            <span class="muted">{{ task.kind === 'create' ? t('llmEnv.kindCreate') : t('llmEnv.kindUpdate') }}</span>
            <span class="mono muted">{{ task.env_name }}</span>
            <span class="pill" :class="task.status === 'error' ? 'pill-err' : task.status === 'running' ? 'pill-blue' : 'pill-ok'">
              {{ envTaskStatusLabel(task.status) }}
            </span>
          </button>
        </div>
        <div v-if="envActiveTask" class="env-log-wrap">
          <div ref="envLogBox" class="env-log mono">
            <div v-for="(line, i) in envActiveTask.log?.slice(-20)" :key="i" class="env-log-line">{{ line }}</div>
          </div>
          <div class="muted small env-log-hint">{{ t('llmEnv.logHint') }}</div>
        </div>
      </div>

      <!-- 更新对话框：切渠道（stable↔nightly 重装）+ 目标 vLLM 版本 -->
      <div v-if="showEnvUpdate" class="modal-backdrop" @click.self="showEnvUpdate = false">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="env-update-title">
          <div class="modal-head">
            <h3 id="env-update-title">{{ t('llmEnv.updateTitle', { name: envUpdateTarget?.name ?? '' }) }}</h3>
            <button class="modal-close" type="button" :disabled="envUpdating" @click="showEnvUpdate = false">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitEnvUpdate">
            <div class="field">
              <label for="env-update-channel">{{ t('llmEnv.channel') }}</label>
              <select id="env-update-channel" v-model="envUpdateChannel" :disabled="envUpdating">
                <option value="stable">{{ t('llmEnv.channelStable') }}</option>
                <option value="nightly">{{ t('llmEnv.channelNightly') }}</option>
              </select>
              <span class="muted small">
                {{ t('llmEnv.updateCurrent') }}
                <span class="mono">{{ envUpdateTarget?.vllm_version_installed || '—' }}</span>
              </span>
            </div>
            <div class="field">
              <label for="env-update-v">{{ t('llmEnv.fieldVllm') }}</label>
              <input
                id="env-update-v"
                v-model="envUpdateVersion"
                type="text"
                class="mono"
                placeholder="latest"
                :disabled="envUpdating || envUpdateNightly"
              />
              <span v-if="envUpdateNightly" class="muted small">{{ t('llmEnv.channelNightlyHint') }}</span>
            </div>
            <div class="env-cmd-preview">
              <div class="muted small">{{ t('llmEnv.cmdPreview') }}</div>
              <code class="mono">{{ envUpdatePreview }}</code>
            </div>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="envUpdating" @click="showEnvUpdate = false">
                {{ t('llmEnv.btnCancel') }}
              </button>
              <button type="submit" class="btn btn-primary" :disabled="envUpdating">
                {{ envUpdating ? t('llmEnv.updateSubmitting') : t('llmEnv.updateSubmit') }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab 配方库（vLLM Recipes 导入） =================== -->
    <section v-show="activeTab === 'recipes'" class="tab-panel">
      <!-- 说明 + 操作 -->
      <div class="card rc-intro">
        <div class="rc-intro-text">
          <span class="panel-title">vLLM Recipes 官方配方库</span>
          <span class="muted small">
            {{ t('llmRecipes.sub', { n: recipes.length || '…' }) }}
            {{ t('llmRecipes.lastFetched') }}{{ recipesCachedAt ? fmtDateTime(recipesCachedAt) : '—' }}
          </span>
        </div>
        <div class="head-actions">
          <a class="btn btn-small" :href="RECIPES_SITE" target="_blank" rel="noopener">查看官网 ↗</a>
          <button class="btn btn-small" :disabled="recipesLoading" @click="loadRecipes(true)">
            <span class="spin" :class="{ spinning: recipesLoading }" aria-hidden="true">↻</span>
            {{ t('llmRecipes.refreshBtn') }}
          </button>
        </div>
      </div>

      <div v-if="recipesError" class="error-box">目录拉取失败：{{ recipesError }}</div>

      <!-- 官方同款树状目录：左侧可折叠提供方树 + 右侧选中速览 -->
      <div class="panel-head">
        <span class="panel-title">配方目录（{{ filteredRecipes.length }} / {{ recipes.length }}）</span>
        <input
          v-model="recipeSearch"
          class="rc-search"
          type="search"
          placeholder="搜索标题 / 提供方 / HF ID…（命中组自动展开）"
        />
      </div>

      <div class="rc-tree-layout panel">
        <!-- 左侧：提供方 → 模型 两级树（父节点带数量徽章，搜索过滤树） -->
        <div class="card rc-tree">
          <div v-if="recipesLoading" class="rc-tree-loading muted">目录加载中…</div>
          <template v-else-if="recipeTree.length">
            <div v-for="g in recipeTree" :key="g.provider" class="rc-tree-group">
              <button
                type="button"
                class="rc-tree-parent"
                :aria-expanded="providerExpanded(g.provider)"
                @click="toggleProvider(g.provider)"
              >
                <span class="rc-tree-caret" :class="{ open: providerExpanded(g.provider) }">▸</span>
                <span class="rc-tree-provider">{{ g.provider || '未标注提供方' }}</span>
                <span class="rc-tree-count">{{ g.items.length }}</span>
              </button>
              <ul v-if="providerExpanded(g.provider)" class="rc-tree-children">
                <li v-for="r in g.items" :key="r.hf_id">
                  <button
                    type="button"
                    class="rc-tree-leaf"
                    :class="{ selected: selectedRecipe?.hf_id === r.hf_id }"
                    :title="r.hf_id"
                    @click="selectRecipe(r)"
                  >
                    <span class="rc-tree-title">{{ r.title || r.hf_id }}</span>
                    <span class="rc-tree-hf mono">{{ r.hf_id }}</span>
                  </button>
                </li>
              </ul>
            </div>
          </template>
          <!-- 空态（未拉取 / 搜索后无匹配） -->
          <div v-else class="rc-tree-loading">{{ recipeEmptyText }}</div>
        </div>

        <!-- 右侧：选中模型速览（点击子节点即选中并打开完整详情模态） -->
        <div class="card rc-quick">
          <template v-if="selectedRecipe">
            <div class="rc-quick-head">
              <span class="rc-title">{{ selectedRecipe.title || selectedRecipe.hf_id }}</span>
              <span v-if="selectedRecipe.provider" class="pill pill-blue">{{ selectedRecipe.provider }}</span>
            </div>
            <div class="mono muted small">{{ selectedRecipe.hf_id }}</div>
            <div v-if="selectedRecipe.date_updated" class="muted small">
              更新 {{ selectedRecipe.date_updated }}
            </div>
            <div class="rc-quick-actions">
              <button class="btn btn-small btn-primary" @click="openRecipe(selectedRecipe.hf_id)">
                查看完整配方
              </button>
              <a
                class="btn btn-small"
                :href="recipeSiteUrl(selectedRecipe.hf_id)"
                target="_blank"
                rel="noopener"
              >查看网站 ↗</a>
            </div>
            <div class="muted small rc-quick-hint">
              点击左侧任一模型即在此速览并弹出完整配方（推荐启动命令 / 硬件需求 / 精度变体）。
            </div>
          </template>
          <div v-else class="rc-quick-empty">
            <div class="rc-quick-empty-title">从左侧选择模型</div>
            <div class="muted small">
              提供方为父节点（可折叠，带数量徽章），模型为子节点——与
              <a :href="RECIPES_SITE" target="_blank" rel="noopener">recipes.vllm.ai</a>
              官方站同款结构；顶部搜索框过滤树（标题 / 提供方 / HF ID）。
            </div>
          </div>
        </div>
      </div>

      <!-- 本地保存的配方（localStorage；后续可接节点执行） -->
      <div class="panel-head">
        <span class="panel-title">本地保存的配方（{{ savedRecipes.length }}）</span>
        <span class="muted small">存于本浏览器 localStorage（llm-recipes-saved）</span>
      </div>
      <div v-if="savedRecipes.length === 0" class="card empty-card">
        暂无本地配方。打开任一配方详情，点击「存为本地配方」。
      </div>
      <ul v-else class="rc-saved-list">
        <li v-for="s in savedRecipes" :key="s.hf_id" class="rc-saved-item">
          <div class="rc-saved-head">
            <span class="rc-title">{{ s.title }}</span>
            <span v-if="s.provider" class="pill pill-muted">{{ s.provider }}</span>
            <span v-if="s.hardware" class="pill pill-cyan">{{ s.hardware }}</span>
            <span class="mono muted small">{{ s.hf_id }}</span>
          </div>
          <pre v-if="s.command" class="rc-cmd mono">{{ s.command }}</pre>
          <div class="rc-saved-actions">
            <button class="btn btn-small" @click="copySavedCommand(s.command)">复制命令</button>
            <a class="btn btn-small" :href="recipeSiteUrl(s.hf_id)" target="_blank" rel="noopener">查看网站 ↗</a>
            <button class="btn btn-small btn-danger" @click="removeSavedRecipe(s.hf_id)">删除</button>
          </div>
        </li>
      </ul>

      <!-- 配方详情（模态） -->
      <div v-if="showRecipeDetail" class="modal-backdrop" @click.self="closeRecipe">
        <div class="modal rc-modal" role="dialog" aria-modal="true" aria-labelledby="rc-detail-title">
          <div class="modal-head">
            <h3 id="rc-detail-title">{{ recipeDetailTitle }}</h3>
            <button class="modal-close" type="button" @click="closeRecipe">×</button>
          </div>
          <div class="modal-body rc-detail-body">
            <div v-if="recipeDetailLoading" class="empty-card">配方加载中…</div>
            <div v-else-if="recipeDetailError" class="error-box">{{ recipeDetailError }}</div>
            <template v-else-if="recipeDetail">
              <!-- meta：HF ID / 提供方 / 难度 / 任务 / 更新时间 -->
              <div class="rc-meta-row">
                <span class="mono muted small">{{ recipeDetailHfId }}</span>
                <span v-if="recipeDetail.meta?.provider" class="pill pill-blue">{{ recipeDetail.meta.provider }}</span>
                <span v-if="recipeDetail.meta?.difficulty" class="pill pill-purple">{{ difficultyLabel(recipeDetail.meta.difficulty) }}</span>
                <span
                  v-for="t in recipeDetail.meta?.tasks ?? []"
                  :key="t"
                  class="pill pill-muted"
                >{{ t }}</span>
                <span v-if="recipeDetail.meta?.date_updated" class="muted small">
                  更新 {{ recipeDetail.meta.date_updated }}
                </span>
              </div>
              <p v-if="recipeDetail.meta?.description" class="rc-desc">{{ recipeDetail.meta.description }}</p>

              <!-- 推荐部署命令 -->
              <div v-if="recipeDetail.recommended_command" class="rc-section">
                <div class="rc-section-head">
                  <span class="panel-title">推荐部署命令</span>
                  <div class="head-actions">
                    <span v-if="recipeDetail.recommended_command.hardware" class="pill pill-cyan">
                      {{ recipeDetail.recommended_command.hardware }}
                    </span>
                    <span v-if="recipeDetail.recommended_command.strategy" class="pill pill-muted">
                      {{ recipeDetail.recommended_command.strategy }}
                    </span>
                    <button class="btn btn-small" @click="copyRecipeCommand">
                      {{ recipeCopied ? '已复制 ✓' : '复制启动命令' }}
                    </button>
                  </div>
                </div>
                <pre class="rc-cmd mono">{{ recipeDetail.recommended_command.command || '—' }}</pre>
                <template v-if="recipeDetail.recommended_command.docker_command">
                  <span class="muted small">
                    Docker（{{ recipeDetail.recommended_command.docker_image || '官方镜像' }}）
                  </span>
                  <pre class="rc-cmd mono">{{ recipeDetail.recommended_command.docker_command }}</pre>
                </template>
              </div>

              <!-- 精度变体（最低显存） -->
              <div v-if="recipeVariants.length" class="rc-section">
                <span class="panel-title">精度变体（最低显存需求）</span>
                <table class="rc-variants">
                  <thead>
                    <tr><th>变体</th><th>精度</th><th>VRAM 最低</th><th>说明</th></tr>
                  </thead>
                  <tbody>
                    <tr v-for="v in recipeVariants" :key="v.name">
                      <td class="mono small">{{ v.name }}</td>
                      <td>{{ v.precision ?? '—' }}</td>
                      <td class="mono">{{ v.vram_minimum_gb != null ? `${v.vram_minimum_gb} GB` : '—' }}</td>
                      <td class="muted small">{{ v.description ?? '—' }}</td>
                    </tr>
                  </tbody>
                </table>
              </div>

              <!-- 部署指南（上游 Markdown，折叠） -->
              <details v-if="recipeGuideHtml" class="rc-guide">
                <summary>部署指南（Markdown）</summary>
                <!-- 上游官方站内容，信任模型同 DevDocs.vue（无 dompurify） -->
                <div class="rc-guide-body" v-html="recipeGuideHtml" />
              </details>

              <div class="form-actions rc-footer">
                <a class="btn" :href="recipeSiteUrl(recipeDetailHfId)" target="_blank" rel="noopener">
                  查看网站 ↗
                </a>
                <button class="btn btn-primary" @click="saveRecipeToLocal">存为本地配方</button>
              </div>
            </template>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 生成（图片 / 视频） =================== -->
    <section v-show="activeTab === 'generate'" class="tab-panel">
      <!-- —— 图片生成（sd-turbo，需 admin token；503=显存不足 / 502=生成失败）—— -->
      <div class="card gen-card">
        <div class="panel-head gen-card-head">
          <span class="panel-title">图片生成（sd-turbo）</span>
          <span class="muted small">与 LLM 实例共用 GPU——显存不足时先停推理实例</span>
        </div>
        <div class="gen-form">
          <div class="field">
            <label for="gen-prompt">Prompt *</label>
            <textarea
              id="gen-prompt"
              v-model="imgForm.prompt"
              class="gen-prompt-input"
              rows="3"
              placeholder="描述要生成的画面，如：赛博朋克城市夜景，霓虹灯，电影感构图"
              :disabled="imgGenerating"
            />
          </div>
          <div class="field-row">
            <div class="field">
              <label for="gen-width">宽度</label>
              <input
                id="gen-width"
                v-model="imgForm.width"
                type="number" min="256" max="1024" step="64"
                placeholder="默认 768"
                :disabled="imgGenerating"
              />
            </div>
            <div class="field">
              <label for="gen-height">高度</label>
              <input
                id="gen-height"
                v-model="imgForm.height"
                type="number" min="256" max="1024" step="64"
                placeholder="默认 432"
                :disabled="imgGenerating"
              />
            </div>
            <div class="field">
              <label for="gen-steps">步数</label>
              <input
                id="gen-steps"
                v-model="imgForm.steps"
                type="number" min="1" max="8"
                placeholder="默认 4"
                :disabled="imgGenerating"
              />
            </div>
            <div class="field gen-submit-field">
              <label class="gen-hidden-label">操作</label>
              <button
                class="btn btn-primary gen-submit-btn"
                :disabled="imgGenerating"
                @click="generateImage"
              >
                <span
                  class="spin"
                  :class="{ spinning: imgGenerating }"
                  aria-hidden="true"
                >↻</span>
                {{ imgGenerating ? '生成中…（最长约 60s）' : '生成' }}
              </button>
            </div>
          </div>
          <p class="muted small">
            宽高须为 64 的倍数（256–1024）；留空用服务端默认 768×432（16:9，不受 64 倍数约束）。
          </p>
        </div>
        <!-- 503/502 错误横幅：文案原样展示（error 已含指引，如"先停 LLM 实例再生成"） -->
        <div v-if="imgError" class="error-box">{{ imgError }}</div>
        <!-- 结果预览 + 下载 -->
        <div v-if="imgResult" class="gen-result">
          <img
            class="gen-image"
            :src="imgResultUrl"
            :width="imgResult.width"
            :height="imgResult.height"
            alt="生成结果"
          />
          <div class="gen-result-meta">
            <span class="muted small">
              {{ imgResult.width }}×{{ imgResult.height }} · 耗时 {{ fmtMs(imgResult.elapsed_ms) }}
            </span>
            <a
              class="btn btn-small gen-download-btn"
              :href="imgResultUrl"
              :download="`nexos-${imgResult.id}.png`"
            >下载 PNG</a>
          </div>
        </div>
      </div>

      <!-- —— 最近生成（环形 50 条，无图，仅信息）—— -->
      <div class="panel-head">
        <span class="panel-title">最近生成</span>
        <button class="btn btn-small" :disabled="recentLoading" @click="loadRecent">
          <span class="spin" :class="{ spinning: recentLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="recentColumns"
            :rows="recentItems"
            :loading="recentLoading"
            empty-text="暂无生成记录。"
          >
            <template #cell-created_at="{ row }">
              <span class="muted small">{{ fmtDateTime(row.created_at) }}</span>
            </template>
            <template #cell-width="{ row }">
              <span class="mono small">{{ row.width }}×{{ row.height }}</span>
            </template>
            <template #cell-elapsed_ms="{ row }">
              <span class="mono small">{{ fmtMs(row.elapsed_ms) }}</span>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- —— 视频生成任务（任务框架；当前后端无可用后端，任务创建即 failed 附指引）—— -->
      <div class="card gen-card">
        <div class="panel-head gen-card-head">
          <span class="panel-title">视频生成任务</span>
          <button class="btn btn-small" :disabled="videoLoading" @click="loadVideoTasks">
            <span class="spin" :class="{ spinning: videoLoading }" aria-hidden="true">↻</span>
            刷新
          </button>
        </div>
        <div class="gen-form">
          <div class="field">
            <label for="vid-prompt">Prompt *</label>
            <textarea
              id="vid-prompt"
              v-model="videoForm.prompt"
              class="gen-prompt-input"
              rows="2"
              placeholder="描述要生成的视频，如：日落延时摄影，海浪拍岸"
              :disabled="videoCreating"
            />
          </div>
          <div class="field-row">
            <div class="field">
              <label for="vid-duration">时长（秒，1–30）</label>
              <input
                id="vid-duration"
                v-model="videoForm.duration_secs"
                type="number" min="1" max="30"
                placeholder="默认 5"
                :disabled="videoCreating"
              />
            </div>
            <div class="field">
              <label for="vid-backend">后端</label>
              <select id="vid-backend" v-model="videoForm.backend" :disabled="videoCreating">
                <option value="external">external（外部 API）</option>
                <option value="local">local（本地模型）</option>
              </select>
            </div>
            <div class="field gen-submit-field">
              <label class="gen-hidden-label">操作</label>
              <button
                class="btn btn-primary gen-submit-btn"
                :disabled="videoCreating"
                @click="createVideoTask"
              >{{ videoCreating ? '创建中…' : '创建任务' }}</button>
            </div>
          </div>
        </div>
        <p v-if="videoError" class="form-msg is-err">{{ videoError }}</p>
        <div v-if="videoTasks.length === 0" class="empty-inline muted small">
          暂无任务。创建后会立即尝试提交——当前无可用视频后端时任务会直接标记失败并给出原因。
        </div>
        <ul v-else class="gen-task-list">
          <li v-for="t in videoTasks" :key="t.id" class="gen-task-item">
            <div class="gen-task-head">
              <span class="pill" :class="videoStatusClass(t.status)">{{ videoStatusLabel(t.status) }}</span>
              <span class="gen-task-prompt">{{ t.prompt }}</span>
              <span class="muted small mono">{{ t.id }}</span>
            </div>
            <div class="gen-task-meta muted small">
              <span>{{ t.duration_secs }}s · {{ t.backend }} · {{ fmtDateTime(t.created_at) }}</span>
              <a v-if="t.video_url" :href="t.video_url" target="_blank" rel="noopener">查看视频</a>
            </div>
            <p v-if="t.status === 'failed' && t.error" class="form-msg is-err gen-task-error">
              {{ t.error }}
            </p>
          </li>
        </ul>
      </div>
    </section>

    <!-- =================== Tab3 对话（统一目标选择器：本地实例 / 外部 API / 联邦大厅） =================== -->
    <!-- 布局照 IM Chat.vue 成熟模式：面板 flex 填满窗口体 + 消息区内部滚动 +
         composer 钉底（flex-shrink:0），无任何 100vh 公式 -->
    <section v-show="activeTab === 'modelchat'" class="tab-panel mc-panel">
      <!-- 目标选择器（三组）：种类下拉 + 按种类条件渲染的目标下拉/模型下拉 -->
      <div class="panel-head mc-head">
        <div class="head-actions mc-selectors">
          <select v-model="mcKind" class="mc-select mc-kind" :disabled="mcSending">
            <option value="instance">{{ t('llmChat.kindInstance') }}</option>
            <option value="extapi">{{ t('llmChat.kindExtapi') }}</option>
            <option value="fed">{{ t('llmChat.kindFed') }}</option>
          </select>

          <!-- a. 本地实例：running 实例下拉（直连端口 SSE 流式） -->
          <select
            v-if="mcKind === 'instance'"
            v-model="mcInstanceId"
            class="mc-select"
            :disabled="mcSending"
          >
            <option value="" disabled>{{ t('llmChat.instancePh') }}</option>
            <option
              v-for="i in mcRunningInstances"
              :key="String(i.id ?? '')"
              :value="String(i.id ?? '')"
            >
              {{ i.name || i.model }} · :{{ i.port }}
            </option>
          </select>

          <!-- b. 外部 API：登记下拉 + 模型下拉（models 来自登记/连通测试回填；
               via_node 非空的联邦导入条目附 🌐 经源节点中继 徽章（失败信息
               透传服务端「经 <节点> 中继失败：<原因>」文案））；
               下拉末项「＋ 手动输入」→ 展开行内小表单（临时/登记双动作） -->
          <template v-else-if="mcKind === 'extapi'">
            <select v-model="mcExtApiId" class="mc-select" :disabled="mcSending">
              <option value="" disabled>{{ t('llmChat.extapiPh') }}</option>
              <option v-for="a in extApis" :key="a.id" :value="a.id">
                {{ a.name }}<template v-if="a.models?.length"> · {{ a.models.length }} 模型</template><template v-if="a.via_node"> · 🌐</template>
              </option>
              <option :value="MC_MANUAL_ID">＋ {{ t('llmChat.manualOption') }}</option>
            </select>
            <span
              v-if="mcExtTarget?.via_node"
              class="pill pill-fed mc-relay-badge"
              :title="`${t('llmExt.relayBadge')} · via_node ${mcExtTarget.via_node}`"
            >🌐 {{ t('llmExt.relayBadge') }}</span>
            <select
              v-if="mcModelOptions.length"
              v-model="mcModel"
              class="mc-select"
              :disabled="mcSending"
            >
              <option v-for="m in mcModelOptions" :key="m" :value="m">{{ m }}</option>
            </select>
          </template>

          <!-- c. 联邦大厅：scope=fed 条目下拉（选中即一键导入为外部 API 登记） -->
          <select
            v-else
            v-model="mcFedListingId"
            class="mc-select mc-fed-select"
            :disabled="mcSending || mcFedImporting || mcFedLoading"
          >
            <option value="" disabled>
              {{ mcFedLoading ? t('llmChat.fedLoading') : t('llmChat.fedPh') }}
            </option>
            <option v-for="l in mcFedListings" :key="String(l.id ?? '')" :value="String(l.id ?? '')">
              {{ l.api_name }}<template v-if="l.source_node"> · {{ l.source_node }}</template>
            </option>
          </select>
        </div>
        <div class="head-actions">
          <button class="btn btn-small" :disabled="mcSending" @click="mcClearChat">
            {{ t('llmChat.clear') }}
          </button>
          <button
            v-if="mcKind === 'instance'"
            class="btn btn-small"
            :disabled="instancesLoading"
            @click="loadInstances"
          >
            <span
              class="spin"
              :class="{ spinning: instancesLoading }"
              aria-hidden="true"
            >↻</span>
            {{ t('llmChat.refresh') }}
          </button>
          <button
            v-else-if="mcKind === 'extapi'"
            class="btn btn-small"
            :disabled="extLoading"
            @click="loadExtApis"
          >
            <span
              class="spin"
              :class="{ spinning: extLoading }"
              aria-hidden="true"
            >↻</span>
            {{ t('llmChat.refresh') }}
          </button>
          <button
            v-else
            class="btn btn-small"
            :disabled="mcFedLoading || mcFedImporting"
            @click="loadFedListings"
          >
            <span
              class="spin"
              :class="{ spinning: mcFedLoading }"
              aria-hidden="true"
            >↻</span>
            {{ t('llmChat.refresh') }}
          </button>
        </div>
      </div>

      <!-- 按目标种类的加载/降级提示 -->
      <div v-if="mcKind === 'instance'">
        <div v-if="instancesError" class="error-box">{{ t('llmChat.loadFailed') }}：{{ instancesError }}</div>
        <div v-if="!instancesLoading && mcRunningInstances.length === 0" class="mc-banner-warn">
          {{ t('llmChat.noRunningInstances') }}
        </div>
        <div v-if="mcSelected && mcSelected.status !== 'running'" class="mc-banner-warn">
          {{ t('llmChat.instanceNotRunning', { status: mcSelected.status }) }}
        </div>
      </div>
      <div v-else-if="mcKind === 'extapi'">
        <!-- ＋ 手动输入：行内小表单（Base URL / API Key 可选 / 模型名；双动作——
             「临时对话」内存目标直连开聊（不落库），「登记并对话」POST 落库后切换） -->
        <div v-if="mcManualOpen" class="card mc-manual-card">
          <div class="panel-title">{{ t('llmChat.manualTitle') }}</div>
          <div class="mc-manual-grid">
            <label class="field mc-manual-field-url">
              <span>{{ t('llmChat.manualBaseUrl') }}</span>
              <input
                v-model="mcManualForm.base_url"
                :placeholder="t('llmChat.manualBaseUrlPh')"
                :disabled="mcSending || mcManualRegistering"
                @input="mcManualError = ''"
              />
            </label>
            <label class="field">
              <span>{{ t('llmChat.manualApiKey') }}<span class="muted small">（{{ t('llmChat.manualApiKeyOptional') }}）</span></span>
              <input
                v-model="mcManualForm.api_key"
                type="password"
                autocomplete="off"
                :placeholder="t('llmChat.manualApiKeyPh')"
                :disabled="mcSending || mcManualRegistering"
              />
            </label>
            <label class="field">
              <span>{{ t('llmChat.manualModel') }}</span>
              <input
                v-model="mcManualForm.model"
                :placeholder="t('llmChat.manualModelPh')"
                :disabled="mcSending || mcManualRegistering"
                @input="mcManualError = ''"
              />
            </label>
          </div>
          <p class="muted small manual-hint">{{ t('llmChat.manualHint') }}</p>
          <p v-if="mcManualError" class="form-msg is-err">{{ mcManualError }}</p>
          <div class="form-actions">
            <button
              type="button"
              class="btn"
              :disabled="mcSending || mcManualRegistering"
              @click="mcManualClose"
            >{{ t('llmChat.manualCancel') }}</button>
            <button
              type="button"
              class="btn"
              :disabled="mcSending || mcManualRegistering"
              @click="mcManualRegister"
            >{{ mcManualRegistering ? t('llmChat.manualRegistering') : t('llmChat.manualRegisterBtn') }}</button>
            <button
              type="button"
              class="btn btn-primary"
              :disabled="mcSending || mcManualRegistering"
              @click="mcManualTemp"
            >{{ t('llmChat.manualTempBtn') }}</button>
          </div>
        </div>

        <!-- 临时目标生效横幅（表单收起时；本会话内存目标，不落库） -->
        <div v-else-if="mcManualActive" class="mc-banner-info mc-manual-active">
          <span>✳ {{ t('llmChat.manualActiveTag') }}</span>
          <span class="mono small">{{ mcManualForm.base_url }}</span>
          <span class="muted small">· {{ mcModel || '—' }}</span>
          <span class="head-actions">
            <button
              class="btn btn-small"
              :disabled="mcSending"
              @click="mcManualOpen = true"
            >{{ t('llmChat.manualEditBtn') }}</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="mcSending"
              @click="mcManualExit"
            >{{ t('llmChat.manualExitBtn') }}</button>
          </span>
        </div>

        <div v-if="extError" class="error-box">{{ t('llmExt.loadFailed') }}：{{ extError }}</div>
        <div v-if="!extLoading && extApis.length === 0 && !mcManualActive" class="mc-banner-warn">
          {{ t('llmChat.noExtApis') }}
        </div>
        <div v-if="mcExtTarget && !mcManualActive && mcModelOptions.length === 0" class="mc-banner-warn">
          {{ t('llmChat.extNoModels') }}
        </div>
      </div>
      <div v-else>
        <div v-if="mcFedError" class="error-box">{{ t('llmChat.fedLoadFailed') }}：{{ mcFedError }}</div>
        <div v-if="mcFedImporting" class="mc-banner-info">{{ t('llmChat.fedImporting') }}</div>
        <div v-if="!mcFedLoading && mcFedListings.length === 0" class="mc-banner-warn">
          {{ t('llmChat.fedEmpty') }}
        </div>
      </div>

      <!-- 对话区（flex 填满 + 内部滚动） -->
      <div ref="mcScrollEl" class="card mc-chat-area">
        <div v-if="mcBubbles.length === 0" class="mc-empty">
          <AppIcon name="modelchat" :size="40" />
          <p>{{ t('llmChat.emptyHint') }}</p>
          <p class="muted small">{{ t('llmChat.emptyNote') }}</p>
        </div>
        <div
          v-for="b in mcBubbles"
          :key="b.id"
          class="mc-bubble-row"
          :class="b.role === 'user' ? 'mc-row-user' : 'mc-row-ai'"
        >
          <div class="mc-bubble" :class="{ 'mc-b-user': b.role === 'user', 'mc-b-ai': b.role === 'assistant', 'mc-b-error': b.error }">
            <span class="mc-role-tag">{{ b.role === 'user' ? t('llmChat.roleMe') : t('llmChat.roleAi') }}</span>
            <!-- 思考段折叠（content 空且思考段非空 → max_tokens 调大提示） -->
            <p v-if="!b.content && b.reasoning && !b.streaming" class="mc-thinking-hint">
              {{ t('llmLog.thinkingHint') }}——{{ t('llmLog.thinkingAdjust') }}
            </p>
            <details v-if="b.reasoning" class="mc-reasoning">
              <summary>{{ t('llmLog.reasoningToggle') }}</summary>
              <pre class="mc-reasoning-text">{{ b.reasoning }}</pre>
            </details>
            <div class="mc-bubble-content">
              <span>{{ b.content }}</span><span v-if="b.streaming" class="mc-cursor">▋</span>
            </div>
          </div>
        </div>
      </div>

      <!-- 输入区（composer 钉底：shrink 0 不随内容滚走） -->
      <div class="card mc-input-area">
        <textarea
          v-model="mcInput"
          class="mc-textarea"
          rows="2"
          :placeholder="t('llmChat.inputPh')"
          :disabled="mcSending"
          @keydown="mcOnKeydown"
        />
        <label class="mc-max-tokens" :title="t('llmChat.maxTokensHint')">
          <span class="muted small">{{ t('llmChat.maxTokens') }}</span>
          <input
            v-model.number="mcMaxTokens"
            type="number"
            min="1"
            step="256"
            :disabled="mcSending"
          />
        </label>
        <button
          class="btn btn-primary"
          :disabled="!mcInput.trim() || mcSending || !mcReady"
          @click="mcSend"
        >
          {{ mcSending ? t('llmChat.sending') : t('llmChat.send') }}
        </button>
      </div>
    </section>

    <!-- =================== Tab4 实例监控（可复用组件 InstanceMonitor） =================== -->
    <!-- v-if 挂载：Tab 激活才拉数据/轮询（组件自包含实例列表、10s metrics 轮询、
         30s 网关聚合刷新），卸载即停，与旧版宿主接线行为一致 -->
    <section v-if="activeTab === 'metrics'" class="tab-panel">
      <InstanceMonitor />
    </section>

    <!-- =================== Tab6 GPU 监控 =================== -->
    <section v-show="activeTab === 'gpu'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">GPU 详细信息</span>
        <button class="btn btn-small" :disabled="gpuLoading" @click="loadGpu">
          <span class="spin" :class="{ spinning: gpuLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div v-if="gpuError" class="error-box">{{ gpuError }}</div>
      <div v-if="gpuLoading && !(gpu.devices?.length)" class="card empty-card">加载中…</div>
      <div v-else-if="!gpu.available" class="card empty-card">
        未检测到 GPU（CPU 模式或未安装驱动）。<br />
        vLLM 需要 GPU 才能高效推理；如需使用，请安装 NVIDIA 驱动 + CUDA 或 AMD ROCm。
      </div>
      <section v-else class="gpu-grid">
        <div v-for="d in gpu.devices" :key="d.index" class="card gpu-card">
          <div class="gpu-card-head">
            <span class="gpu-card-name">{{ d.name }}</span>
            <span v-if="d.unified_memory" class="pill pill-purple">统一内存</span>
            <span class="pill pill-muted">#{{ d.index }}</span>
          </div>
          <!-- 统一内存架构（GB10/Jetson）：独立显存字段 null，容量/占用按共享池展示 -->
          <template v-if="d.unified_memory">
            <div class="gpu-row">
              <span class="muted small">独立显存</span>
              <span class="mono">N/A（CPU/GPU 共享统一内存）</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">统一内存</span>
              <span class="mono">{{ mibToGB(d.unified_memory_total_mib) || '—' }}</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">已用（共享池）</span>
              <span class="mono">{{ d.unified_memory_used_mib ?? '—' }} MiB</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">可用（共享池）</span>
              <span class="mono">{{ d.unified_memory_free_mib ?? '—' }} MiB</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">内存占用</span>
              <span class="prog-wrap">
                <span class="prog-bar"><span class="prog-fill" :style="{ width: memPct(d) + '%' }" /></span>
                <span class="prog-text">{{ Math.round(memPct(d)) }}%</span>
              </span>
            </div>
          </template>
          <template v-else>
            <div class="gpu-row">
              <span class="muted small">总显存</span>
              <span class="mono">{{ d.memory_total_mib ?? 0 }} MiB</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">已用</span>
              <span class="mono">{{ d.memory_used_mib ?? 0 }} MiB</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">空闲</span>
              <span class="mono">{{ d.memory_free_mib ?? 0 }} MiB</span>
            </div>
            <div class="gpu-row">
              <span class="muted small">显存占用</span>
              <span class="prog-wrap">
                <span class="prog-bar"><span class="prog-fill" :style="{ width: memPct(d) + '%' }" /></span>
                <span class="prog-text">{{ Math.round(memPct(d)) }}%</span>
              </span>
            </div>
          </template>
          <div class="gpu-row">
            <span class="muted small">GPU 使用率</span>
            <span class="prog-wrap">
              <span class="prog-bar"><span class="prog-fill fill-ok" :style="{ width: gpuPct(d.utilization_pct) + '%' }" /></span>
              <span class="prog-text">{{ d.utilization_pct ?? 0 }}%</span>
            </span>
          </div>
        </div>
      </section>
    </section>

    <!-- =================== Tab8 外部 API（接入别家 OpenAI 兼容端点）=================== -->
    <section v-show="activeTab === 'external'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">{{ t('llmExt.title') }}</span>
        <span class="muted small">{{ t('llmExt.sub') }}</span>
        <div class="head-actions">
          <button class="btn btn-small" :disabled="extLoading" @click="loadExtApis">
            <span class="spin" :class="{ spinning: extLoading }" aria-hidden="true">↻</span>
            {{ t('llmExt.refresh') }}
          </button>
        </div>
      </div>

      <div v-if="extError" class="error-box">{{ t('llmExt.loadFailed') }}：{{ extError }}</div>

      <!-- 登记表单 -->
      <div class="card ext-form-card">
        <div class="ext-form-grid">
          <label class="field">
            <span>{{ t('llmExt.fName') }}</span>
            <input v-model="extForm.name" :placeholder="t('llmExt.fNamePh')" :disabled="extCreating" />
          </label>
          <label class="field ext-field-url">
            <span>{{ t('llmExt.fBaseUrl') }}</span>
            <input v-model="extForm.base_url" placeholder="http://192.0.2.106:8000/v1" :disabled="extCreating" />
          </label>
          <label class="field">
            <span>{{ t('llmExt.fApiKey') }}</span>
            <input v-model="extForm.api_key" type="password" :placeholder="t('llmExt.fApiKeyPh')" :disabled="extCreating" autocomplete="off" />
          </label>
        </div>
        <div class="ext-form-grid">
          <label class="field ext-field-models">
            <span>{{ t('llmExt.fModels') }}</span>
            <textarea v-model="extForm.models_text" rows="2" :placeholder="t('llmExt.fModelsPh')" :disabled="extCreating" />
          </label>
          <label class="field">
            <span>{{ t('llmExt.fNotes') }}</span>
            <input v-model="extForm.notes" :placeholder="t('llmExt.fNotesPh')" :disabled="extCreating" />
          </label>
        </div>
        <div class="form-actions">
          <span class="muted small">{{ t('llmExt.formHint') }}</span>
          <button class="btn btn-primary" :disabled="extCreating" @click="submitExtCreate">
            {{ extCreating ? t('llmExt.creating') : t('llmExt.createBtn') }}
          </button>
        </div>
      </div>

      <!-- 登记列表卡 -->
      <div v-if="!extLoading && extApis.length === 0" class="card empty-card">
        {{ t('llmExt.empty') }}
      </div>
      <div v-for="row in extApis" :key="row.id" class="card ext-row-card">
        <div class="ext-row-head">
          <div class="ext-row-title">
            <strong>{{ row.name }}</strong>
            <span
              v-if="row.via_node"
              class="pill pill-fed"
              :title="`${t('llmExt.relayBadge')} · via_node ${row.via_node}`"
            >🌐 {{ t('llmExt.relayBadge') }}</span>
            <span class="pill" :class="extStatusClass(row.status)">{{ t(`llmExt.status_${row.status || 'unknown'}`) }}</span>
          </div>
          <div class="head-actions">
            <!-- 发布到网关（联邦中继双向打通）：一键把该登记导入为网关渠道，
                 本局域网 AI 即可经本节点网关调用（详见 docs/LLM_EXTERNAL_APIS.md） -->
            <button
              class="btn btn-small"
              :title="t('llmExt.publishBtnTitle')"
              :disabled="extBusyId === row.id || extPublishingId === row.id"
              @click="publishExtToGateway(row)"
            >{{ extPublishingId === row.id ? t('llmExt.publishing') : t('llmExt.publishBtn') }}</button>
            <button
              class="btn btn-small"
              :disabled="extBusyId === row.id"
              @click="testExtApi(row)"
            >{{ extBusyId === row.id ? t('llmExt.testing') : t('llmExt.testBtn') }}</button>
            <button
              class="btn btn-small"
              :disabled="extBusyId === row.id || extEditing"
              @click="openExtEdit(row)"
            >{{ t('llmExt.editBtn') }}</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="extBusyId === row.id"
              @click="removeExtApi(row)"
            >{{ t('llmExt.deleteBtn') }}</button>
          </div>
        </div>
        <dl class="ext-meta">
          <div>
            <dt>{{ t('llmExt.fBaseUrl') }}</dt>
            <!-- via_node 条目：默认遮蔽内网地址（消费侧由源节点代发，无须知道真实
                 端点）；持有 API token（本控制台 admin 凭据）可展开查看真实值 -->
            <dd v-if="row.via_node && !extUrlRevealed[String(row.id ?? '')]" class="mono ext-url-masked">
              🌐 {{ t('llmExt.relayMasked') }}
              <button
                v-if="hasApiToken"
                type="button"
                class="btn btn-small ext-url-toggle"
                :title="t('llmExt.revealHint')"
                @click="toggleExtUrlReveal(row.id)"
              >{{ t('llmExt.revealBtn') }}</button>
              <span v-else class="muted small">{{ t('llmExt.revealAdminOnly') }}</span>
            </dd>
            <dd v-else class="mono">
              <template v-if="row.via_node">
                {{ row.base_url }}
                <button
                  type="button"
                  class="btn btn-small ext-url-toggle"
                  @click="toggleExtUrlReveal(row.id)"
                >{{ t('llmExt.hideBtn') }}</button>
              </template>
              <template v-else>{{ row.base_url }}</template>
            </dd>
          </div>
          <div>
            <dt>{{ t('llmExt.fApiKey') }}</dt>
            <dd class="mono">{{ row.api_key_masked || t('llmExt.noKey') }}</dd>
          </div>
          <div><dt>{{ t('llmExt.fModelsCount') }}</dt><dd>{{ (row.models ?? []).length }}</dd></div>
          <div>
            <dt>{{ t('llmExt.fLastCheck') }}</dt>
            <dd>{{ row.last_check_at || '—' }}</dd>
          </div>
        </dl>
        <div v-if="(row.models ?? []).length" class="ext-models">
          <code v-for="m in row.models" :key="m" class="ext-model-chip">{{ m }}</code>
        </div>
        <p v-if="row.notes" class="muted small">{{ row.notes }}</p>
      </div>

      <!-- 连通测试结果面板（真实 models 清单 + 延迟） -->
      <div v-if="extTest" class="card ext-test-card" :class="extTest.ok ? 'ext-test-ok' : 'ext-test-err'">
        <div class="ext-row-head">
          <span class="panel-title">
            {{ t('llmExt.testResult') }} · {{ extTest.name }}
            <span v-if="extTest.ok" class="muted small">（{{ extTest.latency_ms }}ms）</span>
          </span>
          <button class="btn btn-small" @click="extTest = null">×</button>
        </div>
        <template v-if="extTest.ok">
          <p class="muted small">{{ t('llmExt.testOkHint', { n: extTest.models.length, ms: extTest.latency_ms }) }}</p>
          <div class="ext-models">
            <code v-for="m in extTest.models" :key="m" class="ext-model-chip">{{ m }}</code>
          </div>
        </template>
        <p v-else class="form-msg is-err">{{ t('llmExt.testError') }}：{{ extTest.error }}</p>
      </div>

      <!-- 编辑弹窗（复用登记表单字段；api_key 留空保留原 key） -->
      <div v-if="extEditShow" class="modal-backdrop" @click.self="!extEditing && (extEditShow = false)">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="llm-ext-edit-title">
          <div class="modal-head">
            <h3 id="llm-ext-edit-title">{{ t('llmExt.editTitle') }}</h3>
            <button class="modal-close" type="button" :disabled="extEditing" @click="extEditShow = false">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitExtEdit">
            <div class="ext-form-grid">
              <label class="field">
                <span>{{ t('llmExt.fName') }}</span>
                <input v-model="extEditForm.name" :placeholder="t('llmExt.fNamePh')" :disabled="extEditing" />
              </label>
              <label class="field ext-field-url">
                <span>{{ t('llmExt.fBaseUrl') }}</span>
                <input v-model="extEditForm.base_url" placeholder="http://192.0.2.106:8000/v1" :disabled="extEditing" />
              </label>
              <label class="field">
                <span>{{ t('llmExt.fApiKey') }}</span>
                <input
                  v-model="extEditForm.api_key"
                  type="password"
                  :placeholder="t('llmExt.fApiKeyKeepPh', { masked: extEditKeyMasked || t('llmExt.noKey') })"
                  :disabled="extEditing"
                  autocomplete="off"
                />
              </label>
            </div>
            <div class="ext-form-grid">
              <label class="field ext-field-models">
                <span>{{ t('llmExt.fModels') }}</span>
                <textarea v-model="extEditForm.models_text" rows="2" :placeholder="t('llmExt.fModelsPh')" :disabled="extEditing" />
              </label>
              <label class="field">
                <span>{{ t('llmExt.fNotes') }}</span>
                <input v-model="extEditForm.notes" :placeholder="t('llmExt.fNotesPh')" :disabled="extEditing" />
              </label>
            </div>
            <p class="muted small">{{ t('llmExt.editHint') }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="extEditing" @click="extEditShow = false">
                {{ t('llmExt.cancelBtn') }}
              </button>
              <button type="submit" class="btn btn-primary" :disabled="extEditing">
                {{ extEditing ? t('llmExt.editing') : t('llmExt.editSubmit') }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab7 参数说明 =================== -->
    <section v-show="activeTab === 'params'" class="tab-panel">
      <div class="card params-card">
        <h3 class="params-title">vLLM serve 常用参数说明</h3>
        <p class="muted small">以下为创建实例时可配置的启动参数，帮助理解何时调整。</p>

        <dl class="param-list">
          <div class="param-item">
            <dt>tensor_parallel_size</dt>
            <dd>
              张量并行度，跨多张 GPU 切分模型。默认 1（单卡）。有多张同型号 GPU 且想用满显存时增加
              （如双卡设 2）。注意多卡会引入通信开销，并非越大越好。
            </dd>
          </div>
          <div class="param-item">
            <dt>gpu_memory_utilization</dt>
            <dd>
              vLLM 可占用的 GPU 显存比例，范围 0~1，默认 0.9。增大可容纳更长上下文 / 更大 KV 缓存，
              但可能影响同卡其它进程。显存紧张或与其它 GPU 任务共用一卡时调低（如 0.5）。
            </dd>
          </div>
          <div class="param-item">
            <dt>max_model_len</dt>
            <dd>
              模型支持的最大上下文长度（token 数），默认 8192。增大允许更长输入+输出，但显存占用
              随之上升。受模型本身上下文上限与显存双重约束。
            </dd>
          </div>
          <div class="param-item">
            <dt>quantization</dt>
            <dd>
              量化方案，None（不量化）/ awq / gptq。量化模型显存占用更小、可跑更大模型，但精度
              略有损失。仅当模型本身是量化版本时设置对应值。
            </dd>
          </div>
          <div class="param-item">
            <dt>dtype</dt>
            <dd>
              计算精度，auto（自动按硬件）/ float16 / bfloat16。多数情况 auto 即可。较新卡（A100/
              H100/RTX 30+）优先 bfloat16 数值更稳；老卡可能不支持 bf16，用 float16。
            </dd>
          </div>
          <div class="param-item">
            <dt>served_model_name</dt>
            <dd>
              API 对外暴露的模型名（OpenAI 兼容 /v1/chat/completions 的 model 字段）。默认等于
              model。设置别名便于客户端解耦真实模型路径。
            </dd>
          </div>
          <div class="param-item">
            <dt>trust_remote_code</dt>
            <dd>
              是否允许执行模型仓库的自定义 Python 代码（Qwen/GLM 等国产模型常需）。有安全风险，
              仅信任来源可信的模型时开启。
            </dd>
          </div>
        </dl>
      </div>
    </section>

    <!-- =================== 实例拉起日志抽屉（右侧滑出，2s 轮询跟尾） =================== -->
    <div v-if="logDrawerOpen" class="log-drawer-backdrop" @click.self="closeInstLog">
      <aside class="log-drawer" role="dialog" aria-modal="true" aria-labelledby="llm-log-title">
        <div class="log-drawer-head">
          <div class="log-drawer-title">
            <h3 id="llm-log-title">{{ t('llmLog.title') }}</h3>
            <span class="pill mono" :class="logStatusClass(logStatus)">{{ logStatus || '—' }}</span>
            <span class="muted small mono">{{ logInstanceId }}</span>
          </div>
          <div class="log-drawer-actions">
            <button
              class="btn btn-small"
              :class="{ 'btn-primary': !logFollow }"
              :title="t('llmLog.followHint')"
              @click="logFollow = !logFollow"
            >{{ logFollow ? t('llmLog.followOn') : t('llmLog.followOff') }}</button>
            <button class="btn btn-small" @click="clearInstLog">{{ t('llmLog.clear') }}</button>
            <button class="btn btn-small" :disabled="!logFollow" @click="fetchInstLog">
              {{ t('llmLog.refresh') }}
            </button>
            <button class="modal-close" type="button" @click="closeInstLog">×</button>
          </div>
        </div>
        <div class="log-drawer-meta muted small">
          <span class="mono">{{ logFile || '—' }}</span>
          <span> · {{ t('llmLog.metaHint') }}</span>
        </div>
        <div ref="logBox" class="log-drawer-body mono">
          <div v-if="logError" class="log-empty muted">{{ t('llmLog.loadFailed') }}：{{ logError }}</div>
          <div v-else-if="logLines.length === 0" class="log-empty muted">{{ t('llmLog.empty') }}</div>
          <div v-for="(line, i) in logLines" :key="i" class="log-line">{{ line }}</div>
        </div>
      </aside>
    </div>

    <!-- =================== 「接入说明」弹窗（三段式钉底：head 固定 + body 滚动
         + 底部操作条 sticky——同 2026-08-31 修复后的 modal 结构；内容全部由
         实例真实数据动态渲染，样式复用 CodeHub 接入说明 Tab 的 ob-* 模式）
         =================== -->
    <div v-if="showAccess && accessInst" class="modal-backdrop" @click.self="closeAccess">
      <div class="modal access-modal" role="dialog" aria-modal="true" aria-labelledby="llm-access-title">
        <div class="modal-head">
          <h3 id="llm-access-title">
            {{ t('llmAccess.title', { name: accessInst.name ?? accessInst.id ?? '' }) }}
          </h3>
          <button class="modal-close" type="button" @click="closeAccess">×</button>
        </div>
        <div class="modal-body">
          <!-- ① 直连 vLLM（OpenAI 兼容） -->
          <section class="acc-section">
            <div class="acc-sec-title">{{ t('llmAccess.sec1Title') }}</div>
            <div class="acc-kv">
              <span class="acc-k">{{ t('llmAccess.baseUrlLabel') }}</span>
              <code class="acc-v">{{ accessBaseUrl }}</code>
              <button
                class="btn btn-small acc-copy-inline"
                :class="{ copied: accessCopied === 'baseUrl' }"
                @click="copyAccess('baseUrl', accessBaseUrl)"
              >{{ accessCopied === 'baseUrl' ? '✓' : t('llmAccess.copy') }}</button>
              <span class="muted small">
                {{ t('llmAccess.baseUrlHint', { host: accessInst.config?.host ?? '0.0.0.0' }) }}
              </span>
            </div>
            <!-- 模型名：大字 + 复制 + 历史坑警示（model 必须用 served_model_name） -->
            <div class="acc-model-row">
              <span class="acc-k">{{ t('llmAccess.modelNameLabel') }}</span>
              <code class="acc-model-name">{{ accessModelName }}</code>
              <button
                class="btn btn-small acc-copy-inline"
                :class="{ copied: accessCopied === 'model' }"
                @click="copyAccess('model', accessModelName)"
              >{{ accessCopied === 'model' ? '✓' : t('llmAccess.copy') }}</button>
              <span class="acc-warn" :title="t('llmAccess.modelNotPathHint')">
                ⚠ {{ accessHasServedName ? t('llmAccess.modelNotPath') : t('llmAccess.modelFallbackPath') }}
              </span>
            </div>
            <p class="acc-note">{{ accessHasServedName ? t('llmAccess.modelNotPathHint') : t('llmAccess.modelFallbackHint') }}</p>
            <div class="acc-kv">
              <span class="acc-k">{{ t('llmAccess.apiKeyLabel') }}</span>
              <code v-if="accessApiKey" class="acc-v">{{ accessApiKey }}</code>
              <span v-else class="muted small">{{ t('llmAccess.noApiKey') }}</span>
            </div>
            <div class="acc-step">{{ t('llmAccess.curlLabel') }}</div>
            <div class="acc-code">
              <pre class="acc-pre">{{ accessCurl }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'curl' }"
                @click="copyAccess('curl', accessCurl)"
              >{{ accessCopied === 'curl' ? '✓' : t('llmAccess.copy') }}</button>
            </div>
          </section>

          <!-- ② 经网关调用（NexOS API） -->
          <section class="acc-section">
            <div class="acc-sec-title">{{ t('llmAccess.sec2Title') }}</div>
            <p class="acc-note">{{ t('llmAccess.sec2Sub') }}</p>
            <div class="acc-kv">
              <span class="acc-k">{{ t('llmAccess.gatewayUrlLabel') }}</span>
              <code class="acc-v">{{ accessGatewayUrl }}</code>
              <button
                class="btn btn-small acc-copy-inline"
                :class="{ copied: accessCopied === 'gwUrl' }"
                @click="copyAccess('gwUrl', accessGatewayUrl)"
              >{{ accessCopied === 'gwUrl' ? '✓' : t('llmAccess.copy') }}</button>
            </div>
            <div class="acc-step">{{ t('llmAccess.gatewayReqLabel') }}</div>
            <div class="acc-code">
              <pre class="acc-pre">{{ accessGatewayCurl }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'gwCurl' }"
                @click="copyAccess('gwCurl', accessGatewayCurl)"
              >{{ accessCopied === 'gwCurl' ? '✓' : t('llmAccess.copy') }}</button>
            </div>
            <p class="acc-note">{{ t('llmAccess.gatewayAuthNote') }}</p>
            <div class="acc-step">{{ t('llmAccess.gatewayRespLabel') }}</div>
            <div class="acc-code">
              <pre class="acc-pre">{{ accessGatewayResp }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'gwResp' }"
                @click="copyAccess('gwResp', accessGatewayResp)"
              >{{ accessCopied === 'gwResp' ? '✓' : t('llmAccess.copy') }}</button>
            </div>
            <p class="acc-note">{{ t('llmAccess.reasoningNote') }}</p>
          </section>

          <!-- ③ 实例参数速览（精简四项：模型名/上下文/端口/API Key；全部来自实例
               真实数据，上下文窗口醒目展示——2026-08-31 按用户裁决精简） -->
          <section class="acc-section">
            <div class="acc-sec-title">{{ t('llmAccess.sec3Title') }}</div>
            <dl class="acc-params">
              <div class="acc-param">
                <dt>{{ t('llmAccess.fServedName') }}</dt>
                <dd class="mono">{{ accessModelName || t('llmAccess.none') }}</dd>
              </div>
              <div class="acc-param acc-param-ctx">
                <dt>{{ t('llmAccess.fContext') }}</dt>
                <dd>
                  <strong class="acc-ctx-value">{{ accessInst.config?.max_model_len ?? '—' }}</strong>
                  <span class="muted small">{{ t('llmAccess.fContextUnit') }}</span>
                </dd>
              </div>
              <div class="acc-param">
                <dt>{{ t('llmAccess.fPort') }}</dt>
                <dd class="mono">{{ accessInst.port ?? '—' }}</dd>
              </div>
              <div class="acc-param">
                <dt>{{ t('llmAccess.apiKeyLabel') }}</dt>
                <dd class="mono">
                  <template v-if="accessApiKey">{{ accessApiKey }}</template>
                  <span v-else class="muted small">{{ t('llmAccess.noApiKey') }}</span>
                </dd>
              </div>
            </dl>
          </section>

          <!-- ④ 启动参数（后端 launch_command 透出：真实 argv / 按 config 构造，
               前端只渲染+复制不猜格式；见 accessLaunchCommand 注释） -->
          <section class="acc-section">
            <div class="acc-sec-title">{{ t('llmAccess.launchTitle') }}</div>
            <p class="acc-note">{{ t('llmAccess.launchNote') }}</p>
            <div class="acc-code">
              <pre class="acc-pre">{{ accessLaunchCommand }}</pre>
              <button
                class="btn btn-small acc-copy"
                :class="{ copied: accessCopied === 'launch' }"
                @click="copyAccess('launch', accessLaunchCommand)"
              >{{ accessCopied === 'launch' ? '✓' : t('llmAccess.copy') }}</button>
            </div>
          </section>

          <div class="form-actions">
            <button type="button" class="btn" @click="closeAccess">{{ t('llmAccess.close') }}</button>
          </div>
        </div>
      </div>
    </div>
    </template>
  </div>
</template>

<style scoped>
.llm-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
  /* 「对话」Tab 高度链（照 IM Chat.vue 实际生效实现，禁 100vh）：必须用
   * height:100% 精确钉住窗口体（window-body flex:1 有确定高度）——之前
   * min-height:100% 会被长对话内容撑高（页面超出窗口 → window-body 外层
   * 滚动 → composer 被顶出可视区随滚动跑，v0.1.13 病灶）。现在页面恒等于
   * 窗口体高：mc-panel flex:1 在确定高度内拉伸，消息区内滚 overflow-y:auto，
   * composer flex-shrink:0 钉底；其余 Tab 内容超出时照旧溢出到 window-body
   * 滚动（overflow:visible 子树照常贡献滚动区），互不影响。 */
  height: 100%;
  min-height: 0;
  box-sizing: border-box;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }

/* Tabs */
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover { color: var(--text, #2B2B2B); }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }

/* 二级 Tab（组内子页，2026-09-03 两级化）：比一级 Tab 小一号，虚线下边线区分
 * 层级——与 ModelHubPanel / ApiGateway「本地大厅/联邦大厅」二级 Tab 同款 */
.sub-tabs { gap: 2px; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.sub-tab { padding: 5px 12px; font-size: 13px; }

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
.empty-inline { padding-top: 4px; }
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* GPU 摘要卡 */
.gpu-summary-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.gpu-summary-head { display: flex; align-items: center; gap: 8px; }
.gpu-mini-list { display: flex; flex-direction: column; gap: 8px; }
.gpu-mini-item { display: grid; grid-template-columns: 1fr auto 160px; align-items: center; gap: 12px; font-size: 13px; }
.gpu-mini-name { font-weight: 500; }
.gpu-mini-mem { font-size: 12px; color: var(--text-muted, #5E5C5F); }

/* 进度条 */
.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { display: block; height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-ok { background: #0E8420; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-purple { color: #7c3aed; background: #ede9fe; }
.pill-cyan { color: #0e7490; background: #cffafe; }
/* 经源节点中继徽章（via_node 非空——联邦导入条目，chat/test 走 overlay 中继） */
.pill-fed { color: #1d4ed8; background: #e0e7ff; }

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
.switch { display: inline-flex; align-items: center; gap: 6px; font-size: 13px; cursor: pointer; padding-top: 4px; }
.switch input { width: 16px; height: 16px; cursor: pointer; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.mono { font-family: var(--mono); }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
/* 2026-08-31 修复：弹窗自身不再整体 overflow:auto（底部输入/操作区会跟着内容
 * 滚走）——改为 header 固定 + body 滚动 + 操作区 sticky 钉底的三段结构。 */
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

/* ============ 实例拉起日志抽屉（2026-08-31） ============ */
/* 实例行「日志」按钮：starting 时高亮（看模型加载进度） */
.log-btn-starting {
  border-color: var(--accent, #E95420); color: var(--accent, #E95420);
  font-weight: 600; background: rgba(233, 84, 32, 0.07);
}
.log-drawer-backdrop {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.25);
  display: flex; justify-content: flex-end; z-index: 110;
}
.log-drawer {
  width: min(720px, 92vw); height: 100%; display: flex; flex-direction: column;
  background: var(--bg-card, #fff); box-shadow: -12px 0 40px rgba(0, 0, 0, 0.2);
  border-left: 1px solid var(--border-soft, #EDEDED);
}
.log-drawer-head {
  display: flex; align-items: center; justify-content: space-between; gap: 10px;
  padding: 14px 18px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-shrink: 0;
  flex-wrap: wrap;
}
.log-drawer-title { display: flex; align-items: center; gap: 10px; min-width: 0; flex-wrap: wrap; }
.log-drawer-title h3 { font-size: 15px; font-weight: 600; margin: 0; }
.log-drawer-actions { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.log-drawer-actions .modal-close { font-size: 20px; }
.log-drawer-meta {
  padding: 6px 18px; border-bottom: 1px solid var(--border-soft, #EDEDED);
  flex-shrink: 0; word-break: break-all;
}
.log-drawer-body {
  flex: 1 1 auto; min-height: 0; overflow-y: auto; padding: 10px 14px;
  font-size: 12px; line-height: 1.55; background: var(--bg-app, #FAFAFA);
}
.log-line { white-space: pre-wrap; word-break: break-all; color: var(--text, #2B2B2B); }
.log-empty { padding: 24px 8px; text-align: center; font-size: 13px; }

/* ============ 「接入说明」弹窗（2026-08-31；CodeHub 接入说明 Tab 的
   ob-* 卡片/代码块/复制按钮模式本页版；三段式钉底结构复用 .modal） ============ */
/* 三段内容较宽（curl/JSON 示例），在默认 560px 基础上放宽 */
.access-modal { width: min(720px, 100%); }
.access-modal .modal-body { gap: 0; }
.acc-section {
  display: flex; flex-direction: column; gap: 10px;
  padding: 14px 0 16px; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.acc-section:last-of-type { border-bottom: none; }
.acc-sec-title { font-size: 14px; font-weight: 700; color: var(--text, #2B2B2B); }
/* 键值行（CodeHub ob-kv 同款） */
.acc-kv { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; font-size: 12.5px; }
.acc-k { flex-shrink: 0; min-width: 64px; font-weight: 600; color: var(--text, #2B2B2B); }
.acc-v {
  font-family: var(--mono, 'Ubuntu Mono', Consolas, monospace); font-size: 12px;
  word-break: break-all; padding: 2px 8px; border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa); color: var(--text, #2B2B2B);
}
/* 模型名行：served_model_name 大字展示（用户点名要的核心信息） */
.acc-model-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.acc-model-row .acc-k { align-self: center; font-size: 12.5px; }
.acc-model-name {
  font-family: var(--mono, 'Ubuntu Mono', Consolas, monospace);
  font-size: 16px; font-weight: 700; color: var(--accent, #E95420);
  word-break: break-all; padding: 3px 10px; border-radius: var(--radius-sm, 6px);
  background: rgba(233, 84, 32, 0.07); border: 1px solid rgba(233, 84, 32, 0.25);
}
/* 历史坑警示标签：model 参数用模型名不是路径（曾因传路径 404） */
.acc-warn {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 11.5px; font-weight: 600; white-space: nowrap;
  color: #92400e; background: #fef3c7; border: 1px solid rgba(146, 64, 14, 0.25);
}
.acc-note { margin: 0; font-size: 12.5px; line-height: 1.6; color: var(--text-muted, #5E5C5F); }
.acc-step { margin-top: 2px; font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
/* 代码块：深色底等宽（右上角悬浮复制按钮，长命令自动换行——ob-pre 同款） */
.acc-code { position: relative; }
.acc-pre {
  margin: 0; padding: 10px 60px 10px 12px; border-radius: var(--radius-sm, 8px);
  background: #26292f; color: #e8e4e8;
  font-family: var(--mono, 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace);
  font-size: 12px; line-height: 1.55; white-space: pre-wrap; word-break: break-word;
}
/* 深色代码块右上角复制小按钮（半透明；成功 ✓ 绿色反馈） */
.acc-copy {
  position: absolute; top: 5px; right: 5px; padding: 2px 9px; font-size: 11px;
  background: rgba(255, 255, 255, 0.1); border: 1px solid rgba(255, 255, 255, 0.25);
  color: #e8e4e8;
}
.acc-copy:hover { background: rgba(255, 255, 255, 0.2); }
.acc-copy.copied {
  color: #4ade80; border-color: rgba(74, 222, 128, 0.55); background: rgba(74, 222, 128, 0.12);
}
/* 行内复制小按钮（浅色键值行右侧；成功绿色反馈） */
.acc-copy-inline { padding: 2px 9px; font-size: 11px; }
.acc-copy-inline.copied { color: #15803d; border-color: rgba(21, 128, 61, 0.4); background: #dcfce7; }
/* ③ 参数速览：dt/dd 网格（标签列窄、值列可换行） */
.acc-params { margin: 0; display: flex; flex-direction: column; gap: 6px; }
.acc-param {
  display: grid; grid-template-columns: 220px 1fr; gap: 10px; align-items: baseline;
  font-size: 12.5px;
}
.acc-param dt { font-weight: 600; color: var(--text, #2B2B2B); }
.acc-param dd { margin: 0; word-break: break-all; color: var(--text, #2B2B2B); }
/* 上下文窗口 max_model_len：用户点名要的，醒目展示（大号橙色数值） */
.acc-param-ctx {
  padding: 6px 10px; border-radius: var(--radius-sm, 8px);
  background: rgba(233, 84, 32, 0.06); border: 1px solid rgba(233, 84, 32, 0.2);
}
.acc-ctx-value { font-size: 16px; font-weight: 700; color: var(--accent, #E95420); }
/* extra_args 键值对齐列表（flag 列对齐 + 取值列换行） */
.acc-extra-list {
  display: flex; flex-direction: column; gap: 4px; padding: 8px 10px;
  border-radius: var(--radius-sm, 8px); background: var(--bg-code, #fafafa);
}
.acc-extra-row { display: grid; grid-template-columns: minmax(140px, auto) 1fr; gap: 12px; align-items: baseline; }
.acc-extra-flag { color: var(--text, #2B2B2B); word-break: break-all; }
.acc-extra-val { color: var(--text-muted, #5E5C5F); word-break: break-all; }

/* 创建对话框：Qwen 快速模板区 */
.preset-box {
  border: 1px dashed var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  background: var(--border-soft, #FAFAFA); padding: 10px 12px;
  display: flex; flex-direction: column; gap: 6px;
}
.preset-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.preset-label { font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
.preset-btns { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.preset-btns .btn + .btn { margin-left: 0; }
.preset-btn.preset-active {
  border-color: var(--accent, #E95420); color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.07); font-weight: 600;
}
.preset-hint { margin: 0; }

/* extra_args 可折叠文本域 */
.extra-toggle { align-self: flex-start; }
.extra-args-input {
  width: 100%; resize: vertical; padding: 8px 10px;
  border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-size: 12.5px; line-height: 1.6; background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); white-space: pre; overflow-x: auto;
}
.extra-args-input:focus { outline: none; border-color: var(--accent, #E95420); }

/* 对话 Tab（统一目标选择器：本地实例 / 外部 API / 联邦大厅导入） */
/* 面板本体：flex 拉伸填满窗口体剩余空间（.llm-page height:100% 高度链），
 * 内部三段——选择器头部 / 消息区（flex 滚动）/ composer（shrink 0 钉底）。
 * 照 IM Chat.vue 成熟模式，无 100vh 公式。min-height:0（去 460px 下限）：
 * 消息区随窗口高度伸缩，composer 在任意窗口高度都钉底不被顶出。 */
.mc-panel { flex: 1 1 auto; min-height: 0; }
.mc-head { flex-wrap: wrap; row-gap: 8px; }
.mc-selectors { flex-wrap: wrap; }
.mc-kind { font-weight: 600; }
.mc-fed-select { max-width: 340px; }
/* 目标选择器旁的中继徽章（via_node 非空——联邦导入条目） */
.mc-relay-badge { white-space: nowrap; flex-shrink: 0; }
.mc-select {
  padding: 5px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B); font-size: 13px; min-width: 200px; font-family: inherit;
}
.mc-banner-warn {
  background: #fff4e0; color: #8a5a00; border: 1px solid #f0d9a8;
  padding: 8px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px;
}
.mc-banner-info {
  background: #eff6ff; color: #1d4ed8; border: 1px solid #bfdbfe;
  padding: 8px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px;
}
.mc-chat-area {
  /* 消息区：flex 占满面板剩余空间 + 内部滚动（overflow≠visible 使 flex 收缩
   * 生效——长对话只在区内滚，composer 恒钉底）。min-height:0（去 240px
   * 下限）：短窗口下也能收缩，不把 composer 顶出面板。 */
  padding: 12px; flex: 1 1 auto; min-height: 0; overflow-y: auto;
  display: flex; flex-direction: column; gap: 10px;
}
.mc-empty { margin: auto; text-align: center; color: var(--text-muted, #5E5C5F); display: flex; flex-direction: column; align-items: center; gap: 8px; }
.mc-bubble-row { display: flex; }
.mc-row-user { justify-content: flex-end; }
.mc-row-ai { justify-content: flex-start; }
.mc-bubble {
  max-width: 75%; padding: 8px 12px; border-radius: 12px; position: relative;
  display: flex; flex-direction: column; gap: 4px;
}
.mc-b-user { background: var(--accent, #E95420); color: #fff; border-bottom-right-radius: 4px; }
.mc-b-ai {
  background: var(--border-soft, #FAFAFA); color: var(--text, #2B2B2B);
  border-bottom-left-radius: 4px; border: 1px solid var(--border, #E5E5E5);
}
.mc-b-error { background: #fee2e2; color: #b91c1c; border-color: #f5c5c0; }
.mc-role-tag { font-size: 10px; opacity: 0.7; text-transform: uppercase; letter-spacing: 0.5px; }
.mc-bubble-content { white-space: pre-wrap; word-break: break-word; font-size: 14px; line-height: 1.5; }
.mc-cursor { animation: mc-blink 1s steps(2) infinite; opacity: 0.7; margin-left: 1px; }
@keyframes mc-blink { 0%, 50% { opacity: 0.7; } 50.01%, 100% { opacity: 0; } }
/* 思考段（vLLM 0.28 reasoning / 0.27 reasoning_content）：气泡内折叠展示 +
 * content 被思考段吃满时的提示条（Qwen 思考模型提示保留） */
.mc-reasoning { border-top: 1px dashed var(--border, #d1d5db); padding-top: 4px; }
.mc-reasoning summary {
  cursor: pointer; font-size: 11.5px; color: var(--text-muted, #5E5C5F);
  user-select: none;
}
.mc-reasoning-text {
  margin: 6px 0 2px; white-space: pre-wrap; word-break: break-word;
  font-size: 12px; color: var(--text-muted, #5E5C5F); font-family: var(--mono);
}
.mc-thinking-hint {
  margin: 0; font-size: 12px; color: var(--text-muted, #5E5C5F);
}
/* composer 钉底：shrink 0 防挤压——只让消息区滚动，输入区绝不被压缩 */
.mc-input-area {
  padding: 12px; display: flex; gap: 10px; align-items: flex-end;
  flex-shrink: 0;
  background: var(--bg-card, #fff);
  border-top: 1px solid var(--border-soft, #EDEDED);
}
.mc-textarea {
  flex: 1; resize: none; padding: 10px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.mc-textarea:focus { outline: none; border-color: var(--accent, #E95420); }
/* max_tokens 快捷输入（思考模型小额度会吃满只剩思考段，可在此调大） */
.mc-max-tokens {
  display: flex; flex-direction: column; gap: 2px; flex: 0 0 auto; text-align: center;
}
.mc-max-tokens input {
  width: 96px; padding: 8px 6px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: var(--mono); font-size: 13px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B); text-align: center;
}
.mc-max-tokens input:focus { outline: none; border-color: var(--accent, #E95420); }

/* ＋ 手动输入外部 API（2026-09-03）：行内小表单卡 + 临时目标生效横幅 */
.mc-manual-card { padding: 12px 14px; display: flex; flex-direction: column; gap: 10px; }
.mc-manual-grid { display: flex; gap: 10px; flex-wrap: wrap; }
.mc-manual-grid .field { min-width: 180px; }
.mc-manual-field-url { flex: 2 1 300px; }
.manual-hint { margin: 0; line-height: 1.55; }
.mc-manual-active { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.mc-manual-active .head-actions { margin-left: auto; }

/* GPU 监控 */
.gpu-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; }
.gpu-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.gpu-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.gpu-card-name { font-size: 15px; font-weight: 600; }
.gpu-row { display: grid; grid-template-columns: 90px 1fr; align-items: center; gap: 10px; font-size: 13px; }

/* 参数说明 */
.params-card { padding: 18px 22px; }
.params-title { font-size: 16px; font-weight: 600; margin-bottom: 4px; }
.param-list { margin: 12px 0 0; display: flex; flex-direction: column; gap: 14px; }
.param-item { display: flex; flex-direction: column; gap: 4px; }
.param-item dt { font-family: var(--mono); font-size: 13px; font-weight: 600; color: var(--accent, #E95420); }
.param-item dd { margin: 0; font-size: 13.5px; line-height: 1.6; color: var(--text, #2B2B2B); }

/* 生成 Tab */
.gen-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; }
.gen-card-head { padding: 0 0 2px; border-bottom: none; }
.gen-form { display: flex; flex-direction: column; gap: 10px; }
.gen-prompt-input {
  width: 100%; resize: vertical; padding: 9px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff);
}
.gen-submit-field { max-width: 220px; }
.gen-submit-btn { width: 100%; white-space: nowrap; }
/* 隐藏标签占位（按钮与输入框底对齐，但保留无障碍 label） */
.gen-hidden-label { visibility: hidden; }
.gen-result { display: flex; flex-direction: column; gap: 8px; align-items: flex-start; border-top: 1px solid var(--border-soft, #EDEDED); padding-top: 12px; }
.gen-image {
  max-width: 100%; max-height: 420px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border-soft, #EDEDED); object-fit: contain; background: var(--bg-code, #fafafa);
}
.gen-result-meta { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.gen-download-btn { text-decoration: none; display: inline-block; }
.gen-task-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; }
.gen-task-item {
  display: flex; flex-direction: column; gap: 6px; padding: 10px 12px;
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
}
.gen-task-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.gen-task-prompt { font-size: 13.5px; color: var(--text, #2B2B2B); word-break: break-word; flex: 1; min-width: 0; }
.gen-task-meta { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.gen-task-meta a { color: var(--accent, #E95420); }
.gen-task-error { word-break: break-all; }

/* 配方库 Tab（vLLM Recipes 导入） */
.rc-intro {
  padding: 14px 18px; display: flex; align-items: center; justify-content: space-between;
  gap: 12px; flex-wrap: wrap;
}
.rc-intro-text { display: flex; flex-direction: column; gap: 4px; min-width: 260px; flex: 1; }
.rc-search {
  padding: 6px 12px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-family: inherit; font-size: 13px; background: var(--bg-card, #fff); min-width: 220px;
}
.rc-search:focus { outline: none; border-color: var(--accent, #E95420); }
/* 官方同款树状目录（2026-08-30 重构，取代 pill 过滤器 + 每提供方一张表）：
   左侧提供方→模型两级可折叠树 + 右侧选中速览 */
.rc-tree-layout {
  display: grid;
  grid-template-columns: minmax(300px, 380px) 1fr;
  gap: 12px;
  align-items: start;
}
@media (max-width: 900px) {
  .rc-tree-layout { grid-template-columns: 1fr; }
}
.rc-tree {
  max-height: 62vh;
  overflow: auto;
  padding: 6px;
}
.rc-tree-loading { padding: 18px 14px; font-size: 13px; }
.rc-tree-group { margin-bottom: 2px; }
.rc-tree-parent {
  display: flex; align-items: center; gap: 8px; width: 100%;
  padding: 7px 10px; border: none; border-radius: var(--radius-sm, 8px);
  background: transparent; font-family: inherit; font-size: 13.5px; font-weight: 600;
  color: var(--text, #2B2B2B); cursor: pointer; text-align: left;
  transition: background 0.12s ease;
}
.rc-tree-parent:hover { background: rgba(0, 0, 0, 0.05); }
.rc-tree-caret {
  display: inline-block; font-size: 11px; color: var(--text-muted, #5E5C5F);
  transition: transform 0.15s ease; line-height: 1;
}
.rc-tree-caret.open { transform: rotate(90deg); }
.rc-tree-provider { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.rc-tree-count {
  font-size: 11px; font-weight: 600; padding: 0 7px; border-radius: var(--radius-pill, 20px);
  background: rgba(0, 0, 0, 0.06); color: var(--text-muted, #5E5C5F); flex-shrink: 0;
}
.rc-tree-children { margin: 0; padding: 0 0 4px 14px; list-style: none; }
.rc-tree-leaf {
  display: flex; flex-direction: column; align-items: flex-start; gap: 1px; width: 100%;
  padding: 5px 10px; border: none; border-radius: var(--radius-sm, 8px);
  background: transparent; font-family: inherit; text-align: left; cursor: pointer;
  transition: background 0.12s ease;
}
.rc-tree-leaf:hover { background: rgba(0, 0, 0, 0.05); }
.rc-tree-leaf.selected { background: rgba(233, 84, 32, 0.1); }
.rc-tree-title { font-size: 13px; color: var(--text, #2B2B2B); line-height: 1.35; }
.rc-tree-leaf.selected .rc-tree-title { font-weight: 600; color: var(--accent, #E95420); }
.rc-tree-hf { font-size: 11px; color: var(--text-muted, #5E5C5F); }
.rc-quick { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.rc-quick-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.rc-quick-actions { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 4px; }
.rc-quick-hint { line-height: 1.55; }
.rc-quick-empty { display: flex; flex-direction: column; gap: 8px; padding: 26px 8px; text-align: center; }
.rc-quick-empty-title { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); }
.rc-quick-empty a { color: var(--accent, #E95420); }
.rc-title { font-weight: 600; font-size: 13.5px; }
.rc-modal { width: min(780px, 100%); }
.rc-detail-body { gap: 16px; }
.rc-meta-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.rc-desc { margin: 0; font-size: 13.5px; line-height: 1.6; color: var(--text, #2B2B2B); }
.rc-section { display: flex; flex-direction: column; gap: 8px; border-top: 1px solid var(--border-soft, #EDEDED); padding-top: 12px; }
.rc-section-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.rc-cmd {
  margin: 0; padding: 10px 12px; background: var(--bg-code, #fafafa);
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
  font-size: 12.5px; line-height: 1.6; white-space: pre-wrap; word-break: break-word;
  color: var(--text, #2B2B2B);
}
.rc-variants { width: 100%; border-collapse: collapse; font-size: 13px; }
.rc-variants th, .rc-variants td {
  text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.rc-variants th { font-size: 12px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.rc-guide { border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); padding: 8px 12px; }
.rc-guide summary { cursor: pointer; font-size: 13px; font-weight: 600; color: var(--accent, #E95420); }
.rc-guide-body { padding-top: 8px; font-size: 13.5px; line-height: 1.7; overflow-x: auto; }
.rc-guide-body pre { background: var(--bg-code, #fafafa); padding: 8px 10px; border-radius: 6px; overflow-x: auto; }
.rc-guide-body code { background: var(--bg-code, #fafafa); padding: 1px 5px; border-radius: 4px; }
.rc-footer { justify-content: flex-start; margin-top: 4px; }
.rc-saved-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; }
.rc-saved-item {
  display: flex; flex-direction: column; gap: 8px; padding: 12px 14px;
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff);
}
.rc-saved-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.rc-saved-actions { display: flex; gap: 6px; flex-wrap: wrap; }
.rc-saved-actions .btn + .btn { margin-left: 0; }

/* ---- Tab 推理环境（env）：卡片样式体系复用 InstanceMonitor 的 stat-card 手法 ---- */
.env-create-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.env-create-form { display: flex; gap: 12px; flex-wrap: wrap; align-items: flex-end; }
.env-create-form .field { min-width: 160px; }
.env-create-submit { justify-content: flex-end; }
.env-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 14px; }
.env-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.env-card-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.env-name { font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); }
.env-status-spin { font-size: 11px; margin-right: 2px; }
.env-meta { display: flex; flex-direction: column; gap: 4px; font-size: 13px; }
.env-meta-row { display: flex; gap: 8px; align-items: baseline; }
.env-meta-label { width: 64px; flex: none; font-size: 12px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.env-path-row .mono { word-break: break-all; }
.env-mismatch { color: #92400e; font-size: 12px; }
.env-error-box { font-size: 12px; padding: 6px 10px; }
.env-actions { display: flex; gap: 6px; flex-wrap: wrap; margin-top: auto; }
.env-actions .btn + .btn { margin-left: 0; }
.env-task-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 12px; }
.env-task-list { display: flex; flex-direction: column; gap: 6px; max-height: 180px; overflow: auto; }
.env-task-item {
  display: flex; align-items: center; gap: 10px; padding: 6px 10px; background: transparent;
  border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px);
  cursor: pointer; font-size: 12.5px; font-family: inherit; color: var(--text, #2B2B2B);
}
.env-task-item:hover { background: rgba(0, 0, 0, 0.03); }
.env-task-item.active { border-color: var(--accent, #E95420); }
.env-log-wrap { display: flex; flex-direction: column; gap: 6px; }
.env-log {
  background: var(--bg-code, #fafafa); border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px); padding: 10px 12px; font-size: 12px; line-height: 1.6;
  max-height: 260px; overflow: auto; white-space: pre-wrap; word-break: break-all;
}
.env-log-line { color: var(--text-muted, #5E5C5F); }
/* 安装命令预览（创建表单下方 + 更新对话框内；等宽代码块，随渠道/版本联动） */
.env-cmd-preview { display: flex; flex-direction: column; gap: 4px; }
.env-cmd-preview code {
  background: var(--bg-code, #fafafa); border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px); padding: 8px 12px; font-size: 12px; line-height: 1.6;
  white-space: pre-wrap; word-break: break-all; display: block; color: var(--text, #2B2B2B);
}

/* ============ 「外部 API」Tab（2026-08-31；登记卡/测试面板/精简聊天窗） ============ */
.ext-form-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.ext-form-grid { display: flex; gap: 10px; flex-wrap: wrap; }
.ext-form-grid .field { min-width: 180px; }
.ext-field-url { flex: 2 1 320px; }
.ext-field-models { flex: 2 1 320px; }
.ext-row-card { padding: 12px 16px; display: flex; flex-direction: column; gap: 8px; }
.ext-row-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.ext-row-title { display: flex; align-items: center; gap: 8px; min-width: 0; }
.ext-meta { display: flex; gap: 18px; flex-wrap: wrap; margin: 0; font-size: 13px; }
.ext-meta dt { color: var(--text-muted, #5E5C5F); margin-right: 4px; display: inline; }
.ext-meta dd { display: inline; margin: 0; }
.ext-meta > div { min-width: 0; }
/* via_node 遮蔽态：占位文案 + admin 展开小按钮（内网地址不进普通视角 DOM 文本） */
.ext-url-masked { display: inline-flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.ext-url-toggle { padding: 1px 8px; font-size: 12px; }
.ext-models { display: flex; flex-wrap: wrap; gap: 6px; }
.ext-model-chip {
  background: var(--bg-code, #fafafa); border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-pill, 20px); padding: 2px 10px; font-size: 12px;
}
.ext-test-card { padding: 12px 16px; display: flex; flex-direction: column; gap: 8px; }
.ext-test-ok { border-left: 3px solid #15803d; }
.ext-test-err { border-left: 3px solid #b91c1c; }

@media (max-width: 640px) {
  .gpu-mini-item { grid-template-columns: 1fr; gap: 4px; }
  .field-row { flex-direction: column; }
  .gen-submit-field { max-width: none; }
  .env-create-form .field { min-width: 100%; }
  .ext-form-grid .field { min-width: 100%; }
  .mc-manual-grid .field { min-width: 100%; }
}
</style>
