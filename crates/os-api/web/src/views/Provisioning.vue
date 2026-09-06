<script setup lang="ts">

// —— 一键安装 Tab（install.sh 动态脚本：入口地址=当前页面 origin，复制到剪贴板）——
const installUrl = ref(`${location.protocol}//${location.host}`);
const installCopied = ref(false);
async function copyInstallCmd(): Promise<void> {
  const text = `sudo bash -c "$(curl -fsSL ${installUrl.value}/api/v1/provisioning/install.sh)"`;
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
    } else {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      ta.remove();
    }
    installCopied.value = true;
    setTimeout(() => (installCopied.value = false), 1500);
  } catch { /* 剪贴板不可用静默 */ }
}
// =============================================================================
// Provisioning.vue —— 系统自举（System Provisioning）
//
// 4 Tab：PXE 网络引导 / ISO 镜像生成 / SSH 远程部署 / 电源控制
// 后端：/api/v1/provisioning/* （ProvisioningRouteHandler + PowerRouteHandler）
//
// 设计：Ubuntu Yaru 风格 .card / .page-head，统计卡 + 表格 + 对话框，三态加载。
// 红线：SSH 对话框仅支持私钥认证，无密码字段（IPMI 设备密码为 BMC 凭据，
// 仅写入后端 state 文件，列表/详情响应脱敏为 has_password 布尔）。
//
// 真实执行接线（docs/PROVISIONING.md）：
// - SSH 部署：POST /ssh/deploy 真实 scp/ssh 子进程执行；对话框切进度模式轮询
//   任务详情（文件级 ✓/✗/skipped + 耗时 + run_cmd 输出折叠区）；下方任务
//   历史表（GET /ssh/deploys，admin）有进行中任务时自动刷新。
// - ISO 构建：pending/failed 任务可「开始构建」（POST /iso/tasks/:id/build，
//   真实 mksquashfs→xorriso→sha256sum）；building 态列表 2s 自动轮询
//   （进度条 + 当前步骤），构建日志折叠面板实时展示。
// - 电源控制（PXE 流水线第一环）：本机 BMC（ipmitool in-band，缺失/无
//   /dev/ipmi0 时降级提示）+ 远程 IPMI 2.0 设备（lanplus test/status/power）
//   + 网段扫描（RMCP Presence Ping 免凭据发现，进度轮询，结果一键转设备）
//   + WoL 魔术唤醒（目标注册/ARP 邻居选 MAC/广播 ×3）。
// =============================================================================
import { computed, onMounted, onUnmounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

// =============================================================================
// 数据模型
// =============================================================================
type BootMode = 'bios' | 'uefi' | 'uefi_arm64' | string;

interface PxeConfig {
  enabled?: boolean;
  tftp_server?: string;
  boot_mode?: BootMode;
  http_repo?: string;
  default_bootfile?: string;
  [k: string]: unknown;
}
interface BootEntry {
  id?: string;
  name?: string;
  kernel?: string;
  initrd?: string;
  cmdline?: string;
  default?: boolean;
  [k: string]: unknown;
}
interface PxeStatus {
  running?: boolean;
  state?: string;
  [k: string]: unknown;
}

type IsoVariant = 'std' | 'clone' | string;
type IsoArch = 'x86_64' | 'aarch64' | string;
type IsoStatus = 'pending' | 'building' | 'completed' | 'failed' | string;
interface IsoTask {
  id?: string;
  name?: string;
  version?: string;
  variant?: IsoVariant;
  arch?: IsoArch;
  ubuntu_version?: string;
  status?: IsoStatus;
  iso_path?: string;
  sha256?: string;
  size_bytes?: number;
  created_at?: string;
  error?: string;
  /** building 时：当前步骤（mksquashfs / xorriso / sha256sum）与进度 0~1 */
  step?: string | null;
  progress?: number | null;
  /** 构建日志（building 时实时，终态后为完整快照） */
  build_log?: string[];
  [k: string]: unknown;
}

type SshStatus = 'unknown' | 'reachable' | 'unreachable' | string;
interface SshTarget {
  id?: string;
  name?: string;
  host?: string;
  port?: number;
  user?: string;
  private_key_path?: string;
  status?: SshStatus;
  last_checked?: string;
  created_at?: string;
  [k: string]: unknown;
}
interface DeployFile {
  local_path: string;
  remote_path: string;
}
type DeployStatus = 'pending' | 'transferring' | 'running' | 'completed' | 'failed' | string;
type FileResultStatus = 'pending' | 'success' | 'failed' | 'skipped' | string;
interface FileTransferResult {
  local_path?: string;
  remote_path?: string;
  status?: FileResultStatus;
  exit_code?: number | null;
  duration_ms?: number | null;
  error?: string | null;
  [k: string]: unknown;
}
interface CmdOutput {
  exit_code?: number;
  stdout?: string;
  stderr?: string;
  duration_ms?: number;
  [k: string]: unknown;
}
interface DeployTask {
  id?: string;
  target_id?: string;
  files?: DeployFile[];
  run_cmd?: string | null;
  status?: DeployStatus;
  created_at?: string;
  error?: string | null;
  results?: FileTransferResult[];
  cmd_output?: CmdOutput | null;
  started_at?: string | null;
  finished_at?: string | null;
  [k: string]: unknown;
}

// —— 电源控制（/api/v1/provisioning/power/*）——
interface KvLine {
  key?: string;
  value?: string;
  [k: string]: unknown;
}
interface BmcInfo {
  available?: boolean;
  ipmitool_found?: boolean;
  chassis?: KvLine[];
  sel?: KvLine[];
  mc?: KvLine[];
  system_power?: string | null;
  hint?: string | null;
  error?: string | null;
  [k: string]: unknown;
}
interface SensorRow {
  name?: string;
  type?: string;
  reading?: string;
  status?: string;
  raw?: string;
  [k: string]: unknown;
}
interface SensorsInfo {
  available?: boolean;
  count?: number;
  truncated?: boolean;
  rows?: SensorRow[];
  hint?: string | null;
  [k: string]: unknown;
}
type IpmiDevStatus = 'unknown' | 'reachable' | 'unreachable' | string;
interface IpmiDevice {
  id?: string;
  name?: string;
  host?: string;
  port?: number;
  username?: string;
  password?: string | null;
  has_password?: boolean;
  cipher?: string | null;
  status?: IpmiDevStatus;
  last_checked?: string | null;
  created_at?: string;
  [k: string]: unknown;
}
interface DeviceTestResult {
  reachable?: boolean;
  system_power?: string | null;
  chassis?: KvLine[];
  output?: string;
  duration_ms?: number;
  [k: string]: unknown;
}
interface PowerActionResult {
  ok?: boolean;
  action?: string;
  target?: string;
  output?: string;
  error?: string | null;
  [k: string]: unknown;
}
interface ScanHit {
  ip?: string;
  rmcp_plus_supported?: boolean;
  ipmi_supported?: boolean;
  asf_version?: string;
  enterprise_iana?: number;
  [k: string]: unknown;
}
type ScanStatus = 'running' | 'completed' | 'failed' | string;
interface ScanTask {
  id?: string;
  cidr?: string;
  port?: number;
  status?: ScanStatus;
  scanned?: number;
  total?: number;
  found?: ScanHit[];
  error?: string | null;
  [k: string]: unknown;
}
interface WolTarget {
  id?: string;
  name?: string;
  mac?: string;
  broadcast?: string;
  port?: number;
  secureon_password?: string | null;
  has_secureon?: boolean;
  created_at?: string;
  [k: string]: unknown;
}
interface WakeResult {
  ok?: boolean;
  target?: string;
  mac?: string;
  broadcast?: string;
  port?: number;
  attempts?: number;
  sent?: number;
  bytes_per_packet?: number;
  secureon?: boolean;
  error?: string | null;
  [k: string]: unknown;
}
interface ArpEntry {
  ip?: string;
  mac?: string;
  dev?: string;
  state?: string;
  [k: string]: unknown;
}
interface ArpInfo {
  available?: boolean;
  neighbors?: ArpEntry[];
  hint?: string | null;
  [k: string]: unknown;
}

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'pxe' | 'iso' | 'ssh' | 'power' | 'install';
const activeTab = ref<TabKey>('pxe');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'install', label: '一键安装' },
  { key: 'pxe', label: 'PXE 网络引导' },
  { key: 'iso', label: 'ISO 镜像生成' },
  { key: 'ssh', label: 'SSH 远程部署' },
  { key: 'power', label: '电源控制' },
];

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该自举接口';
  }
  return m;
}

// =============================================================================
// 选项常量
// =============================================================================
const BOOT_MODES: { value: BootMode; label: string }[] = [
  { value: 'bios', label: 'BIOS（x86 传统）' },
  { value: 'uefi', label: 'UEFI（x86_64）' },
  { value: 'uefi_arm64', label: 'UEFI ARM64（aarch64）' },
];
const ISO_VARIANTS: { value: IsoVariant; label: string }[] = [
  { value: 'std', label: '标准 (std)' },
  { value: 'clone', label: '克隆 (clone)' },
];
const ISO_ARCHS: IsoArch[] = ['x86_64', 'aarch64'];

// =============================================================================
// Tab1：PXE 网络引导
// =============================================================================
const pxeStatus = ref<PxeStatus | null>(null);
const statusLoading = ref(false);
const statusError = ref('');

const isRunning = computed<boolean>(() => {
  if (!pxeStatus.value) return false;
  const r = pxeStatus.value.running;
  if (typeof r === 'boolean') return r;
  const s = String(pxeStatus.value.state ?? '').toLowerCase();
  return s === 'running' || s === 'active';
});

async function loadStatus(): Promise<void> {
  statusLoading.value = true;
  statusError.value = '';
  try {
    pxeStatus.value = (await endpoints.provisioningPxeStatus()) as PxeStatus;
  } catch (e) {
    pxeStatus.value = null;
    statusError.value = friendlyError(e);
  } finally {
    statusLoading.value = false;
  }
}

