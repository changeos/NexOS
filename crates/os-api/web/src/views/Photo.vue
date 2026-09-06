<script setup lang="ts">
// =============================================================================
// Photo.vue —— AI 相册（Qwen3-VL 图片识别 + 标签 + 语义搜索 + 场景分类）
//
// 三个区域：
//   1. 顶部搜索栏：搜索框（语义搜索 tags/description/scene）+ "AI 分析"按钮
//   2. 场景分类侧栏：landscape/portrait/food/architecture/animal/other 过滤
//   3. 照片网格：缩略图 + AI 描述 + 标签徽章；点击 → 详情弹窗
//
// 后端端点（endpoints.photo*）：
//   POST /api/v1/media/photo/analyze        AI 分析（单张/全部，需 admin）
//   GET  /api/v1/media/photo/ai-metadata     AI 元数据列表
//   GET  /api/v1/media/photo/search?q=       语义搜索
//   GET  /api/v1/media/photo/categories      场景分类统计
//
// 前端以宽松字段读取 + 失败友好降级，缺字段显示 '—'。
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

const photoCount = computed(() => stats.value?.photo_count ?? 0);

// —— 扫描 ——
const scanning = ref(false);
async function scanLibrary(): Promise<void> {
  scanning.value = true;
  try {
    await endpoints.mediaScan();
    await loadLibrary();
    await loadStats();
  } finally {
    scanning.value = false;
  }
}

// —— 相册库 ——
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

// —— AI 元数据 ——
interface PhotoAi {
  file_path: string;
  description?: string;
  tags?: string[];
  scene?: string;
  has_people?: boolean;
  colors?: string[];
  analyzed_at?: string;
}

interface SceneCategory {
  scene: string;
  count: number;
}

const items = ref<MediaItem[]>([]);
const aiMap = ref<Map<string, PhotoAi>>(new Map());
const categories = ref<SceneCategory[]>([]);
const loading = ref(false);
const error = ref('');

// AI 分析状态
const analyzing = ref(false);
const analyzeMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

// 搜索
const searchQuery = ref('');
const searchResults = ref<PhotoAi[] | null>(null);
const searching = ref(false);

// 场景过滤
const activeScene = ref<string>('');

// 场景中文名（与后端 scene_to_cn 对齐）
const SCENE_CN: Record<string, string> = {
  landscape: '风景',
  portrait: '人物',
  food: '食物',
  architecture: '建筑',
  animal: '动物',
  other: '其它',
};
/** 全部场景顺序（用于侧栏渲染；含"全部"）。 */
const sceneTabs = computed(() => {
  const tabs: { key: string; label: string; count: number }[] = [
    { key: '', label: '全部', count: items.value.length },
  ];
  for (const c of categories.value) {
    tabs.push({
      key: c.scene,
      label: SCENE_CN[c.scene] ?? c.scene,
      count: c.count,
    });
  }
  // 补全未出现在 categories 中但有定义的场景（空计数）
  for (const k of Object.keys(SCENE_CN)) {
    if (!tabs.some((t) => t.key === k)) {
      tabs.push({ key: k, label: SCENE_CN[k], count: 0 });
    }
  }
  return tabs;
});

