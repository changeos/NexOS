<script setup lang="ts">
// =============================================================================
// AgentHub.vue —— Agent 集合：常用 AI coding agent 一键安装中心
//
// 3 Tab：集合（目录 + 一键安装/卸载）/ 任务（安装任务 + 日志）/ 自定义（发布）
// 后端：/api/v1/agenthub/* （AgentHubRouteHandler，docs/AGENT_HUB.md）
//
// 预置目录：OpenCode / OpenClaw / Claude Code / Codex / Gemini CLI / Qwen Code
// / Aider / Goose / Crush；渠道 npm / script / uv / cargo；已安装探测
// command -v；工具链探测 node/npm/uv/cargo/curl。缺工具链时工具链区显示
// 「安装」按钮（手动触发用户态安装，任务面板轮询日志；i18n 键 agentHub.*）。
//
// Web 界面 agent：目录条目带 web 描述符（仅实测确认有 Web UI 的 agent，
// 首期 OpenCode）的已装卡片显示「打开界面」——未跑点击即后台起服务（转圈）
// 完成后 window.open 新标签直达；已跑直接打开；旁附「停止」小按钮与状态点。
// 状态 5s 轮询（仅集合页可见时）；启动失败经消息条展示后端带回的日志尾。
// =============================================================================
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

const { t } = useI18n();

// =============================================================================
// 数据模型
// =============================================================================
interface CatalogAgent {
  id?: string;
  name?: string;
  description?: string;
  category?: string;
  icon?: string;
  source?: string;
  install_type?: string;
  install_target?: string;
  check_binary?: string;
  homepage?: string;
  publisher?: string;
  tags?: string[];
  installed?: boolean;
  /** Web 界面描述符（仅实测确认有 Web UI 的 agent 标注，首期 OpenCode）。 */
  web?: AgentWebDesc;
  [k: string]: unknown;
}
/** agent Web 界面描述符（start_cmd/port/url_path/note，见 docs/AGENT_HUB.md）。 */
interface AgentWebDesc {
  start_cmd?: string[];
  port?: number;
  url_path?: string;
  note?: string;
}
/** agent Web 服务状态（GET /api/v1/agenthub/web/:id/status）。 */
interface WebStatus {
  agent_id?: string;
  running?: boolean;
  url?: string | null;
  pid?: number | null;
  port?: number;
  started_at?: string | null;
  log_tail?: string | null;
}
interface AgentTask {
  id?: string;
  agent_id?: string;
  agent_name?: string;
  action?: string;
  install_type?: string;
  status?: string;
  pid?: number | null;
  error?: string | null;
  log_tail?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface ToolchainInfo {
  name?: string;
  available?: boolean;
  version?: string;
  [k: string]: unknown;
}
/** 工具链安装任务（GET /api/v1/agenthub/toolchain/install/tasks/:id）。 */
interface ToolchainTask {
  id?: string;
  toolchain?: string;
  status?: string;
  log?: string[];
  started_at?: number;
  finished_at?: number | null;
  [k: string]: unknown;
}
interface AgentHubStats {
  total_agents?: number;
  installed?: number;
  toolchains_ready?: number;
  tasks?: number;
}

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'catalog' | 'tasks' | 'publish';
const activeTab = ref<TabKey>('catalog');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'catalog', label: '集合' },
  { key: 'tasks', label: '任务' },
  { key: 'publish', label: '自定义' },
];

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// =============================================================================
// 统计与工具链
// =============================================================================
const stats = ref<AgentHubStats>({});
async function loadStats(): Promise<void> {
  try {
    stats.value = ((await endpoints.agentHubStats()) as AgentHubStats) ?? {};
  } catch {
    stats.value = {};
  }
}

const toolchains = ref<ToolchainInfo[]>([]);
async function loadToolchains(): Promise<void> {
  try {
    const raw = await endpoints.agentHubToolchains();
    toolchains.value = Array.isArray(raw) ? (raw as ToolchainInfo[]) : [];
  } catch {
    toolchains.value = [];
  }
}

/** 工具链是否可用于某渠道（npm 渠道需 node+npm；script 需 curl+bash…按名字对应）。 */
function toolchainOk(name: string): boolean {
  return toolchains.value.some((t) => t.name === name && t.available);
}

