<script setup lang="ts">
// =============================================================================
// CloudSync.vue —— 云同步
//
// 功能：
//   1. 顶部统计卡（任务总数/同步中/提供商/已同步总量）
//   2. 任务表格（名称/本地路径/provider 徽章/远端路径/同步模式/状态徽章/已同步文件/操作）
//   3. 新建任务对话框
//
// 后端：GET/POST /api/v1/cloudsync/tasks / sync / pause / resume / DELETE / stats
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

interface SyncTask {
  id?: string;
  name?: string;
  local_path?: string;
  remote_provider?: string;
  remote_path?: string;
  sync_mode?: string;
  status?: string;
  last_sync_at?: string | null;
  files_synced?: number;
  total_size_bytes?: number;
  [k: string]: unknown;
}

// =============================================================================
// 列表 + 统计
// =============================================================================
const tasks = ref<SyncTask[]>([]);
const stats = ref<{ total_tasks: number; syncing: number; providers_used: string[]; total_synced_bytes: number }>({
  total_tasks: 0, syncing: 0, providers_used: [], total_synced_bytes: 0,
});
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadTasks(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.syncTasks();
    tasks.value = Array.isArray(raw) ? (raw as SyncTask[]) : [];
  } catch (e) {
    tasks.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.syncStats();
    stats.value = (raw ?? stats.value) as typeof stats.value;
  } catch {
    /* 统计非关键 */
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadTasks(), loadStats()]);
}

// =============================================================================
// 操作
// =============================================================================
const busyId = ref<string>('');

async function triggerSync(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.triggerSync(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '已触发同步' };
  } catch (e) {
    msg.value = { kind: 'err', text: '同步失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function pause(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.pauseSync(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '暂停失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function resume(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.resumeSync(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '继续失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function remove(id: string): Promise<void> {
  if (!window.confirm('确定删除该同步任务？')) return;
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteSyncTask(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 新建任务对话框
// =============================================================================
const showCreate = ref(false);
const createForm = ref({
  name: '',
  local_path: '/tank',
  remote_provider: 's3',
  remote_path: '',
  sync_mode: 'one_way_up',
});
const createSubmitting = ref(false);

const PROVIDERS = ['s3', 'onedrive', 'google', 'webdav', 'aliyun'];
const MODES = [
  { v: 'one_way_up', label: '单向上传' },
  { v: 'one_way_down', label: '单向下传' },
  { v: 'two_way', label: '双向同步' },
];

function openCreate(): void {
  createForm.value = {
    name: '', local_path: '/tank', remote_provider: 's3', remote_path: '', sync_mode: 'one_way_up',
  };
  msg.value = null;
  showCreate.value = true;
}
function closeCreate(): void {
  if (createSubmitting.value) return;
  showCreate.value = false;
}
async function submitCreate(): Promise<void> {
  const name = createForm.value.name.trim();
  const localPath = createForm.value.local_path.trim();
  const remotePath = createForm.value.remote_path.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!localPath) { msg.value = { kind: 'err', text: '请填写本地路径' }; return; }
  if (!remotePath) { msg.value = { kind: 'err', text: '请填写远端路径' }; return; }
  createSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createSyncTask({
      name,
      local_path: localPath,
      remote_provider: createForm.value.remote_provider,
      remote_path: remotePath,
      sync_mode: createForm.value.sync_mode,
    });
    showCreate.value = false;
    await refreshAll();
    msg.value = { kind: 'ok', text: '任务已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    createSubmitting.value = false;
  }
}

// =============================================================================
// 表格列 + 工具
// =============================================================================
const columns: Column<SyncTask>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'local_path', title: '本地路径', accessor: (r) => r.local_path ?? '—' },
  { key: 'remote_provider', title: '云', width: '110px', align: 'center',
    accessor: (r) => r.remote_provider ?? '—' },
  { key: 'remote_path', title: '远端路径', accessor: (r) => r.remote_path ?? '—' },
  { key: 'sync_mode', title: '模式', width: '110px', align: 'center',
    accessor: (r) => modeLabel(r.sync_mode) },
  { key: 'status', title: '状态', width: '100px', align: 'center',
    accessor: (r) => r.status ?? '—' },
  { key: 'files_synced', title: '已同步', width: '110px', align: 'right',
    accessor: (r) => (r.files_synced ?? 0).toLocaleString() },
  { key: 'actions', title: '操作', width: '230px', align: 'right' },
];

function providerClass(p?: string): string {
  switch (p) {
    case 's3': return 'pv-s3';
    case 'onedrive': return 'pv-onedrive';
    case 'google': return 'pv-google';
    case 'webdav': return 'pv-webdav';
    case 'aliyun': return 'pv-aliyun';
    default: return 'pv-muted';
  }
}
function providerLabel(p?: string): string {
  switch (p) {
    case 's3': return 'S3';
    case 'onedrive': return 'OneDrive';
    case 'google': return 'Google';
    case 'webdav': return 'WebDAV';
    case 'aliyun': return '阿里云';
    default: return p ?? '—';
  }
}
function statusClass(s?: string): string {
  switch (s) {
    case 'syncing': return 'pill-blue';
    case 'idle': return 'pill-ok';
    case 'paused': return 'pill-warn';
    case 'error': return 'pill-err';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'syncing': return '同步中';
    case 'idle': return '空闲';
    case 'paused': return '已暂停';
    case 'error': return '错误';
    default: return s ?? '—';
  }
}
function modeLabel(m?: string): string {
  switch (m) {
    case 'one_way_up': return '单向上传';
    case 'one_way_down': return '单向下传';
    case 'two_way': return '双向';
    default: return m ?? '—';
  }
}
function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该云同步接口';
  }
  return m;
}

