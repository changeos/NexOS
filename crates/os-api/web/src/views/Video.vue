<script setup lang="ts">
// =============================================================================
// Video.vue —— 影院（视频媒体库 + TMDB 海报墙 + 内嵌播放器）
//
// 功能：
//   1. 顶部统计卡片：视频数 + 总大小（GET /api/v1/media/stats）
//   2. 海报卡片网格：每部电影一张卡片（海报图 + 标题 + 评分星 + 年份）
//      - 已刮削：展示 TMDB 海报/标题/评分/年份，点击进详情面板
//      - 未刮削：占位海报 + 文件名，可手动点「刮削」
//   3. 详情对话框：大海报 + 剧情 + 评分 + 年份 + 播放按钮
//   4. 内嵌播放器：<video :src controls>（src 取 item.path / stream_url）
//   5. 顶部「刮削全部」按钮（POST /api/v1/media/scrape/all）+ 状态指示
//
// 后端媒体 API 处于早期阶段；前端以宽松字段读取 + 失败友好降级。
// 主题：Ubuntu Yaru（--accent #E95420 等变量）。
// =============================================================================
import { computed, onMounted, ref } from 'vue';
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

const videoCount = computed(() => stats.value?.video_count ?? 0);
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

// —— 视频库 ——
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

// —— TMDB 刮削元数据 ——
interface MediaMetadata {
  id?: string;
  file_path?: string;
  title?: string;
  overview?: string;
  poster_url?: string;
  backdrop_url?: string;
  rating?: number;
  year?: number;
  media_type?: string;
  tmdb_id?: number;
  scraped_at?: string;
  [k: string]: unknown;
}

const items = ref<MediaItem[]>([]);
const metadataMap = ref<Record<string, MediaMetadata>>({});
const loading = ref(false);
const error = ref('');