// =============================================================================
// 工具链手动安装（node 覆盖 node+npm / uv / cargo；用户态安装，无需 sudo）
// =============================================================================
/** 可手动安装的工具链（curl/bash 为系统基础件不装）。 */
const tcInstallable = ['node', 'uv', 'cargo'] as const;
/** 某工具链是否缺失（node 只要 node/npm 任缺即缺失——安装一次覆盖两者）。 */
function tcMissing(name: string): boolean {
  if (name === 'node') return !(toolchainOk('node') && toolchainOk('npm'));
  return !toolchainOk(name);
}
/** 当前缺失且可装的工具链列表（按钮区数据源）。 */
const tcMissingList = computed(() => tcInstallable.filter((n) => tcMissing(n)));
/** 是否有缺失的可装工具链（提示行与按钮区的显隐；探测未加载时也隐藏按钮防误触）。 */
const hasMissingToolchain = computed(() => toolchains.value.length > 0 && tcMissingList.value.length > 0);

const tcBusy = ref('');
/** 当前盯梢的工具链安装任务（含日志；完成后自动刷新工具链探测）。 */
const tcTask = ref<ToolchainTask | null>(null);
const tcLogBox = ref<HTMLElement | null>(null);
let tcPollTimer: ReturnType<typeof setInterval> | null = null;

const tcTaskRunning = computed(() => tcTask.value?.status === 'running');

/** 拉工具链安装任务详情并滚到底；终态时停轮询 + 刷新工具链探测（按钮解禁）。 */
async function refreshTcTask(taskId: string): Promise<void> {
  try {
    const detail = (await endpoints.agentHubToolchainInstallTask(taskId)) as ToolchainTask;
    if (detail) tcTask.value = detail;
    await nextTick();
    if (tcLogBox.value) tcLogBox.value.scrollTop = tcLogBox.value.scrollHeight;
    if (detail && detail.status !== 'running') {
      stopTcPolling();
      // 任务终态：刷新工具链可用性（安装成功 → 前端安装按钮解禁）与统计
      await Promise.all([loadToolchains(), loadStats()]);
      if (detail.status === 'error') {
        msg.value = { kind: 'err', text: t('agentHub.installFailed') + (detail.toolchain ?? detail.id ?? '') };
      }
    }
  } catch {
    // 单任务拉失败保留旧内容（下轮轮询重试）
  }
}

function startTcPolling(): void {
  if (tcPollTimer) return;
  tcPollTimer = setInterval(() => {
    if (tcTaskRunning.value && tcTask.value?.id) {
      void refreshTcTask(tcTask.value.id);
    } else {
      stopTcPolling();
    }
  }, 2000);
}
function stopTcPolling(): void {
  if (tcPollTimer) {
    clearInterval(tcPollTimer);
    tcPollTimer = null;
  }
}

/** 手动安装工具链：确认弹窗（装到用户目录 + 预计下载量）→ 202 任务 → 面板轮询。 */
async function installToolchain(name: string): Promise<void> {
  const confirmKey =
    name === 'node' ? 'agentHub.nodeConfirm' : name === 'uv' ? 'agentHub.uvConfirm' : 'agentHub.cargoConfirm';
  if (!window.confirm(t(confirmKey))) return;
  tcBusy.value = name;
  msg.value = null;
  try {
    const res = (await endpoints.agentHubToolchainInstall(name)) as { task_id?: string; status?: string };
    msg.value = { kind: 'ok', text: t('agentHub.installSubmitted', { name }) };
    if (res?.task_id) {
      await refreshTcTask(res.task_id);
      startTcPolling();
    } else {
      // 幂等命中（已安装）等无任务分支：直接刷新探测
      await Promise.all([loadToolchains(), loadStats()]);
    }
  } catch (e) {
    msg.value = { kind: 'err', text: t('agentHub.installFailed') + friendlyError(e) };
  } finally {
    tcBusy.value = '';
  }
}

function tcTaskStatusLabel(s?: string): string {
  switch (s) {
    case 'running':
      return t('agentHub.taskRunning');
    case 'done':
      return t('agentHub.taskDone');
    case 'error':
      return t('agentHub.taskError');
    default:
      return s ?? '—';
  }
}
function tcTaskStatusClass(s?: string): string {
  switch (s) {
    case 'running':
      return 'pill-blue';
    case 'done':
      return 'pill-ok';
    case 'error':
      return 'pill-err';
    default:
      return 'pill-muted';
  }
}

// =============================================================================
// Tab1：集合
// =============================================================================
const agents = ref<CatalogAgent[]>([]);
const agentsLoading = ref(false);
const agentsError = ref('');
const selectedCategory = ref('');
const search = ref('');
const busyAgentId = ref('');
const busyAction = ref('');

const categoryChips = [
  { key: '', label: '全部' },
  { key: 'coding', label: '编码代理' },
  { key: 'assistant', label: '助手' },
  { key: 'custom', label: '自定义' },
];

async function loadAgents(): Promise<void> {
  agentsLoading.value = true;
  agentsError.value = '';
  try {
    const raw = await endpoints.agentHubAgents(selectedCategory.value || undefined);
    agents.value = Array.isArray(raw) ? (raw as CatalogAgent[]) : [];
  } catch (e) {
    agents.value = [];
    agentsError.value = friendlyError(e);
  } finally {
    agentsLoading.value = false;
  }
}

