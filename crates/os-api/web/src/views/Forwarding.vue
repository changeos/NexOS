<script setup lang="ts">
// =============================================================================
// Forwarding.vue —— 远程转发（SSH 隧道 + RDP 远程桌面）
//
// 2 Tab：SSH 隧道（spawn 系统 ssh -L/-R/-D）/ RDP 转发（纯 Rust TCP 代理）
// 后端：/api/v1/forwarding/*（ForwardingRouteHandler，GET 免认证、写需 admin）
//
// 设计：Ubuntu Yaru 风 .card / .page-head，顶部 stats 小条 + 表格 + 对话框，
// 三态加载；5s 轮询（页面隐藏时暂停）。
// 红线：SSH 隧道仅支持私钥认证（服务器上 ~/.ssh 私钥），表单无密码字段。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';
import type {
  ForwardingStats,
  RdpForward,
  SshTunnel,
  SshTunnelMode,
} from '@/api/client';

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'ssh' | 'rdp';
const activeTab = ref<TabKey>('ssh');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'ssh', label: 'SSH 隧道' },
  { key: 'rdp', label: 'RDP 远程桌面' },
];

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该转发接口';
  }
  return m;
}

// =============================================================================
// 顶部统计小条（GET /api/v1/forwarding/stats，免认证）
// =============================================================================
const stats = ref<ForwardingStats | null>(null);
const statsError = ref('');

async function loadStats(): Promise<void> {
  try {
    stats.value = await endpoints.forwardingStats();
    statsError.value = '';
  } catch (e) {
    stats.value = null;
    statsError.value = friendlyError(e);
  }
}

const statCards = computed(() => [
  { label: 'SSH 隧道', value: stats.value?.ssh_tunnels_total ?? '—' },
  { label: '运行中隧道', value: stats.value?.ssh_tunnels_running ?? '—' },
  { label: 'RDP 转发', value: stats.value?.rdp_forwards_total ?? '—' },
  { label: '累计连接', value: stats.value?.rdp_total_connections ?? '—' },
]);

// =============================================================================
// 两步删除确认（同键第二次点击才真正删除，3s 不点自动复位）
// =============================================================================
/** 待二次确认的删除目标（`ssh:<id>` / `rdp:<id>`），空 = 无待确认。 */
const pendingDelete = ref<string>('');
let pendingDeleteTimer: ReturnType<typeof setTimeout> | null = null;

function requestDelete(key: string): boolean {
  if (pendingDelete.value === key) {
    // 第二步：确认删除
    resetPendingDelete();
    return true;
  }
  // 第一步：进入待确认态，3s 后自动复位
  resetPendingDelete();
  pendingDelete.value = key;
  pendingDeleteTimer = setTimeout(() => {
    pendingDelete.value = '';
  }, 3000);
  return false;
}
function resetPendingDelete(): void {
  if (pendingDeleteTimer) {
    clearTimeout(pendingDeleteTimer);
    pendingDeleteTimer = null;
  }
  pendingDelete.value = '';
}

// =============================================================================
// Tab1：SSH 隧道
// =============================================================================
const tunnels = ref<SshTunnel[]>([]);
const sshLoading = ref(false);
const sshError = ref('');
/** 行内 busy 态（`start:<id>` / `stop:<id>` / `del:<id>`）。 */
const sshBusy = ref<string>('');

async function loadTunnels(): Promise<void> {
  sshLoading.value = true;
  sshError.value = '';
  try {
    const raw = await endpoints.forwardingSshTunnels();
    tunnels.value = Array.isArray(raw) ? raw : [];
  } catch (e) {
    tunnels.value = [];
    sshError.value = friendlyError(e);
  } finally {
    sshLoading.value = false;
  }
}

async function startTunnel(row: SshTunnel): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  sshBusy.value = `start:${id}`;
  msg.value = { kind: 'info', text: '启动中…（密钥/网络不通时约 1s 后报失败）' };
  try {
    const t = await endpoints.startForwardingSshTunnel(id);
    await loadTunnels();
    msg.value =
      t.status === 'running'
        ? { kind: 'ok', text: `隧道「${row.name}」已启动` }
        : { kind: 'err', text: `隧道「${row.name}」启动失败：${t.error ?? '未知错误'}` };
  } catch (e) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) };
    await loadTunnels();
  } finally {
    sshBusy.value = '';
  }
}

