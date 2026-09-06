<script setup lang="ts">
// =============================================================================
// Monitor.vue —— 系统监控（实时指标 + 告警 + ZFS 池）
//
// 功能：
//   1. 实时指标卡网格（CPU/内存/负载/磁盘/网络/进程/uptime）
//   2. 服务状态表（os-api/osd/sshd/zfs 等，running 绿徽章）
//   3. 告警列表（level 徽章 + 消息 + 来源 + 时间 + 确认按钮）
//   4. ZFS 池状态（zpool 健康度 ONLINE 绿）
//   5. onMounted 加载，可选 5s 定时刷新
//
// 后端：
//   GET /api/v1/monitor/metrics / services / alerts / zpools / stats / history
//   POST /api/v1/monitor/alerts/:id/ack
// =============================================================================
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { endpoints } from '@/api/client';

interface SystemMetrics {
  hostname?: string;
  uptime_secs?: number;
  load_avg?: number[];
  cpu_usage?: number;
  cpu_cores?: number;
  mem_total_bytes?: number;
  mem_used_bytes?: number;
  mem_available_bytes?: number;
  swap_total_bytes?: number;
  swap_used_bytes?: number;
  disk_total_bytes?: number;
  disk_used_bytes?: number;
  net_rx_bytes?: number;
  net_tx_bytes?: number;
  processes?: number;
  kernel_version?: string;
  [k: string]: unknown;
}

interface ServiceStatus {
  name?: string;
  status?: string;
  pid?: number | null;
  [k: string]: unknown;
}

interface Alert {
  id?: string;
  level?: string;
  message?: string;
  source?: string;
  timestamp?: string;
  acked?: boolean;
  [k: string]: unknown;
}

interface ZpoolStatus {
  name?: string;
  state?: string;
  size_bytes?: number;
  allocated_bytes?: number;
  free_bytes?: number;
  healthy?: boolean;
  [k: string]: unknown;
}

// =============================================================================
// 数据状态
// =============================================================================
const metrics = ref<SystemMetrics>({});
const services = ref<ServiceStatus[]>([]);
const alerts = ref<Alert[]>([]);
const zpools = ref<ZpoolStatus[]>([]);
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
const autoRefresh = ref(false);
let timer: ReturnType<typeof setInterval> | null = null;

async function loadMetrics(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.monitorMetrics();
    metrics.value = (raw ?? {}) as SystemMetrics;
  } catch (e) {
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadServices(): Promise<void> {
  try {
    const raw = await endpoints.monitorServices();
    services.value = Array.isArray(raw) ? (raw as ServiceStatus[]) : [];
  } catch {
    services.value = [];
  }
}

async function loadAlerts(): Promise<void> {
  try {
    const raw = await endpoints.monitorAlerts();
    alerts.value = Array.isArray(raw) ? (raw as Alert[]) : [];
  } catch {
    alerts.value = [];
  }
}

async function loadZpools(): Promise<void> {
  try {
    const raw = await endpoints.monitorZpools();
    zpools.value = Array.isArray(raw) ? (raw as ZpoolStatus[]) : [];
  } catch {
    zpools.value = [];
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadMetrics(), loadServices(), loadAlerts(), loadZpools()]);
}

async function ackAlert(id: string): Promise<void> {
  msg.value = null;
  try {
    await endpoints.ackMonitorAlert(id);
    await loadAlerts();
    msg.value = { kind: 'ok', text: '告警已确认' };
  } catch (e) {
    msg.value = { kind: 'err', text: '确认失败：' + friendlyError(e) };
  }
}

// =============================================================================
// 定时刷新
// =============================================================================
function toggleAutoRefresh(): void {
  autoRefresh.value = !autoRefresh.value;
  if (autoRefresh.value) {
    timer = setInterval(() => { void refreshAll(); }, 5000);
  } else if (timer) {
    clearInterval(timer);
    timer = null;
  }
}

onMounted(() => {
  void refreshAll();
});
onUnmounted(() => {
  if (timer) clearInterval(timer);
});