const filteredAgents = computed<CatalogAgent[]>(() => {
  const q = search.value.trim().toLowerCase();
  if (!q) return agents.value;
  return agents.value.filter((a) => {
    const name = (a.name ?? '').toLowerCase();
    const desc = (a.description ?? '').toLowerCase();
    const bin = (a.check_binary ?? '').toLowerCase();
    const target = (a.install_target ?? '').toLowerCase();
    return name.includes(q) || desc.includes(q) || bin.includes(q) || target.includes(q);
  });
});

function selectCategory(c: string): void {
  selectedCategory.value = c;
  void loadAgents();
}

/** 某渠道在本机是否可安装（缺工具链时按钮禁用并提示）。 */
function channelReady(t?: string): boolean {
  switch (t) {
    case 'npm':
      return toolchainOk('node') && toolchainOk('npm');
    case 'script':
      return toolchainOk('curl');
    case 'uv':
      return toolchainOk('uv');
    case 'cargo':
      return toolchainOk('cargo');
    default:
      return false;
  }
}

function channelHint(t?: string): string {
  switch (t) {
    case 'npm':
      return toolchainOk('npm') ? 'npm install -g' : '缺少 node/npm 工具链';
    case 'script':
      return toolchainOk('curl') ? 'curl -fsSL … | bash' : '缺少 curl';
    case 'uv':
      return toolchainOk('uv') ? 'uv tool install' : '缺少 uv 工具链';
    case 'cargo':
      return toolchainOk('cargo') ? 'cargo install' : '缺少 cargo 工具链';
    default:
      return '未知渠道';
  }
}

async function installAgent(a: CatalogAgent): Promise<void> {
  const id = String(a.id ?? '');
  if (!id) return;
  busyAgentId.value = id;
  busyAction.value = 'install';
  msg.value = null;
  try {
    await endpoints.agentHubInstall(id);
    msg.value = { kind: 'ok', text: `已创建安装任务：${a.name ?? id}（后台执行中）` };
    activeTab.value = 'tasks';
    await loadTasks();
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '安装失败：' + friendlyError(e) };
  } finally {
    busyAgentId.value = '';
    busyAction.value = '';
  }
}

async function uninstallAgent(a: CatalogAgent): Promise<void> {
  const id = String(a.id ?? '');
  if (!id) return;
  const bin = a.check_binary ?? id;
  if (!window.confirm(`确定卸载 ${a.name ?? id}？（移除可执行文件 ${bin}）`)) return;
  busyAgentId.value = id;
  busyAction.value = 'uninstall';
  msg.value = null;
  try {
    await endpoints.agentHubUninstall(id);
    msg.value = { kind: 'ok', text: `已创建卸载任务：${a.name ?? id}` };
    activeTab.value = 'tasks';
    await loadTasks();
    startPolling();
  } catch (e) {
    msg.value = { kind: 'err', text: '卸载失败：' + friendlyError(e) };
  } finally {
    busyAgentId.value = '';
    busyAction.value = '';
  }
}

// =============================================================================
// Web 界面（打开界面 / 停止 / 状态点 + 5s 轮询）
// =============================================================================
const webStatuses = ref<Record<string, WebStatus>>({});
/** 正在启动/停止 Web 服务的 agent id（转圈与互斥）。 */
const webBusyId = ref('');
/** 该 agent 正在执行的动作（start/stop——按钮转圈文案区分）。 */
const webBusyAction = ref<'start' | 'stop' | ''>('');
let webPollTimer: ReturnType<typeof setInterval> | null = null;

/** 已安装且带 web 描述符的 agent（状态轮询与「打开界面」按钮的数据源）。 */
const webAgents = computed<CatalogAgent[]>(() =>
  agents.value.filter((a) => a.installed && a.web),
);

function webStatusOf(id?: string): WebStatus | null {
  return id ? (webStatuses.value[id] ?? null) : null;
}

/** 拉全部 web agent 状态（单个失败跳过本轮，不整体报错）。 */
async function refreshWebStatuses(): Promise<void> {
  const ids = webAgents.value.map((a) => String(a.id ?? '')).filter(Boolean);
  await Promise.all(
    ids.map(async (id) => {
      try {
        const s = (await endpoints.agentHubWebStatus(id)) as WebStatus;
        webStatuses.value = { ...webStatuses.value, [id]: s ?? { running: false } };
      } catch {
        /* 单个状态拉失败保留旧值，下轮重试 */
      }
    }),
  );
}

