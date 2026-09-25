<script setup lang="ts">
// =============================================================================
// Backup.vue —— 备份管理（ZFS 快照 + 备份任务）
//
// 功能：
//   1. 顶部统计卡（任务总数/运行中/完成/快照数）
//   2. 备份任务表格（名称/源/目标/模式徽章/计划/状态徽章/上次运行/大小/操作 立即执行+删除）
//   3. 创建备份任务对话框
//   4. ZFS 快照列表（名称/池/创建时间/占用/删除）+ 创建快照对话框
//
// 后端：
//   GET/POST /api/v1/backup/tasks / POST tasks/:id/run / DELETE tasks/:id
//   GET/POST /api/v1/backup/snapshots / DELETE snapshots/:name
//   GET /api/v1/backup/stats
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

interface BackupTask {
  id?: string;
  name?: string;
  source?: string;
  dest?: string;
  mode?: string;
  schedule?: string;
  status?: string;
  last_run?: string | null;
  next_run?: string | null;
  size_bytes?: number;
  created_at?: string;
  [k: string]: unknown;
}

interface Snapshot {
  name?: string;
  pool?: string;
  created_at?: string;
  used_bytes?: number;
  referenced_bytes?: number;
  [k: string]: unknown;
}

interface BackupStats {
  tasks_total?: number;
  tasks_running?: number;
  tasks_completed?: number;
  snapshots_total?: number;
  last_backup_size?: number;
  [k: string]: unknown;
}

// =============================================================================
// 数据状态
// =============================================================================
const tasks = ref<BackupTask[]>([]);
const snapshots = ref<Snapshot[]>([]);
const stats = ref<BackupStats>({
  tasks_total: 0,
  tasks_running: 0,
  tasks_completed: 0,
  snapshots_total: 0,
  last_backup_size: 0,
});
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadTasks(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.backupTasks();
    tasks.value = Array.isArray(raw) ? (raw as BackupTask[]) : [];
  } catch (e) {
    tasks.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadSnapshots(): Promise<void> {
  try {
    const raw = await endpoints.backupSnapshots();
    snapshots.value = Array.isArray(raw) ? (raw as Snapshot[]) : [];
  } catch {
    snapshots.value = [];
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.backupStats();
    stats.value = (raw ?? stats.value) as BackupStats;
  } catch {
    /* 统计非关键 */
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadTasks(), loadSnapshots(), loadStats()]);
}

// =============================================================================
// 任务操作：立即执行 / 删除
// =============================================================================
const busyTaskId = ref<string>('');

async function runTask(id: string): Promise<void> {
  busyTaskId.value = id;
  msg.value = null;
  try {
    await endpoints.runBackupTask(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '任务已触发执行' };
  } catch (e) {
    msg.value = { kind: 'err', text: '执行失败：' + friendlyError(e) };
  } finally {
    busyTaskId.value = '';
  }
}

async function deleteTask(id: string): Promise<void> {
  if (!window.confirm('确定删除该备份任务？')) return;
  busyTaskId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteBackupTask(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busyTaskId.value = '';
  }
}

// =============================================================================
// 快照操作：删除
// =============================================================================
const busySnap = ref<string>('');

async function deleteSnapshot(name: string): Promise<void> {
  if (!window.confirm(`确定删除快照 ${name}？此操作不可撤销。`)) return;
  busySnap.value = name;
  msg.value = null;
  try {
    const raw = (await endpoints.deleteBackupSnapshot(name)) as { ok?: boolean; warning?: string };
    if (raw && raw.warning) {
      msg.value = { kind: 'err', text: '删除未生效：' + raw.warning };
    } else {
      msg.value = { kind: 'ok', text: '快照已删除' };
    }
    await Promise.all([loadSnapshots(), loadStats()]);
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busySnap.value = '';
  }
}

// =============================================================================
// 创建备份任务对话框
// =============================================================================
const showCreateTask = ref(false);
const taskForm = ref({ name: '', source: '', dest: '', mode: 'full', schedule: 'manual' });
const taskSubmitting = ref(false);

function openCreateTask(): void {
  taskForm.value = { name: '', source: 'tank/data', dest: '/backup/tank-data', mode: 'full', schedule: 'manual' };
  msg.value = null;
  showCreateTask.value = true;
}
function closeCreateTask(): void {
  if (taskSubmitting.value) return;
  showCreateTask.value = false;
}
async function submitCreateTask(): Promise<void> {
  const name = taskForm.value.name.trim();
  const source = taskForm.value.source.trim();
  const dest = taskForm.value.dest.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写任务名称' }; return; }
  if (!source) { msg.value = { kind: 'err', text: '请填写源路径/dataset' }; return; }
  if (!dest) { msg.value = { kind: 'err', text: '请填写目标' }; return; }
  taskSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createBackupTask({
      name,
      source,
      dest,
      mode: taskForm.value.mode,
      schedule: taskForm.value.schedule,
    });
    showCreateTask.value = false;
    await refreshAll();
    msg.value = { kind: 'ok', text: '任务已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    taskSubmitting.value = false;
  }
}