async function stopTunnel(row: SshTunnel): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  sshBusy.value = `stop:${id}`;
  msg.value = null;
  try {
    await endpoints.stopForwardingSshTunnel(id);
    await loadTunnels();
    msg.value = { kind: 'ok', text: `隧道「${row.name}」已停止` };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
    await loadTunnels();
  } finally {
    sshBusy.value = '';
  }
}

async function deleteTunnel(row: SshTunnel): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!requestDelete(`ssh:${id}`)) return; // 第一步只切换确认态
  sshBusy.value = `del:${id}`;
  msg.value = null;
  try {
    await endpoints.deleteForwardingSshTunnel(id);
    await loadTunnels();
    void loadStats();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
    await loadTunnels();
  } finally {
    sshBusy.value = '';
  }
}

// —— 展示辅助 ——
/** 模式徽章：L/R/D 单字母 + 中文。 */
function modeLabel(m?: string): string {
  switch (m) {
    case 'local':
      return '本地 L';
    case 'remote':
      return '远程 R';
    case 'dynamic':
      return '动态 D';
    default:
      return m ?? '—';
  }
}
function modeClass(m?: string): string {
  switch (m) {
    case 'local':
      return 'pill-blue';
    case 'remote':
      return 'pill-purple';
    case 'dynamic':
      return 'pill-warn';
    default:
      return 'pill-muted';
  }
}
/** 绑定/转发地址：local → bind → 目标；remote → 目标 ← bind（远端）；dynamic → bind (SOCKS)。 */
function forwardDesc(r: SshTunnel): string {
  const bind = r.local_bind ?? '—';
  if (r.mode === 'dynamic') return `${bind} (SOCKS5)`;
  const target = r.remote_host && r.remote_port ? `${r.remote_host}:${r.remote_port}` : '—';
  return r.mode === 'remote' ? `${bind} ← ${target}` : `${bind} → ${target}`;
}
/** 状态徽章：running 绿 / stopped 灰 / failed 红。 */
function sshStatusClass(s?: string): string {
  switch (s) {
    case 'running':
      return 'pill-ok';
    case 'failed':
      return 'pill-err';
    case 'stopped':
    default:
      return 'pill-muted';
  }
}
function sshStatusLabel(s?: string): string {
  switch (s) {
    case 'running':
      return '运行中';
    case 'failed':
      return '失败';
    case 'stopped':
      return '已停止';
    default:
      return s ?? '—';
  }
}

const sshColumns: Column<SshTunnel>[] = [
  { key: 'name', title: '名称', sortable: true, accessor: (r) => r.name ?? '—' },
  { key: 'mode', title: '模式', width: '90px', align: 'center', accessor: (r) => r.mode ?? '—' },
  { key: 'target', title: 'SSH 目标', accessor: (r) => `${r.ssh_user ?? '—'}@${r.ssh_host ?? '—'}:${r.ssh_port ?? 22}` },
  { key: 'forward', title: '绑定 / 转发地址', accessor: (r) => forwardDesc(r) },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'pid', title: 'PID', width: '70px', align: 'right', accessor: (r) => r.pid ?? '—' },
  { key: 'autostart', title: '自启', width: '70px', align: 'center', accessor: (r) => (r.autostart ? 1 : 0) },
  { key: 'actions', title: '操作', width: '200px', align: 'right' },
];

// —— 创建隧道对话框（模式切换显隐 remote_host/remote_port；无密码字段红线）——
const showSshCreate = ref(false);
const sshForm = ref({
  name: '',
  ssh_host: '',
  ssh_port: '22',
  ssh_user: 'root',
  private_key_path: '',
  mode: 'local' as SshTunnelMode,
  local_bind: '',
  remote_host: '',
  remote_port: '',
  autostart: false,
});
const sshSubmitting = ref(false);
/** dynamic（-D SOCKS）模式下隐藏 remote_host/remote_port。 */
const needsRemote = computed(() => sshForm.value.mode !== 'dynamic');