function startWebPolling(): void {
  if (webPollTimer) return;
  webPollTimer = setInterval(() => {
    // 仅集合页可见且有 web agent 时轮询（卡片不在屏不盯梢）
    if (activeTab.value === 'catalog' && webAgents.value.length > 0) {
      void refreshWebStatuses();
    }
  }, 5000);
}
function stopWebPolling(): void {
  if (webPollTimer) {
    clearInterval(webPollTimer);
    webPollTimer = null;
  }
}

/** 「打开界面」：已跑直接开新标签；未跑 → POST start（转圈）→ 完成即直达；
 * 启动失败展示后端带回的日志尾（错误串内嵌）。 */
async function openWebUi(a: CatalogAgent): Promise<void> {
  const id = String(a.id ?? '');
  if (!id || webBusyId.value) return;
  const st = webStatusOf(id);
  if (st?.running && st.url) {
    window.open(st.url, '_blank', 'noopener');
    return;
  }
  webBusyId.value = id;
  webBusyAction.value = 'start';
  msg.value = null;
  try {
    const res = (await endpoints.agentHubWebStart(id)) as { url?: string };
    if (res?.url) {
      window.open(res.url, '_blank', 'noopener');
    } else {
      msg.value = { kind: 'err', text: t('agentHub.webStartFailed') + JSON.stringify(res ?? {}) };
    }
    await refreshWebStatuses();
  } catch (e) {
    // 后端 500 的 error 串已内嵌日志尾，直接展示
    msg.value = { kind: 'err', text: t('agentHub.webStartFailed') + friendlyError(e) };
    await refreshWebStatuses();
  } finally {
    webBusyId.value = '';
    webBusyAction.value = '';
  }
}

/** 「停止」小按钮：确认 → POST stop → 刷新状态点。 */
async function stopWebUi(a: CatalogAgent): Promise<void> {
  const id = String(a.id ?? '');
  if (!id || webBusyId.value) return;
  if (!window.confirm(t('agentHub.webStopConfirm', { name: a.name ?? id }))) return;
  webBusyId.value = id;
  webBusyAction.value = 'stop';
  msg.value = null;
  try {
    await endpoints.agentHubWebStop(id);
    msg.value = { kind: 'ok', text: t('agentHub.webStoppedOk', { name: a.name ?? id }) };
    await refreshWebStatuses();
  } catch (e) {
    msg.value = { kind: 'err', text: t('agentHub.webStopFailed') + friendlyError(e) };
  } finally {
    webBusyId.value = '';
    webBusyAction.value = '';
  }
}

// =============================================================================
// Tab2：任务
// =============================================================================
const tasks = ref<AgentTask[]>([]);
const tasksLoading = ref(false);
const tasksError = ref('');
const expandedTask = ref<string>('');
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function loadTasks(): Promise<void> {
  tasksLoading.value = true;
  tasksError.value = '';
  try {
    const raw = await endpoints.agentHubTasks();
    tasks.value = Array.isArray(raw) ? (raw as AgentTask[]) : [];
  } catch (e) {
    tasks.value = [];
    tasksError.value = friendlyError(e);
  } finally {
    tasksLoading.value = false;
  }
}

const taskColumns: Column<AgentTask>[] = [
  { key: 'agent_name', title: 'Agent', sortable: true },
  { key: 'action', title: '动作', width: '90px' },
  { key: 'install_type', title: '渠道', width: '90px' },
  { key: 'status', title: '状态', width: '100px' },
  { key: 'pid', title: 'PID', width: '80px' },
  { key: 'created_at', title: '创建时间', width: '170px' },
  { key: 'actions', title: '操作', width: '100px', align: 'right' },
];

async function refreshTaskDetail(taskId: string): Promise<void> {
  try {
    const t = (await endpoints.agentHubTaskDetail(taskId)) as AgentTask;
    const i = tasks.value.findIndex((x) => x.id === taskId);
    if (i >= 0 && t) tasks.value[i] = t;
  } catch {
    /* 详情拉取失败保持现状 */
  }
}

function toggleTaskLog(taskId: string): void {
  expandedTask.value = expandedTask.value === taskId ? '' : taskId;
}