// =============================================================================
// 创建快照对话框
// =============================================================================
const showCreateSnap = ref(false);
const snapForm = ref({ pool: 'tank/data', name: '' });
const snapSubmitting = ref(false);

function openCreateSnap(): void {
  snapForm.value = { pool: 'tank/data', name: '' };
  msg.value = null;
  showCreateSnap.value = true;
}
function closeCreateSnap(): void {
  if (snapSubmitting.value) return;
  showCreateSnap.value = false;
}
async function submitCreateSnap(): Promise<void> {
  const pool = snapForm.value.pool.trim();
  const name = snapForm.value.name.trim();
  if (!pool) { msg.value = { kind: 'err', text: '请填写 pool/dataset' }; return; }
  if (!name) { msg.value = { kind: 'err', text: '请填写快照名' }; return; }
  snapSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    const raw = (await endpoints.createBackupSnapshot({ pool, name })) as { ok?: boolean; warning?: string };
    if (raw && raw.warning) {
      msg.value = { kind: 'err', text: '创建未生效：' + raw.warning };
    } else {
      showCreateSnap.value = false;
      msg.value = { kind: 'ok', text: '快照已创建' };
    }
    await Promise.all([loadSnapshots(), loadStats()]);
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    snapSubmitting.value = false;
  }
}

// =============================================================================
// 表格列 + 工具
// =============================================================================
const taskColumns: Column<BackupTask>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'source', title: '源', accessor: (r) => r.source ?? '—' },
  { key: 'dest', title: '目标', accessor: (r) => r.dest ?? '—' },
  { key: 'mode', title: '模式', width: '90px', align: 'center', accessor: (r) => r.mode ?? '—' },
  { key: 'schedule', title: '计划', width: '80px', align: 'center', accessor: (r) => r.schedule ?? '—' },
  { key: 'status', title: '状态', width: '100px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'size_bytes', title: '大小', width: '100px', align: 'right', accessor: (r) => formatBytes(r.size_bytes ?? 0) },
  { key: 'actions', title: '操作', width: '180px', align: 'right' },
];

const snapColumns: Column<Snapshot>[] = [
  { key: 'name', title: '快照名', accessor: (r) => r.name ?? '—' },
  { key: 'pool', title: '池/Dataset', accessor: (r) => r.pool ?? '—' },
  { key: 'created_at', title: '创建时间', width: '180px', accessor: (r) => r.created_at ?? '—' },
  { key: 'used_bytes', title: '占用', width: '100px', align: 'right', accessor: (r) => formatBytes(r.used_bytes ?? 0) },
  { key: 'actions', title: '操作', width: '100px', align: 'right' },
];

function modeClass(m?: string): string {
  switch (m) {
    case 'full': return 'pill-blue';
    case 'incremental': return 'pill-cyan';
    case 'snapshot': return 'pill-purple';
    default: return 'pill-muted';
  }
}
function modeLabel(m?: string): string {
  switch (m) {
    case 'full': return '全量';
    case 'incremental': return '增量';
    case 'snapshot': return '快照';
    default: return m ?? '—';
  }
}
function statusClass(s?: string): string {
  switch (s) {
    case 'running': return 'pill-blue';
    case 'completed': return 'pill-ok';
    case 'failed': return 'pill-err';
    case 'idle': return 'pill-muted';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'running': return '运行中';
    case 'completed': return '已完成';
    case 'failed': return '已失败';
    case 'idle': return '空闲';
    default: return s ?? '—';
  }
}

function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该备份接口';
  }
  return m;
}

