<script setup lang="ts">
// =============================================================================
// Containers.vue —— 容器管理
//
// 功能：
//   1. 顶部统计卡（容器数/运行中/镜像数）
//   2. 标签页：容器列表 / 镜像列表
//   3. 容器表（名称/镜像/状态徽章/CPU/内存/端口/操作 启动/停止/重启/删除）+ 创建对话框
//   4. 镜像表（名称:tag/大小/创建时间）
//
// 后端：GET /api/v1/containers/list|images|stats / POST create|start|stop|restart / DELETE :id
// =============================================================================
import { onMounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

interface Container {
  id?: string;
  name?: string;
  image?: string;
  status?: string;
  ports?: string[];
  created_at?: string;
  cpu_percent?: number;
  mem_usage_mb?: number;
  [k: string]: unknown;
}
interface Image {
  id?: string;
  name?: string;
  tag?: string;
  size_bytes?: number;
  created_at?: string;
  [k: string]: unknown;
}

const tab = ref<'containers' | 'images'>('containers');

// =============================================================================
// 列表 + 统计
// =============================================================================
const containers = ref<Container[]>([]);
const images = ref<Image[]>([]);
const stats = ref<{ container_count: number; running: number; image_count: number }>({
  container_count: 0, running: 0, image_count: 0,
});
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadContainers(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.containerList();
    containers.value = Array.isArray(raw) ? (raw as Container[]) : [];
  } catch (e) {
    containers.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}
async function loadImages(): Promise<void> {
  try {
    const raw = await endpoints.containerImages();
    images.value = Array.isArray(raw) ? (raw as Image[]) : [];
  } catch {
    /* 静默 */
  }
}
async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.containerStats();
    stats.value = (raw ?? stats.value) as typeof stats.value;
  } catch {
    /* 统计非关键 */
  }
}
async function refreshAll(): Promise<void> {
  await Promise.all([loadContainers(), loadImages(), loadStats()]);
}

// =============================================================================
// 容器操作
// =============================================================================
const busyId = ref<string>('');

async function start(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try { await endpoints.startContainer(id); await refreshAll(); }
  catch (e) { msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) }; }
  finally { busyId.value = ''; }
}
async function stop(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try { await endpoints.stopContainer(id); await refreshAll(); }
  catch (e) { msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) }; }
  finally { busyId.value = ''; }
}
async function restart(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try { await endpoints.restartContainer(id); await refreshAll(); }
  catch (e) { msg.value = { kind: 'err', text: '重启失败：' + friendlyError(e) }; }
  finally { busyId.value = ''; }
}
async function remove(id: string): Promise<void> {
  if (!window.confirm('确定删除该容器？该操作不可撤销。')) return;
  busyId.value = id;
  msg.value = null;
  try { await endpoints.deleteContainer(id); await refreshAll(); msg.value = { kind: 'ok', text: '已删除' }; }
  catch (e) { msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) }; }
  finally { busyId.value = ''; }
}

// =============================================================================
// 创建对话框
// =============================================================================
const showCreate = ref(false);
const createForm = ref({ name: '', image: '' });
const createSubmitting = ref(false);

function openCreate(): void {
  createForm.value = { name: '', image: '' };
  msg.value = null;
  showCreate.value = true;
}
function closeCreate(): void {
  if (createSubmitting.value) return;
  showCreate.value = false;
}
async function submitCreate(): Promise<void> {
  const name = createForm.value.name.trim();
  const image = createForm.value.image.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写容器名' }; return; }
  if (!image) { msg.value = { kind: 'err', text: '请填写镜像' }; return; }
  createSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createContainer(name, image);
    showCreate.value = false;
    await refreshAll();
    msg.value = { kind: 'ok', text: '容器已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    createSubmitting.value = false;
  }
}

// =============================================================================
// 表格列 + 工具
// =============================================================================
const containerColumns: Column<Container>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'image', title: '镜像', accessor: (r) => r.image ?? '—' },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'cpu_percent', title: 'CPU', width: '80px', align: 'right', accessor: (r) => (r.cpu_percent ?? 0).toFixed(2) + '%' },
  { key: 'mem_usage_mb', title: '内存', width: '100px', align: 'right', accessor: (r) => formatMem(r.mem_usage_mb ?? 0) },
  { key: 'ports', title: '端口', accessor: (r) => (r.ports ?? []).join(', ') || '—' },
  { key: 'actions', title: '操作', width: '240px', align: 'right' },
];
const imageColumns: Column<Image>[] = [
  { key: 'name', title: '镜像', accessor: (r) => `${r.name ?? '—'}:${r.tag ?? 'latest'}` },
  { key: 'size_bytes', title: '大小', width: '120px', align: 'right', accessor: (r) => formatBytes(r.size_bytes ?? 0) },
  { key: 'created_at', title: '创建时间', width: '180px', accessor: (r) => formatDate(r.created_at) },
];

