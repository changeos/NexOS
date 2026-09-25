<script setup lang="ts">
// =============================================================================
// Downloads.vue —— 下载中心（双 Tab：aria2 公网下载 + P2P 网状传输）
//
// Tab 1「HTTP/BT 下载」（aria2，公网）：
//   1. 统计卡（总任务/下载中/已完成/总大小）
//   2. 任务表格（名称/URL/类型徽章/状态徽章/进度条/速度/操作 暂停/继续/取消/删除）
//   3. 新建任务对话框（URL + 保存路径；磁力链/ed2k/直链/本地种子路径）
//   4. 上传种子（.torrent → base64-JSON → POST /downloads/torrent）
//
// Tab 2「P2P 传输」（transfer 组件，经 os-p2p 叠加层——打洞/中继，不依赖公网 IP）：
//   1. 统计卡（任务/进行中/已发布清单/做种供出）
//   2. 传输任务表（来源徽章 🔗、进度条=块位图、速度、源节点短 ID、暂停/继续/取消）
//   3. 已发布清单面板（sha256/transfer_id + 复制按钮 + 下架）
//   4. 发布对话框（本地路径「发布为可传输」→ 得 sha256 分享给其他节点）
//   5. 拉取对话框（粘贴 sha256 / tr_ transfer_id → fetch）
//
// 后端：GET /api/v1/downloads/tasks|stats + POST tasks/torrent/:id/{pause,resume,cancel}
//       GET /api/v1/transfer/tasks|manifests|stats + POST publish/fetch/:id/{pause,resume,cancel}
//       + DELETE /api/v1/transfer/manifests/:id
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

interface DownloadTask {
  id?: string;
  name?: string;
  url?: string;
  save_path?: string;
  status?: string;
  /** 任务类型：http（直链）/ magnet（磁力链）/ torrent（种子）/ ed2k */
  type?: string;
  progress?: number;
  size_bytes?: number;
  downloaded_bytes?: number;
  speed_bytes_sec?: number;
  created_at?: string;
  [k: string]: unknown;
}

/** P2P 传输任务（os-p2p TransferTaskView；status 词与 downloads 对齐） */
interface TransferTask {
  id: string;
  name: string;
  sha256: string;
  transfer_id?: string;
  phase?: string;
  status?: string;
  size_bytes?: number;
  done_bytes?: number;
  progress?: number;
  chunks_total?: number;
  chunks_done?: number;
  speed_bytes_sec?: number;
  sources?: string[];
  dest_path?: string;
  error?: string;
  [k: string]: unknown;
}

/** 已发布清单条目 */
interface TransferManifestEntry {
  manifest?: { transfer_id?: string; sha256?: string; name?: string; size?: number; chunks?: number; published_at?: number };
  path?: string;
}

// =============================================================================
// Tab 切换（web = aria2 公网下载；p2p = transfer 网状分发）
// =============================================================================
const { t } = useI18n();

const activeTab = ref<'web' | 'p2p'>('web');

// =============================================================================
// Tab 1：aria2 任务列表 + 统计
// =============================================================================
const tasks = ref<DownloadTask[]>([]);
const stats = ref<{ total: number; downloading: number; completed: number; total_size_bytes: number }>({
  total: 0, downloading: 0, completed: 0, total_size_bytes: 0,
});
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadTasks(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.downloadTasks();
    tasks.value = Array.isArray(raw) ? (raw as DownloadTask[]) : [];
  } catch (e) {
    tasks.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.downloadStats();
    stats.value = (raw ?? stats.value) as typeof stats.value;
  } catch {
    /* 统计非关键 */
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadTasks(), loadStats(), loadTransferAll()]);
}

// =============================================================================
// Tab 1：状态切换 + 删除
// =============================================================================
const busyId = ref<string>('');