async function loadLibrary(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.mediaLibrary('photo');
    const arr = Array.isArray(raw) ? raw : raw ? [raw] : [];
    items.value = arr as MediaItem[];
  } catch (e) {
    items.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadAiMetadata(): Promise<void> {
  try {
    const raw = await endpoints.photoAiMetadata();
    const arr = (Array.isArray(raw) ? raw : []) as PhotoAi[];
    const m = new Map<string, PhotoAi>();
    for (const a of arr) {
      if (a.file_path) m.set(a.file_path, a);
    }
    aiMap.value = m;
  } catch {
    aiMap.value = new Map();
  }
}

async function loadCategories(): Promise<void> {
  try {
    const raw = await endpoints.photoCategories();
    categories.value = (Array.isArray(raw) ? raw : []) as SceneCategory[];
  } catch {
    categories.value = [];
  }
}

const refreshing = computed(() => loading.value || statsLoading.value);
function refreshAll(): void {
  void loadLibrary();
  void loadStats();
  void loadAiMetadata();
  void loadCategories();
}

/** 某条目的 AI 元数据（按 path 匹配）。 */
function aiOf(item: MediaItem): PhotoAi | undefined {
  return item.path ? aiMap.value.get(item.path) : undefined;
}

// —— 当前展示列表（搜索结果优先；否则按场景过滤）——
interface DisplayCard {
  item?: MediaItem;
  ai: PhotoAi | undefined;
  key: string;
  title: string;
  thumb: string | null;
}

const displayCards = computed<DisplayCard[]>(() => {
  // 搜索模式：以 AI 命中记录为主，尝试匹配回 MediaItem（缩略图/标题）
  if (searchResults.value) {
    return searchResults.value.map((ai, idx) => {
      const item = items.value.find((it) => it.path === ai.file_path);
      return {
        ai,
        item,
        key: `s-${ai.file_path}-${idx}`,
        title: item?.title ?? ai.file_path.split('/').pop() ?? ai.file_path,
        thumb: item?.thumbnail_url ?? null,
      };
    });
  }
  // 普通模式：过滤场景
  return items.value
    .filter((it) => {
      if (!activeScene.value) return true;
      return aiOf(it)?.scene === activeScene.value;
    })
    .map((it) => ({
      item: it,
      ai: aiOf(it),
      key: (it.id ?? it.path ?? it.title) as string,
      title: it.title ?? '—',
      thumb: it.thumbnail_url ?? null,
    }));
});

// —— 搜索 ——
async function doSearch(): Promise<void> {
  const q = searchQuery.value.trim();
  if (!q) {
    searchResults.value = null;
    return;
  }
  searching.value = true;
  try {
    const raw = await endpoints.photoSearch(q);
    searchResults.value = (Array.isArray(raw) ? raw : []) as PhotoAi[];
    activeScene.value = ''; // 搜索时清空场景过滤
  } catch (e) {
    analyzeMsg.value = { kind: 'err', text: '搜索失败：' + friendlyError(e) };
  } finally {
    searching.value = false;
  }
}

function clearSearch(): void {
  searchQuery.value = '';
  searchResults.value = null;
}

// —— AI 分析 ——
async function analyzeAll(): Promise<void> {
  analyzing.value = true;
  analyzeMsg.value = { kind: 'info', text: 'AI 分析已触发，正在逐张识别（可能需要一些时间）…' };
  try {
    const raw = (await endpoints.photoAnalyze()) as Record<string, unknown>;
    const status = String(raw?.status ?? '');
    if (status === 'skipped') {
      analyzeMsg.value = {
        kind: 'info',
        text: String(raw?.reason ?? 'vLLM 未运行，已跳过'),
      };
    } else if (status === 'failed') {
      analyzeMsg.value = { kind: 'err', text: String(raw?.reason ?? '分析失败') };
    } else {
      const analyzed = Number(raw?.analyzed ?? 0);
      const total = Number(raw?.total ?? 0);
      analyzeMsg.value = {
        kind: 'ok',
        text: `分析完成（共 ${total}，成功 ${analyzed}）`,
      };
    }
    await loadAiMetadata();
    await loadCategories();
  } catch (e) {
    analyzeMsg.value = { kind: 'err', text: '分析失败：' + friendlyError(e) };
  } finally {
    analyzing.value = false;
  }
}

async function analyzeOne(item: MediaItem): Promise<void> {
  if (!item.path) return;
  analyzing.value = true;
  try {
    const raw = (await endpoints.photoAnalyze(item.path)) as Record<string, unknown>;
    const status = String(raw?.status ?? '');
    if (status === 'ok') {
      analyzeMsg.value = { kind: 'ok', text: '已分析单张' };
    } else if (status === 'skipped') {
      analyzeMsg.value = { kind: 'info', text: String(raw?.reason ?? 'vLLM 未运行') };
    } else {
      analyzeMsg.value = { kind: 'err', text: String(raw?.reason ?? '分析失败') };
    }
    await loadAiMetadata();
    await loadCategories();
  } catch (e) {
    analyzeMsg.value = { kind: 'err', text: '分析失败：' + friendlyError(e) };
  } finally {
    analyzing.value = false;
  }
}

// —— 详情弹窗 ——
const selected = ref<DisplayCard | null>(null);
function openDetail(card: DisplayCard): void {
  selected.value = card;
}
function closeDetail(): void {
  selected.value = null;
}

// —— 是否含有示例数据 ——
const hasDemo = computed(() => items.value.some((i) => i.demo));
const analyzedCount = computed(() => aiMap.value.size);

// —— 工具 ——
function formatBytes(bytes?: number | null): string {
  if (!bytes || bytes <= 0) return '—';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

/** 按标题/路径生成确定性色调（无缩略图时的纯色占位背景）。 */
function placeholderColor(seedText?: string): string {
  const palettes = [
    'linear-gradient(135deg, #667eea, #764ba2)',
    'linear-gradient(135deg, #f093fb, #f5576c)',
    'linear-gradient(135deg, #4facfe, #00f2fe)',
    'linear-gradient(135deg, #43e97b, #38f9d7)',
    'linear-gradient(135deg, #fa709a, #fee140)',
    'linear-gradient(135deg, #30cfd0, #330867)',
  ];
  const seed = (seedText ?? '').split('').reduce((a, c) => a + c.charCodeAt(0), 0);
  return palettes[seed % palettes.length];
}

/** 标签徽章确定性配色（按 tag 哈希取色）。 */
function tagColor(tag: string): string {
  const colors = [
    'rgba(233, 84, 32, 0.14)',
    'rgba(119, 119, 235, 0.16)',
    'rgba(67, 233, 123, 0.16)',
    'rgba(79, 172, 254, 0.16)',
    'rgba(250, 112, 154, 0.16)',
    'rgba(48, 207, 208, 0.16)',
  ];
  const seed = tag.split('').reduce((a, c) => a + c.charCodeAt(0), 0);
  return colors[seed % colors.length];
}

function sceneLabel(scene?: string): string {
  if (!scene) return '—';
  return SCENE_CN[scene] ?? scene;
}

function truncate(s: string | undefined, n = 38): string {
  if (!s) return '';
  return s.length > n ? s.slice(0, n) + '…' : s;
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
  <div class="photo-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">AI 相册</h2>
        <div class="page-sub muted">
          图片媒体库 · Qwen3-VL 智能识别 · 语义搜索与场景分类
        </div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small" :disabled="scanning" @click="scanLibrary">
          {{ scanning ? '扫描中…' : '扫描媒体库' }}
        </button>
        <button
          class="btn btn-primary btn-small"
          :disabled="analyzing"
          @click="analyzeAll"
          title="调用 vLLM Qwen3-VL 分析全部未分析照片"
        >
          {{ analyzing ? 'AI 分析中…' : 'AI 分析全部' }}
        </button>
      </div>
    </div>

    <!-- 搜索栏 -->
    <section class="search-bar card">
      <form class="search-form" @submit.prevent="doSearch">
        <input
          v-model="searchQuery"
          class="search-input"
          type="text"
          placeholder='语义搜索：输入"海边日落"、"猫咪"、"风景"…'
          aria-label="语义搜索照片"
        />
        <button class="btn btn-primary btn-small" type="submit" :disabled="searching">
          {{ searching ? '搜索中…' : '搜索' }}
        </button>
        <button
          v-if="searchResults"
          class="btn btn-small"
          type="button"
          @click="clearSearch"
        >
          清除
        </button>
      </form>
      <div class="search-meta muted small">
        已分析 <strong>{{ analyzedCount }}</strong> / {{ photoCount }} 张
        <span v-if="searchResults"> · 搜索命中 {{ searchResults.length }} 张</span>
      </div>
    </section>

    <p v-if="analyzeMsg" :class="['form-msg', `is-${analyzeMsg.kind}`]">{{ analyzeMsg.text }}</p>
    <div v-if="statsError" class="error-box">统计加载失败：{{ statsError }}</div>

    <!-- 示例数据提示横幅（Ubuntu Yaru info 风） -->
    <div v-if="hasDemo" class="demo-banner" role="status">
      当前为示例数据。将媒体文件放入 <code>/tank/media/photo/</code> 后将自动显示真实内容。
    </div>

    <!-- 主体：左侧场景分类 + 右侧照片网格 -->
    <section class="panel body-row">
      <!-- 场景分类侧栏 -->
      <aside class="scene-sidebar">
        <div class="panel-head"><h3>场景分类</h3></div>
        <div class="scene-list">
          <button
            v-for="tab in sceneTabs"
            :key="tab.key || 'all'"
            class="scene-pill"
            :class="{ active: activeScene === tab.key }"
            type="button"
            @click="activeScene = tab.key; searchResults = null"
          >
            <span class="scene-label">{{ tab.label }}</span>
            <span class="scene-count">{{ tab.count }}</span>
          </button>
        </div>
      </aside>

      <!-- 照片网格 -->
      <div class="grid-wrap">
        <div class="panel-head">
          <h3>{{ searchResults ? '搜索结果' : '照片库' }}</h3>
          <span v-if="loading || searching" class="muted small">加载中…</span>
        </div>
        <div v-if="error" class="error-box">{{ error }}</div>

        <div
          v-else-if="!loading && !searching && displayCards.length === 0"
          class="card empty-state"
        >
          <div class="empty-icon">🖼️</div>
          <div class="empty-text">暂无照片</div>
          <div class="empty-hint muted">
            点击右上角「扫描媒体库」或「AI 分析全部」。
          </div>
        </div>

        <div v-else class="photo-grid">
          <div
            v-for="card in displayCards"
            :key="card.key"
            class="photo-card"
            :title="card.title"
            @click="openDetail(card)"
          >
            <div class="thumb-wrap">
              <img
                v-if="card.thumb"
                :src="card.thumb"
                :alt="card.title"
                loading="lazy"
              />
              <div
                v-else
                class="photo-placeholder"
                :style="{ background: placeholderColor(card.title) }"
              >
                <span>🖼️</span>
              </div>
              <span v-if="card.ai?.scene" class="scene-badge">
                {{ sceneLabel(card.ai.scene) }}
              </span>
              <span v-else-if="!card.ai" class="scene-badge unanalyzed">未分析</span>
            </div>
            <div class="photo-caption">
              <span class="photo-title">{{ card.title }}</span>
              <span class="photo-size muted mono">{{ formatBytes(card.item?.size_bytes) }}</span>
            </div>
            <div v-if="card.ai?.description" class="photo-desc muted">
              {{ truncate(card.ai.description, 48) }}
            </div>
            <div class="photo-tags">
              <span
                v-for="t in (card.ai?.tags ?? []).slice(0, 4)"
                :key="t"
                class="tag-badge"
                :style="{ background: tagColor(t) }"
                >{{ t }}</span
              >
              <button
                v-if="!card.ai && card.item"
                class="tag-badge analyze-btn"
                type="button"
                :disabled="analyzing"
                @click.stop="analyzeOne(card.item)"
              >
                {{ analyzing ? '分析中…' : '分析此张' }}
              </button>
            </div>
          </div>
        </div>
      </div>
    </section>

    <!-- 详情弹窗 -->
    <div v-if="selected" class="modal-backdrop" @click.self="closeDetail">
      <div class="modal card" role="dialog" aria-modal="true">
        <button class="modal-close" type="button" @click="closeDetail" aria-label="关闭">×</button>
        <div class="modal-thumb">
          <img
            v-if="selected.thumb"
            :src="selected.thumb"
            :alt="selected.title"
          />
          <div
            v-else
            class="photo-placeholder lg"
            :style="{ background: placeholderColor(selected.title) }"
          >
            <span>🖼️</span>
          </div>
        </div>
        <div class="modal-body">
          <h3 class="modal-title">{{ selected.title }}</h3>
          <div v-if="selected.ai" class="modal-meta">
            <span class="tag-badge scene-tag">{{ sceneLabel(selected.ai.scene) }}</span>
            <span v-if="selected.ai.has_people" class="tag-badge">含人物</span>
            <span
              v-for="c in selected.ai.colors ?? []"
              :key="c"
              class="tag-badge color-tag"
              >🎨 {{ c }}</span
            >
          </div>
          <div v-else class="modal-meta">
            <span class="tag-badge unanalyzed">未分析</span>
            <button
              v-if="selected.item"
              class="btn btn-primary btn-small"
              type="button"
              :disabled="analyzing"
              @click="analyzeOne(selected.item!)"
            >
              {{ analyzing ? '分析中…' : 'AI 分析此张' }}
            </button>
          </div>
          <div class="modal-section">
            <div class="modal-section-title">描述</div>
            <p class="modal-desc">{{ selected.ai?.description || '暂无 AI 描述' }}</p>
          </div>
          <div class="modal-section">
            <div class="modal-section-title">标签</div>
            <div v-if="selected.ai?.tags?.length" class="photo-tags">
              <span
                v-for="t in selected.ai.tags"
                :key="t"
                class="tag-badge"
                :style="{ background: tagColor(t) }"
                >{{ t }}</span
              >
            </div>
            <p v-else class="muted small">暂无标签</p>
          </div>
          <div v-if="selected.item?.path" class="modal-section">
            <div class="modal-section-title">文件路径</div>
            <code class="modal-path">{{ selected.item.path }}</code>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.photo-page {
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
  color: var(--text, #2b2b2b);
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
  color: var(--text-muted, #5e5c5f);
}
.small {
  font-size: 12.5px;
}

/* —— 搜索栏 —— */
.search-bar {
  padding: 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.search-form {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.search-input {
  flex: 1 1 280px;
  min-width: 200px;
  padding: 8px 12px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d9d9d9);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2b2b2b);
  font-size: 14px;
  font-family: inherit;
  transition: border-color 0.15s ease;
}
.search-input:focus {
  outline: none;
  border-color: var(--accent, #e95420);
}
.search-meta {
  font-size: 12.5px;
}

/* —— 主体两栏 —— */
.body-row {
  display: grid;
  grid-template-columns: 180px 1fr;
  gap: 18px;
  align-items: start;
}
@media (max-width: 860px) {
  .body-row {
    grid-template-columns: 1fr;
  }
}

/* —— 场景侧栏 —— */
.scene-sidebar {
  display: flex;
  flex-direction: column;
  gap: 8px;
  position: sticky;
  top: 12px;
}
.scene-list {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
@media (max-width: 860px) {
  .scene-list {
    flex-direction: row;
    flex-wrap: wrap;
  }
}
.scene-pill {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
  padding: 7px 12px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d9d9d9);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2b2b2b);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  transition: background 0.15s ease, border-color 0.15s ease;
}
.scene-pill:hover {
  background: rgba(0, 0, 0, 0.04);
}
.scene-pill.active {
  background: var(--accent-soft, rgba(233, 84, 32, 0.12));
  border-color: var(--accent, #e95420);
  color: var(--accent, #e95420);
  font-weight: 600;
}
.scene-count {
  font-size: 11px;
  background: rgba(0, 0, 0, 0.06);
  border-radius: 10px;
  padding: 1px 7px;
  color: var(--text-muted, #5e5c5f);
}
.scene-pill.active .scene-count {
  background: rgba(233, 84, 32, 0.2);
  color: var(--accent, #e95420);
}

/* —— 面板 —— */
.grid-wrap {
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-width: 0;
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
  color: var(--text, #2b2b2b);
}

/* —— 照片网格 —— */
.photo-grid {
  columns: 3;
  column-gap: 14px;
}
@media (max-width: 1100px) {
  .photo-grid {
    columns: 2;
  }
}
@media (max-width: 620px) {
  .photo-grid {
    columns: 1;
  }
}
.photo-card {
  break-inside: avoid;
  margin-bottom: 14px;
  border-radius: var(--radius-md, 12px);
  overflow: hidden;
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #d9d9d9);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
  transition: transform 0.16s ease, box-shadow 0.16s ease;
  cursor: pointer;
}
.photo-card:hover {
  transform: translateY(-2px);
  box-shadow: var(--shadow-lg, 0 6px 16px rgba(0, 0, 0, 0.12));
}
.thumb-wrap {
  position: relative;
}
.photo-card img {
  display: block;
  width: 100%;
  height: auto;
}
.photo-placeholder {
  width: 100%;
  aspect-ratio: 3 / 2;
  display: flex;
  align-items: center;
  justify-content: center;
  color: rgba(255, 255, 255, 0.85);
  font-size: 36px;
}
.photo-placeholder.lg {
  aspect-ratio: 16 / 10;
  font-size: 64px;
}
.scene-badge {
  position: absolute;
  top: 8px;
  left: 8px;
  font-size: 11px;
  padding: 2px 9px;
  border-radius: 10px;
  background: rgba(0, 0, 0, 0.55);
  color: #fff;
  backdrop-filter: blur(2px);
}
.scene-badge.unanalyzed {
  background: rgba(0, 0, 0, 0.4);
  color: rgba(255, 255, 255, 0.85);
}
.photo-caption {
  padding: 8px 12px 2px;
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 8px;
}
.photo-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text, #2b2b2b);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.photo-size {
  font-size: 11px;
  flex-shrink: 0;
}
.photo-desc {
  padding: 2px 12px 6px;
  font-size: 12px;
  line-height: 1.4;
}
.photo-tags {
  padding: 0 12px 10px;
  display: flex;
  flex-wrap: wrap;
  gap: 5px;
}
.tag-badge {
  font-size: 11px;
  padding: 2px 8px;
  border-radius: 10px;
  color: var(--text, #2b2b2b);
  background: rgba(0, 0, 0, 0.05);
  line-height: 1.5;
}
.scene-tag {
  background: var(--accent-soft, rgba(233, 84, 32, 0.14));
  color: var(--accent, #e95420);
  font-weight: 600;
}
.color-tag {
  background: rgba(0, 0, 0, 0.05);
}
.tag-badge.unanalyzed {
  background: rgba(0, 0, 0, 0.06);
  color: var(--text-muted, #5e5c5f);
}
.analyze-btn {
  border: 1px dashed var(--accent, #e95420);
  color: var(--accent, #e95420);
  background: transparent;
  cursor: pointer;
  font-family: inherit;
}
.analyze-btn:disabled {
  opacity: 0.6;
  cursor: not-allowed;
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
  color: var(--text, #2b2b2b);
}
.empty-hint {
  font-size: 13px;
}

/* —— 详情弹窗 —— */
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 1000;
  padding: 20px;
}
.modal {
  position: relative;
  max-width: 720px;
  width: 100%;
  max-height: 88vh;
  overflow-y: auto;
  border-radius: var(--radius-md, 12px);
  background: var(--bg-card, #ffffff);
}
.modal-close {
  position: absolute;
  top: 10px;
  right: 12px;
  width: 30px;
  height: 30px;
  border-radius: 50%;
  border: none;
  background: rgba(0, 0, 0, 0.45);
  color: #fff;
  font-size: 20px;
  line-height: 1;
  cursor: pointer;
  z-index: 2;
}
.modal-thumb img {
  display: block;
  width: 100%;
  max-height: 360px;
  object-fit: cover;
}
.modal-body {
  padding: 16px 20px 22px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.modal-title {
  font-size: 18px;
  font-weight: 700;
  color: var(--text, #2b2b2b);
}
.modal-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.modal-section-title {
  font-size: 12px;
  text-transform: uppercase;
  letter-spacing: 0.5px;
  color: var(--text-muted, #5e5c5f);
  font-weight: 600;
  margin-bottom: 5px;
}
.modal-desc {
  font-size: 14px;
  line-height: 1.6;
  color: var(--text, #2b2b2b);
  margin: 0;
}
.modal-path {
  font-family: var(--mono, monospace);
  font-size: 12px;
  background: rgba(0, 0, 0, 0.05);
  padding: 4px 8px;
  border-radius: 6px;
  word-break: break-all;
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

/* —— 卡片 / 按钮 —— */
.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #d9d9d9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.btn {
  padding: 6px 14px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2b2b2b);
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
  background: var(--accent, #e95420);
  color: #ffffff;
  border-color: var(--accent, #e95420);
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
  border-left: 4px solid var(--accent, #e95420);
  border-radius: var(--radius-md, 8px);
  padding: 10px 14px;
  font-size: 13px;
  color: var(--text, #2b2b2b);
}
.demo-banner code {
  font-family: var(--mono, monospace);
  font-size: 12.5px;
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 5px;
  border-radius: 4px;
}

@media (max-width: 720px) {
  .photo-page {
    padding: 16px;
  }
}
</style>