function statusClass(s?: string): string {
  switch (s) {
    case 'running': return 'pill-ok';
    case 'paused': return 'pill-warn';
    case 'stopped': return 'pill-muted';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'running': return '运行中';
    case 'paused': return '已暂停';
    case 'stopped': return '已停止';
    default: return s ?? '—';
  }
}
function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function formatMem(mb: number): string {
  if (mb <= 0) return '—';
  return mb >= 1024 ? `${(mb / 1024).toFixed(2)} GiB` : `${mb.toFixed(1)} MiB`;
}
function formatDate(iso?: string): string {
  if (!iso) return '—';
  return iso.replace('T', ' ').slice(0, 16);
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该容器接口';
  }
  return m;
}

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="containers-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">容器管理</h2>
        <div class="page-sub muted">管理容器与镜像（演示数据）</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small btn-primary" @click="openCreate">＋ 创建容器</button>
      </div>
    </div>

    <!-- 统计卡 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">容器</div>
        <div class="stat-value">{{ stats.container_count }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">运行中</div>
        <div class="stat-value">{{ stats.running }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">镜像</div>
        <div class="stat-value">{{ stats.image_count }}</div>
      </div>
    </section>

    <div v-if="error" class="error-box">{{ error }}</div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 标签页 -->
    <section class="panel">
      <div class="tab-bar">
        <button class="tab" :class="{ active: tab === 'containers' }" @click="tab = 'containers'">容器列表</button>
        <button class="tab" :class="{ active: tab === 'images' }" @click="tab = 'images'">镜像列表</button>
      </div>

      <div v-show="tab === 'containers'" class="card card-table">
        <DataTable
          :columns="containerColumns"
          :rows="containers"
          :loading="loading"
          empty-text="暂无容器，点击右上角「创建容器」。"
        >
          <template #cell-status="{ row }">
            <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
          </template>
          <template #cell-actions="{ row }">
            <button
              v-if="row.status !== 'running'"
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="start(String(row.id ?? ''))"
            >启动</button>
            <button
              v-if="row.status === 'running'"
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="stop(String(row.id ?? ''))"
            >停止</button>
            <button
              v-if="row.status === 'running'"
              class="btn btn-small"
              :disabled="busyId === row.id"
              @click.stop="restart(String(row.id ?? ''))"
            >重启</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="busyId === row.id"
              @click.stop="remove(String(row.id ?? ''))"
            >删除</button>
          </template>
        </DataTable>
      </div>

      <div v-show="tab === 'images'" class="card card-table">
        <DataTable
          :columns="imageColumns"
          :rows="images"
          :loading="loading"
          empty-text="暂无镜像。"
        />
      </div>
    </section>

    <!-- ============ 创建容器对话框 ============ -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-ctn-title">
        <div class="modal-head">
          <h3 id="create-ctn-title">创建容器</h3>
          <button class="modal-close" type="button" :disabled="createSubmitting" @click="closeCreate">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreate">
          <div class="field">
            <label for="ctn-name">容器名</label>
            <input id="ctn-name" v-model="createForm.name" type="text" placeholder="例如 my-app" :disabled="createSubmitting" />
          </div>
          <div class="field">
            <label for="ctn-image">镜像</label>
            <input id="ctn-image" v-model="createForm.image" type="text" placeholder="例如 nginx:1.27" :disabled="createSubmitting" />
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
.containers-page {
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

.card { background: var(--bg-card, #fff); border: 1px solid var(--border, #D9D9D9); border-radius: var(--radius-md, 12px); box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1)); }
.card-table { padding: 0; overflow: hidden; }
.panel { display: flex; flex-direction: column; gap: 12px; }

.tab-bar { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.tab { background: transparent; border: none; padding: 8px 16px; font-family: inherit; font-size: 14px; color: var(--text-muted, #5E5C5F); cursor: pointer; border-bottom: 2px solid transparent; }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); font-weight: 600; }
.tab:hover:not(.active) { color: var(--text, #2B2B2B); }

.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-warn { color: #b45309; background: #fef3c7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }

.btn { padding: 6px 14px; border-radius: var(--radius-sm, 8px); border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff); color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit; transition: background 0.15s ease; }
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }

.field { display: flex; flex-direction: column; gap: 4px; }
.field label { font-size: 13px; font-weight: 500; }
.field input { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(480px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
</style>