const lastBackupText = computed(() => formatBytes(stats.value.last_backup_size ?? 0));

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="backup-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">备份管理</h2>
        <div class="page-sub muted">快照、计划任务与数据回溯保护</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small btn-primary" @click="openCreateSnap">＋ 新建快照</button>
        <button class="btn btn-small btn-primary" @click="openCreateTask">＋ 新建任务</button>
      </div>
    </div>

    <!-- 统计卡 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">任务总数</div>
        <div class="stat-value">{{ stats.tasks_total ?? 0 }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">运行中</div>
        <div class="stat-value text-blue">{{ stats.tasks_running ?? 0 }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">已完成</div>
        <div class="stat-value text-ok">{{ stats.tasks_completed ?? 0 }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">快照数</div>
        <div class="stat-value">{{ stats.snapshots_total ?? 0 }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">上次备份大小</div>
        <div class="stat-value stat-sm">{{ lastBackupText }}</div>
      </div>
    </section>

    <div v-if="error" class="error-box">{{ error }}</div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 备份任务表格 -->
    <section class="panel">
      <div class="card card-table">
        <div class="card-title">备份任务</div>
        <DataTable
          :columns="taskColumns"
          :rows="tasks"
          :loading="loading"
          empty-text="暂无备份任务，点击右上角「新建任务」。"
        >
          <template #cell-mode="{ row }">
            <span class="pill" :class="modeClass(row.mode)">{{ modeLabel(row.mode) }}</span>
          </template>
          <template #cell-status="{ row }">
            <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
          </template>
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small"
              :disabled="busyTaskId === row.id || row.status === 'running'"
              @click.stop="runTask(String(row.id ?? ''))"
            >立即执行</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="busyTaskId === row.id"
              @click.stop="deleteTask(String(row.id ?? ''))"
            >删除</button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- ZFS 快照列表 -->
    <section class="panel">
      <div class="card card-table">
        <div class="card-title">ZFS 快照</div>
        <DataTable
          :columns="snapColumns"
          :rows="snapshots"
          empty-text="暂无快照，点击右上角「新建快照」。"
        >
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small btn-danger"
              :disabled="busySnap === row.name"
              @click.stop="deleteSnapshot(String(row.name ?? ''))"
            >删除</button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- ============ 新建备份任务对话框 ============ -->
    <div v-if="showCreateTask" class="modal-backdrop" @click.self="closeCreateTask">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-task-title">
        <div class="modal-head">
          <h3 id="create-task-title">新建备份任务</h3>
          <button class="modal-close" type="button" :disabled="taskSubmitting" @click="closeCreateTask">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateTask">
          <div class="field">
            <label for="bk-name">任务名称</label>
            <input id="bk-name" v-model="taskForm.name" type="text" placeholder="tank/data 全量备份" :disabled="taskSubmitting" />
          </div>
          <div class="field">
            <label for="bk-src">源（路径 / dataset）</label>
            <input id="bk-src" v-model="taskForm.source" type="text" placeholder="tank/data" :disabled="taskSubmitting" />
          </div>
          <div class="field">
            <label for="bk-dst">目标</label>
            <input id="bk-dst" v-model="taskForm.dest" type="text" placeholder="/backup/tank-data 或 s3://bucket" :disabled="taskSubmitting" />
          </div>
          <div class="field-row">
            <div class="field">
              <label for="bk-mode">模式</label>
              <select id="bk-mode" v-model="taskForm.mode" :disabled="taskSubmitting">
                <option value="full">全量</option>
                <option value="incremental">增量</option>
                <option value="snapshot">快照</option>
              </select>
            </div>
            <div class="field">
              <label for="bk-sched">计划</label>
              <select id="bk-sched" v-model="taskForm.schedule" :disabled="taskSubmitting">
                <option value="manual">手动</option>
                <option value="daily">每日</option>
                <option value="weekly">每周</option>
              </select>
            </div>
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="taskSubmitting" @click="closeCreateTask">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="taskSubmitting">
              {{ taskSubmitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 新建快照对话框 ============ -->
    <div v-if="showCreateSnap" class="modal-backdrop" @click.self="closeCreateSnap">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-snap-title">
        <div class="modal-head">
          <h3 id="create-snap-title">新建 ZFS 快照</h3>
          <button class="modal-close" type="button" :disabled="snapSubmitting" @click="closeCreateSnap">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateSnap">
          <div class="field">
            <label for="snap-pool">Pool / Dataset</label>
            <input id="snap-pool" v-model="snapForm.pool" type="text" placeholder="tank/data" :disabled="snapSubmitting" />
          </div>
          <div class="field">
            <label for="snap-name">快照名</label>
            <input id="snap-name" v-model="snapForm.name" type="text" placeholder="auto-daily-20260808" :disabled="snapSubmitting" />
            <small class="muted">将执行 <code>zfs snapshot {{ snapForm.pool }}@{{ snapForm.name || '&lt;name&gt;' }}</code></small>
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="snapSubmitting" @click="closeCreateSnap">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="snapSubmitting">
              {{ snapSubmitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>
  </div>
</template>

<style scoped>
.backup-page {
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

.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
  gap: 14px;
}
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-sm { font-size: 20px; }
.text-blue { color: #C7421A; }
.text-ok { color: #15803d; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.card-title { padding: 12px 16px; font-size: 14px; font-weight: 600; border-bottom: 1px solid var(--border-soft, #EDEDED); color: var(--text, #2B2B2B); }
.panel { display: flex; flex-direction: column; gap: 12px; }

.pill {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 12px; font-weight: 600;
}
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-cyan { color: #0e7490; background: #cffafe; }
.pill-purple { color: #7e22ce; background: #f3e8ff; }
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
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }

.field { display: flex; flex-direction: column; gap: 4px; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff); color: var(--text, #2B2B2B); }
.field small { font-size: 12px; color: var(--text-muted, #5E5C5F); }
.field small code { background: rgba(0,0,0,0.05); padding: 1px 5px; border-radius: 4px; font-size: 11px; }
.field-row { display: flex; gap: 12px; }
.field-row .field { flex: 1; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(520px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
</style>
