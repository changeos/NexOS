<script setup lang="ts">
// =============================================================================
// Music.vue —— 音乐（音频媒体库）
//
// 功能：
//   1. 顶部统计卡片：曲目数 + 总大小（GET /api/v1/media/stats）
//   2. 扫描按钮：触发媒体库扫描（POST /api/v1/media/scan）
//   3. 曲目表格：标题 / 艺术家占位 / 时长 / 格式（GET /api/v1/media/library?type=music）
//
// 后端媒体 API 处于早期阶段；前端以宽松字段读取 + 失败友好降级，缺字段显示 '—'。
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

// —— 统计 ——
interface Stats {
  video_count?: number;
  music_count?: number;
  photo_count?: number;
  total_size_bytes?: number;
}
const stats = ref<Stats | null>(null);
const statsLoading = ref(false);
const statsError = ref('');

async function loadStats(): Promise<void> {
  statsLoading.value = true;
  statsError.value = '';
  try {
    stats.value = (await endpoints.mediaStats()) as Stats;
  } catch (e) {
    stats.value = null;
    statsError.value = friendlyError(e);
  } finally {
    statsLoading.value = false;
  }
}

const trackCount = computed(() => stats.value?.music_count ?? 0);
const totalSizeText = computed(() => formatBytes(stats.value?.total_size_bytes));

// —— 扫描 ——
const scanning = ref(false);
const scanMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function scanLibrary(): Promise<void> {
  scanning.value = true;
  scanMsg.value = { kind: 'info', text: '扫描已触发，请稍候刷新…' };
  try {
    await endpoints.mediaScan();
    scanMsg.value = { kind: 'ok', text: '扫描已触发，正在重新加载库…' };
    await loadLibrary();
    await loadStats();
  } catch (e) {
    scanMsg.value = { kind: 'err', text: '扫描失败：' + friendlyError(e) };
  } finally {
    scanning.value = false;
  }
}

// —— 音乐库 ——
interface MediaItem {
  id?: string;
  title?: string;
  path?: string;
  mime_type?: string;
  size_bytes?: number;
  duration_secs?: number | null;
  thumbnail_url?: string | null;
  created_at?: string;
  tags?: string[];
  demo?: boolean;
  [k: string]: unknown;
}

const items = ref<MediaItem[]>([]);
const loading = ref(false);
const error = ref('');