async function loadLibrary(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.mediaLibrary('video');
    const arr = Array.isArray(raw) ? raw : raw ? [raw] : [];
    items.value = arr as MediaItem[];
  } catch (e) {
    items.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadMetadata(): Promise<void> {
  try {
    const raw = await endpoints.mediaMetadata();
    const arr = Array.isArray(raw) ? raw : raw ? [raw] : [];
    const map: Record<string, MediaMetadata> = {};
    for (const m of arr as MediaMetadata[]) {
      if (m?.file_path) map[m.file_path] = m;
    }
    metadataMap.value = map;
  } catch {
    // 元数据接口缺失时静默降级（按未刮削展示）
    metadataMap.value = {};
  }
}

const refreshing = computed(() => loading.value || statsLoading.value);
function refreshAll(): void {
  void loadLibrary();
  void loadMetadata();
  void loadStats();
}

// —— 是否含有示例数据（真盘空时后端返回 demo，需提示用户）——
const hasDemo = computed(() => items.value.some((i) => i.demo));

// —— 刮削状态 ——
interface ScrapeStatus {
  status?: string;
  last_run_at?: string | null;
  scraped_count?: number;
  skipped_count?: number;
  failed_count?: number;
}
const scrapeStatus = ref<ScrapeStatus | null>(null);
const scraping = ref(false);
const scrapeMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadScrapeStatus(): Promise<void> {
  try {
    scrapeStatus.value = (await endpoints.mediaScrapeStatus()) as ScrapeStatus;
  } catch {
    scrapeStatus.value = null;
  }
}

async function scrapeAll(): Promise<void> {
  scraping.value = true;
  scrapeMsg.value = { kind: 'info', text: '正在批量刮削，请稍候…' };
  try {
    const res = (await endpoints.mediaScrapeAll()) as {
      scraped?: number;
      skipped?: number;
      failed?: number;
      total?: number;
    };
    scrapeMsg.value = {
      kind: res?.skipped && !res?.scraped ? 'info' : 'ok',
      text: `刮削完成：成功 ${res?.scraped ?? 0} · 跳过 ${res?.skipped ?? 0} · 失败 ${res?.failed ?? 0}`,
    };
    await loadMetadata();
    await loadScrapeStatus();
  } catch (e) {
    scrapeMsg.value = { kind: 'err', text: '刮削失败：' + friendlyError(e) };
  } finally {
    scraping.value = false;
  }
}

async function scrapeOne(item: MediaItem, ev: Event): Promise<void> {
  ev.stopPropagation();
  if (!item.path) return;
  scraping.value = true;
  try {
    const res = (await endpoints.mediaScrape(item.path, 'movie')) as {
      status?: string;
      reason?: string;
    };
    if (res?.status === 'ok') {
      scrapeMsg.value = { kind: 'ok', text: `已刮削：${item.title ?? item.path}` };
    } else {
      scrapeMsg.value = {
        kind: 'info',
        text: res?.reason ? `未刮削：${res.reason}` : '未刮削（可能未配置 TMDB_API_KEY）',
      };
    }
    await loadMetadata();
    await loadScrapeStatus();
  } catch (e) {
    scrapeMsg.value = { kind: 'err', text: '刮削失败：' + friendlyError(e) };
  } finally {
    scraping.value = false;
  }
}

// —— 元数据匹配 ——
function metaFor(item: MediaItem): MediaMetadata | null {
  return item.path ? metadataMap.value[item.path] ?? null : null;
}
function isScraped(item: MediaItem): boolean {
  return metaFor(item) !== null;
}

// —— 详情对话框 ——
const selected = ref<MediaItem | null>(null);
function openDetail(item: MediaItem): void {
  selected.value = item;
}
function closeDetail(): void {
  selected.value = null;
}
const selectedMeta = computed<MediaMetadata | null>(() =>
  selected.value ? metaFor(selected.value) : null,
);

// —— 内嵌播放器 ——
const playing = ref(false);
const playerSrc = ref('');
const playerTitle = ref('');

function playItem(item: MediaItem): void {
  const streamUrl = `/api/v1/media/stream/${encodeURIComponent(item.id ?? '')}`;
  // 优先用文件路径（真盘可直链），回退到 stream 端点
  playerSrc.value = item.path && item.path.length > 0 ? item.path : streamUrl;
  playerTitle.value = item.title ?? '视频播放';
  playing.value = true;
}
function closePlayer(): void {
  playing.value = false;
  playerSrc.value = '';
}

// —— 工具 ——
function formatBytes(bytes?: number | null): string {
  if (!bytes || bytes <= 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatDuration(sec?: number | null): string {
  if (!sec || sec <= 0) return '—';
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = Math.floor(sec % 60);
  if (h > 0) return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  return `${m}:${String(s).padStart(2, '0')}`;
}

function formatRating(r?: number | null): string {
  if (!r || r <= 0) return '—';
  return r.toFixed(1);
}

function formatYear(y?: number | null): string {
  return y && y > 0 ? String(y) : '—';
}

function ratingStars(r?: number | null): string {
  if (!r || r <= 0) return '';
  // TMDB 0-10 → 0-5 星
  const stars = Math.round((r / 2) * 2) / 2;
  const full = Math.floor(stars);
  const half = stars - full >= 0.5 ? 1 : 0;
  const empty = 5 - full - half;
  return '★'.repeat(full) + (half ? '⯨' : '') + '☆'.repeat(Math.max(0, empty));
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
  void loadScrapeStatus();
});
</script>

<template>
  <div class="video-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">影院</h2>
        <div class="page-sub muted">视频媒体库 · TMDB 海报墙 · 内嵌播放器</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small" :disabled="scanning" @click="scanLibrary">
          {{ scanning ? '扫描中…' : '扫描媒体库' }}
        </button>
        <button class="btn btn-primary btn-small" :disabled="scraping" @click="scrapeAll">
          {{ scraping ? '刮削中…' : '刮削全部' }}
        </button>
        <span v-if="scrapeStatus" class="scrape-badge" :data-state="scrapeStatus.status">
          刮削：{{ scrapeStatus.status }} · 成功 {{ scrapeStatus.scraped_count ?? 0 }}
        </span>
      </div>
    </div>

    <!-- 统计卡片 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">视频数</div>
        <div class="stat-value">{{ statsLoading ? '—' : videoCount }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">总大小</div>
        <div class="stat-value">{{ statsLoading ? '—' : totalSizeText }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">已刮削</div>
        <div class="stat-value">{{ Object.keys(metadataMap).length }}</div>
      </div>
    </section>
    <div v-if="statsError" class="error-box">统计加载失败：{{ statsError }}</div>
    <p v-if="scanMsg" :class="['form-msg', `is-${scanMsg.kind}`]">{{ scanMsg.text }}</p>
    <p v-if="scrapeMsg" :class="['form-msg', `is-${scrapeMsg.kind}`]">{{ scrapeMsg.text }}</p>

    <!-- 示例数据提示横幅（Ubuntu Yaru info 风） -->
    <div v-if="hasDemo" class="demo-banner" role="status">
      当前为示例数据。将媒体文件放入 <code>/tank/media/video/</code> 后将自动显示真实内容。
    </div>

    <!-- 海报墙 -->
    <section class="panel">
      <div class="panel-head">
        <h3>海报墙</h3>
        <span v-if="loading" class="muted small">加载中…</span>
      </div>
      <div v-if="error" class="error-box">{{ error }}</div>

      <div v-else-if="!loading && items.length === 0" class="card empty-state">
        <div class="empty-icon">🎬</div>
        <div class="empty-text">暂无视频</div>
        <div class="empty-hint muted">点击右上角「扫描媒体库」或检查媒体目录配置。</div>
      </div>

      <div v-else class="poster-grid">
        <div
          v-for="item in items"
          :key="item.id ?? item.title"
          class="card poster-card"
          :class="{ 'is-scraped': isScraped(item) }"
          @click="openDetail(item)"
        >
          <div class="poster">
            <img
              v-if="metaFor(item)?.poster_url"
              :src="metaFor(item)!.poster_url!"
              :alt="metaFor(item)?.title ?? item.title ?? 'poster'"
              loading="lazy"
            />
            <img
              v-else-if="item.thumbnail_url"
              :src="item.thumbnail_url!"
              :alt="item.title ?? 'video'"
              loading="lazy"
            />
            <div v-else class="poster-placeholder"><span>🎬</span></div>
            <span v-if="!isScraped(item)" class="badge badge-unscraped">未刮削</span>
            <span v-else class="badge badge-year">{{ formatYear(metaFor(item)?.year) }}</span>
          </div>
          <div class="poster-body">
            <div class="poster-title" :title="metaFor(item)?.title ?? item.title">
              {{ metaFor(item)?.title ?? item.title ?? '—' }}
            </div>
            <div class="poster-meta">
              <span v-if="metaFor(item)?.rating" class="rating">★ {{ formatRating(metaFor(item)?.rating) }}</span>
              <span v-else class="muted small">{{ formatDuration(item.duration_secs) }}</span>
            </div>
            <button
              v-if="!isScraped(item)"
              class="btn btn-mini scrape-mini"
              :disabled="scraping"
              @click="scrapeOne(item, $event)"
            >
              刮削
            </button>
          </div>
        </div>
      </div>
    </section>

    <!-- 详情对话框 -->
    <div v-if="selected" class="modal-backdrop" @click.self="closeDetail">
      <div class="modal detail-modal card">
        <button class="modal-close" aria-label="关闭" @click="closeDetail">✕</button>
        <div class="detail-backdrop" :style="selectedMeta?.backdrop_url ? `background-image:url(${selectedMeta.backdrop_url})` : ''">
          <div class="detail-head">
            <img
              v-if="selectedMeta?.poster_url"
              class="detail-poster"
              :src="selectedMeta.poster_url"
              :alt="selectedMeta.title"
            />
            <div v-else class="detail-poster detail-poster-placeholder"><span>🎬</span></div>
            <div class="detail-info">
              <h3 class="detail-title">{{ selectedMeta?.title ?? selected.title ?? '—' }}</h3>
              <div class="detail-sub muted">
                <span v-if="selectedMeta?.year">{{ selectedMeta.year }}</span>
                <span v-if="selectedMeta?.media_type"> · {{ selectedMeta.media_type === 'tv' ? '剧集' : '电影' }}</span>
                <span v-if="selectedMeta?.rating"> · ★ {{ formatRating(selectedMeta.rating) }}</span>
                <span v-if="selected?.mime_type"> · {{ selected.mime_type }}</span>
              </div>
              <div v-if="ratingStars(selectedMeta?.rating)" class="detail-stars">{{ ratingStars(selectedMeta?.rating) }}</div>
              <button class="btn btn-primary" @click="playItem(selected)">
                ▶ 播放
              </button>
            </div>
          </div>
        </div>
        <div class="detail-overview">
          <h4>剧情简介</h4>
          <p v-if="selectedMeta?.overview">{{ selectedMeta.overview }}</p>
          <p v-else class="muted">暂无剧情简介（未刮削或 TMDB 无数据）。文件：<code>{{ selected.path }}</code></p>
          <div v-if="selected.tags && selected.tags.length" class="tag-row">
            <span v-for="t in selected.tags" :key="t" class="tag">{{ t }}</span>
          </div>
        </div>
        <div v-if="!selectedMeta" class="detail-actions">
          <button class="btn btn-primary" :disabled="scraping" @click="scrapeOne(selected, $event)">
            手动刮削此视频
          </button>
        </div>
      </div>
    </div>

    <!-- 内嵌播放器对话框 -->
    <div v-if="playing" class="modal-backdrop player-backdrop" @click.self="closePlayer">
      <div class="modal player-modal card">
        <div class="player-head">
          <span class="player-title">{{ playerTitle }}</span>
          <button class="modal-close" aria-label="关闭" @click="closePlayer">✕</button>
        </div>
        <video class="player-video" :src="playerSrc" controls autoplay preload="metadata">
          你的浏览器不支持视频播放。
        </video>
      </div>
    </div>
  </div>
</template>

<style scoped>
.video-page {
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
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
  gap: 16px;
}
.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
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

/* —— 海报墙 —— */
.poster-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
  gap: 16px;
}
.poster-card {
  overflow: hidden;
  display: flex;
  flex-direction: column;
  cursor: pointer;
  transition: transform 0.15s ease, box-shadow 0.15s ease;
}
.poster-card:hover {
  transform: translateY(-3px);
  box-shadow: 0 6px 16px rgba(0, 0, 0, 0.18);
}
.poster {
  position: relative;
  width: 100%;
  aspect-ratio: 2 / 3;
  background: #111;
  display: flex;
  align-items: center;
  justify-content: center;
}
.poster img {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.poster-placeholder {
  color: rgba(255, 255, 255, 0.5);
  font-size: 40px;
}
.badge {
  position: absolute;
  font-size: 11px;
  padding: 2px 7px;
  border-radius: 4px;
  font-weight: 600;
}
.badge-unscraped {
  top: 8px;
  left: 8px;
  background: rgba(233, 84, 32, 0.92);
  color: #fff;
}
.badge-year {
  top: 8px;
  left: 8px;
  background: rgba(0, 0, 0, 0.72);
  color: #fff;
}
.poster-body {
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.poster-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.poster-meta {
  font-size: 12px;
  display: flex;
  align-items: center;
  gap: 6px;
}
.rating {
  color: #E95420;
  font-weight: 600;
}
.scrape-mini {
  align-self: flex-start;
  margin-top: 4px;
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
.btn-mini {
  padding: 2px 8px;
  font-size: 11px;
}
.btn-primary {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
}
.btn-primary:hover:not(:disabled) {
  background: var(--accent-hi, #c8431a);
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

/* —— 刮削状态徽标 —— */
.scrape-badge {
  font-size: 12px;
  padding: 3px 9px;
  border-radius: var(--radius-pill, 20px);
  background: rgba(0, 0, 0, 0.06);
  color: var(--text-muted, #5E5C5F);
  border: 1px solid var(--border, #D9D9D9);
}
.scrape-badge[data-state='running'] {
  background: rgba(233, 84, 32, 0.12);
  color: #E95420;
}

/* —— 对话框（详情 / 播放器）—— */
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.6);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
  padding: 20px;
}
.modal {
  position: relative;
  width: 100%;
  max-width: 720px;
  max-height: 90vh;
  overflow-y: auto;
}
.modal-close {
  position: absolute;
  top: 8px;
  right: 10px;
  background: rgba(0, 0, 0, 0.55);
  color: #fff;
  border: none;
  border-radius: 50%;
  width: 30px;
  height: 30px;
  font-size: 14px;
  cursor: pointer;
  z-index: 2;
}
.modal-close:hover {
  background: rgba(0, 0, 0, 0.8);
}

/* —— 详情对话框 —— */
.detail-modal {
  padding: 0;
  overflow: hidden;
}
.detail-backdrop {
  background-size: cover;
  background-position: center;
  background-color: #1a1a1a;
}
.detail-head {
  display: flex;
  gap: 18px;
  padding: 20px;
  background: linear-gradient(180deg, rgba(0, 0, 0, 0.1), rgba(0, 0, 0, 0.75));
}
.detail-poster {
  width: 150px;
  height: 225px;
  border-radius: var(--radius-md, 10px);
  object-fit: cover;
  flex-shrink: 0;
  background: #000;
  box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
}
.detail-poster-placeholder {
  display: flex;
  align-items: center;
  justify-content: center;
  color: rgba(255, 255, 255, 0.5);
  font-size: 40px;
}
.detail-info {
  display: flex;
  flex-direction: column;
  gap: 8px;
  justify-content: flex-end;
}
.detail-title {
  font-size: 22px;
  font-weight: 700;
  color: #fff;
  letter-spacing: -0.01em;
}
.detail-sub {
  font-size: 13px;
  color: rgba(255, 255, 255, 0.78);
}
.detail-stars {
  color: #ffb400;
  font-size: 16px;
  letter-spacing: 1px;
}
.detail-overview {
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.detail-overview h4 {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  text-transform: uppercase;
  letter-spacing: 0.5px;
}
.detail-overview p {
  font-size: 14px;
  line-height: 1.6;
  color: var(--text, #2B2B2B);
}
.detail-overview code {
  font-family: var(--mono, monospace);
  font-size: 12px;
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 5px;
  border-radius: 4px;
  word-break: break-all;
}
.detail-actions {
  padding: 0 20px 18px;
}

/* —— 播放器对话框 —— */
.player-backdrop {
  background: rgba(0, 0, 0, 0.85);
}
.player-modal {
  max-width: 960px;
  padding: 0;
  overflow: hidden;
}
.player-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 10px 14px;
  background: #111;
}
.player-title {
  color: #fff;
  font-size: 14px;
  font-weight: 600;
}
.player-video {
  width: 100%;
  max-height: 78vh;
  display: block;
  background: #000;
}

/* —— 标签 —— */
.tag-row {
  display: flex;
  flex-wrap: wrap;
  gap: 4px;
  margin-top: 4px;
}
.tag {
  font-size: 11px;
  padding: 1px 8px;
  border-radius: var(--radius-pill, 20px);
  background: rgba(233, 84, 32, 0.12);
  color: #c8431a;
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
  .video-page {
    padding: 16px;
  }
  .detail-head {
    flex-direction: column;
    align-items: center;
    text-align: center;
  }
  .detail-info {
    align-items: center;
  }
}
</style>