const toggling = ref(false);
async function startService(): Promise<void> {
  toggling.value = true;
  msg.value = null;
  try {
    await endpoints.startProvisioningPxe();
    await loadStatus();
    msg.value = { kind: 'ok', text: 'PXE 服务已启动' };
  } catch (e) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) };
  } finally {
    toggling.value = false;
  }
}
async function stopService(): Promise<void> {
  if (!window.confirm('确定停止 PXE 服务？')) return;
  toggling.value = true;
  msg.value = null;
  try {
    await endpoints.stopProvisioningPxe();
    await loadStatus();
    msg.value = { kind: 'ok', text: 'PXE 服务已停止' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
  } finally {
    toggling.value = false;
  }
}

// —— PXE 配置 ——
const configLoading = ref(false);
const configError = ref('');
const configSaving = ref(false);

const configForm = ref({
  enabled: false,
  tftp_server: '',
  boot_mode: 'uefi' as BootMode,
  http_repo: '',
  default_bootfile: '',
});

async function loadConfig(): Promise<void> {
  configLoading.value = true;
  configError.value = '';
  try {
    const raw = (await endpoints.provisioningPxeConfig()) as PxeConfig;
    configForm.value.enabled = !!raw.enabled;
    configForm.value.tftp_server = pickStr(raw, 'tftp_server', 'tftp_server_ip', 'tftp_ip');
    const mode = pickStr(raw, 'boot_mode', 'mode') as BootMode;
    configForm.value.boot_mode = BOOT_MODES.some((m) => m.value === mode) ? mode : 'uefi';
    configForm.value.http_repo = pickStr(raw, 'http_repo', 'http_repo_url', 'repo_url');
    configForm.value.default_bootfile = pickStr(raw, 'default_bootfile', 'default_boot_file');
  } catch (e) {
    configError.value = friendlyError(e);
  } finally {
    configLoading.value = false;
  }
}

async function saveConfig(): Promise<void> {
  configSaving.value = true;
  msg.value = { kind: 'info', text: '保存中…' };
  try {
    await endpoints.updateProvisioningPxeConfig({
      enabled: configForm.value.enabled,
      tftp_server: configForm.value.tftp_server.trim(),
      boot_mode: configForm.value.boot_mode,
      http_repo: configForm.value.http_repo.trim(),
      default_bootfile: configForm.value.default_bootfile.trim(),
    });
    msg.value = { kind: 'ok', text: '配置已保存' };
    void loadConfig();
  } catch (e) {
    msg.value = { kind: 'err', text: '保存失败：' + friendlyError(e) };
  } finally {
    configSaving.value = false;
  }
}

// —— 启动条目 ——
const entries = ref<BootEntry[]>([]);
const entriesLoading = ref(false);
const entriesError = ref('');
const entryBusyId = ref<string>('');

async function loadEntries(): Promise<void> {
  entriesLoading.value = true;
  entriesError.value = '';
  try {
    const raw = await endpoints.provisioningPxeBootEntries();
    const arr = Array.isArray(raw) ? raw : raw ? [raw] : [];
    entries.value = arr as BootEntry[];
  } catch (e) {
    entries.value = [];
    entriesError.value = friendlyError(e);
  } finally {
    entriesLoading.value = false;
  }
}

async function deleteEntry(row: BootEntry): Promise<void> {
  const id = String(row.id ?? row.name ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除启动条目「${row.name ?? id}」？该操作不可撤销。`)) return;
  entryBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteProvisioningPxeBootEntry(id);
    await loadEntries();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    entryBusyId.value = '';
  }
}

const entryColumns: Column<BootEntry>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? r.id ?? '—' },
  { key: 'kernel', title: '内核', accessor: (r) => r.kernel ?? '—' },
  { key: 'initrd', title: 'initrd', accessor: (r) => r.initrd ?? '—' },
  { key: 'cmdline', title: '内核参数', accessor: (r) => r.cmdline ?? '—' },
  { key: 'default', title: '默认', width: '80px', align: 'center', accessor: (r) => (r.default ? 1 : 0) },
  { key: 'actions', title: '操作', width: '100px', align: 'right' },
];

// 添加启动条目对话框
const showEntryCreate = ref(false);
const entryForm = ref({ name: '', kernel: '', initrd: '', cmdline: '', default: false });
const entrySubmitting = ref(false);

function openEntryCreate(): void {
  entryForm.value = { name: '', kernel: '', initrd: '', cmdline: '', default: false };
  msg.value = null;
  showEntryCreate.value = true;
}
function closeEntryCreate(): void {
  if (entrySubmitting.value) return;
  showEntryCreate.value = false;
}
async function submitEntryCreate(): Promise<void> {
  const name = entryForm.value.name.trim();
  const kernel = entryForm.value.kernel.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!kernel) { msg.value = { kind: 'err', text: '请填写内核路径' }; return; }
  entrySubmitting.value = true;
  msg.value = { kind: 'info', text: '添加中…' };
  try {
    await endpoints.addProvisioningPxeBootEntry({
      name,
      kernel,
      initrd: entryForm.value.initrd.trim() || undefined,
      cmdline: entryForm.value.cmdline.trim() || undefined,
      default: entryForm.value.default,
    });
    showEntryCreate.value = false;
    await loadEntries();
    msg.value = { kind: 'ok', text: '已添加' };
  } catch (e) {
    msg.value = { kind: 'err', text: '添加失败：' + friendlyError(e) };
  } finally {
    entrySubmitting.value = false;
  }
}

// =============================================================================
// Tab2：ISO 镜像生成
// =============================================================================
const isoTasks = ref<IsoTask[]>([]);
const isoLoading = ref(false);
const isoError = ref('');
const isoBusyId = ref<string>('');

async function loadIsoTasks(): Promise<void> {
  isoLoading.value = true;
  isoError.value = '';
  try {
    const raw = await endpoints.provisioningIsoTasks();
    isoTasks.value = Array.isArray(raw) ? (raw as IsoTask[]) : [];
  } catch (e) {
    isoTasks.value = [];
    isoError.value = friendlyError(e);
  } finally {
    isoLoading.value = false;
    syncIsoPolling();
  }
}

// —— 真实构建：开始构建 + building 态自动轮询 + 日志详情 ——
const isoBuildBusyId = ref<string>('');
const isoDetailId = ref<string>('');

/** 有任务在构建时每 2s 刷新列表（后端 building 态带实时 step/progress/log）。 */
let isoTimer: number | undefined;
function syncIsoPolling(): void {
  const building = isoTasks.value.some((t) => t.status === 'building');
  if (building && isoTimer === undefined) {
    isoTimer = window.setInterval(() => { void loadIsoTasks(); }, 2000);
  } else if (!building && isoTimer !== undefined) {
    window.clearInterval(isoTimer);
    isoTimer = undefined;
  }
}

async function startIsoBuild(row: IsoTask): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  isoBuildBusyId.value = id;
  msg.value = null;
  try {
    const updated = (await endpoints.buildProvisioningIsoTask(id)) as IsoTask;
    msg.value = updated.status === 'failed'
      ? { kind: 'err', text: `构建未启动：${updated.error ?? '未知原因'}` }
      : { kind: 'ok', text: '构建已启动，正在后台执行（mksquashfs → xorriso → sha256sum）' };
    await loadIsoTasks();
  } catch (e) {
    msg.value = { kind: 'err', text: '启动构建失败：' + friendlyError(e) };
  } finally {
    isoBuildBusyId.value = '';
  }
}

function toggleIsoDetail(row: IsoTask): void {
  isoDetailId.value = isoDetailId.value === row.id ? '' : String(row.id ?? '');
}

function isoProgressPct(t: IsoTask): number {
  if (t.status === 'completed') return 100;
  const p = typeof t.progress === 'number' ? t.progress : null;
  return p === null ? 0 : Math.round(p * 100);
}

const isoStats = computed(() => ({
  total: isoTasks.value.length,
  completed: isoTasks.value.filter((t) => t.status === 'completed').length,
  failed: isoTasks.value.filter((t) => t.status === 'failed').length,
}));

async function deleteIsoTask(row: IsoTask): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除 ISO 任务「${row.name ?? id}」？该操作不可撤销。`)) return;
  isoBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteProvisioningIsoTask(id);
    await loadIsoTasks();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    isoBusyId.value = '';
  }
}