async function pause(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.pauseDownload(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.pauseFailed') + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function resume(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.resumeDownload(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.resumeFailed') + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function cancel(id: string): Promise<void> {
  if (!window.confirm(t('downloads.confirmCancel'))) return;
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.cancelDownload(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.cancelFailed') + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}
async function remove(id: string): Promise<void> {
  if (!window.confirm(t('downloads.confirmRemove'))) return;
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteDownload(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: t('downloads.removed') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.removeFailed') + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// Tab 1：新建任务对话框（url 模式：直链/磁力链/ed2k/本地种子路径；torrent 模式：上传 .torrent）
// =============================================================================
const showCreate = ref(false);
/** 对话框模式：url = 填 URL；torrent = 上传种子文件 */
const createMode = ref<'url' | 'torrent'>('url');
const createForm = ref({ url: '', savePath: '/tank/downloads', name: '' });
/** torrent 模式选中的 .torrent 文件 */
const torrentFile = ref<File | null>(null);
const createSubmitting = ref(false);

function openCreate(mode: 'url' | 'torrent' = 'url'): void {
  createForm.value = { url: '', savePath: '/tank/downloads', name: '' };
  createMode.value = mode;
  torrentFile.value = null;
  msg.value = null;
  showCreate.value = true;
}
function closeCreate(): void {
  if (createSubmitting.value) return;
  showCreate.value = false;
}
function onTorrentPicked(e: Event): void {
  const input = e.target as HTMLInputElement;
  const f = input.files?.[0] ?? null;
  if (f && !f.name.toLowerCase().endsWith('.torrent')) {
    msg.value = { kind: 'err', text: t('downloads.needTorrentFile') };
    input.value = '';
    torrentFile.value = null;
    return;
  }
  torrentFile.value = f;
}
/** File → base64（标准字母表；分块 btoa 防大文件 String.fromCharCode 栈溢出） */
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error(t('downloads.readTorrentFailed')));
    reader.onload = () => {
      const buf = reader.result as ArrayBuffer;
      const bytes = new Uint8Array(buf);
      let binary = '';
      const CHUNK = 0x8000;
      for (let i = 0; i < bytes.length; i += CHUNK) {
        binary += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
      }
      resolve(btoa(binary));
    };
    reader.readAsArrayBuffer(file);
  });
}
async function submitCreate(): Promise<void> {
  const savePath = createForm.value.savePath.trim();
  if (!savePath) { msg.value = { kind: 'err', text: t('downloads.needSavePath') }; return; }
  const name = createForm.value.name.trim() || undefined;
  createSubmitting.value = true;
  msg.value = { kind: 'info', text: t('downloads.submitting') };
  try {
    if (createMode.value === 'torrent') {
      if (!torrentFile.value) {
        msg.value = { kind: 'err', text: t('downloads.needTorrentFile') };
        return;
      }
      const b64 = await fileToBase64(torrentFile.value);
      await endpoints.uploadTorrentDownload(torrentFile.value.name, b64, savePath, name);
    } else {
      const url = createForm.value.url.trim();
      if (!url) { msg.value = { kind: 'err', text: t('downloads.needUrl') }; return; }
      await endpoints.createDownload(url, savePath, name);
    }
    showCreate.value = false;
    await refreshAll();
    msg.value = { kind: 'ok', text: t('downloads.taskCreated') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.createFailed') + friendlyError(e) };
  } finally {
    createSubmitting.value = false;
  }
}

// =============================================================================
// Tab 2：P2P 传输任务 + 清单
// =============================================================================
const transferTasks = ref<TransferTask[]>([]);
const transferManifests = ref<TransferManifestEntry[]>([]);
const transferStats = ref<{ tasks?: number; active?: number; manifests?: number; chunks_served?: number; bytes_served?: number }>({});
const transferLoading = ref(false);
const transferBusyId = ref<string>('');

async function loadTransferAll(): Promise<void> {
  transferLoading.value = true;
  try {
    const [t, m, s] = await Promise.all([
      endpoints.transferTasks(),
      endpoints.transferManifests(),
      endpoints.transferStats(),
    ]);
    transferTasks.value = Array.isArray(t) ? (t as TransferTask[]) : [];
    const rawM = m as { manifests?: TransferManifestEntry[] } | null;
    transferManifests.value = rawM?.manifests ?? [];
    transferStats.value = (s ?? {}) as typeof transferStats.value;
  } catch {
    transferTasks.value = [];
    transferManifests.value = [];
  } finally {
    transferLoading.value = false;
  }
}

async function transferAction(id: string, action: 'pause' | 'resume' | 'cancel'): Promise<void> {
  if (action === 'cancel' && !window.confirm(t('downloads.confirmTransferCancel'))) return;
  transferBusyId.value = id;
  msg.value = null;
  try {
    if (action === 'pause') await endpoints.transferPause(id);
    else if (action === 'resume') await endpoints.transferResume(id);
    else await endpoints.transferCancel(id);
    await loadTransferAll();
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.transferActionFailed', { action: t(`downloads.${action}`) }) + friendlyError(e) };
  } finally {
    transferBusyId.value = '';
  }
}

async function unpublishManifest(id: string): Promise<void> {
  if (!window.confirm(t('downloads.confirmUnpublish'))) return;
  msg.value = null;
  try {
    await endpoints.transferUnpublish(id);
    await loadTransferAll();
    msg.value = { kind: 'ok', text: t('downloads.unpublishDone') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.unpublishFailed') + friendlyError(e) };
  }
}

/** 复制到剪贴板（发布后把 sha256 发给其他节点用户即可） */
async function copyText(text: string, label: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    msg.value = { kind: 'ok', text: t('downloads.copied', { label }) };
  } catch {
    msg.value = { kind: 'err', text: t('downloads.copyFailed') };
  }
}

// —— 发布对话框（本地路径 → 可传输清单）——
const showPublish = ref(false);
const publishForm = ref({ path: '', name: '' });
const publishSubmitting = ref(false);
const publishResult = ref<{ transfer_id: string; sha256: string } | null>(null);

function openPublish(): void {
  publishForm.value = { path: '', name: '' };
  publishResult.value = null;
  msg.value = null;
  showPublish.value = true;
}
function closePublish(): void {
  if (publishSubmitting.value) return;
  showPublish.value = false;
}
async function submitPublish(): Promise<void> {
  const path = publishForm.value.path.trim();
  if (!path) { msg.value = { kind: 'err', text: t('downloads.needPath') }; return; }
  publishSubmitting.value = true;
  msg.value = { kind: 'info', text: t('downloads.publishBusy') };
  try {
    const name = publishForm.value.name.trim() || undefined;
    const raw = await endpoints.transferPublish(path, name);
    const r = raw as { transfer_id?: string; sha256?: string };
    if (r.transfer_id && r.sha256) {
      publishResult.value = { transfer_id: r.transfer_id, sha256: r.sha256 };
      msg.value = { kind: 'ok', text: t('downloads.publishDone') };
    }
    await loadTransferAll();
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.publishFailed') + friendlyError(e) };
  } finally {
    publishSubmitting.value = false;
  }
}

// —— 拉取对话框（粘贴 sha256 / tr_ transfer_id）——
const showFetch = ref(false);
const fetchForm = ref({ key: '', name: '' });
const fetchSubmitting = ref(false);

function openFetch(): void {
  fetchForm.value = { key: '', name: '' };
  msg.value = null;
  showFetch.value = true;
}
function closeFetch(): void {
  if (fetchSubmitting.value) return;
  showFetch.value = false;
}
async function submitFetch(): Promise<void> {
  const key = fetchForm.value.key.trim();
  if (!key) { msg.value = { kind: 'err', text: t('downloads.needKey') }; return; }
  fetchSubmitting.value = true;
  msg.value = { kind: 'info', text: t('downloads.fetchStarted') };
  try {
    const name = fetchForm.value.name.trim() || undefined;
    await endpoints.transferFetch(key, name);
    showFetch.value = false;
    await loadTransferAll();
    msg.value = { kind: 'ok', text: t('downloads.fetchDone') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('downloads.fetchFailed') + friendlyError(e) };
  } finally {
    fetchSubmitting.value = false;
  }
}

// =============================================================================
// 表格列 + 工具
// =============================================================================
const columns: Column<DownloadTask>[] = [
  { key: 'name', title: t('downloads.colName'), accessor: (r) => r.name ?? r.url ?? '—' },
  { key: 'url', title: 'URL', accessor: (r) => r.url ?? '—' },
  { key: 'type', title: t('downloads.colType'), width: '90px', align: 'center',
    accessor: (r) => typeLabel(r.type) },
  { key: 'status', title: t('downloads.colStatus'), width: '100px', align: 'center',
    accessor: (r) => r.status ?? '—' },
  { key: 'progress', title: t('downloads.colProgress'), width: '180px',
    accessor: (r) => r.progress ?? 0 },
  { key: 'speed_bytes_sec', title: t('downloads.colSpeed'), width: '110px', align: 'right',
    accessor: (r) => formatSpeed(r.speed_bytes_sec ?? 0) },
  { key: 'actions', title: t('downloads.colActions'), width: '220px', align: 'right' },
];

const transferColumns: Column<TransferTask>[] = [
  { key: 'name', title: t('downloads.colName'), accessor: (r) => r.name ?? '—' },
  { key: 'sha256', title: t('downloads.colShaSources'), accessor: (r) =>
      t('downloads.shaCell', { sha: (r.sha256 ?? '').slice(0, 12), src: (r.sources ?? []).join(' ') || '—' }) },
  { key: 'status', title: t('downloads.colStatus'), width: '100px', align: 'center',
    accessor: (r) => transferStatusLabel(r.status) },
  { key: 'progress', title: t('downloads.colProgressChunks'), width: '190px',
    accessor: (r) => t('downloads.progressChunks', { progress: r.progress ?? 0, done: r.chunks_done ?? 0, total: r.chunks_total ?? 0 }) },
  { key: 'size_bytes', title: t('downloads.colSize'), width: '100px', align: 'right',
    accessor: (r) => formatBytes(r.size_bytes ?? 0) },
  { key: 'speed_bytes_sec', title: t('downloads.colSpeed'), width: '110px', align: 'right',
    accessor: (r) => formatSpeed(r.speed_bytes_sec ?? 0) },
  { key: 'actions', title: t('downloads.colActions'), width: '200px', align: 'right' },
];

/** 任务类型徽章（后端 type 字段：http/magnet/torrent/ed2k，旧任务缺省 http） */
function typeLabel(raw?: string): string {
  switch (raw) {
    case 'magnet': return t('downloads.typeMagnet');
    case 'torrent': return t('downloads.typeTorrent');
    case 'ed2k': return '⚡ ED2K';
    default: return '🌐 HTTP';
  }
}
function typeClass(t?: string): string {
  switch (t) {
    case 'magnet': return 'pill-magnet';
    case 'torrent': return 'pill-torrent';
    case 'ed2k': return 'pill-ed2k';
    default: return 'pill-muted';
  }
}

function statusClass(s?: string): string {
  switch (s) {
    case 'downloading': return 'pill-blue';
    case 'completed': return 'pill-ok';
    case 'paused': return 'pill-warn';
    case 'pending': return 'pill-muted';
    case 'error': return 'pill-err';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'downloading': return t('downloads.statusDownloading');
    case 'completed': return t('downloads.statusCompleted');
    case 'paused': return t('downloads.statusPaused');
    case 'pending': return t('downloads.statusPending');
    case 'error': return t('downloads.statusCancelled');
    default: return s ?? '—';
  }
}

/** P2P 传输状态（status 与 downloads 同词表；phase 细分查询中/已取消） */
function transferStatusLabel(s?: string, phase?: string): string {
  if (phase === 'querying') return t('downloads.statusQuerying');
  if (phase === 'cancelled') return t('downloads.statusCancelled');
  return statusLabel(s);
}

function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function formatSpeed(bps: number): string {
  if (!bps || bps <= 0) return '—';
  return `${formatBytes(bps)}/s`;
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return t('downloads.backendMissing');
  }
  if (/503|P2P 传输未启用|NEXOS_P2P_ENABLE/i.test(m)) {
    return t('downloads.p2pDisabled');
  }
  return m;
}

const totalSizeText = computed(() => formatBytes(stats.value.total_size_bytes));
const servedText = computed(() => formatBytes(transferStats.value.bytes_served ?? 0));

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="downloads-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">{{ t('downloads.title') }}</h2>
        <div class="page-sub muted">{{ t('downloads.subtitle') }}</div>
      </div>
      <div class="head-actions">
        <!-- Tab 切换 -->
        <div class="tab-switch">
          <button
            class="btn btn-small tab-btn"
            :class="{ 'tab-active': activeTab === 'web' }"
            @click="activeTab = 'web'"
          >{{ t('downloads.tabWeb') }}</button>
          <button
            class="btn btn-small tab-btn"
            :class="{ 'tab-active': activeTab === 'p2p' }"
            @click="activeTab = 'p2p'"
          >{{ t('downloads.tabP2p') }}</button>
        </div>
        <button class="btn btn-small" :disabled="loading || transferLoading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading || transferLoading }" aria-hidden="true">↻</span>
          {{ t('downloads.refresh') }}
        </button>
        <template v-if="activeTab === 'web'">
          <button class="btn btn-small" @click="openCreate('torrent')">{{ t('downloads.uploadTorrent') }}</button>
          <button class="btn btn-small btn-primary" @click="openCreate('url')">{{ t('downloads.newTask') }}</button>
        </template>
        <template v-else>
          <button class="btn btn-small" @click="openPublish">{{ t('downloads.publishFile') }}</button>
          <button class="btn btn-small btn-primary" @click="openFetch">{{ t('downloads.fetchFile') }}</button>
        </template>
      </div>
    </div>

    <!-- ============ Tab 1：aria2 公网下载 ============ -->
    <template v-if="activeTab === 'web'">
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statTotal') }}</div>
          <div class="stat-value">{{ stats.total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statDownloading') }}</div>
          <div class="stat-value">{{ stats.downloading }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statCompleted') }}</div>
          <div class="stat-value">{{ stats.completed }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statTotalSize') }}</div>
          <div class="stat-value">{{ totalSizeText }}</div>
        </div>
      </section>

      <div v-if="error" class="error-box">{{ error }}</div>

      <!-- 任务表格 -->
      <section class="panel">
        <div class="card card-table">
          <DataTable
            :columns="columns"
            :rows="tasks"
            :loading="loading"
            :empty-text="t('downloads.emptyWeb')"
          >
            <template #cell-type="{ row }">
              <span class="pill" :class="typeClass(row.type)">{{ typeLabel(row.type) }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-progress="{ row }">
              <div class="prog-wrap">
                <div class="prog-bar">
                  <div
                    class="prog-fill"
                    :class="{
                      'fill-ok': row.status === 'completed',
                      'fill-warn': row.status === 'paused',
                      'fill-err': row.status === 'error',
                    }"
                    :style="{ width: (row.progress ?? 0) + '%' }"
                  ></div>
                </div>
                <span class="prog-text">{{ row.progress ?? 0 }}%</span>
              </div>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status === 'downloading' || row.status === 'pending'"
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="pause(String(row.id ?? ''))"
              >{{ t('downloads.pause') }}</button>
              <button
                v-if="row.status === 'paused' || row.status === 'error'"
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="resume(String(row.id ?? ''))"
              >{{ t('downloads.resume') }}</button>
              <button
                v-if="row.status !== 'completed' && row.status !== 'error'"
                class="btn btn-small"
                :disabled="busyId === row.id"
                @click.stop="cancel(String(row.id ?? ''))"
              >{{ t('downloads.cancel') }}</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="busyId === row.id"
                @click.stop="remove(String(row.id ?? ''))"
              >{{ t('downloads.remove') }}</button>
            </template>
          </DataTable>
        </div>
      </section>
    </template>

    <!-- ============ Tab 2：P2P 网状传输 ============ -->
    <template v-else>
      <!-- 统计卡 -->
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statTransferTasks') }}</div>
          <div class="stat-value">{{ transferStats.tasks ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statActive') }}</div>
          <div class="stat-value">{{ transferStats.active ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statManifests') }}</div>
          <div class="stat-value">{{ transferStats.manifests ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">{{ t('downloads.statServed') }}</div>
          <div class="stat-value">{{ servedText }}</div>
        </div>
      </section>

      <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

      <!-- 传输任务表格 -->
      <section class="panel">
        <div class="card card-table">
          <DataTable
            :columns="transferColumns"
            :rows="transferTasks"
            :loading="transferLoading"
            :empty-text="t('downloads.emptyP2p')"
          >
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ transferStatusLabel(row.status, row.phase) }}</span>
            </template>
            <template #cell-progress="{ row }">
              <div class="prog-wrap">
                <div class="prog-bar">
                  <div
                    class="prog-fill"
                    :class="{
                      'fill-ok': row.status === 'completed',
                      'fill-warn': row.status === 'paused',
                      'fill-err': row.status === 'error',
                    }"
                    :style="{ width: (row.progress ?? 0) + '%' }"
                  ></div>
                </div>
                <span class="prog-text">{{ row.progress ?? 0 }}%</span>
              </div>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status === 'downloading' || row.status === 'pending'"
                class="btn btn-small"
                :disabled="transferBusyId === row.id"
                @click.stop="transferAction(String(row.id ?? ''), 'pause')"
              >{{ t('downloads.pause') }}</button>
              <button
                v-if="row.status === 'paused'"
                class="btn btn-small"
                :disabled="transferBusyId === row.id"
                @click.stop="transferAction(String(row.id ?? ''), 'resume')"
              >{{ t('downloads.resume') }}</button>
              <button
                v-if="row.status !== 'completed' && row.status !== 'error'"
                class="btn btn-small"
                :disabled="transferBusyId === row.id"
                @click.stop="transferAction(String(row.id ?? ''), 'cancel')"
              >{{ t('downloads.cancel') }}</button>
            </template>
          </DataTable>
        </div>
      </section>

      <!-- 已发布清单 -->
      <section class="panel">
        <div class="card manifests-card">
          <div class="manifests-head">
            <h3 class="section-title">{{ t('downloads.manifestsTitle') }}</h3>
            <span class="muted section-sub">{{ t('downloads.manifestsSub') }}</span>
          </div>
          <div v-if="transferManifests.length === 0" class="muted empty-hint">
            {{ t('downloads.manifestsEmpty') }}
          </div>
          <ul v-else class="manifest-list">
            <li v-for="m in transferManifests" :key="m.manifest?.sha256" class="manifest-item">
              <div class="manifest-main">
                <div class="manifest-name">🔗 {{ m.manifest?.name ?? '—' }}（{{ formatBytes(m.manifest?.size ?? 0) }}）</div>
                <div class="manifest-meta muted">
                  {{ t('downloads.manifestMeta', { path: m.path, chunks: m.manifest?.chunks ?? 0 }) }}
                </div>
              </div>
              <div class="manifest-actions">
                <button class="btn btn-small" @click="copyText(m.manifest?.sha256 ?? '', 'sha256')">{{ t('downloads.copySha') }}</button>
                <button class="btn btn-small" @click="copyText(m.manifest?.transfer_id ?? '', 'transfer_id')">{{ t('downloads.copyId') }}</button>
                <button class="btn btn-small btn-danger" @click="unpublishManifest(String(m.manifest?.transfer_id ?? ''))">{{ t('downloads.unpublish') }}</button>
              </div>
            </li>
          </ul>
        </div>
      </section>
    </template>

    <!-- ============ Tab 1 新建任务对话框（url / torrent 双模式） ============ -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-dl-title">
        <div class="modal-head">
          <h3 id="create-dl-title">
            {{ createMode === 'torrent' ? t('downloads.createTitleTorrent') : t('downloads.createTitleUrl') }}
          </h3>
          <button class="modal-close" type="button" :disabled="createSubmitting" @click="closeCreate">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreate">
          <!-- 模式切换 -->
          <div class="mode-switch">
            <button
              type="button"
              class="btn btn-small"
              :class="{ 'mode-active': createMode === 'url' }"
              :disabled="createSubmitting"
              @click="createMode = 'url'"
            >{{ t('downloads.modeUrl') }}</button>
            <button
              type="button"
              class="btn btn-small"
              :class="{ 'mode-active': createMode === 'torrent' }"
              :disabled="createSubmitting"
              @click="createMode = 'torrent'"
            >{{ t('downloads.modeTorrent') }}</button>
          </div>

          <div v-if="createMode === 'url'" class="field">
            <label for="dl-url">{{ t('downloads.urlLabel') }}</label>
            <input
              id="dl-url"
              v-model="createForm.url"
              type="text"
              :placeholder="t('downloads.urlPlaceholder')"
              :disabled="createSubmitting"
            />
            <span class="field-hint muted">
              {{ t('downloads.urlHint') }}
            </span>
          </div>
          <div v-else class="field">
            <label for="dl-torrent">{{ t('downloads.torrentLabel') }}</label>
            <input
              id="dl-torrent"
              type="file"
              accept=".torrent,application/x-bittorrent"
              :disabled="createSubmitting"
              @change="onTorrentPicked"
            />
            <span v-if="torrentFile" class="field-hint ok">{{ t('downloads.torrentSelected', { name: torrentFile.name, kib: Math.ceil(torrentFile.size / 1024) }) }}</span>
            <span class="field-hint muted">{{ t('downloads.torrentHint') }}</span>
          </div>
          <div class="field">
            <label for="dl-save">{{ t('downloads.savePathLabel') }}</label>
            <input
              id="dl-save"
              v-model="createForm.savePath"
              type="text"
              placeholder="/tank/downloads"
              :disabled="createSubmitting"
            />
          </div>
          <div class="field">
            <label for="dl-name">{{ t('downloads.nameLabel') }}</label>
            <input
              id="dl-name"
              v-model="createForm.name"
              type="text"
              :placeholder="createMode === 'torrent' ? t('downloads.namePlaceholderTorrent') : t('downloads.namePlaceholderUrl')"
              :disabled="createSubmitting"
            />
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createSubmitting" @click="closeCreate">{{ t('downloads.cancel') }}</button>
            <button type="submit" class="btn btn-primary" :disabled="createSubmitting">
              {{ createSubmitting ? t('downloads.submitting') : t('downloads.create') }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ Tab 2 发布对话框（本地文件 → 可传输清单） ============ -->
    <div v-if="showPublish" class="modal-backdrop" @click.self="closePublish">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="publish-title">
        <div class="modal-head">
          <h3 id="publish-title">{{ t('downloads.publishTitle') }}</h3>
          <button class="modal-close" type="button" :disabled="publishSubmitting" @click="closePublish">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitPublish">
          <div class="field">
            <label for="pub-path">{{ t('downloads.publishPathLabel') }}</label>
            <input
              id="pub-path"
              v-model="publishForm.path"
              type="text"
              placeholder="/tank/iso/ubuntu-24.04.iso"
              :disabled="publishSubmitting"
            />
            <span class="field-hint muted">
              {{ t('downloads.publishPathHint') }}
            </span>
          </div>
          <div class="field">
            <label for="pub-name">{{ t('downloads.publishNameLabel') }}</label>
            <input
              id="pub-name"
              v-model="publishForm.name"
              type="text"
              :placeholder="t('downloads.publishNamePlaceholder')"
              :disabled="publishSubmitting"
            />
          </div>
          <!-- 发布结果：sha256/transfer_id + 复制 -->
          <div v-if="publishResult" class="publish-result">
            <div class="field">
              <label>{{ t('downloads.shaLabel') }}</label>
              <div class="copy-row">
                <code class="copy-code">{{ publishResult.sha256 }}</code>
                <button type="button" class="btn btn-small" @click="copyText(publishResult.sha256, 'sha256')">{{ t('downloads.copy') }}</button>
              </div>
            </div>
            <div class="field">
              <label>transfer_id</label>
              <div class="copy-row">
                <code class="copy-code">{{ publishResult.transfer_id }}</code>
                <button type="button" class="btn btn-small" @click="copyText(publishResult.transfer_id, 'transfer_id')">{{ t('downloads.copy') }}</button>
              </div>
            </div>
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="publishSubmitting" @click="closePublish">{{ t('downloads.close') }}</button>
            <button type="submit" class="btn btn-primary" :disabled="publishSubmitting">
              {{ publishSubmitting ? t('downloads.publishSubmitting') : (publishResult ? t('downloads.publishAnother') : t('downloads.publish')) }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ Tab 2 拉取对话框（粘贴 sha256 / transfer_id） ============ -->
    <div v-if="showFetch" class="modal-backdrop" @click.self="closeFetch">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="fetch-title">
        <div class="modal-head">
          <h3 id="fetch-title">{{ t('downloads.fetchTitle') }}</h3>
          <button class="modal-close" type="button" :disabled="fetchSubmitting" @click="closeFetch">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitFetch">
          <div class="field">
            <label for="fetch-key">{{ t('downloads.fetchKeyLabel') }}</label>
            <input
              id="fetch-key"
              v-model="fetchForm.key"
              type="text"
              :placeholder="t('downloads.fetchKeyPlaceholder')"
              :disabled="fetchSubmitting"
            />
            <span class="field-hint muted">
              {{ t('downloads.fetchKeyHint') }}
            </span>
          </div>
          <div class="field">
            <label for="fetch-name">{{ t('downloads.fetchNameLabel') }}</label>
            <input
              id="fetch-name"
              v-model="fetchForm.name"
              type="text"
              :placeholder="t('downloads.fetchNamePlaceholder')"
              :disabled="fetchSubmitting"
            />
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="fetchSubmitting" @click="closeFetch">{{ t('downloads.cancel') }}</button>
            <button type="submit" class="btn btn-primary" :disabled="fetchSubmitting">
              {{ fetchSubmitting ? t('downloads.submitting') : t('downloads.fetchSubmit') }}
            </button>
          </div>
        </form>
      </div>
    </div>
  </div>
</template>

<style scoped>
.downloads-page {
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

/* Tab 切换 */
.tab-switch { display: flex; gap: 4px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); padding: 3px; }
.tab-btn { border: none; background: transparent; border-radius: var(--radius-pill, 20px); }
.tab-btn.tab-active { background: var(--bg-card, #fff); color: var(--accent, #E95420); font-weight: 600; box-shadow: 0 1px 2px rgba(0,0,0,0.08); }

.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(160px, 1fr));
  gap: 14px;
}
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

.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-ok { background: #0E8420; }
.prog-fill.fill-warn { background: #F99B11; }
.prog-fill.fill-err { background: #C7162B; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

.pill {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 12px; font-weight: 600;
}
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-warn { color: #b45309; background: #fef3c7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-magnet { color: #7c3aed; background: #ede9fe; }
.pill-torrent { color: #0e7490; background: #cffafe; }
.pill-ed2k { color: #a16207; background: #fef9c3; }

/* 已发布清单面板 */
.manifests-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.manifests-head { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; }
.section-title { font-size: 15px; font-weight: 600; }
.section-sub { font-size: 12px; }
.empty-hint { font-size: 13px; padding: 8px 0; }
.manifest-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 10px; }
.manifest-item {
  display: flex; align-items: center; justify-content: space-between; gap: 12px;
  padding: 10px 12px; border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px); flex-wrap: wrap;
}
.manifest-main { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.manifest-name { font-size: 13.5px; font-weight: 600; }
.manifest-meta { font-size: 12px; word-break: break-all; }
.manifest-actions { display: flex; gap: 6px; flex-wrap: wrap; }

.mode-switch { display: flex; gap: 8px; }
.mode-switch .btn.mode-active {
  border-color: var(--accent, #E95420);
  color: var(--accent, #E95420);
  font-weight: 600;
}
.field-hint { font-size: 12px; }
.field-hint.ok { color: #15803d; }

/* 发布结果复制行 */
.publish-result { display: flex; flex-direction: column; gap: 10px; padding: 10px 12px; background: var(--border-soft, #F6F6F6); border-radius: var(--radius-sm, 8px); }
.copy-row { display: flex; align-items: center; gap: 8px; }
.copy-code { flex: 1; font-size: 12px; word-break: break-all; padding: 6px 8px; background: var(--bg-card, #fff); border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #5E5C5F); }
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
.field input { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; }
.field input[type='file'] { padding: 5px 8px; font-size: 13px; }
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