async function loadLibrary(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.mediaLibrary('music');
    const arr = Array.isArray(raw) ? raw : raw ? [raw] : [];
    items.value = arr as MediaItem[];
  } catch (e) {
    items.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

const columns: Column<MediaItem>[] = [
  { key: 'title', title: '标题', accessor: (r) => r.title ?? '—' },
  { key: 'artist', title: '艺术家', accessor: (r) => (r.tags && r.tags[0]) ?? '未知艺术家' },
  { key: 'format', title: '格式', accessor: (r) => r.mime_type ?? '—' },
  {
    key: 'duration',
    title: '时长',
    width: '100px',
    align: 'right',
    accessor: (r) => r.duration_secs ?? 0,
  },
  {
    key: 'size',
    title: '大小',
    width: '110px',
    align: 'right',
    accessor: (r) => r.size_bytes ?? 0,
  },
];

const refreshing = computed(() => loading.value || statsLoading.value);
function refreshAll(): void {
  void loadLibrary();
  void loadStats();
}

// —— 是否含有示例数据（真盘空时后端返回 demo，需提示用户）——
const hasDemo = computed(() => items.value.some((i) => i.demo));

// —— 工具 ——
function formatBytes(bytes?: number | null): string {
  if (!bytes || bytes <= 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatDuration(sec?: number | null): string {
  if (!sec || sec <= 0) return '—';
  const m = Math.floor(sec / 60);
  const s = Math.floor(sec % 60);
  return `${m}:${String(s).padStart(2, '0')}`;
}

function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该媒体接口';
  }
  return m;
}

onMounted(() => {
  refreshAll();
});
</script>

<template>
  <div class="music-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">音乐</h2>
        <div class="page-sub muted">音频媒体库 · 浏览与扫描本地音乐</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-primary btn-small" :disabled="scanning" @click="scanLibrary">
          {{ scanning ? '扫描中…' : '扫描媒体库' }}
        </button>
      </div>
    </div>

    <!-- 统计卡片 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">曲目数</div>
        <div class="stat-value">{{ statsLoading ? '—' : trackCount }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">总大小</div>
        <div class="stat-value">{{ statsLoading ? '—' : totalSizeText }}</div>
      </div>
    </section>
    <div v-if="statsError" class="error-box">统计加载失败：{{ statsError }}</div>
    <p v-if="scanMsg" :class="['form-msg', `is-${scanMsg.kind}`]">{{ scanMsg.text }}</p>

    <!-- 示例数据提示横幅（Ubuntu Yaru info 风） -->
    <div v-if="hasDemo" class="demo-banner" role="status">
      当前为示例数据。将媒体文件放入 <code>/tank/media/music/</code> 后将自动显示真实内容。
    </div>

    <!-- 曲目列表 -->
    <section class="panel">
      <div class="panel-head">
        <h3>曲目列表</h3>
        <span v-if="loading" class="muted small">加载中…</span>
      </div>
      <div v-if="error" class="error-box">{{ error }}</div>
      <div v-else-if="!loading && items.length === 0" class="card empty-state">
        <div class="empty-icon">🎵</div>
        <div class="empty-text">暂无音乐</div>
        <div class="empty-hint muted">点击右上角「扫描媒体库」或检查媒体目录配置。</div>
      </div>
      <div v-else class="card card-table">
        <DataTable :columns="columns" :rows="items" :loading="loading" empty-text="暂无音乐">
          <template #cell-format="{ row }">
            <span class="mime-tag mono">{{ row.mime_type ?? '—' }}</span>
          </template>
          <template #cell-duration="{ row }">
            <span class="mono">{{ formatDuration(row.duration_secs) }}</span>
          </template>
          <template #cell-size="{ row }">
            <span class="mono">{{ formatBytes(row.size_bytes) }}</span>
          </template>
        </DataTable>
      </div>
    </section>
  </div>
</template>

<style scoped>
.music-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.page-title {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.02em;
}
.page-sub {
  margin-top: 4px;
  font-size: 13px;
}
.head-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.muted {
  color: var(--text-muted, #5E5C5F);
}
.small {
  font-size: 12.5px;
}

/* —— 统计卡片 —— */
.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
  gap: 16px;
}
.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table {
  padding: 0;
  overflow: hidden;
}
.stat-card {
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.stat-label {
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.6px;
  color: var(--text-muted, #5E5C5F);
  font-weight: 600;
}
.stat-value {
  font-size: 28px;
  font-weight: 700;
  letter-spacing: -0.02em;
  color: var(--text, #2B2B2B);
}

/* —— 面板 —— */
.panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.panel-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.panel-head h3 {
  font-size: 16px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}

.mime-tag {
  font-family: var(--mono, monospace);
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 6px;
  border-radius: 4px;
  font-size: 12px;
}

/* —— 空态 —— */
.empty-state {
  padding: 48px 20px;
  text-align: center;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
}
.empty-icon {
  font-size: 44px;
}
.empty-text {
  font-size: 16px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
.empty-hint {
  font-size: 13px;
}

/* —— 消息 —— */
.error-box {
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
}
.form-msg {
  font-size: 13px;
}
.form-msg.is-err {
  color: #b91c1c;
}
.form-msg.is-ok {
  color: #15803d;
}
.form-msg.is-info {
  color: var(--text-muted, #6b7280);
}

/* —— 按钮 —— */
.btn {
  padding: 6px 14px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2B2B2B);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  transition: background 0.15s ease, opacity 0.15s ease;
}
.btn:hover {
  background: rgba(0, 0, 0, 0.04);
}
.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.btn-small {
  padding: 4px 10px;
  font-size: 12.5px;
}
.btn-primary {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
}
.btn-primary:hover:not(:disabled) {
  background: var(--accent-hi, #0077ed);
}
.spin {
  display: inline-block;
  font-size: 14px;
  line-height: 1;
}
.spin.spinning {
  animation: spin 0.8s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
.mono {
  font-family: var(--mono, monospace);
}

/* —— 示例数据提示横幅（Ubuntu Yaru info 风）—— */
.demo-banner {
  background: var(--accent-soft, rgba(233, 84, 32, 0.12));
  border-left: 4px solid var(--accent, #E95420);
  border-radius: var(--radius-md, 8px);
  padding: 10px 14px;
  font-size: 13px;
  color: var(--text, #2B2B2B);
}
.demo-banner code {
  font-family: var(--mono, monospace);
  font-size: 12.5px;
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 5px;
  border-radius: 4px;
}

@media (max-width: 720px) {
  .music-page {
    padding: 16px;
  }
}
</style>