const totalSyncedText = computed(() => formatBytes(stats.value.total_synced_bytes));
const providersText = computed(() => {
  const list = stats.value.providers_used ?? [];
  return list.length ? list.map((p) => providerLabel(p)).join('、') : '—';
});

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="cloudsync-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">云同步</h2>
        <div class="page-sub muted">管理本地与云端之间的同步任务</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small btn-primary" @click="openCreate">＋ 新建任务</button>
      </div>
    </div>

    <!-- 统计卡 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">同步任务</div>
        <div class="stat-value">{{ stats.total_tasks }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">同步中</div>
        <div class="stat-value">{{ stats.syncing }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">提供商</div>
        <div class="stat-value stat-value-sm">{{ providersText }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">已同步总量</div>
        <div class="stat-value">{{ totalSyncedText }}</div>
      </div>
    </section>

    <div v-if="error" class="error-box">{{ error }}</div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 任务表格 -->
    <section class="panel">
      <div class="card card-table">
        <DataTable
          :columns="columns"
          :rows="tasks"
          :loading="loading"
          empty-text="暂无同步任务，点击右上角「新建任务」。"
        >
          <template #cell-remote_provider="{ row }">
            <span class="pv" :class="providerClass(row.remote_provider)">{{ providerLabel(row.remote_provider) }}</span>
          </template>
          <template #cell-status="{ row }">
            <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
          </template>
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="triggerSync(String(row.id ?? ''))"
            >立即同步</button>
            <button
              v-if="row.status === 'paused' || row.status === 'error'"
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="resume(String(row.id ?? ''))"
            >继续</button>
            <button
              v-else
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="pause(String(row.id ?? ''))"
            >暂停</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="busyId === row.id"
              @click.stop="remove(String(row.id ?? ''))"
            >删除</button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- ============ 新建任务对话框 ============ -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-sync-title">
        <div class="modal-head">
          <h3 id="create-sync-title">新建同步任务</h3>
          <button class="modal-close" type="button" :disabled="createSubmitting" @click="closeCreate">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreate">
          <div class="field">
            <label for="sync-name">名称</label>
            <input id="sync-name" v-model="createForm.name" type="text" placeholder="照片备份" :disabled="createSubmitting" />
          </div>
          <div class="field-row">
            <div class="field">
              <label for="sync-local">本地路径</label>
              <input id="sync-local" v-model="createForm.local_path" type="text" placeholder="/tank/photo" :disabled="createSubmitting" />
            </div>
            <div class="field">
              <label for="sync-remote">远端路径</label>
              <input id="sync-remote" v-model="createForm.remote_path" type="text" placeholder="s3://bucket/path" :disabled="createSubmitting" />
            </div>
          </div>
          <div class="field-row">
            <div class="field">
              <label for="sync-prov">云提供商</label>
              <select id="sync-prov" v-model="createForm.remote_provider" :disabled="createSubmitting">
                <option v-for="p in PROVIDERS" :key="p" :value="p">{{ providerLabel(p) }}</option>
              </select>
            </div>
            <div class="field">
              <label for="sync-mode">同步模式</label>
              <select id="sync-mode" v-model="createForm.sync_mode" :disabled="createSubmitting">
                <option v-for="m in MODES" :key="m.v" :value="m.v">{{ m.label }}</option>
              </select>
            </div>
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createSubmitting" @click="closeCreate">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="createSubmitting">
              {{ createSubmitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>
  </div>
</template>

<style scoped>
.cloudsync-page {
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

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-value-sm { font-size: 15px; line-height: 1.3; padding-top: 4px; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.panel { display: flex; flex-direction: column; gap: 12px; }

/* provider 彩色徽章 */
.pv { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pv-s3 { color: #C7421A; background: #dbeafe; }
.pv-onedrive { color: #0e7490; background: #cffafe; }
.pv-google { color: #b45309; background: #fef3c7; }
.pv-webdav { color: #6d28d9; background: #ede9fe; }
.pv-aliyun { color: #be185d; background: #fce7f3; }
.pv-muted { color: #6b7280; background: #f3f4f6; }

.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
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
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }

.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff); }
.field-row { display: flex; gap: 12px; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(560px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
</style>