function isoStatusClass(s?: string): string {
  switch (s) {
    case 'completed': return 'pill-ok';
    case 'building': return 'pill-warn';
    case 'pending': return 'pill-muted';
    case 'failed': return 'pill-err';
    default: return 'pill-muted';
  }
}
function isoStatusLabel(s?: string): string {
  switch (s) {
    case 'completed': return '已完成';
    case 'building': return '构建中';
    case 'pending': return '排队';
    case 'failed': return '失败';
    default: return s ?? '—';
  }
}
function isoVariantClass(v?: string): string {
  return v === 'clone' ? 'pill-purple' : 'pill-blue';
}
function isoVariantLabel(v?: string): string {
  return v === 'clone' ? '克隆' : v === 'std' ? '标准' : (v ?? '—');
}
function truncate(s: string | undefined, n = 12): string {
  if (!s) return '—';
  return s.length > n ? s.slice(0, n) + '…' : s;
}
function formatBytes(bytes?: number): string {
  if (!bytes || bytes <= 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

const isoColumns: Column<IsoTask>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'version', title: '版本', width: '90px', accessor: (r) => r.version ?? '—' },
  { key: 'variant', title: '变体', width: '90px', align: 'center', accessor: (r) => r.variant ?? '—' },
  { key: 'arch', title: '架构', width: '100px', align: 'center', accessor: (r) => r.arch ?? '—' },
  { key: 'ubuntu_version', title: 'Ubuntu', width: '100px', align: 'center', accessor: (r) => r.ubuntu_version ?? '—' },
  { key: 'status', title: '状态', width: '100px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'iso_path', title: '产物路径', accessor: (r) => r.iso_path ?? '—' },
  { key: 'sha256', title: 'SHA256', width: '130px', accessor: (r) => truncate(r.sha256, 12) },
  { key: 'size_bytes', title: '大小', width: '90px', align: 'right', accessor: (r) => formatBytes(r.size_bytes) },
  { key: 'actions', title: '操作', width: '230px', align: 'right' },
];

// 创建 ISO 任务对话框
const showIsoCreate = ref(false);
const isoForm = ref({
  name: '',
  version: '0.1.0',
  variant: 'std' as IsoVariant,
  arch: 'x86_64' as IsoArch,
  ubuntu_version: '26.04',
});
const isoSubmitting = ref(false);

function openIsoCreate(): void {
  isoForm.value = { name: '', version: '0.1.0', variant: 'std', arch: 'x86_64', ubuntu_version: '26.04' };
  msg.value = null;
  showIsoCreate.value = true;
}
function closeIsoCreate(): void {
  if (isoSubmitting.value) return;
  showIsoCreate.value = false;
}
async function submitIsoCreate(): Promise<void> {
  const name = isoForm.value.name.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  isoSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createProvisioningIsoTask({
      name,
      version: isoForm.value.version.trim() || undefined,
      variant: isoForm.value.variant,
      arch: isoForm.value.arch,
      ubuntu_version: isoForm.value.ubuntu_version.trim() || undefined,
    });
    showIsoCreate.value = false;
    await loadIsoTasks();
    msg.value = { kind: 'ok', text: '已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    isoSubmitting.value = false;
  }
}

// =============================================================================
// Tab3：SSH 远程部署
// =============================================================================
const sshTargets = ref<SshTarget[]>([]);
const sshLoading = ref(false);
const sshError = ref('');
const sshBusyId = ref<string>('');

async function loadSshTargets(): Promise<void> {
  sshLoading.value = true;
  sshError.value = '';
  try {
    const raw = await endpoints.provisioningSshTargets();
    sshTargets.value = Array.isArray(raw) ? (raw as SshTarget[]) : [];
  } catch (e) {
    sshTargets.value = [];
    sshError.value = friendlyError(e);
  } finally {
    sshLoading.value = false;
  }
}

const sshStats = computed(() => ({
  total: sshTargets.value.length,
  reachable: sshTargets.value.filter((t) => t.status === 'reachable').length,
}));

async function deleteSshTarget(row: SshTarget): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除 SSH 目标「${row.name ?? id}」？该操作不可撤销。`)) return;
  sshBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteProvisioningSshTarget(id);
    await loadSshTargets();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    sshBusyId.value = '';
  }
}

async function testSshTarget(row: SshTarget): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  sshBusyId.value = id;
  msg.value = { kind: 'info', text: '测试连接中…' };
  try {
    await endpoints.testProvisioningSshTarget(id);
    await loadSshTargets();
    msg.value = { kind: 'ok', text: '测试完成' };
  } catch (e) {
    msg.value = { kind: 'err', text: '测试失败：' + friendlyError(e) };
  } finally {
    sshBusyId.value = '';
  }
}

function sshStatusClass(s?: string): string {
  switch (s) {
    case 'reachable': return 'pill-ok';
    case 'unreachable': return 'pill-err';
    case 'unknown':
    default:
      return 'pill-muted';
  }
}
function sshStatusLabel(s?: string): string {
  switch (s) {
    case 'reachable': return '可达';
    case 'unreachable': return '不可达';
    case 'unknown': return '未知';
    default: return s ?? '—';
  }
}

const sshColumns: Column<SshTarget>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'host', title: '地址', accessor: (r) => `${r.host ?? '—'}:${r.port ?? 22}` },
  { key: 'user', title: '用户', width: '100px', accessor: (r) => r.user ?? '—' },
  { key: 'private_key_path', title: '密钥路径', accessor: (r) => truncate(r.private_key_path, 28) },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? 'unknown' },
  { key: 'last_checked', title: '最后检查', width: '160px', accessor: (r) => r.last_checked ?? '—' },
  { key: 'actions', title: '操作', width: '170px', align: 'right' },
];

// 添加 SSH 目标对话框（仅私钥认证，无密码字段）
const showSshCreate = ref(false);
const sshForm = ref({
  name: '',
  host: '',
  port: '22',
  user: 'root',
  private_key_path: '~/.ssh/id_ed25519',
});
const sshSubmitting = ref(false);

function openSshCreate(): void {
  sshForm.value = { name: '', host: '', port: '22', user: 'root', private_key_path: '~/.ssh/id_ed25519' };
  msg.value = null;
  showSshCreate.value = true;
}
function closeSshCreate(): void {
  if (sshSubmitting.value) return;
  showSshCreate.value = false;
}
async function submitSshCreate(): Promise<void> {
  const name = sshForm.value.name.trim();
  const host = sshForm.value.host.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!host) { msg.value = { kind: 'err', text: '请填写主机地址' }; return; }
  sshSubmitting.value = true;
  msg.value = { kind: 'info', text: '添加中…' };
  try {
    await endpoints.addProvisioningSshTarget({
      name,
      host,
      port: Number(sshForm.value.port) || undefined,
      user: sshForm.value.user.trim() || undefined,
      private_key_path: sshForm.value.private_key_path.trim() || undefined,
    });
    showSshCreate.value = false;
    await loadSshTargets();
    msg.value = { kind: 'ok', text: '已添加' };
  } catch (e) {
    msg.value = { kind: 'err', text: '添加失败：' + friendlyError(e) };
  } finally {
    sshSubmitting.value = false;
  }
}

// 部署对话框（选目标 + 多文件传输 + 远程命令）
const showDeploy = ref(false);
const deployForm = ref<{ target_id: string; files: DeployFile[]; run_cmd: string }>({
  target_id: '',
  files: [{ local_path: '', remote_path: '' }],
  run_cmd: '',
});
const deploySubmitting = ref(false);

function openDeploy(): void {
  deployForm.value = {
    target_id: sshTargets.value[0]?.id ?? '',
    files: [{ local_path: '', remote_path: '' }],
    run_cmd: '',
  };
  msg.value = null;
  showDeploy.value = true;
}
function closeDeploy(): void {
  if (deploySubmitting.value) return;
  showDeploy.value = false;
}
function addDeployFile(): void {
  deployForm.value.files.push({ local_path: '', remote_path: '' });
}
function removeDeployFile(idx: number): void {
  if (deployForm.value.files.length <= 1) return;
  deployForm.value.files.splice(idx, 1);
}
async function submitDeploy(): Promise<void> {
  const targetId = deployForm.value.target_id.trim();
  if (!targetId) { msg.value = { kind: 'err', text: '请选择目标' }; return; }
  const files = deployForm.value.files
    .map((f) => ({ local_path: f.local_path.trim(), remote_path: f.remote_path.trim() }))
    .filter((f) => f.local_path && f.remote_path);
  const runCmd = deployForm.value.run_cmd.trim();
  if (!files.length && !runCmd) { msg.value = { kind: 'err', text: '请至少填写一组文件传输或一条远程命令' }; return; }
  deploySubmitting.value = true;
  msg.value = { kind: 'info', text: '部署提交中…' };
  try {
    const created = (await endpoints.provisioningDeploy({
      target_id: targetId,
      files,
      run_cmd: runCmd || undefined,
    })) as DeployTask;
    const id = String(created.id ?? '');
    if (!id) throw new Error('后端未返回任务 id');
    msg.value = { kind: 'ok', text: '部署任务已提交，正在执行' };
    startDeployProgress(id);
  } catch (e) {
    msg.value = { kind: 'err', text: '部署失败：' + friendlyError(e) };
  } finally {
    deploySubmitting.value = false;
  }
}

// —— 部署真实进度：对话框内轮询任务详情（文件级 ✓/✗ + 命令输出）——
const deployProgressId = ref<string>('');
const deployProgress = ref<DeployTask | null>(null);
let deployProgressTimer: number | undefined;

function isTerminalDeploy(s?: string): boolean {
  return s === 'completed' || s === 'failed';
}

function startDeployProgress(id: string): void {
  deployProgressId.value = id;
  deployProgress.value = null;
  stopDeployProgressTimer();
  void refreshDeployProgress();
  deployProgressTimer = window.setInterval(() => { void refreshDeployProgress(); }, 1200);
}

async function refreshDeployProgress(): Promise<void> {
  const id = deployProgressId.value;
  if (!id) return;
  try {
    deployProgress.value = (await endpoints.getProvisioningDeploy(id)) as DeployTask;
    if (isTerminalDeploy(deployProgress.value?.status)) {
      stopDeployProgressTimer();
      void loadDeployTasks();
    }
  } catch (e) {
    stopDeployProgressTimer();
    msg.value = { kind: 'err', text: '部署进度查询失败：' + friendlyError(e) };
  }
}

function stopDeployProgressTimer(): void {
  if (deployProgressTimer !== undefined) {
    window.clearInterval(deployProgressTimer);
    deployProgressTimer = undefined;
  }
}

/** 关闭进度视图（不取消后台执行；历史列表继续自动刷新到终态）。 */
function closeDeployProgress(): void {
  stopDeployProgressTimer();
  deployProgressId.value = '';
  deployProgress.value = null;
  showDeploy.value = false;
}

/** 进度终态后一键回到表单（预填上次内容）。 */
function resetDeployForm(): void {
  closeDeployProgress();
  openDeploy();
}

function deployStatusClass(s?: string): string {
  switch (s) {
    case 'completed': return 'pill-ok';
    case 'running':
    case 'transferring': return 'pill-warn';
    case 'failed': return 'pill-err';
    case 'pending':
    default: return 'pill-muted';
  }
}
function deployStatusLabel(s?: string): string {
  switch (s) {
    case 'completed': return '已完成';
    case 'running': return '执行命令';
    case 'transferring': return '传输中';
    case 'failed': return '失败';
    case 'pending': return '排队';
    default: return s ?? '—';
  }
}
function fileResultIcon(s?: string): string {
  switch (s) {
    case 'success': return '✓';
    case 'failed': return '✗';
    case 'skipped': return '–';
    default: return '⏳';
  }
}
function fileResultClass(s?: string): string {
  switch (s) {
    case 'success': return 'fr-ok';
    case 'failed': return 'fr-err';
    case 'skipped': return 'fr-skip';
    default: return 'fr-pending';
  }
}

// —— 部署历史列表（GET /ssh/deploys，admin；有进行中任务时自动刷新）——
const deployTasks = ref<DeployTask[]>([]);
const deployTasksLoading = ref(false);
const deployTasksError = ref('');
const deployDetailId = ref<string>('');

async function loadDeployTasks(): Promise<void> {
  deployTasksLoading.value = true;
  deployTasksError.value = '';
  try {
    const raw = await endpoints.provisioningDeployTasks();
    deployTasks.value = Array.isArray(raw) ? (raw as DeployTask[]) : [];
  } catch (e) {
    deployTasks.value = [];
    deployTasksError.value = friendlyError(e);
  } finally {
    deployTasksLoading.value = false;
    syncDeployListPolling();
  }
}

let deployListTimer: number | undefined;
function syncDeployListPolling(): void {
  const active = deployTasks.value.some((t) => !isTerminalDeploy(t.status));
  if (active && deployListTimer === undefined) {
    deployListTimer = window.setInterval(() => { void loadDeployTasks(); }, 2000);
  } else if (!active && deployListTimer !== undefined) {
    window.clearInterval(deployListTimer);
    deployListTimer = undefined;
  }
}

function toggleDeployDetail(row: DeployTask): void {
  deployDetailId.value = deployDetailId.value === row.id ? '' : String(row.id ?? '');
}

function deployTargetName(targetId?: string): string {
  const t = sshTargets.value.find((x) => x.id === targetId);
  return t ? `${t.name ?? t.id}（${t.host}）` : (targetId ?? '—');
}

const deployColumns: Column<DeployTask>[] = [
  { key: 'id', title: '任务', width: '100px', accessor: (r) => r.id ?? '—' },
  { key: 'target_id', title: '目标', accessor: (r) => deployTargetName(r.target_id) },
  { key: 'status', title: '状态', width: '110px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'files', title: '文件', width: '60px', align: 'center', accessor: (r) => String(r.files?.length ?? 0) },
  { key: 'run_cmd', title: '远程命令', accessor: (r) => r.run_cmd ?? '—' },
  { key: 'created_at', title: '发起时间', width: '160px', accessor: (r) => r.created_at ?? '—' },
  { key: 'actions', title: '操作', width: '90px', align: 'right' },
];

// =============================================================================
// Tab4：电源控制（PXE 装机流水线第一环：先唤醒/上电，再 PXE 引导）
// =============================================================================

// —— 本机 BMC（in-band）——
const bmcInfo = ref<BmcInfo | null>(null);
const bmcLoading = ref(false);
const bmcError = ref('');
const bmcBusy = ref(false);
const showSensors = ref(false);
const sensorsInfo = ref<SensorsInfo | null>(null);
const sensorsLoading = ref(false);

const bmcPowerOn = computed(() => bmcInfo.value?.system_power === 'on');
const bmcFirmware = computed(
  () => pickStr(kvToRecord(bmcInfo.value?.mc), 'Firmware Revision', 'BMC Firmware Revision') || '—',
);
const bmcSelEntries = computed(
  () => pickStr(kvToRecord(bmcInfo.value?.sel), 'Entries') || '—',
);

function kvToRecord(kvs?: KvLine[]): Record<string, unknown> {
  const o: Record<string, unknown> = {};
  for (const kv of kvs ?? []) {
    if (kv?.key) o[kv.key] = kv.value ?? '';
  }
  return o;
}

async function loadBmc(): Promise<void> {
  bmcLoading.value = true;
  bmcError.value = '';
  try {
    bmcInfo.value = (await endpoints.powerBmc()) as BmcInfo;
  } catch (e) {
    bmcInfo.value = null;
    bmcError.value = friendlyError(e);
  } finally {
    bmcLoading.value = false;
  }
}

async function bmcPower(action: string): Promise<void> {
  const labels: Record<string, string> = {
    on: '上电', off: '断电', cycle: '电源重启', soft: '软关机',
  };
  if (action === 'off' || action === 'cycle' || action === 'soft') {
    if (!window.confirm(`确定对本机执行「${labels[action] ?? action}」？`)) return;
  }
  bmcBusy.value = true;
  msg.value = null;
  try {
    const r = (await endpoints.powerBmcPower(action)) as PowerActionResult;
    if (r.ok) {
      msg.value = { kind: 'ok', text: `本机电源 ${action} 已执行：${r.output || 'OK'}` };
    } else {
      msg.value = { kind: 'err', text: `电源 ${action} 失败：${r.error ?? '未知错误'}` };
    }
    await loadBmc();
  } catch (e) {
    msg.value = { kind: 'err', text: `电源 ${action} 失败：` + friendlyError(e) };
  } finally {
    bmcBusy.value = false;
  }
}

async function toggleSensors(): Promise<void> {
  showSensors.value = !showSensors.value;
  if (showSensors.value && !sensorsInfo.value) {
    sensorsLoading.value = true;
    try {
      sensorsInfo.value = (await endpoints.powerBmcSensors()) as SensorsInfo;
    } catch (e) {
      msg.value = { kind: 'err', text: '传感器读取失败：' + friendlyError(e) };
    } finally {
      sensorsLoading.value = false;
    }
  }
}

// —— 远程 IPMI 2.0 设备（lanplus RMCP+）——
const ipmiDevices = ref<IpmiDevice[]>([]);
const ipmiLoading = ref(false);
const ipmiError = ref('');
const ipmiBusyId = ref<string>('');

async function loadIpmiDevices(): Promise<void> {
  ipmiLoading.value = true;
  ipmiError.value = '';
  try {
    const raw = await endpoints.powerIpmiDevices();
    ipmiDevices.value = Array.isArray(raw) ? (raw as IpmiDevice[]) : [];
  } catch (e) {
    ipmiDevices.value = [];
    ipmiError.value = friendlyError(e);
  } finally {
    ipmiLoading.value = false;
  }
}

const showIpmiCreate = ref(false);
const ipmiSubmitting = ref(false);
const ipmiForm = ref({ name: '', host: '', port: '623', username: 'admin', password: '', cipher: '' });

function openIpmiCreate(prefillHost = ''): void {
  ipmiForm.value = { name: prefillHost || '', host: prefillHost, port: '623', username: 'admin', password: '', cipher: '' };
  msg.value = null;
  showIpmiCreate.value = true;
}
function closeIpmiCreate(): void {
  if (ipmiSubmitting.value) return;
  showIpmiCreate.value = false;
}
async function submitIpmiCreate(): Promise<void> {
  const name = ipmiForm.value.name.trim();
  const host = ipmiForm.value.host.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写设备名称' }; return; }
  if (!host) { msg.value = { kind: 'err', text: '请填写 BMC 主机地址' }; return; }
  const port = Number(ipmiForm.value.port.trim()) || 623;
  ipmiSubmitting.value = true;
  msg.value = { kind: 'info', text: '注册中…' };
  try {
    await endpoints.addPowerIpmiDevice({
      name,
      host,
      port,
      username: ipmiForm.value.username.trim() || undefined,
      password: ipmiForm.value.password || undefined,
      cipher: ipmiForm.value.cipher.trim() || undefined,
    });
    showIpmiCreate.value = false;
    await loadIpmiDevices();
    msg.value = { kind: 'ok', text: `设备「${name}」已注册` };
  } catch (e) {
    msg.value = { kind: 'err', text: '注册失败：' + friendlyError(e) };
  } finally {
    ipmiSubmitting.value = false;
  }
}

async function deleteIpmiDevice(row: IpmiDevice): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除 IPMI 设备「${row.name ?? id}」？`)) return;
  ipmiBusyId.value = id;
  try {
    await endpoints.deletePowerIpmiDevice(id);
    await loadIpmiDevices();
    msg.value = { kind: 'ok', text: '设备已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    ipmiBusyId.value = '';
  }
}