// =============================================================================
// 工具：格式化 + 徽章
// =============================================================================
function formatBytes(bytes?: number): string {
  const b = bytes ?? 0;
  if (!b || b <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(b) / Math.log(1024)));
  return `${(b / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatUptime(secs?: number): string {
  const s = secs ?? 0;
  if (s < 60) return `${s}秒`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}分`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}小时${m % 60}分`;
  const d = Math.floor(h / 24);
  return `${d}天${h % 24}小时`;
}

function memPercent(m: SystemMetrics): number {
  if (!m.mem_total_bytes) return 0;
  return Math.round(((m.mem_used_bytes ?? 0) / m.mem_total_bytes) * 100);
}
function diskPercent(m: SystemMetrics): number {
  if (!m.disk_total_bytes) return 0;
  return Math.round(((m.disk_used_bytes ?? 0) / m.disk_total_bytes) * 100);
}
function cpuPercent(m: SystemMetrics): number {
  return Math.round(m.cpu_usage ?? 0);
}

function serviceClass(s?: string): string {
  switch (s) {
    case 'running': return 'pill-ok';
    case 'stopped': return 'pill-err';
    default: return 'pill-muted';
  }
}
function serviceLabel(s?: string): string {
  switch (s) {
    case 'running': return '运行中';
    case 'stopped': return '已停止';
    default: return '未知';
  }
}

function alertClass(level?: string): string {
  switch (level) {
    case 'critical': return 'pill-err';
    case 'warning': return 'pill-warn';
    case 'info': return 'pill-blue';
    default: return 'pill-muted';
  }
}
function alertLabel(level?: string): string {
  switch (level) {
    case 'critical': return '严重';
    case 'warning': return '警告';
    case 'info': return '信息';
    default: return level ?? '—';
  }
}

function poolClass(state?: string): string {
  switch (state) {
    case 'ONLINE': return 'pill-ok';
    case 'DEGRADED': return 'pill-warn';
    case 'OFFLINE':
    case 'FAULTED':
      return 'pill-err';
    default: return 'pill-muted';
  }
}

function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该监控接口';
  }
  return m;
}

const unackedAlerts = computed(() => alerts.value.filter((a) => !a.acked));
const cpuColor = computed(() => {
  const c = cpuPercent(metrics.value);
  if (c >= 90) return '#C7162B';
  if (c >= 70) return '#F99B11';
  return '#0E8420';
});
const memColor = computed(() => {
  const c = memPercent(metrics.value);
  if (c >= 90) return '#C7162B';
  if (c >= 70) return '#F99B11';
  return '#0E8420';
});
const diskColor = computed(() => {
  const c = diskPercent(metrics.value);
  if (c >= 90) return '#C7162B';
  if (c >= 70) return '#F99B11';
  return '#0E8420';
});
</script>

<template>
  <div class="monitor-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">系统监控</h2>
        <div class="page-sub muted">
          {{ metrics.hostname ?? '—' }} · 内核 {{ metrics.kernel_version ?? '—' }}
        </div>
      </div>
      <div class="head-actions">
        <button
          class="btn btn-small"
          :class="{ 'btn-primary': autoRefresh }"
          @click="toggleAutoRefresh"
        >{{ autoRefresh ? '⏸ 停止刷新' : '▶ 自动刷新' }}</button>
        <button class="btn btn-small btn-primary" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
    </div>

    <div v-if="error" class="error-box">{{ error }}</div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 实时指标卡网格 -->
    <section class="metric-grid">
      <div class="card metric-card">
        <div class="metric-label">CPU 使用率</div>
        <div class="metric-value">{{ cpuPercent(metrics) }}<span class="unit">%</span></div>
        <div class="metric-sub muted">{{ metrics.cpu_cores ?? 0 }} 核心</div>
        <div class="prog-bar"><div class="prog-fill" :style="{ width: cpuPercent(metrics) + '%', background: cpuColor }"></div></div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">内存</div>
        <div class="metric-value">{{ memPercent(metrics) }}<span class="unit">%</span></div>
        <div class="metric-sub muted">{{ formatBytes(metrics.mem_used_bytes) }} / {{ formatBytes(metrics.mem_total_bytes) }}</div>
        <div class="prog-bar"><div class="prog-fill" :style="{ width: memPercent(metrics) + '%', background: memColor }"></div></div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">磁盘</div>
        <div class="metric-value">{{ diskPercent(metrics) }}<span class="unit">%</span></div>
        <div class="metric-sub muted">{{ formatBytes(metrics.disk_used_bytes) }} / {{ formatBytes(metrics.disk_total_bytes) }}</div>
        <div class="prog-bar"><div class="prog-fill" :style="{ width: diskPercent(metrics) + '%', background: diskColor }"></div></div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">负载 (1/5/15m)</div>
        <div class="metric-value">{{ (metrics.load_avg ?? [0, 0, 0])[0]?.toFixed(2) ?? '0.00' }}</div>
        <div class="metric-sub muted">
          {{ (metrics.load_avg ?? [0, 0, 0])[1]?.toFixed(2) }} / {{ (metrics.load_avg ?? [0, 0, 0])[2]?.toFixed(2) }}
        </div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">网络流量</div>
        <div class="metric-value metric-sm">↓ {{ formatBytes(metrics.net_rx_bytes) }}</div>
        <div class="metric-sub muted">↑ {{ formatBytes(metrics.net_tx_bytes) }}</div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">进程数</div>
        <div class="metric-value">{{ metrics.processes ?? 0 }}</div>
        <div class="metric-sub muted">Swap {{ formatBytes(metrics.swap_used_bytes) }} / {{ formatBytes(metrics.swap_total_bytes) }}</div>
      </div>
      <div class="card metric-card">
        <div class="metric-label">运行时间</div>
        <div class="metric-value metric-sm">{{ formatUptime(metrics.uptime_secs) }}</div>
        <div class="metric-sub muted">自启动起累计</div>
      </div>
    </section>

    <!-- 服务状态 -->
    <section class="panel">
      <div class="card card-table">
        <div class="card-title">服务状态</div>
        <table class="data-table">
          <thead>
            <tr>
              <th>服务</th>
              <th class="col-center">状态</th>
              <th class="col-right">PID</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="s in services" :key="s.name ?? ''">
              <td>{{ s.name ?? '—' }}</td>
              <td class="col-center"><span class="pill" :class="serviceClass(s.status)">{{ serviceLabel(s.status) }}</span></td>
              <td class="col-right mono">{{ s.pid ?? '—' }}</td>
            </tr>
            <tr v-if="services.length === 0">
              <td colspan="3" class="empty-row muted">暂无服务数据</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- 告警列表 -->
    <section class="panel">
      <div class="card card-table">
        <div class="card-title">
          告警（{{ alerts.length }} · 未确认 {{ unackedAlerts.length }}）
        </div>
        <table class="data-table">
          <thead>
            <tr>
              <th class="col-center">级别</th>
              <th>消息</th>
              <th>来源</th>
              <th>时间</th>
              <th class="col-right">操作</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="a in alerts" :key="a.id ?? ''">
              <td class="col-center"><span class="pill" :class="alertClass(a.level)">{{ alertLabel(a.level) }}</span></td>
              <td>{{ a.message ?? '—' }}</td>
              <td class="mono">{{ a.source ?? '—' }}</td>
              <td class="muted">{{ a.timestamp ?? '—' }}</td>
              <td class="col-right">
                <span v-if="a.acked" class="pill pill-muted">已确认</span>
                <button v-else class="btn btn-small" @click="ackAlert(String(a.id ?? ''))">确认</button>
              </td>
            </tr>
            <tr v-if="alerts.length === 0">
              <td colspan="5" class="empty-row muted">暂无告警</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- ZFS 池状态 -->
    <section class="panel">
      <div class="card card-table">
        <div class="card-title">ZFS 池状态</div>
        <table class="data-table">
          <thead>
            <tr>
              <th>池名</th>
              <th class="col-center">状态</th>
              <th class="col-right">大小</th>
              <th class="col-right">已分配</th>
              <th class="col-right">空闲</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="p in zpools" :key="p.name ?? ''">
              <td>{{ p.name ?? '—' }}</td>
              <td class="col-center"><span class="pill" :class="poolClass(p.state)">{{ p.state ?? '—' }}</span></td>
              <td class="col-right mono">{{ formatBytes(p.size_bytes) }}</td>
              <td class="col-right mono">{{ formatBytes(p.allocated_bytes) }}</td>
              <td class="col-right mono">{{ formatBytes(p.free_bytes) }}</td>
            </tr>
            <tr v-if="zpools.length === 0">
              <td colspan="5" class="empty-row muted">暂无 ZFS 池</td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>
  </div>
</template>

<style scoped>
.monitor-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head {
  display: flex; justify-content: space-between; align-items: center;
  gap: 12px; flex-wrap: wrap;
}
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }

.metric-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(190px, 1fr));
  gap: 14px;
}
.metric-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.metric-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.metric-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; line-height: 1.1; }
.metric-value.metric-sm { font-size: 18px; }
.metric-value .unit { font-size: 14px; font-weight: 600; color: var(--text-muted, #5E5C5F); margin-left: 2px; }
.metric-sub { font-size: 12px; }

.prog-bar { height: 6px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; margin-top: 4px; }
.prog-fill { height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.4s ease; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.card-title { padding: 12px 16px; font-size: 14px; font-weight: 600; border-bottom: 1px solid var(--border-soft, #EDEDED); color: var(--text, #2B2B2B); }
.panel { display: flex; flex-direction: column; gap: 12px; }

.data-table { width: 100%; border-collapse: collapse; font-size: 13px; }
.data-table thead th { text-align: left; padding: 10px 16px; font-weight: 600; color: var(--text-muted, #5E5C5F); border-bottom: 1px solid var(--border-soft, #EDEDED); font-size: 12px; text-transform: uppercase; letter-spacing: 0.4px; background: var(--bg-soft, #fafafa); }
.data-table tbody td { padding: 10px 16px; border-bottom: 1px solid var(--border-soft, #EDEDED); color: var(--text, #2B2B2B); }
.data-table tbody tr:last-child td { border-bottom: none; }
.data-table .col-center { text-align: center; }
.data-table .col-right { text-align: right; }
.data-table .empty-row { text-align: center; padding: 24px 16px; }
.mono { font-family: var(--mono); font-size: 12px; }

.pill {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 12px; font-weight: 600;
}
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-warn { color: #b45309; background: #fef3c7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-err { color: #b91c1c; background: #fee2e2; }

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
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