function openSshCreate(): void {
  sshForm.value = {
    name: '',
    ssh_host: '',
    ssh_port: '22',
    ssh_user: 'root',
    private_key_path: '',
    mode: 'local',
    local_bind: '',
    remote_host: '',
    remote_port: '',
    autostart: false,
  };
  msg.value = null;
  showSshCreate.value = true;
}
function closeSshCreate(): void {
  if (sshSubmitting.value) return;
  showSshCreate.value = false;
}
async function submitSshCreate(): Promise<void> {
  const f = sshForm.value;
  const name = f.name.trim();
  const host = f.ssh_host.trim();
  const bind = f.local_bind.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!host) { msg.value = { kind: 'err', text: '请填写 SSH 服务器' }; return; }
  if (!f.ssh_user.trim()) { msg.value = { kind: 'err', text: '请填写 SSH 用户名' }; return; }
  if (!bind) { msg.value = { kind: 'err', text: '请填写本地绑定地址' }; return; }
  if (!/^[^\s:]+:\d+$/.test(bind)) {
    msg.value = { kind: 'err', text: '绑定地址须为 host:port 形式（如 127.0.0.1:8080）' };
    return;
  }
  const remoteHost = f.remote_host.trim();
  const remotePort = f.remote_port.trim();
  if (f.mode !== 'dynamic' && !remoteHost) {
    msg.value = { kind: 'err', text: `${modeLabel(f.mode)}模式必须填写转发目标主机` };
    return;
  }
  if (f.mode !== 'dynamic' && !/^\d+$/.test(remotePort)) {
    msg.value = { kind: 'err', text: `${modeLabel(f.mode)}模式必须填写转发目标端口` };
    return;
  }
  sshSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createForwardingSshTunnel({
      name,
      ssh_host: host,
      ssh_user: f.ssh_user.trim(),
      ssh_port: Number(f.ssh_port) || 22,
      private_key_path: f.private_key_path.trim() || undefined,
      mode: f.mode,
      local_bind: bind,
      remote_host: f.mode !== 'dynamic' ? remoteHost : undefined,
      remote_port: f.mode !== 'dynamic' ? Number(remotePort) : undefined,
      autostart: f.autostart,
    });
    showSshCreate.value = false;
    await loadTunnels();
    void loadStats();
    msg.value = { kind: 'ok', text: '隧道已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    sshSubmitting.value = false;
  }
}

// =============================================================================
// Tab2：RDP 远程桌面
// =============================================================================
const rdpList = ref<RdpForward[]>([]);
const rdpLoading = ref(false);
const rdpError = ref('');
const rdpBusy = ref<string>('');

async function loadRdp(): Promise<void> {
  rdpLoading.value = true;
  rdpError.value = '';
  try {
    const raw = await endpoints.forwardingRdpForwards();
    rdpList.value = Array.isArray(raw) ? raw : [];
  } catch (e) {
    rdpList.value = [];
    rdpError.value = friendlyError(e);
  } finally {
    rdpLoading.value = false;
  }
}

async function startRdp(row: RdpForward): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  rdpBusy.value = `start:${id}`;
  msg.value = null;
  try {
    await endpoints.startForwardingRdp(id);
    await loadRdp();
    msg.value = { kind: 'ok', text: `转发「${row.name}」已启动` };
  } catch (e) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) };
    await loadRdp();
  } finally {
    rdpBusy.value = '';
  }
}

async function stopRdp(row: RdpForward): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  rdpBusy.value = `stop:${id}`;
  msg.value = null;
  try {
    await endpoints.stopForwardingRdp(id);
    await loadRdp();
    msg.value = { kind: 'ok', text: `转发「${row.name}」已停止` };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
    await loadRdp();
  } finally {
    rdpBusy.value = '';
  }
}

async function deleteRdp(row: RdpForward): Promise<void> {
  const id = String(row.id ?? '');
  if (!id) return;
  if (!requestDelete(`rdp:${id}`)) return; // 第一步只切换确认态
  rdpBusy.value = `del:${id}`;
  msg.value = null;
  try {
    await endpoints.deleteForwardingRdp(id);
    await loadRdp();
    void loadStats();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
    await loadRdp();
  } finally {
    rdpBusy.value = '';
  }
}