async function testIpmiDevice(row: IpmiDevice): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  ipmiBusyId.value = id;
  msg.value = { kind: 'info', text: `正在测试 ${row.host ?? id}（lanplus，10s 超时）…` };
  try {
    const r = (await endpoints.testPowerIpmiDevice(id)) as DeviceTestResult;
    msg.value = r.reachable
      ? { kind: 'ok', text: `${row.name ?? id} 可达（电源：${r.system_power ?? '未知'}，${r.duration_ms ?? 0}ms）` }
      : { kind: 'err', text: `${row.name ?? id} 不可达：${r.output ?? ''}` };
    await loadIpmiDevices();
  } catch (e) {
    msg.value = { kind: 'err', text: '测试失败：' + friendlyError(e) };
  } finally {
    ipmiBusyId.value = '';
  }
}

async function ipmiDevicePower(row: IpmiDevice, action: string): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (action !== 'on' && !window.confirm(`确定对 ${row.name ?? id}（${row.host ?? ''}）执行电源 ${action}？`)) return;
  ipmiBusyId.value = id;
  msg.value = null;
  try {
    const r = (await endpoints.powerIpmiDevicePower(id, action)) as PowerActionResult;
    msg.value = r.ok
      ? { kind: 'ok', text: `${row.name ?? id} 电源 ${action} 已执行：${r.output || 'OK'}` }
      : { kind: 'err', text: `${row.name ?? id} 电源 ${action} 失败：${r.error ?? '未知错误'}` };
    await loadIpmiDevices();
  } catch (e) {
    msg.value = { kind: 'err', text: `电源 ${action} 失败：` + friendlyError(e) };
  } finally {
    ipmiBusyId.value = '';
  }
}

async function ipmiDeviceStatus(row: IpmiDevice): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  ipmiBusyId.value = id;
  msg.value = null;
  try {
    const raw = (await endpoints.powerIpmiDeviceStatus(id)) as {
      reachable?: boolean;
      system_power?: string | null;
      error?: string | null;
    };
    msg.value = raw.reachable
      ? { kind: 'ok', text: `${row.name ?? id} 状态：电源 ${raw.system_power ?? '未知'}` }
      : { kind: 'err', text: `${row.name ?? id} 状态不可达：${raw.error ?? ''}` };
    await loadIpmiDevices();
  } catch (e) {
    msg.value = { kind: 'err', text: '状态查询失败：' + friendlyError(e) };
  } finally {
    ipmiBusyId.value = '';
  }
}

function ipmiStatusClass(s?: string): string {
  switch (s) {
    case 'reachable': return 'pill-ok';
    case 'unreachable': return 'pill-err';
    default: return 'pill-muted';
  }
}
function ipmiStatusLabel(s?: string): string {
  switch (s) {
    case 'reachable': return '可达';
    case 'unreachable': return '不可达';
    default: return '未知';
  }
}

const ipmiColumns: Column<IpmiDevice>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'host', title: 'BMC 地址', accessor: (r) => `${r.host ?? '—'}:${r.port ?? 623}` },
  { key: 'username', title: '用户', width: '90px', accessor: (r) => r.username ?? '—' },
  { key: 'has_password', title: '凭据', width: '70px', align: 'center', accessor: (r) => (r.has_password ? '已存' : '无') },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'actions', title: '操作', width: '300px', align: 'right' },
];

// —— 网段扫描（RMCP Presence Ping 免凭据发现）——
const scanForm = ref({ cidr: '192.0.2.0/24', port: '623', timeout_ms: '500', concurrency: '64' });
const scanStarting = ref(false);
const scanTask = ref<ScanTask | null>(null);
const scanError = ref('');
let scanTimer: number | undefined;

async function startScan(): Promise<void> {
  const cidr = scanForm.value.cidr.trim();
  if (!/^\d{1,3}(\.\d{1,3}){3}\/(2[4-9]|3[0-2])$/.test(cidr)) {
    msg.value = { kind: 'err', text: 'CIDR 需形如 192.0.2.0/24（仅允许 /24 ~ /32）' };
    return;
  }
  scanStarting.value = true;
  scanError.value = '';
  msg.value = { kind: 'info', text: `扫描 ${cidr} 已发起（RMCP Presence Ping，免凭据）` };
  try {
    const raw = (await endpoints.startPowerIpmiScan({
      cidr,
      port: Number(scanForm.value.port) || 623,
      timeout_ms: Number(scanForm.value.timeout_ms) || 500,
      concurrency: Number(scanForm.value.concurrency) || 64,
    })) as ScanTask;
    scanTask.value = raw;
    startScanPolling(String(raw.id ?? ''));
  } catch (e) {
    scanError.value = friendlyError(e);
    msg.value = { kind: 'err', text: '扫描发起失败：' + friendlyError(e) };
  } finally {
    scanStarting.value = false;
  }
}

function startScanPolling(id: string): void {
  stopScanPolling();
  scanTimer = window.setInterval(() => { void refreshScan(id); }, 1000);
  void refreshScan(id);
}

async function refreshScan(id: string): Promise<void> {
  try {
    const raw = (await endpoints.getPowerIpmiScan(id)) as ScanTask;
    scanTask.value = raw;
    if (raw.status !== 'running') {
      stopScanPolling();
      if (raw.status === 'completed') {
        msg.value = {
          kind: 'ok',
          text: `扫描完成：${raw.found?.length ?? 0} 台 IPMI 设备（${raw.cidr ?? ''}）`,
        };
      } else {
        msg.value = { kind: 'err', text: `扫描失败：${raw.error ?? '未知原因'}` };
      }
    }
  } catch (e) {
    stopScanPolling();
    scanError.value = friendlyError(e);
  }
}

function stopScanPolling(): void {
  if (scanTimer !== undefined) {
    window.clearInterval(scanTimer);
    scanTimer = undefined;
  }
}

const scanPercent = computed(() => {
  const t = scanTask.value;
  if (!t || !t.total) return 0;
  return Math.min(100, Math.round(((t.scanned ?? 0) / t.total) * 100));
});

// —— WoL 魔术唤醒 ——
const wolTargets = ref<WolTarget[]>([]);
const wolLoading = ref(false);
const wolError = ref('');
const wolBusyId = ref<string>('');
const arpNeighbors = ref<ArpEntry[]>([]);
const arpInfo = ref<ArpInfo | null>(null);

async function loadWolTargets(): Promise<void> {
  wolLoading.value = true;
  wolError.value = '';
  try {
    const raw = await endpoints.powerWolTargets();
    wolTargets.value = Array.isArray(raw) ? (raw as WolTarget[]) : [];
  } catch (e) {
    wolTargets.value = [];
    wolError.value = friendlyError(e);
  } finally {
    wolLoading.value = false;
  }
}

async function loadArp(): Promise<void> {
  try {
    arpInfo.value = (await endpoints.powerWolArp()) as ArpInfo;
    arpNeighbors.value = arpInfo.value?.neighbors ?? [];
  } catch {
    arpNeighbors.value = [];
  }
}

const showWolCreate = ref(false);
const wolSubmitting = ref(false);
const wolForm = ref({ name: '', mac: '', broadcast: '255.255.255.255', port: '9', secureon_password: '' });

function openWolCreate(): void {
  wolForm.value = { name: '', mac: '', broadcast: '255.255.255.255', port: '9', secureon_password: '' };
  msg.value = null;
  void loadArp();
  showWolCreate.value = true;
}
function closeWolCreate(): void {
  if (wolSubmitting.value) return;
  showWolCreate.value = false;
}
function fillMacFromArp(mac: string): void {
  wolForm.value.mac = mac;
}
async function submitWolCreate(): Promise<void> {
  const name = wolForm.value.name.trim();
  const mac = wolForm.value.mac.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写目标名称' }; return; }
  if (!/^([0-9a-fA-F]{2}[:-]){5}[0-9a-fA-F]{2}$|^[0-9a-fA-F]{12}$/.test(mac)) {
    msg.value = { kind: 'err', text: 'MAC 非法：需形如 aa:bb:cc:dd:ee:ff（或从 ARP 邻居选择）' };
    return;
  }
  wolSubmitting.value = true;
  msg.value = { kind: 'info', text: '注册中…' };
  try {
    await endpoints.addPowerWolTarget({
      name,
      mac,
      broadcast: wolForm.value.broadcast.trim() || undefined,
      port: Number(wolForm.value.port) || 9,
      secureon_password: wolForm.value.secureon_password.trim() || undefined,
    });
    showWolCreate.value = false;
    await loadWolTargets();
    msg.value = { kind: 'ok', text: `WoL 目标「${name}」已注册` };
  } catch (e) {
    msg.value = { kind: 'err', text: '注册失败：' + friendlyError(e) };
  } finally {
    wolSubmitting.value = false;
  }
}