function statusClass(s?: string): string {
  switch (s) {
    case 'running':
      return 'pill-blue';
    case 'pending':
      return 'pill-muted';
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
    case 'running':
      return '执行中';
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
function actionLabel(a?: string): string {
  return a === 'uninstall' ? '卸载' : '安装';
}

function installTypeBadge(t?: string): { cls: string; label: string } {
  switch (t) {
    case 'npm':
      return { cls: 'pill-cyan', label: 'npm' };
    case 'script':
      return { cls: 'pill-purple', label: '脚本' };
    case 'uv':
      return { cls: 'pill-blue', label: 'uv' };
    case 'cargo':
      return { cls: 'pill-muted', label: 'cargo' };
    default:
      return { cls: 'pill-muted', label: t ?? '—' };
  }
}

const hasActiveTasks = computed(() =>
  tasks.value.some((t) => t.status === 'running' || t.status === 'pending'),
);

function startPolling(): void {
  if (pollTimer) return;
  pollTimer = setInterval(() => {
    if (hasActiveTasks.value) {
      void loadTasks();
      if (expandedTask.value) void refreshTaskDetail(expandedTask.value);
    } else {
      stopPolling();
      // 活跃任务清零时刷新目录（installed 状态可能已变）
      void loadAgents();
      void loadStats();
    }
  }, 3000);
}
function stopPolling(): void {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

// =============================================================================
// Tab3：自定义（发布 + 管理）
// =============================================================================
const publishForm = ref({
  name: '',
  description: '',
  install_type: 'npm',
  install_target: '',
  check_binary: '',
  homepage: '',
});
const publishing = ref(false);
const userAgents = computed<CatalogAgent[]>(() =>
  agents.value.filter((a) => a.source === 'user'),
);

async function submitPublish(): Promise<void> {
  const f = publishForm.value;
  if (!f.name.trim() || !f.install_target.trim() || !f.check_binary.trim()) {
    msg.value = { kind: 'err', text: '名称 / 安装目标 / 可执行文件名 均不可为空' };
    return;
  }
  publishing.value = true;
  msg.value = null;
  try {
    const created = await endpoints.agentHubPublish({ ...f });
    msg.value = {
      kind: 'ok',
      text: `已发布自定义 agent：${(created as CatalogAgent)?.name ?? f.name}`,
    };
    publishForm.value = {
      name: '',
      description: '',
      install_type: 'npm',
      install_target: '',
      check_binary: '',
      homepage: '',
    };
    await loadAgents();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: '发布失败：' + friendlyError(e) };
  } finally {
    publishing.value = false;
  }
}

async function deleteUserAgent(a: CatalogAgent): Promise<void> {
  const id = String(a.id ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除自定义 agent ${a.name ?? id}？`)) return;
  try {
    await endpoints.deleteAgentHubPublished(id);
    msg.value = { kind: 'ok', text: `已删除：${a.name ?? id}` };
    await loadAgents();
    await loadStats();
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  }
}

/** 安装目标输入框占位文案（按渠道）。 */
const targetPlaceholder = computed(() => {
  switch (publishForm.value.install_type) {
    case 'npm':
      return 'npm 包名，如 my-agent-cli';
    case 'script':
      return 'https://example.com/install.sh';
    case 'uv':
      return 'uv 包名，如 some-agent';
    case 'cargo':
      return 'crate 名，如 some-agent';
    default:
      return '安装目标';
  }
});

const userColumns: Column<CatalogAgent>[] = [
  { key: 'name', title: '名称', sortable: true },
  { key: 'install_type', title: '渠道', width: '90px' },
  { key: 'install_target', title: '安装目标' },
  { key: 'check_binary', title: '命令', width: '120px' },
  { key: 'actions', title: '操作', width: '90px', align: 'right' },
];

// =============================================================================
// 刷新与初始化
// =============================================================================
async function refreshAll(): Promise<void> {
  await Promise.all([loadAgents(), loadToolchains(), loadTasks(), loadStats()]);
  await refreshWebStatuses(); // 依赖 agents（web 描述符 + installed）
  if (hasActiveTasks.value) startPolling();
  startWebPolling();
}

onMounted(() => {
  void refreshAll();
});

onBeforeUnmount(() => {
  stopPolling();
  stopTcPolling();
  stopWebPolling();
});
</script>

<template>
  <div class="ah-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">Agent 集合</h2>
        <div class="page-sub muted">常用 AI coding agent 一键安装 · OpenCode / OpenClaw / Claude Code / Codex …</div>
      </div>
      <div class="head-actions">
        <button
          class="btn btn-small"
          :disabled="agentsLoading || tasksLoading"
          @click="refreshAll"
        >
          <span class="spin" :class="{ spinning: agentsLoading || tasksLoading }" aria-hidden="true">↻</span>
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

    <!-- =================== Tab1 集合 =================== -->
    <section v-show="activeTab === 'catalog'" class="tab-panel">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">目录总数</div>
          <div class="stat-value">{{ stats.total_agents ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已安装</div>
          <div class="stat-value">{{ stats.installed ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">可用工具链</div>
          <div class="stat-value">{{ stats.toolchains_ready ?? 0 }}<span class="stat-unit">/ 5</span></div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">任务数</div>
          <div class="stat-value">{{ stats.tasks ?? 0 }}</div>
        </div>
      </section>

      <!-- 工具链徽章 + 手动安装入口（缺失项显示「安装」按钮：用户态安装，无需 sudo） -->
      <div class="toolchain-row">
        <span
          v-for="t in toolchains"
          :key="t.name"
          class="pill"
          :class="t.available ? 'pill-ok' : 'pill-muted'"
          :title="t.available ? t.version : '不可用'"
        >{{ t.name }} {{ t.available ? '✓' : '✗' }}</span>
        <template v-if="hasMissingToolchain">
          <button
            v-for="tc in tcMissingList"
            :key="tc"
            class="btn btn-small btn-primary"
            :disabled="tcBusy !== '' || tcTaskRunning"
            :title="tc === 'node' ? 'nvm 安装 Node.js LTS + npm 到 ~/.nvm' : `安装 ${tc} 到用户目录`"
            @click.stop="installToolchain(tc)"
          >{{ tcBusy === tc ? t('agentHub.installing') : `${t('agentHub.install')} ${tc}` }}</button>
        </template>
        <span v-if="hasMissingToolchain" class="muted small">{{ t('agentHub.missingHint') }}</span>
        <span v-else class="muted small">安装渠道依赖左侧工具链（npm 渠道需 node+npm 等）</span>
      </div>

      <!-- 工具链安装任务面板：轮询 2s，日志尾自动滚动（照推理环境任务面板样式） -->
      <div v-if="tcTask" class="card tc-task-card">
        <div class="tc-task-head">
          <span class="panel-title">
            {{ t('agentHub.taskPanel') }} · {{ tcTask.toolchain }}（{{ tcTask.id }}）
          </span>
          <span class="pill" :class="tcTaskStatusClass(tcTask.status)">
            {{ tcTaskStatusLabel(tcTask.status) }}
          </span>
        </div>
        <div ref="tcLogBox" class="tc-log mono">
          <div v-for="(line, i) in tcTask.log?.slice(-20)" :key="i" class="tc-log-line">{{ line }}</div>
        </div>
        <div class="muted small">{{ t('agentHub.logHint') }}</div>
      </div>

      <!-- 分类 + 搜索 -->
      <div class="store-toolbar">
        <div class="cat-chips">
          <button
            v-for="c in categoryChips"
            :key="c.key"
            class="cat-chip"
            :class="{ active: selectedCategory === c.key }"
            type="button"
            @click="selectCategory(c.key)"
          >{{ c.label }}</button>
        </div>
        <input
          v-model="search"
          class="search-input"
          type="text"
          placeholder="搜索 agent 名称 / 描述 / 命令名…"
        />
        <span class="muted small">{{ filteredAgents.length }} 个</span>
      </div>

      <div v-if="agentsError" class="error-box">{{ agentsError }}</div>
      <div v-if="agentsLoading && !agents.length" class="card empty-card">加载中…</div>
      <div v-else-if="!filteredAgents.length" class="card empty-card">
        未找到匹配的 agent，去<a class="link" @click="activeTab = 'publish'">自定义页</a>发布。
      </div>
      <div v-else class="agent-grid">
        <div v-for="a in filteredAgents" :key="a.id" class="card agent-card">
          <div class="agent-card-head">
            <span class="agent-icon-emoji">{{ a.icon || '🧩' }}</span>
            <div class="agent-card-title">
              <span class="agent-name">
                {{ a.name ?? '—' }}
                <a
                  v-if="a.homepage"
                  class="agent-home"
                  :href="a.homepage"
                  target="_blank"
                  rel="noopener noreferrer"
                  title="主页"
                >↗</a>
              </span>
              <span class="agent-publisher muted small">{{ a.publisher ?? '' }}</span>
            </div>
            <span class="pill" :class="installTypeBadge(a.install_type).cls">
              {{ installTypeBadge(a.install_type).label }}
            </span>
          </div>
          <p class="agent-desc">{{ a.description ?? '' }}</p>
          <div v-if="a.tags && a.tags.length" class="agent-tags">
            <span v-for="t in a.tags" :key="t" class="tag-chip">{{ t }}</span>
          </div>
          <div class="agent-target-row">
            <span class="muted small">命令</span>
            <code class="mono agent-target">{{ a.check_binary ?? '—' }}</code>
            <span class="muted small">目标</span>
            <code class="mono agent-target">{{ a.install_target ?? '—' }}</code>
          </div>
          <div class="agent-card-actions">
            <template v-if="a.installed">
              <span class="pill pill-ok">已安装</span>
              <!-- Web 界面（仅 web 描述符标注的 agent）：状态点 + 打开界面 + 停止 -->
              <template v-if="a.web">
                <span
                  class="web-dot"
                  :class="webStatusOf(a.id)?.running ? 'web-dot-on' : 'web-dot-off'"
                  :title="webStatusOf(a.id)?.running ? t('agentHub.webRunning') : t('agentHub.webStopped')"
                  aria-hidden="true"
                ></span>
                <button
                  class="btn btn-small btn-primary"
                  :disabled="webBusyId !== ''"
                  :title="a.web.note"
                  @click.stop="openWebUi(a)"
                >{{ webBusyId === a.id && webBusyAction === 'start' ? t('agentHub.webStarting') : t('agentHub.openUi') }}</button>
                <button
                  v-if="webStatusOf(a.id)?.running"
                  class="btn btn-small btn-danger"
                  :disabled="webBusyId !== ''"
                  :title="t('agentHub.webStop')"
                  @click.stop="stopWebUi(a)"
                >{{ webBusyId === a.id && webBusyAction === 'stop' ? t('agentHub.webStopping') : t('agentHub.webStop') }}</button>
              </template>
              <button
                v-if="a.install_type !== 'script'"
                class="btn btn-small btn-danger"
                :disabled="busyAgentId === a.id"
                :title="channelHint(a.install_type)"
                @click.stop="uninstallAgent(a)"
              >{{ busyAgentId === a.id && busyAction === 'uninstall' ? '创建中…' : '卸载' }}</button>
              <span v-else class="muted small" title="官方脚本安装，无统一卸载命令">脚本渠道不支持卸载</span>
            </template>
            <template v-else>
              <button
                class="btn btn-small btn-primary"
                :disabled="busyAgentId === a.id || !channelReady(a.install_type)"
                :title="channelHint(a.install_type)"
                @click.stop="installAgent(a)"
              >{{ busyAgentId === a.id && busyAction === 'install' ? '创建中…' : '一键安装' }}</button>
              <span v-if="!channelReady(a.install_type)" class="muted small">{{ channelHint(a.install_type) }}</span>
            </template>
          </div>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 任务 =================== -->
    <section v-show="activeTab === 'tasks'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">安装 / 卸载任务</span>
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
            empty-text="暂无任务，去集合页一键安装 agent。"
          >
            <template #cell-action="{ row }">
              <span class="pill" :class="row.action === 'uninstall' ? 'pill-muted' : 'pill-cyan'">
                {{ actionLabel(row.action) }}
              </span>
            </template>
            <template #cell-install_type="{ row }">
              <span class="pill" :class="installTypeBadge(row.install_type).cls">
                {{ installTypeBadge(row.install_type).label }}
              </span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-pid="{ row }">
              <span class="mono">{{ row.pid ?? '—' }}</span>
            </template>
            <template #cell-created_at="{ row }">
              <span class="mono small">{{ row.created_at ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button class="btn btn-small" @click.stop="toggleTaskLog(String(row.id ?? ''))">
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
          <button class="btn btn-small" @click="refreshTaskDetail(expandedTask)">刷新</button>
        </div>
        <template v-for="t in tasks" :key="t.id">
          <template v-if="t.id === expandedTask">
            <div v-if="t.error" class="task-error">错误：{{ t.error }}</div>
            <pre v-if="t.log_tail" class="task-log">{{ t.log_tail }}</pre>
            <p v-if="!t.error && !t.log_tail" class="muted small">暂无日志输出。</p>
          </template>
        </template>
      </div>

      <p v-if="hasActiveTasks" class="muted small">检测到执行中任务，状态每 3 秒自动刷新。</p>
    </section>

    <!-- =================== Tab3 自定义 =================== -->
    <section v-show="activeTab === 'publish'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">发布自定义 agent</span>
      </div>
      <div class="card publish-card">
        <form class="publish-form" @submit.prevent="submitPublish">
          <div class="field-row">
            <div class="field">
              <label for="ah-name">名称 *</label>
              <input id="ah-name" v-model="publishForm.name" type="text" placeholder="如 MyAgent CLI" :disabled="publishing" />
            </div>
            <div class="field">
              <label for="ah-type">安装渠道 *</label>
              <select id="ah-type" v-model="publishForm.install_type" :disabled="publishing">
                <option value="npm">npm（npm install -g）</option>
                <option value="script">script（curl -fsSL … | bash）</option>
                <option value="uv">uv（uv tool install）</option>
                <option value="cargo">cargo（cargo install）</option>
              </select>
            </div>
          </div>
          <div class="field-row">
            <div class="field">
              <label for="ah-target">安装目标 *</label>
              <input id="ah-target" v-model="publishForm.install_target" type="text" :placeholder="targetPlaceholder" :disabled="publishing" />
            </div>
            <div class="field">
              <label for="ah-bin">可执行文件名 *（探测已安装用）</label>
              <input id="ah-bin" v-model="publishForm.check_binary" type="text" placeholder="如 myagent" :disabled="publishing" />
            </div>
          </div>
          <div class="field-row">
            <div class="field">
              <label for="ah-home">主页</label>
              <input id="ah-home" v-model="publishForm.homepage" type="text" placeholder="https://…" :disabled="publishing" />
            </div>
            <div class="field">
              <label for="ah-desc">简介</label>
              <input id="ah-desc" v-model="publishForm.description" type="text" placeholder="一句话介绍" :disabled="publishing" />
            </div>
          </div>
          <div class="form-actions">
            <button class="btn btn-primary" type="submit" :disabled="publishing">
              {{ publishing ? '发布中…' : '发布到集合' }}
            </button>
          </div>
        </form>
      </div>

      <div class="panel-head">
        <span class="panel-title">我发布的（{{ userAgents.length }}）</span>
      </div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="userColumns"
            :rows="userAgents"
            empty-text="尚未发布自定义 agent。"
          >
            <template #cell-install_type="{ row }">
              <span class="pill" :class="installTypeBadge(row.install_type).cls">
                {{ installTypeBadge(row.install_type).label }}
              </span>
            </template>
            <template #cell-install_target="{ row }">
              <code class="mono agent-target">{{ row.install_target ?? '—' }}</code>
            </template>
            <template #cell-check_binary="{ row }">
              <code class="mono">{{ row.check_binary ?? '—' }}</code>
            </template>
            <template #cell-actions="{ row }">
              <button class="btn btn-small btn-danger" @click.stop="deleteUserAgent(row)">删除</button>
            </template>
          </DataTable>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.ah-page {
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

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-unit { font-size: 14px; color: var(--text-muted, #5E5C5F); }

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

/* 工具链徽章行 */
.toolchain-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }

/* 工具链安装任务面板（照推理环境任务面板样式） */
.tc-task-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.tc-task-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.tc-log {
  margin: 0; padding: 10px 12px; background: #1e1e1e; color: #e5e5e5;
  border-radius: var(--radius-sm, 8px); font-size: 12px; line-height: 1.5;
  font-family: var(--mono, monospace); white-space: pre-wrap; word-break: break-all;
  max-height: 240px; overflow: auto;
}
.tc-log-line { min-height: 1em; }

/* 分类 chips + 搜索 */
.store-toolbar { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.cat-chips { display: flex; gap: 6px; flex-wrap: wrap; }
.cat-chip {
  padding: 5px 12px; background: transparent; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-pill, 20px); font-size: 12.5px; color: var(--text, #2B2B2B);
  cursor: pointer; font-family: inherit; transition: all 0.15s ease;
}
.cat-chip:hover { background: rgba(0, 0, 0, 0.04); }
.cat-chip.active { background: rgba(233, 84, 32, 0.1); color: var(--accent, #E95420); border-color: var(--accent, #E95420); font-weight: 600; }
.search-input {
  flex: 1; min-width: 180px; padding: 7px 12px;
  border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }

/* agent 卡片网格 */
.agent-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(300px, 1fr)); gap: 14px; }
.agent-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 8px; }
.agent-card-head { display: flex; align-items: center; gap: 10px; }
.agent-icon-emoji { font-size: 28px; line-height: 1; }
.agent-card-title { display: flex; flex-direction: column; gap: 2px; flex: 1; min-width: 0; }
.agent-name { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); word-break: break-word; }
.agent-home { font-size: 12px; margin-left: 4px; color: var(--accent, #E95420); text-decoration: none; }
.agent-publisher { font-size: 11px; }
.agent-desc { margin: 0; font-size: 13px; line-height: 1.5; color: var(--text, #2B2B2B); min-height: 20px; }
.agent-tags { display: flex; gap: 6px; flex-wrap: wrap; }
.tag-chip {
  font-size: 11px; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  color: var(--text-muted, #5E5C5F); background: var(--border-soft, #F3F4F6);
}
.agent-target-row { display: flex; align-items: center; gap: 6px; font-size: 12px; flex-wrap: wrap; }
.agent-target { background: var(--border-soft, #F3F4F6); padding: 1px 6px; border-radius: 4px; font-size: 11px; word-break: break-all; }
.agent-card-actions { display: flex; justify-content: flex-end; align-items: center; gap: 8px; margin-top: 4px; flex-wrap: wrap; }

/* Web 界面状态点（绿=运行中，灰=未运行） */
.web-dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; flex: none; }
.web-dot-on { background: #22c55e; box-shadow: 0 0 4px rgba(34, 197, 94, 0.6); }
.web-dot-off { background: #d1d5db; }

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
  .field-row { grid-template-columns: 1fr; }
  .store-toolbar { flex-direction: column; align-items: stretch; }
}
</style>