function rdpStatusClass(s?: string): string {
  switch (s) {
    case 'running':
      return 'pill-ok';
    case 'error':
      return 'pill-err';
    case 'stopped':
    default:
      return 'pill-muted';
  }
}
function rdpStatusLabel(s?: string): string {
  switch (s) {
    case 'running':
      return '运行中';
    case 'error':
      return '错误';
    case 'stopped':
      return '已停止';
    default:
      return s ?? '—';
  }
}

const rdpColumns: Column<RdpForward>[] = [
  { key: 'name', title: '名称', sortable: true, accessor: (r) => r.name ?? '—' },
  { key: 'target', title: '目标主机', accessor: (r) => `${r.target_host ?? '—'}:${r.target_port ?? 3389}` },
  { key: 'listen_port', title: '监听端口', width: '110px', accessor: (r) => `0.0.0.0:${r.listen_port ?? '—'}` },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'connections', title: '累计连接', width: '90px', align: 'right', sortable: true, accessor: (r) => r.connections ?? 0 },
  { key: 'autostart', title: '自启', width: '70px', align: 'center', accessor: (r) => (r.autostart ? 1 : 0) },
  { key: 'actions', title: '操作', width: '260px', align: 'right' },
];

// —— 创建 RDP 转发对话框 ——
const showRdpCreate = ref(false);
const rdpForm = ref({
  name: '',
  target_host: '',
  target_port: '3389',
  listen_port: '',
  autostart: false,
});
const rdpSubmitting = ref(false);

function openRdpCreate(): void {
  rdpForm.value = { name: '', target_host: '', target_port: '3389', listen_port: '', autostart: false };
  msg.value = null;
  showRdpCreate.value = true;
}
function closeRdpCreate(): void {
  if (rdpSubmitting.value) return;
  showRdpCreate.value = false;
}
async function submitRdpCreate(): Promise<void> {
  const f = rdpForm.value;
  const name = f.name.trim();
  const host = f.target_host.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!host) { msg.value = { kind: 'err', text: '请填写目标 Windows 主机' }; return; }
  if (!/^\d+$/.test(f.listen_port.trim())) {
    msg.value = { kind: 'err', text: '监听端口须为数字（1-65535）' };
    return;
  }
  rdpSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createForwardingRdp({
      name,
      target_host: host,
      target_port: Number(f.target_port) || 3389,
      listen_port: Number(f.listen_port),
      autostart: f.autostart,
    });
    showRdpCreate.value = false;
    await loadRdp();
    void loadStats();
    msg.value = { kind: 'ok', text: 'RDP 转发已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    rdpSubmitting.value = false;
  }
}

// —— 下载 .rdp 对话框（GET 免认证可直接链；username 选填写入文件）——
const showRdpFile = ref(false);
const rdpFileTarget = ref<RdpForward | null>(null);
const rdpFileUsername = ref('');

function openRdpFile(row: RdpForward): void {
  rdpFileTarget.value = row;
  rdpFileUsername.value = '';
  msg.value = null;
  showRdpFile.value = true;
}
function closeRdpFile(): void {
  showRdpFile.value = false;
  rdpFileTarget.value = null;
  rdpFileUsername.value = '';
}
function downloadRdpFile(): void {
  const row = rdpFileTarget.value;
  if (!row?.id) return;
  const username = rdpFileUsername.value.trim();
  // GET /rdp-file 免认证 → window.open 直链下载
  window.open(endpoints.forwardingRdpFileUrl(String(row.id), username || undefined), '_blank');
  closeRdpFile();
}

// =============================================================================
// 刷新 / 轮询（5s；document.hidden 时跳过，避免后台空转）
// =============================================================================
const POLL_MS = 5000;
let pollTimer: ReturnType<typeof setInterval> | null = null;

const refreshing = computed(() => sshLoading.value || rdpLoading.value);

async function refreshAll(): Promise<void> {
  await Promise.all([loadStats(), loadTunnels(), loadRdp()]);
}

function onPollTick(): void {
  if (typeof document !== 'undefined' && document.hidden) return; // 隐藏暂停
  void refreshAll();
}

onMounted(() => {
  void refreshAll();
  pollTimer = setInterval(onPollTick, POLL_MS);
});

onBeforeUnmount(() => {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  resetPendingDelete();
});
</script>