async function deleteWolTarget(row: WolTarget): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!window.confirm(`确定删除 WoL 目标「${row.name ?? id}」？`)) return;
  wolBusyId.value = id;
  try {
    await endpoints.deletePowerWolTarget(id);
    await loadWolTargets();
    msg.value = { kind: 'ok', text: 'WoL 目标已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    wolBusyId.value = '';
  }
}

async function wakeTarget(row: WolTarget): Promise<void> {
  const id = String(row.id ?? '');
  const name = String(row.name ?? '');
  if (!name) return;
  wolBusyId.value = id;
  msg.value = { kind: 'info', text: `正在唤醒「${name}」（魔术包广播 ×3）…` };
  try {
    const r = (await endpoints.wakePowerWol({ name })) as WakeResult;
    msg.value = r.ok
      ? {
          kind: 'ok',
          text: `魔术包已发送 ${r.sent ?? 0}/${r.attempts ?? 3} 次（${r.bytes_per_packet ?? 102} 字节/包${r.secureon ? '，含 SecureOn' : ''}）——目标约需数秒至一分钟启动`,
        }
      : { kind: 'err', text: `唤醒失败：${r.error ?? '未知错误'}` };
  } catch (e) {
    msg.value = { kind: 'err', text: '唤醒失败：' + friendlyError(e) };
  } finally {
    wolBusyId.value = '';
  }
}

const wolColumns: Column<WolTarget>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'mac', title: 'MAC', accessor: (r) => r.mac ?? '—' },
  { key: 'broadcast', title: '广播地址', accessor: (r) => r.broadcast ?? '—' },
  { key: 'port', title: '端口', width: '70px', align: 'center', accessor: (r) => String(r.port ?? 9) },
  { key: 'has_secureon', title: 'SecureOn', width: '90px', align: 'center', accessor: (r) => (r.has_secureon ? '已配置' : '—') },
  { key: 'actions', title: '操作', width: '170px', align: 'right' },
];

// =============================================================================
// 辅助
// =============================================================================
function pickStr(o: Record<string, unknown>, ...keys: string[]): string {
  for (const k of keys) {
    const v = o[k];
    if (typeof v === 'string' && v.trim()) return v;
    if (typeof v === 'number' && Number.isFinite(v)) return String(v);
  }
  return '';
}

// =============================================================================
// 刷新与初始化
// =============================================================================
const refreshing = computed(
  () => statusLoading.value || configLoading.value || entriesLoading.value
    || isoLoading.value || sshLoading.value || deployTasksLoading.value
    || bmcLoading.value || ipmiLoading.value || wolLoading.value,
);
async function refreshAll(): Promise<void> {
  await Promise.all([
    loadStatus(),
    loadConfig(),
    loadEntries(),
    loadIsoTasks(),
    loadSshTargets(),
    loadDeployTasks(),
    // 电源控制域（BMC 探测可能各花 10s，且仅本 Tab 使用——按需刷新不阻塞首屏）
    loadBmc(),
    loadIpmiDevices(),
    loadWolTargets(),
  ]);
}

onMounted(() => {
  void refreshAll();
});

onUnmounted(() => {
  // 轮询定时器统一清理（页面离开即停，避免后台空转与内存泄漏）
  if (isoTimer !== undefined) window.clearInterval(isoTimer);
  stopDeployProgressTimer();
  if (deployListTimer !== undefined) window.clearInterval(deployListTimer);
  stopScanPolling();
});
</script>

<template>
  <div class="provisioning-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">系统自举</h2>
        <div class="page-sub muted">PXE 网络引导 · ISO 镜像生成 · SSH 远程部署</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
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

    <!-- =================== Tab1 PXE 网络引导 =================== -->
    <section v-show="activeTab === 'pxe'" class="tab-panel">
      <!-- 服务状态卡 -->
      <section class="card status-card">
        <div class="status-left">
          <div
            class="status-dot"
            :class="{ running: isRunning, stopped: !isRunning, pending: statusLoading }"
            aria-hidden="true"
          ></div>
          <div class="status-meta">
            <div class="status-line">
              <span class="status-label">PXE 服务状态</span>
              <span class="pill" :class="isRunning ? 'pill-ok' : 'pill-muted'">
                {{ statusLoading ? '查询中…' : isRunning ? '运行中' : '已停止' }}
              </span>
              <span v-if="pxeStatus && pxeStatus.state" class="muted small mono">
                state: {{ pxeStatus.state }}
              </span>
            </div>
            <div v-if="statusError" class="error-text">{{ statusError }}</div>
          </div>
        </div>
        <div class="status-actions">
          <button
            v-if="!isRunning"
            class="btn btn-primary"
            :disabled="toggling || statusLoading"
            @click="startService"
          >{{ toggling ? '处理中…' : '启动服务' }}</button>
          <button
            v-else
            class="btn btn-danger"
            :disabled="toggling || statusLoading"
            @click="stopService"
          >{{ toggling ? '处理中…' : '停止服务' }}</button>
        </div>
      </section>

      <!-- 配置表单 -->
      <section class="card config-card">
        <div class="panel-head">
          <h3>PXE 配置</h3>
          <span v-if="configLoading" class="muted small">加载中…</span>
        </div>
        <div v-if="configError" class="error-box">配置加载失败：{{ configError }}</div>
        <form class="config-form" @submit.prevent="saveConfig">
          <label class="switch">
            <input v-model="configForm.enabled" type="checkbox" :disabled="configSaving" />
            <span>启用 PXE 服务</span>
          </label>
          <div class="form-grid">
            <div class="field">
              <label for="pxe-tftp">TFTP 服务器</label>
              <input id="pxe-tftp" v-model="configForm.tftp_server" type="text" placeholder="例如 192.168.1.10" :disabled="configSaving" />
            </div>
            <div class="field">
              <label for="pxe-mode">引导模式</label>
              <select id="pxe-mode" v-model="configForm.boot_mode" :disabled="configSaving">
                <option v-for="m in BOOT_MODES" :key="m.value" :value="m.value">{{ m.label }}</option>
              </select>
            </div>
            <div class="field field-wide">
              <label for="pxe-repo">HTTP 镜像仓库</label>
              <input id="pxe-repo" v-model="configForm.http_repo" type="text" placeholder="例如 http://repo.example.com/pxe" :disabled="configSaving" />
            </div>
            <div class="field field-wide">
              <label for="pxe-bootfile">默认启动文件</label>
              <input id="pxe-bootfile" v-model="configForm.default_bootfile" type="text" placeholder="例如 pxelinux.0 / grub.efi" :disabled="configSaving" />
            </div>
          </div>
          <div class="form-actions">
            <button type="submit" class="btn btn-primary" :disabled="configSaving || configLoading">
              {{ configSaving ? '保存中…' : '保存配置' }}
            </button>
          </div>
        </form>
      </section>

      <!-- 启动条目 -->
      <div class="panel-head">
        <span class="panel-title">启动条目</span>
        <button class="btn btn-small btn-primary" @click="openEntryCreate">＋ 添加启动条目</button>
      </div>
      <div v-if="entriesError" class="error-box">{{ entriesError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="entryColumns"
            :rows="entries"
            :loading="entriesLoading"
            empty-text="暂无启动条目，点击右上角「添加启动条目」。"
          >
            <template #cell-default="{ row }">
              <span class="pill" :class="row.default ? 'pill-ok' : 'pill-muted'">
                {{ row.default ? '默认' : '—' }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                :disabled="entryBusyId === (row.id ?? row.name ?? '')"
                @click.stop="deleteEntry(row)"
              >{{ entryBusyId === (row.id ?? row.name ?? '') ? '删除中…' : '删除' }}</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 添加启动条目对话框 -->
      <div v-if="showEntryCreate" class="modal-backdrop" @click.self="closeEntryCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="entry-create-title">
          <div class="modal-head">
            <h3 id="entry-create-title">添加启动条目</h3>
            <button class="modal-close" type="button" :disabled="entrySubmitting" @click="closeEntryCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitEntryCreate">
            <div class="field">
              <label for="entry-name">名称</label>
              <input id="entry-name" v-model="entryForm.name" type="text" placeholder="例如 ubuntu-26.04" :disabled="entrySubmitting" />
            </div>
            <div class="field">
              <label for="entry-kernel">内核路径</label>
              <input id="entry-kernel" v-model="entryForm.kernel" type="text" placeholder="例如 ubuntu/vmlinuz" :disabled="entrySubmitting" />
            </div>
            <div class="field">
              <label for="entry-initrd">initrd 路径（可选）</label>
              <input id="entry-initrd" v-model="entryForm.initrd" type="text" placeholder="例如 ubuntu/initrd.img" :disabled="entrySubmitting" />
            </div>
            <div class="field">
              <label for="entry-cmdline">内核参数（可选）</label>
              <input id="entry-cmdline" v-model="entryForm.cmdline" type="text" placeholder="例如 root=/dev/nfs rw ip=dhcp" :disabled="entrySubmitting" />
            </div>
            <label class="switch">
              <input v-model="entryForm.default" type="checkbox" :disabled="entrySubmitting" />
              <span>设为默认启动条目</span>
            </label>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="entrySubmitting" @click="closeEntryCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="entrySubmitting">
                {{ entrySubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 ISO 镜像生成 =================== -->
    <section v-show="activeTab === 'iso'" class="tab-panel">
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">任务总数</div>
          <div class="stat-value">{{ isoStats.total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已完成</div>
          <div class="stat-value">{{ isoStats.completed }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">失败</div>
          <div class="stat-value">{{ isoStats.failed }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">ISO 构建任务</span>
        <button class="btn btn-small btn-primary" @click="openIsoCreate">＋ 创建任务</button>
      </div>

      <div v-if="isoError" class="error-box">{{ isoError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="isoColumns"
            :rows="isoTasks"
            :loading="isoLoading"
            empty-text="暂无 ISO 构建任务，点击右上角「创建任务」。"
          >
            <template #cell-variant="{ row }">
              <span class="pill" :class="isoVariantClass(row.variant)">{{ isoVariantLabel(row.variant) }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="isoStatusClass(row.status)">{{ isoStatusLabel(row.status) }}</span>
              <div v-if="row.status === 'building'" class="building-meta">
                <div class="progress-track" aria-hidden="true">
                  <div class="progress-fill" :style="{ width: isoProgressPct(row) + '%' }"></div>
                </div>
                <span class="muted small mono">{{ row.step ?? '…' }} · {{ isoProgressPct(row) }}%</span>
              </div>
            </template>
            <template #cell-iso_path="{ row }">
              <span v-if="row.status === 'completed' && row.iso_path" class="mono small cell-path" :title="row.iso_path">
                {{ row.iso_path }}
              </span>
              <span v-else class="muted">—</span>
            </template>
            <template #cell-sha256="{ row }">
              <span class="mono small" :title="row.sha256 ?? ''">{{ truncate(row.sha256, 12) }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status === 'pending' || row.status === 'failed'"
                class="btn btn-small btn-primary"
                :disabled="isoBuildBusyId === row.id"
                @click.stop="startIsoBuild(row)"
              >{{ isoBuildBusyId === row.id ? '启动中…' : '开始构建' }}</button>
              <button
                class="btn btn-small"
                :disabled="!(row.build_log && row.build_log.length)"
                :title="row.build_log && row.build_log.length ? '查看构建日志' : '暂无构建日志'"
                @click.stop="toggleIsoDetail(row)"
              >日志</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="isoBusyId === row.id || row.status === 'building'"
                @click.stop="deleteIsoTask(row)"
              >{{ isoBusyId === row.id ? '删除中…' : '删除' }}</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 单任务构建日志（折叠面板：命令行 + 退出码 + 输出摘要） -->
      <section v-if="isoDetailId" class="card log-card">
        <div class="panel-head">
          <h3>构建日志 · {{ isoTasks.find((t) => t.id === isoDetailId)?.name ?? isoDetailId }}</h3>
          <button class="btn btn-small" @click="isoDetailId = ''">收起</button>
        </div>
        <pre class="log-pre mono small">{{ (isoTasks.find((t) => t.id === isoDetailId)?.build_log ?? []).join('\n') || '（暂无日志）' }}</pre>
        <p v-if="isoTasks.find((t) => t.id === isoDetailId)?.error" class="error-text">
          {{ isoTasks.find((t) => t.id === isoDetailId)?.error }}
        </p>
      </section>

      <!-- 创建 ISO 任务对话框 -->
      <div v-if="showIsoCreate" class="modal-backdrop" @click.self="closeIsoCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="iso-create-title">
          <div class="modal-head">
            <h3 id="iso-create-title">创建 ISO 构建任务</h3>
            <button class="modal-close" type="button" :disabled="isoSubmitting" @click="closeIsoCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitIsoCreate">
            <div class="field">
              <label for="iso-name">名称</label>
              <input id="iso-name" v-model="isoForm.name" type="text" placeholder="例如 os-installer" :disabled="isoSubmitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="iso-version">版本（可选）</label>
                <input id="iso-version" v-model="isoForm.version" type="text" placeholder="0.1.0" :disabled="isoSubmitting" />
              </div>
              <div class="field">
                <label for="iso-arch">架构</label>
                <select id="iso-arch" v-model="isoForm.arch" :disabled="isoSubmitting">
                  <option v-for="a in ISO_ARCHS" :key="a" :value="a">{{ a }}</option>
                </select>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="iso-variant">变体</label>
                <select id="iso-variant" v-model="isoForm.variant" :disabled="isoSubmitting">
                  <option v-for="v in ISO_VARIANTS" :key="v.value" :value="v.value">{{ v.label }}</option>
                </select>
              </div>
              <div class="field">
                <label for="iso-ubuntu">Ubuntu 版本（可选）</label>
                <input id="iso-ubuntu" v-model="isoForm.ubuntu_version" type="text" placeholder="26.04" :disabled="isoSubmitting" />
              </div>
            </div>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="isoSubmitting" @click="closeIsoCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="isoSubmitting">
                {{ isoSubmitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab3 SSH 远程部署 =================== -->
    <section v-show="activeTab === 'ssh'" class="tab-panel">
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">目标总数</div>
          <div class="stat-value">{{ sshStats.total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">可达</div>
          <div class="stat-value">{{ sshStats.reachable }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">SSH 目标</span>
        <div class="head-actions-inline">
          <button class="btn btn-small btn-primary" @click="openDeploy">＋ 发起部署</button>
          <button class="btn btn-small" @click="openSshCreate">＋ 添加目标</button>
        </div>
      </div>

      <div v-if="sshError" class="error-box">{{ sshError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="sshColumns"
            :rows="sshTargets"
            :loading="sshLoading"
            empty-text="暂无 SSH 目标，点击右上角「添加目标」。"
          >
            <template #cell-private_key_path="{ row }">
              <span class="mono small" :title="row.private_key_path ?? ''">{{ truncate(row.private_key_path, 28) }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="sshStatusClass(row.status)">{{ sshStatusLabel(row.status) }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="sshBusyId === row.id"
                @click.stop="testSshTarget(row)"
              >{{ sshBusyId === row.id ? '测试中…' : '测试连接' }}</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="sshBusyId === row.id"
                @click.stop="deleteSshTarget(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 部署任务历史（真实执行记录：文件级结果 + 命令输出，admin 读） -->
      <div class="panel-head">
        <span class="panel-title">部署任务</span>
        <button class="btn btn-small" :disabled="deployTasksLoading" @click="loadDeployTasks">
          <span class="spin" :class="{ spinning: deployTasksLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div v-if="deployTasksError" class="error-box">{{ deployTasksError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="deployColumns"
            :rows="deployTasks"
            :loading="deployTasksLoading"
            empty-text="暂无部署任务，点击右上角「发起部署」。"
          >
            <template #cell-status="{ row }">
              <span class="pill" :class="deployStatusClass(row.status)">{{ deployStatusLabel(row.status) }}</span>
            </template>
            <template #cell-run_cmd="{ row }">
              <span class="mono small cell-path" :title="row.run_cmd ?? ''">{{ row.run_cmd ?? '—' }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                @click.stop="toggleDeployDetail(row)"
              >{{ deployDetailId === row.id ? '收起' : '详情' }}</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 单条部署任务详情（文件级结果 + 命令输出折叠区） -->
      <section v-if="deployDetailId" class="card log-card">
        <div class="panel-head">
          <h3>部署详情 · {{ deployDetailId }}</h3>
          <button class="btn btn-small" @click="deployDetailId = ''">收起</button>
        </div>
        <template v-if="deployTasks.find((t) => t.id === deployDetailId)">
          <div class="file-results">
            <div
              v-for="(r, idx) in deployTasks.find((t) => t.id === deployDetailId)?.results ?? []"
              :key="idx"
              class="file-result-row"
            >
              <span class="fr-icon" :class="fileResultClass(r.status)">{{ fileResultIcon(r.status) }}</span>
              <span class="mono small fr-path" :title="`${r.local_path} → ${r.remote_path}`">
                {{ r.local_path }} → {{ r.remote_path }}
              </span>
              <span v-if="r.duration_ms != null" class="muted small">{{ r.duration_ms }}ms</span>
              <span v-if="r.error" class="error-text small" :title="r.error">{{ r.error }}</span>
            </div>
          </div>
          <p v-if="deployTasks.find((t) => t.id === deployDetailId)?.error" class="error-text">
            {{ deployTasks.find((t) => t.id === deployDetailId)?.error }}
          </p>
          <details
            v-if="deployTasks.find((t) => t.id === deployDetailId)?.cmd_output"
            class="cmd-details"
          >
            <summary class="small muted">
              远程命令输出 · exit {{ deployTasks.find((t) => t.id === deployDetailId)?.cmd_output?.exit_code }}
            </summary>
            <pre class="log-pre mono small">stdout:
{{ deployTasks.find((t) => t.id === deployDetailId)?.cmd_output?.stdout || '（空）' }}

stderr:
{{ deployTasks.find((t) => t.id === deployDetailId)?.cmd_output?.stderr || '（空）' }}</pre>
          </details>
        </template>
      </section>

      <!-- 添加 SSH 目标对话框（仅私钥认证，无密码字段） -->
      <div v-if="showSshCreate" class="modal-backdrop" @click.self="closeSshCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="ssh-create-title">
          <div class="modal-head">
            <h3 id="ssh-create-title">添加 SSH 目标</h3>
            <button class="modal-close" type="button" :disabled="sshSubmitting" @click="closeSshCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitSshCreate">
            <div class="field">
              <label for="ssh-name">名称</label>
              <input id="ssh-name" v-model="sshForm.name" type="text" placeholder="例如 node-02" :disabled="sshSubmitting" />
            </div>
            <div class="field-row">
              <div class="field field-grow-2">
                <label for="ssh-host">主机地址</label>
                <input id="ssh-host" v-model="sshForm.host" type="text" placeholder="例如 192.168.1.20" :disabled="sshSubmitting" />
              </div>
              <div class="field">
                <label for="ssh-port">端口</label>
                <input id="ssh-port" v-model="sshForm.port" type="text" placeholder="22" :disabled="sshSubmitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="ssh-user">用户名</label>
                <input id="ssh-user" v-model="sshForm.user" type="text" placeholder="root" :disabled="sshSubmitting" />
              </div>
              <div class="field">
                <label for="ssh-key">私钥路径（可选）</label>
                <input id="ssh-key" v-model="sshForm.private_key_path" type="text" placeholder="~/.ssh/id_ed25519" :disabled="sshSubmitting" />
              </div>
            </div>
            <p class="hint">仅支持私钥认证，不收集密码。</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="sshSubmitting" @click="closeSshCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="sshSubmitting">
                {{ sshSubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 部署对话框：表单模式 / 进度模式（提交后不关闭，轮询文件级结果） -->
      <div v-if="showDeploy" class="modal-backdrop" @click.self="deploySubmitting ? null : closeDeployProgress()">
        <div class="modal modal-wide" role="dialog" aria-modal="true" aria-labelledby="deploy-title">
          <div class="modal-head">
            <h3 id="deploy-title">{{ deployProgressId ? '部署执行中' : '发起远程部署' }}</h3>
            <button class="modal-close" type="button" :disabled="deploySubmitting" @click="closeDeployProgress">×</button>
          </div>

          <!-- —— 进度模式：文件级 ✓/✗ + 命令输出 —— -->
          <div v-if="deployProgressId" class="modal-body">
            <div class="progress-summary">
              <span class="pill" :class="deployStatusClass(deployProgress?.status)">
                {{ deployStatusLabel(deployProgress?.status) }}
              </span>
              <span class="muted small mono">任务 {{ deployProgressId }}</span>
              <span v-if="deployProgress?.error" class="error-text">{{ deployProgress.error }}</span>
            </div>

            <div v-if="!deployProgress" class="muted small">正在获取任务状态…</div>
            <template v-else>
              <div v-if="(deployProgress.results ?? []).length" class="file-results">
                <div
                  v-for="(r, idx) in deployProgress.results ?? []"
                  :key="idx"
                  class="file-result-row"
                >
                  <span class="fr-icon" :class="fileResultClass(r.status)">{{ fileResultIcon(r.status) }}</span>
                  <span class="mono small fr-path" :title="`${r.local_path} → ${r.remote_path}`">
                    {{ r.local_path }} → {{ r.remote_path }}
                  </span>
                  <span v-if="r.duration_ms != null" class="muted small">{{ r.duration_ms }}ms</span>
                  <span v-if="r.error" class="error-text small" :title="r.error">{{ r.error }}</span>
                </div>
              </div>

              <div v-if="deployProgress.run_cmd" class="cmd-card">
                <div class="cmd-head">
                  <span class="mono small">$ {{ deployProgress.run_cmd }}</span>
                  <span
                    v-if="deployProgress.cmd_output"
                    class="pill"
                    :class="deployProgress.cmd_output.exit_code === 0 ? 'pill-ok' : 'pill-err'"
                  >exit {{ deployProgress.cmd_output.exit_code }}</span>
                  <span v-else class="pill pill-muted">待执行</span>
                </div>
                <details v-if="deployProgress.cmd_output" class="cmd-details">
                  <summary class="small muted">stdout / stderr（各截 8KB）</summary>
                  <pre class="log-pre mono small">stdout:
{{ deployProgress.cmd_output.stdout || '（空）' }}

stderr:
{{ deployProgress.cmd_output.stderr || '（空）' }}</pre>
                </details>
              </div>

              <div class="form-actions">
                <button
                  v-if="isTerminalDeploy(deployProgress.status)"
                  type="button"
                  class="btn"
                  @click="resetDeployForm"
                >再次部署</button>
                <button type="button" class="btn btn-primary" @click="closeDeployProgress">关闭</button>
              </div>
            </template>
          </div>

          <!-- —— 表单模式（原表单） —— -->
          <form v-else class="modal-body" @submit.prevent="submitDeploy">
            <div class="field">
              <label for="deploy-target">目标</label>
              <select id="deploy-target" v-model="deployForm.target_id" :disabled="deploySubmitting">
                <option value="">（请选择）</option>
                <option v-for="t in sshTargets" :key="t.id" :value="t.id">
                  {{ t.name ?? t.id }} ({{ t.host }}:{{ t.port ?? 22 }})
                </option>
              </select>
            </div>

            <div class="field">
              <label>文件传输（local → remote）</label>
              <div class="file-list">
                <div
                  v-for="(f, idx) in deployForm.files"
                  :key="idx"
                  class="file-row"
                >
                  <input
                    v-model="f.local_path"
                    type="text"
                    placeholder="本地路径 /tank/cfg/hosts"
                    :disabled="deploySubmitting"
                  />
                  <span class="arrow mono">→</span>
                  <input
                    v-model="f.remote_path"
                    type="text"
                    placeholder="远端路径 /etc/hosts"
                    :disabled="deploySubmitting"
                  />
                  <button
                    type="button"
                    class="btn btn-small btn-danger file-del"
                    :disabled="deploySubmitting || deployForm.files.length <= 1"
                    @click="removeDeployFile(idx)"
                  >×</button>
                </div>
              </div>
              <button type="button" class="btn btn-small" :disabled="deploySubmitting" @click="addDeployFile">＋ 添加一行</button>
            </div>

            <div class="field">
              <label for="deploy-cmd">远程命令（可选）</label>
              <textarea
                id="deploy-cmd"
                v-model="deployForm.run_cmd"
                rows="3"
                placeholder="例如 systemctl restart nginx"
                :disabled="deploySubmitting"
              ></textarea>
            </div>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="deploySubmitting" @click="closeDeploy">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="deploySubmitting">
                {{ deploySubmitting ? '部署中…' : '部署' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab4 电源控制（PXE 流水线第一环）=================== -->
    <section v-show="activeTab === 'power'" class="tab-panel">
      <!-- —— 本机 BMC（in-band）—— -->
      <section class="card status-card">
        <div class="status-left">
          <div
            class="status-dot"
            :class="{ running: bmcPowerOn, stopped: !!bmcInfo && !bmcPowerOn, pending: bmcLoading || !bmcInfo }"
            aria-hidden="true"
          ></div>
          <div class="status-meta">
            <div class="status-line">
              <span class="status-label">本机 BMC</span>
              <span v-if="bmcLoading" class="pill pill-muted">查询中…</span>
              <span v-else-if="bmcInfo?.available" class="pill" :class="bmcPowerOn ? 'pill-ok' : 'pill-err'">
                {{ bmcPowerOn ? '电源开' : '电源关' }}
              </span>
              <span v-else class="pill pill-warn">不可用</span>
              <span v-if="bmcInfo?.available" class="muted small mono">
                固件 {{ bmcFirmware }} · SEL {{ bmcSelEntries }} 条
              </span>
            </div>
            <div v-if="bmcError" class="error-text">{{ bmcError }}</div>
            <div v-else-if="bmcInfo && !bmcInfo.available" class="muted small">
              {{ bmcInfo.hint ?? '本机 BMC 不可用' }}
              <span v-if="bmcInfo.error" class="error-text">（{{ bmcInfo.error }}）</span>
            </div>
          </div>
        </div>
        <div class="status-actions power-actions">
          <button class="btn btn-small btn-primary" :disabled="bmcBusy || !bmcInfo?.available" @click="bmcPower('on')">上电</button>
          <button class="btn btn-small" :disabled="bmcBusy || !bmcInfo?.available" @click="bmcPower('soft')">软关机</button>
          <button class="btn btn-small" :disabled="bmcBusy || !bmcInfo?.available" @click="bmcPower('cycle')">电源重启</button>
          <button class="btn btn-small btn-danger" :disabled="bmcBusy || !bmcInfo?.available" @click="bmcPower('off')">断电</button>
        </div>
      
</section>

      <!-- 传感器折叠表 -->
      <section class="card log-card">
        <div class="panel-head">
          <h3>传感器（chassis / SEL / 传感器表）</h3>
          <button class="btn btn-small" @click="toggleSensors">
            {{ showSensors ? '收起' : '展开' }}
          </button>
        </div>
        <div v-if="showSensors">
          <div v-if="sensorsLoading" class="muted small">读取中…</div>
          <div v-else-if="sensorsInfo && !sensorsInfo.available" class="muted small">
            {{ sensorsInfo.hint ?? '传感器不可用' }}
          </div>
          <template v-else-if="sensorsInfo">
            <p class="muted small">共 {{ sensorsInfo.count ?? 0 }} 条{{ sensorsInfo.truncated ? '（超出 200 行已截断）' : '' }}</p>
            <div class="sensor-table mono small">
              <div v-for="(s, idx) in sensorsInfo.rows ?? []" :key="idx" class="sensor-row">
                <span class="sensor-name">{{ s.name ?? '—' }}</span>
                <span class="sensor-type">{{ s.type ?? '' }}</span>
                <span class="sensor-reading">{{ s.reading ?? '—' }}</span>
                <span class="pill" :class="s.status === 'ok' ? 'pill-ok' : s.status ? 'pill-warn' : 'pill-muted'">
                  {{ s.status || '—' }}
                </span>
              </div>
            </div>
          </template>
        </div>
      </section>


      <!-- —— 远程 IPMI 2.0 设备 —— -->
      <div class="panel-head">
        <span class="panel-title">远程 IPMI 设备（RMCP+ / lanplus）</span>
        <button class="btn btn-small btn-primary" @click="openIpmiCreate()">＋ 添加设备</button>
      </div>
      <div v-if="ipmiError" class="error-box">{{ ipmiError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="ipmiColumns"
            :rows="ipmiDevices"
            :loading="ipmiLoading"
            empty-text="暂无远程 IPMI 设备——可先在下方扫描网段，或点击「添加设备」。"
          >
            <template #cell-has_password="{ row }">
              <span class="pill" :class="row.has_password ? 'pill-blue' : 'pill-muted'">
                {{ row.has_password ? '已存' : '无' }}
              </span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="ipmiStatusClass(row.status)">{{ ipmiStatusLabel(row.status) }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="ipmiBusyId === row.id"
                @click.stop="testIpmiDevice(row)"
              >{{ ipmiBusyId === row.id ? '执行中…' : '测试' }}</button>
              <button
                class="btn btn-small"
                :disabled="ipmiBusyId === row.id"
                @click.stop="ipmiDeviceStatus(row)"
              >状态</button>
              <button
                class="btn btn-small"
                :disabled="ipmiBusyId === row.id"
                @click.stop="ipmiDevicePower(row, 'on')"
              >上电</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="ipmiBusyId === row.id"
                @click.stop="ipmiDevicePower(row, 'cycle')"
              >重启</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="ipmiBusyId === row.id"
                @click.stop="deleteIpmiDevice(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- —— 网段扫描（RMCP Presence Ping 免凭据发现）—— -->
      <div class="panel-head">
        <span class="panel-title">网段扫描（RMCP+ 发现，免凭据）</span>
      </div>
      <section class="card scan-card">
        <form class="config-form" @submit.prevent="startScan">
          <div class="form-grid">
            <div class="field">
              <label for="scan-cidr">网段 CIDR（仅 /24 ~ /32）</label>
              <input id="scan-cidr" v-model="scanForm.cidr" type="text" placeholder="192.0.2.0/24" :disabled="scanStarting" />
            </div>
            <div class="field">
              <label for="scan-port">RMCP 端口</label>
              <input id="scan-port" v-model="scanForm.port" type="text" placeholder="623" :disabled="scanStarting" />
            </div>
            <div class="field">
              <label for="scan-timeout">单批超时（ms）</label>
              <input id="scan-timeout" v-model="scanForm.timeout_ms" type="text" placeholder="500" :disabled="scanStarting" />
            </div>
            <div class="field">
              <label for="scan-conc">并发</label>
              <input id="scan-conc" v-model="scanForm.concurrency" type="text" placeholder="64" :disabled="scanStarting" />
            </div>
          </div>
          <div class="form-actions">
            <button type="submit" class="btn btn-primary" :disabled="scanStarting || scanTask?.status === 'running'">
              {{ scanTask?.status === 'running' ? '扫描中…' : scanStarting ? '发起中…' : '开始扫描' }}
            </button>
          </div>
        </form>

        <div v-if="scanError" class="error-text">{{ scanError }}</div>
        <template v-if="scanTask">
          <div class="progress-summary">
            <span
              class="pill"
              :class="scanTask.status === 'completed' ? 'pill-ok' : scanTask.status === 'failed' ? 'pill-err' : 'pill-warn'"
            >
              {{ scanTask.status === 'running' ? `扫描中 ${scanPercent}%` : scanTask.status === 'completed' ? '已完成' : scanTask.status }}
            </span>
            <span class="muted small mono">
              {{ scanTask.cidr }} · {{ scanTask.scanned ?? 0 }}/{{ scanTask.total ?? 0 }} · 命中 {{ scanTask.found?.length ?? 0 }}
            </span>
          </div>
          <div v-if="scanTask.status === 'running'" class="progress-track scan-track" aria-hidden="true">
            <div class="progress-fill" :style="{ width: scanPercent + '%' }"></div>
          </div>

          <div v-if="(scanTask.found ?? []).length" class="sensor-table mono small scan-results">
            <div v-for="(hit, idx) in scanTask.found ?? []" :key="idx" class="sensor-row">
              <span class="sensor-name">{{ hit.ip }}</span>
              <span class="pill" :class="hit.rmcp_plus_supported ? 'pill-ok' : 'pill-muted'">RMCP+</span>
              <span class="pill" :class="hit.ipmi_supported ? 'pill-blue' : 'pill-muted'">IPMI</span>
              <span class="sensor-type">ASF {{ hit.asf_version ?? '?' }} · IANA {{ hit.enterprise_iana ?? '?' }}</span>
              <button
                type="button"
                class="btn btn-small"
                @click="openIpmiCreate(String(hit.ip ?? ''))"
              >加入设备</button>
            </div>
          </div>
          <p v-else-if="scanTask.status === 'completed'" class="muted small">
            该网段未发现 IPMI 设备（BMC 未接管理口或 RMCP 被防火墙拦截）。
          </p>
        </template>
      </section>

      <!-- —— WoL 魔术唤醒 —— -->
      <div class="panel-head">
        <span class="panel-title">LAN 魔术唤醒（WoL）</span>
        <button class="btn btn-small btn-primary" @click="openWolCreate">＋ 添加目标</button>
      </div>
      <div v-if="wolError" class="error-box">{{ wolError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="wolColumns"
            :rows="wolTargets"
            :loading="wolLoading"
            empty-text="暂无 WoL 目标，点击右上角「添加目标」（可从 ARP 邻居选 MAC）。"
          >
            <template #cell-mac="{ row }">
              <span class="mono small">{{ row.mac ?? '—' }}</span>
            </template>
            <template #cell-has_secureon="{ row }">
              <span class="pill" :class="row.has_secureon ? 'pill-blue' : 'pill-muted'">
                {{ row.has_secureon ? '已配置' : '—' }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-primary"
                :disabled="wolBusyId === row.id"
                @click.stop="wakeTarget(row)"
              >{{ wolBusyId === row.id ? '发送中…' : '唤醒' }}</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="wolBusyId === row.id"
                @click.stop="deleteWolTarget(row)"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>
      <p class="hint">流水线：WoL / IPMI 上电 → PXE 引导 → SSH 部署收尾。广播包无凭据，唤醒后目标约需数秒至一分钟启动。</p>

      <!-- 添加 IPMI 设备对话框（扫描「加入设备」带出 host 预填） -->
      <div v-if="showIpmiCreate" class="modal-backdrop" @click.self="closeIpmiCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="ipmi-create-title">
          <div class="modal-head">
            <h3 id="ipmi-create-title">添加远程 IPMI 设备</h3>
            <button class="modal-close" type="button" :disabled="ipmiSubmitting" @click="closeIpmiCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitIpmiCreate">
            <div class="field">
              <label for="ipmi-name">名称</label>
              <input id="ipmi-name" v-model="ipmiForm.name" type="text" placeholder="例如 节点01-BMC" :disabled="ipmiSubmitting" />
            </div>
            <div class="field-row">
              <div class="field field-grow-2">
                <label for="ipmi-host">BMC 地址</label>
                <input id="ipmi-host" v-model="ipmiForm.host" type="text" placeholder="例如 192.0.2.77" :disabled="ipmiSubmitting" />
              </div>
              <div class="field">
                <label for="ipmi-port">端口</label>
                <input id="ipmi-port" v-model="ipmiForm.port" type="text" placeholder="623" :disabled="ipmiSubmitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="ipmi-user">用户名</label>
                <input id="ipmi-user" v-model="ipmiForm.username" type="text" placeholder="admin" :disabled="ipmiSubmitting" />
              </div>
              <div class="field">
                <label for="ipmi-pass">密码</label>
                <input id="ipmi-pass" v-model="ipmiForm.password" type="password" autocomplete="new-password" placeholder="BMC 密码" :disabled="ipmiSubmitting" />
              </div>
            </div>
            <div class="field">
              <label for="ipmi-cipher">Cipher suite（可选，如 3 / 17）</label>
              <input id="ipmi-cipher" v-model="ipmiForm.cipher" type="text" placeholder="留空自动协商" :disabled="ipmiSubmitting" />
            </div>
            <p class="hint">密码仅存服务端 state 文件（生产部署须 vault 化），列表/详情一律脱敏。</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="ipmiSubmitting" @click="closeIpmiCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="ipmiSubmitting">
                {{ ipmiSubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 添加 WoL 目标对话框（ARP 邻居下拉自动填 MAC） -->
      <div v-if="showWolCreate" class="modal-backdrop" @click.self="closeWolCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="wol-create-title">
          <div class="modal-head">
            <h3 id="wol-create-title">添加 WoL 目标</h3>
            <button class="modal-close" type="button" :disabled="wolSubmitting" @click="closeWolCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitWolCreate">
            <div class="field">
              <label for="wol-name">名称</label>
              <input id="wol-name" v-model="wolForm.name" type="text" placeholder="例如 节点01" :disabled="wolSubmitting" />
            </div>
            <div class="field">
              <label for="wol-mac">MAC 地址</label>
              <input id="wol-mac" v-model="wolForm.mac" type="text" placeholder="aa:bb:cc:dd:ee:ff" :disabled="wolSubmitting" />
            </div>
            <div class="field">
              <label for="wol-arp">从局域网邻居选择（ip neigh）</label>
              <select id="wol-arp" :disabled="wolSubmitting || !arpNeighbors.length" @change="fillMacFromArp(($event.target as HTMLSelectElement).value)">
                <option value="">{{ arpNeighbors.length ? '（选择自动填 MAC）' : '（暂无邻居记录）' }}</option>
                <option v-for="n in arpNeighbors" :key="n.ip" :value="String(n.mac ?? '')">
                  {{ n.ip }} · {{ n.mac }} · {{ n.state ?? '' }}
                </option>
              </select>
            </div>
            <div class="field-row">
              <div class="field field-grow-2">
                <label for="wol-broadcast">广播地址</label>
                <input id="wol-broadcast" v-model="wolForm.broadcast" type="text" placeholder="255.255.255.255" :disabled="wolSubmitting" />
              </div>
              <div class="field">
                <label for="wol-port">UDP 端口</label>
                <input id="wol-port" v-model="wolForm.port" type="text" placeholder="9" :disabled="wolSubmitting" />
              </div>
            </div>
            <div class="field">
              <label for="wol-secureon">SecureOn 密码（可选，6 字节十六进制）</label>
              <input id="wol-secureon" v-model="wolForm.secureon_password" type="password" autocomplete="new-password" placeholder="例如 00:11:22:33:44:55" :disabled="wolSubmitting" />
            </div>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="wolSubmitting" @click="closeWolCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="wolSubmitting">
                {{ wolSubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>
    <!-- ============ Tab: 一键安装（install.sh 动态生成脚本 + 入口命令） ============ -->
    <section v-show="activeTab === 'install'" class="tab-panel">
      <div class="card" style="margin-bottom: 16px">
        <div class="card-head">
          <h3>🚀 一键安装 NexOS（NAT 后 Ubuntu 适用）</h3>
        </div>
        <p class="muted small" style="margin: 8px 0">
          在一台全新的 Ubuntu 22.04/24.04 机器上执行下面这条命令，即可完成 NexOS 安装并自动加入集群。
          安装源 = 当前节点（本页所在节点的公网/内网地址），新节点的 P2P bootstrap 也自动指向它。
          依赖：Ubuntu 22.04/24.04 · root/sudo · 能访问本节点 8558 端口。
        </p>
        <div class="ob-code" style="position:relative; background:#1b1b22; border-radius:8px; padding:14px 44px 14px 14px; overflow:auto">
          <button class="ob-copy" type="button" style="position:absolute; top:8px; right:8px; background:rgba(255,255,255,.12); border:none; color:#fff; border-radius:6px; padding:4px 10px; cursor:pointer; font-size:12px"
            @click="copyInstallCmd">{{ installCopied ? '✓ 已复制' : '复制' }}</button>
          <pre style="margin:0; font-family:var(--mono, monospace); color:#d8e0ea; white-space:pre-wrap; word-break:break-all">sudo bash -c "$(curl -fsSL {{ installUrl }}/api/v1/provisioning/install.sh)"</pre>
        </div>
        <p class="muted small" style="margin-top:8px">
          安装源：<b>{{ installUrl }}</b>（本页所在节点，任一公网/内网入口节点均可互换）·
          新节点 P2P bootstrap 默认指向 <b>203.0.113.2:7070 / 198.51.100.114:7070</b> 两个公网入口 ·
          详细参数见 <code>docs/BOOTSTRAP_INSTALL.md</code>
        </p>
      </div>
    </section>
  </div>
</template>

<style scoped>
.provisioning-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.head-actions-inline { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.mono { font-family: var(--mono); }

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

/* 统计卡 */
.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 14px; }
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
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-head h3 { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* PXE 状态卡 */
.status-card { padding: 18px 20px; display: flex; justify-content: space-between; align-items: center; gap: 16px; flex-wrap: wrap; }
.status-left { display: flex; align-items: center; gap: 14px; min-width: 0; }
.status-dot {
  width: 12px; height: 12px; border-radius: 50%; flex-shrink: 0;
  box-shadow: 0 0 0 4px rgba(0, 0, 0, 0.04);
}
.status-dot.running { background: #0E8420; box-shadow: 0 0 0 4px rgba(14, 132, 32, 0.18); }
.status-dot.stopped { background: #8e8e93; box-shadow: 0 0 0 4px rgba(142, 142, 147, 0.18); }
.status-dot.pending { background: #E95420; box-shadow: 0 0 0 4px rgba(233, 84, 32, 0.18); }
.status-meta { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
.status-line { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.status-label { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.error-text { color: #b91c1c; font-size: 12.5px; }

/* 配置表单 */
.config-card { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
.config-form { display: flex; flex-direction: column; gap: 14px; }
.form-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
.field-wide { grid-column: 1 / -1; }
.field-grow-2 { flex: 2; }
.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; color: var(--text, #2B2B2B); }
.field input, .field select, .field textarea {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
  box-sizing: border-box;
}
.field textarea { resize: vertical; font-family: var(--mono); }
.field input:focus, .field select:focus, .field textarea:focus {
  outline: none; border-color: var(--accent, #E95420); box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
}
.field-row { display: flex; gap: 12px; }
.hint { font-size: 12px; color: var(--text-muted, #6b7280); line-height: 1.6; }

.switch { display: inline-flex; align-items: center; gap: 8px; font-size: 13px; cursor: pointer; color: var(--text, #2B2B2B); }
.switch input[type='checkbox'] { width: 16px; height: 16px; cursor: pointer; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-warn { color: #92600a; background: #fef3c7; }
.pill-purple { color: #7c3aed; background: #ede9fe; }

/* 按钮 */
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

/* 自旋刷新 */
.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

/* 表格内截断单元格 */
.cell-path { display: inline-block; max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; vertical-align: middle; color: var(--text, #2B2B2B); }

/* 模态框 */
.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(560px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-wide { width: min(640px, 100%); }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-close:hover:not(:disabled) { color: var(--text, #2B2B2B); }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }

/* 部署对话框文件列表 */
.file-list { display: flex; flex-direction: column; gap: 8px; }
.file-row { display: flex; align-items: center; gap: 8px; }
.file-row input { flex: 1; padding: 6px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 13px; background: var(--bg-card, #fff); color: var(--text, #2B2B2B); box-sizing: border-box; }
.file-row input:focus { outline: none; border-color: var(--accent, #E95420); box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15); }
.file-row .arrow { color: var(--text-muted, #5E5C5F); font-size: 14px; }
.file-del { padding: 2px 8px; line-height: 1.2; }

/* 部署进度（文件级结果 + 命令输出） */
.progress-summary { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.file-results { display: flex; flex-direction: column; gap: 6px; }
.file-result-row { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding: 4px 0; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.file-result-row:last-child { border-bottom: none; }
.fr-icon { display: inline-flex; align-items: center; justify-content: center; width: 20px; height: 20px; border-radius: 50%; font-size: 12px; font-weight: 700; flex-shrink: 0; }
.fr-ok { color: #15803d; background: #dcfce7; }
.fr-err { color: #b91c1c; background: #fee2e2; }
.fr-skip { color: #6b7280; background: #f3f4f6; }
.fr-pending { color: #92600a; background: #fef3c7; }
.fr-path { flex: 1; min-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cmd-card { display: flex; flex-direction: column; gap: 8px; padding: 10px 12px; border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }
.cmd-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.cmd-details summary { cursor: pointer; user-select: none; }

/* 日志/构建进度 */
.log-card { padding: 14px 18px; display: flex; flex-direction: column; gap: 10px; }
.log-pre { margin: 0; padding: 10px 12px; background: rgba(0, 0, 0, 0.04); border-radius: var(--radius-sm, 8px); max-height: 260px; overflow: auto; white-space: pre-wrap; word-break: break-all; color: var(--text, #2B2B2B); }
.building-meta { display: flex; align-items: center; gap: 8px; margin-top: 4px; }
.progress-track { width: 80px; height: 6px; border-radius: 3px; background: rgba(0, 0, 0, 0.08); overflow: hidden; }
.progress-fill { height: 100%; border-radius: 3px; background: var(--accent, #E95420); transition: width 0.4s ease; }

/* 电源控制 Tab（本机 BMC 电源按钮组 / 传感器表 / 扫描进度） */
.power-actions { display: flex; gap: 8px; flex-wrap: wrap; }
.sensor-table { display: flex; flex-direction: column; gap: 4px; max-height: 300px; overflow: auto; padding: 8px 10px; background: rgba(0, 0, 0, 0.03); border-radius: var(--radius-sm, 8px); }
.sensor-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.sensor-name { min-width: 140px; font-weight: 600; }
.sensor-type { color: var(--text-muted, #6b6b6b); }
.sensor-reading { flex: 1; min-width: 160px; }
.scan-card { padding: 14px 18px; display: flex; flex-direction: column; gap: 12px; }
.scan-track { width: 100%; height: 8px; }
.scan-results .btn { margin-left: auto; }

@media (max-width: 720px) {
  .provisioning-page { padding: 16px; }
  .form-grid { grid-template-columns: 1fr; }
  .field-row { flex-direction: column; }
  .file-row { flex-wrap: wrap; }
  .file-row input { min-width: 140px; }
}
</style>