<template>
  <div class="forwarding-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">远程转发</h2>
        <div class="page-sub muted">SSH 隧道（-L/-R/-D）· RDP 远程桌面 TCP 转发</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
    </div>

    <!-- 顶部 stats 小条 -->
    <section class="stat-grid">
      <div v-for="c in statCards" :key="c.label" class="card stat-card">
        <div class="stat-label">{{ c.label }}</div>
        <div class="stat-value">{{ c.value }}</div>
      </div>
    </section>
    <div v-if="statsError" class="error-box">统计加载失败：{{ statsError }}</div>

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

    <!-- =================== Tab1 SSH 隧道 =================== -->
    <section v-show="activeTab === 'ssh'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">SSH 隧道</span>
        <button class="btn btn-small btn-primary" @click="openSshCreate">＋ 创建隧道</button>
      </div>

      <div v-if="sshError" class="error-box">{{ sshError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="sshColumns"
            :rows="tunnels"
            :loading="sshLoading"
            empty-text="暂无 SSH 隧道，点击右上角「创建隧道」。"
          >
            <template #cell-mode="{ row }">
              <span class="pill" :class="modeClass(row.mode)">{{ modeLabel(row.mode) }}</span>
            </template>
            <template #cell-target="{ row }">
              <span class="mono small">{{ row.ssh_user }}@{{ row.ssh_host }}:{{ row.ssh_port ?? 22 }}</span>
            </template>
            <template #cell-forward="{ row }">
              <span class="mono small">{{ forwardDesc(row) }}</span>
            </template>
            <template #cell-status="{ row }">
              <span
                class="pill clickable"
                :class="sshStatusClass(row.status)"
                :title="row.error || ''"
                @click.stop
              >{{ sshStatusLabel(row.status) }}</span>
            </template>
            <template #cell-pid="{ row }">
              <span class="mono small">{{ row.pid ?? '—' }}</span>
            </template>
            <template #cell-autostart="{ row }">
              <span class="pill" :class="row.autostart ? 'pill-blue' : 'pill-muted'">
                {{ row.autostart ? '自启' : '—' }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status !== 'running'"
                class="btn btn-small btn-primary"
                :disabled="sshBusy !== ''"
                @click.stop="startTunnel(row)"
              >{{ sshBusy === `start:${row.id}` ? '启动中…' : '启动' }}</button>
              <button
                v-else
                class="btn btn-small"
                :disabled="sshBusy !== ''"
                @click.stop="stopTunnel(row)"
              >{{ sshBusy === `stop:${row.id}` ? '停止中…' : '停止' }}</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="sshBusy !== ''"
                @click.stop="deleteTunnel(row)"
              >{{ sshBusy === `del:${row.id}` ? '删除中…' : pendingDelete === `ssh:${row.id}` ? '确认删除？' : '删除' }}</button>
            </template>
          </DataTable>
        </div>
      </div>
      <p class="hint">状态徽章悬停可查看错误摘要；SSH 认证使用服务器上的 ~/.ssh 私钥，不支持密码。</p>

      <!-- 创建隧道对话框（无密码字段——密钥认证红线） -->
      <div v-if="showSshCreate" class="modal-backdrop" @click.self="closeSshCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="ssh-create-title">
          <div class="modal-head">
            <h3 id="ssh-create-title">创建 SSH 隧道</h3>
            <button class="modal-close" type="button" :disabled="sshSubmitting" @click="closeSshCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitSshCreate">
            <div class="field">
              <label for="ssh-name">名称</label>
              <input id="ssh-name" v-model="sshForm.name" type="text" placeholder="例如 内网穿透" :disabled="sshSubmitting" />
            </div>
            <div class="field-row">
              <div class="field field-grow-2">
                <label for="ssh-host">SSH 服务器</label>
                <input id="ssh-host" v-model="sshForm.ssh_host" type="text" placeholder="例如 203.0.113.10" :disabled="sshSubmitting" />
              </div>
              <div class="field">
                <label for="ssh-port">SSH 端口</label>
                <input id="ssh-port" v-model="sshForm.ssh_port" type="text" placeholder="22" :disabled="sshSubmitting" />
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="ssh-user">SSH 用户名</label>
                <input id="ssh-user" v-model="sshForm.ssh_user" type="text" placeholder="root" :disabled="sshSubmitting" />
              </div>
              <div class="field">
                <label for="ssh-key">私钥路径（可选）</label>
                <input id="ssh-key" v-model="sshForm.private_key_path" type="text" placeholder="~/.ssh/id_ed25519" :disabled="sshSubmitting" />
              </div>
            </div>
            <div class="field">
              <label for="ssh-mode">模式</label>
              <select id="ssh-mode" v-model="sshForm.mode" :disabled="sshSubmitting">
                <option value="local">本地转发 local（-L）</option>
                <option value="remote">远程转发 remote（-R）</option>
                <option value="dynamic">动态 SOCKS dynamic（-D）</option>
              </select>
            </div>
            <div class="field">
              <label for="ssh-bind">本地绑定地址</label>
              <input id="ssh-bind" v-model="sshForm.local_bind" type="text" placeholder="127.0.0.1:8080" :disabled="sshSubmitting" />
              <p class="hint">host:port 形式；remote 模式下作为远端绑定地址。</p>
            </div>
            <div v-if="needsRemote" class="field-row">
              <div class="field field-grow-2">
                <label for="ssh-remote-host">转发目标主机</label>
                <input id="ssh-remote-host" v-model="sshForm.remote_host" type="text" placeholder="例如 192.168.1.20" :disabled="sshSubmitting" />
              </div>
              <div class="field">
                <label for="ssh-remote-port">目标端口</label>
                <input id="ssh-remote-port" v-model="sshForm.remote_port" type="text" placeholder="例如 3389" :disabled="sshSubmitting" />
              </div>
            </div>
            <label class="switch">
              <input v-model="sshForm.autostart" type="checkbox" :disabled="sshSubmitting" />
              <span>随 os-api 启动自动拉起（autostart）</span>
            </label>
            <p class="hint">认证使用服务器上 <span class="mono">~/.ssh</span> 私钥（默认 id_ed25519），不支持密码登录。</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="sshSubmitting" @click="closeSshCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="sshSubmitting">
                {{ sshSubmitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 RDP 远程桌面 =================== -->
    <section v-show="activeTab === 'rdp'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">RDP 远程桌面转发</span>
        <button class="btn btn-small btn-primary" @click="openRdpCreate">＋ 创建转发</button>
      </div>

      <div v-if="rdpError" class="error-box">{{ rdpError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="rdpColumns"
            :rows="rdpList"
            :loading="rdpLoading"
            empty-text="暂无 RDP 转发，点击右上角「创建转发」。"
          >
            <template #cell-target="{ row }">
              <span class="mono small">{{ row.target_host }}:{{ row.target_port ?? 3389 }}</span>
            </template>
            <template #cell-listen_port="{ row }">
              <span class="mono small">0.0.0.0:{{ row.listen_port }}</span>
            </template>
            <template #cell-status="{ row }">
              <span
                class="pill clickable"
                :class="rdpStatusClass(row.status)"
                :title="row.error || ''"
                @click.stop
              >{{ rdpStatusLabel(row.status) }}</span>
            </template>
            <template #cell-connections="{ row }">
              <span class="mono">{{ row.connections ?? 0 }}</span>
            </template>
            <template #cell-autostart="{ row }">
              <span class="pill" :class="row.autostart ? 'pill-blue' : 'pill-muted'">
                {{ row.autostart ? '自启' : '—' }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status !== 'running'"
                class="btn btn-small btn-primary"
                :disabled="rdpBusy !== ''"
                @click.stop="startRdp(row)"
              >{{ rdpBusy === `start:${row.id}` ? '启动中…' : '启动' }}</button>
              <button
                v-else
                class="btn btn-small"
                :disabled="rdpBusy !== ''"
                @click.stop="stopRdp(row)"
              >{{ rdpBusy === `stop:${row.id}` ? '停止中…' : '停止' }}</button>
              <button
                class="btn btn-small"
                :disabled="rdpBusy !== ''"
                @click.stop="openRdpFile(row)"
              >下载 .rdp</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="rdpBusy !== ''"
                @click.stop="deleteRdp(row)"
              >{{ rdpBusy === `del:${row.id}` ? '删除中…' : pendingDelete === `rdp:${row.id}` ? '确认删除？' : '删除' }}</button>
            </template>
          </DataTable>
        </div>
      </div>
      <p class="hint">
        用 Windows 远程桌面客户端（mstsc）打开下载的 .rdp 文件即可连接；请确保目标机已开启远程桌面。
      </p>

      <!-- 创建 RDP 转发对话框 -->
      <div v-if="showRdpCreate" class="modal-backdrop" @click.self="closeRdpCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="rdp-create-title">
          <div class="modal-head">
            <h3 id="rdp-create-title">创建 RDP 转发</h3>
            <button class="modal-close" type="button" :disabled="rdpSubmitting" @click="closeRdpCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitRdpCreate">
            <div class="field">
              <label for="rdp-name">名称</label>
              <input id="rdp-name" v-model="rdpForm.name" type="text" placeholder="例如 办公室电脑" :disabled="rdpSubmitting" />
            </div>
            <div class="field-row">
              <div class="field field-grow-2">
                <label for="rdp-host">目标 Windows 主机</label>
                <input id="rdp-host" v-model="rdpForm.target_host" type="text" placeholder="例如 192.168.1.100" :disabled="rdpSubmitting" />
              </div>
              <div class="field">
                <label for="rdp-target-port">RDP 端口</label>
                <input id="rdp-target-port" v-model="rdpForm.target_port" type="text" placeholder="3389" :disabled="rdpSubmitting" />
              </div>
            </div>
            <div class="field">
              <label for="rdp-listen">本机监听端口</label>
              <input id="rdp-listen" v-model="rdpForm.listen_port" type="text" placeholder="例如 13389" :disabled="rdpSubmitting" />
              <p class="hint">0.0.0.0:&lt;监听端口&gt; → 目标主机:RDP 端口 的 TCP 代理。</p>
            </div>
            <label class="switch">
              <input v-model="rdpForm.autostart" type="checkbox" :disabled="rdpSubmitting" />
              <span>随 os-api 启动自动拉起（autostart）</span>
            </label>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="rdpSubmitting" @click="closeRdpCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="rdpSubmitting">
                {{ rdpSubmitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>

      <!-- 下载 .rdp 对话框（username 选填） -->
      <div v-if="showRdpFile" class="modal-backdrop" @click.self="closeRdpFile">
        <div class="modal modal-narrow" role="dialog" aria-modal="true" aria-labelledby="rdp-file-title">
          <div class="modal-head">
            <h3 id="rdp-file-title">下载 .rdp 连接文件</h3>
            <button class="modal-close" type="button" @click="closeRdpFile">×</button>
          </div>
          <form class="modal-body" @submit.prevent="downloadRdpFile">
            <p class="hint">
              转发「{{ rdpFileTarget?.name }}」→
              <span class="mono">{{ rdpFileTarget?.target_host }}:{{ rdpFileTarget?.target_port }}</span>
            </p>
            <div class="field">
              <label for="rdp-file-user">用户名（可选）</label>
              <input id="rdp-file-user" v-model="rdpFileUsername" type="text" placeholder="例如 Administrator（留空由客户端提示）" />
            </div>
            <p class="hint">用 Windows 远程桌面客户端打开该文件；确保目标机已开启远程桌面。</p>
            <div class="form-actions">
              <button type="button" class="btn" @click="closeRdpFile">取消</button>
              <button type="submit" class="btn btn-primary">下载 .rdp</button>
            </div>
          </form>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.forwarding-page {
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

/* 统计卡（顶部 stats 小条） */
.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 14px; }
.stat-card { padding: 14px 18px; display: flex; flex-direction: column; gap: 4px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 24px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }

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

/* 表单 */
.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; color: var(--text, #2B2B2B); }
.field input, .field select {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
  box-sizing: border-box;
}
.field input:focus, .field select:focus {
  outline: none; border-color: var(--accent, #E95420); box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
}
.field-row { display: flex; gap: 12px; }
.field-grow-2 { flex: 2; }
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
/* 带错误 tooltip 的状态徽章 */
.pill.clickable { cursor: help; }

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

/* 模态框 */
.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(560px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-narrow { width: min(440px, 100%); }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-close:hover:not(:disabled) { color: var(--text, #2B2B2B); }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }

@media (max-width: 720px) {
  .forwarding-page { padding: 16px; }
  .field-row { flex-direction: column; }
}
</style>
