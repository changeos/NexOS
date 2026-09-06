<script setup lang="ts">
// =============================================================================
// DevDocs.vue —— 开发者中心（文档门户）
//
// 架构：文档唯一事实源 = 仓库 docs/（随代码演进，git push 即更新——
//   post-receive 钩子已自动化）。本应用只是**渲染与服务层**：
//   GET /api/v1/devdocs/index  → 左侧目录树（分类分组 + 搜索过滤）
//   GET /api/v1/devdocs/doc/*p → 右侧 Markdown 渲染（marked）
//
// 文档语言切换（AI 翻译，本地 LLM 管线，docs/DEVDOCS_DEV_CENTER.md）：
//   - 中文原文：零开销直读；
//   - English / 繁體中文：?lang= 请求——缓存命中即时渲染；未命中后端起异步
//     翻译任务（202），本视图轮询 GET /devdocs/translate/tasks/:id 展示
//     「AI 翻译生成中 · 块 i/N」进度，done 后重取命中缓存；
//   - 无本地模型节点：后端 503 + 降级文案（「中文原文可用」），本视图展示
//     降级提示 + 重试按钮（?retry=1）；联邦节点的任务 404 回退定时重取。
//   文档语言独立于界面语言（chrome 走 vue-i18n 四语言；内容语言是文档维度）。
//
// 信任模型：文档源是自家仓库可信内容（docs/ 受「功能文档同步铁律」约束，
// 非任意用户输入），marked 渲染结果直插 v-html；既有依赖无 dompurify，
// 不新增——XSS 面仅限"仓库作者自身"，与直接读源码等价。
//
// 布局：顶部说明条（事实源声明 + 根路径 + 文档语言切换）+ 左侧目录树 +
// 右侧文档区（标题 / 源路径 + mtime「随仓库更新」/ 正文）。代码块深底等宽
// 样式复用 CodeHub 接入说明 Tab 的 ob-pre 风格。
// =============================================================================
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { marked } from 'marked';
import {
  endpoints,
  type DevDocEntry,
  type DevDocsIndexResp,
  type DevDocResp,
  type DevDocsTranslateTask,
  devdocsDocLang,
  devdocsTranslateTask,
  ApiError,
} from '@/api/client';

const { t } = useI18n();

// marked：GFM（表格/任务列表/删除线）；同步渲染（async:false → string）。
marked.setOptions({ gfm: true, breaks: false, async: false });

// =============================================================================
// 索引加载 + 目录树分组
// =============================================================================
const index = ref<DevDocsIndexResp | null>(null);
const indexLoading = ref(false);
const indexError = ref('');

/** 搜索关键词（标题/路径/分类 过滤，大小写不敏感）。 */
const query = ref('');

/** 当前选中文档相对路径。 */
const currentPath = ref('');

/** 当前文档（原文或译文——二者同形 DevDocResp）。 */
const current = ref<DevDocResp | null>(null);
const docLoading = ref(false);
const docError = ref('');

// =============================================================================
// 文档语言（内容翻译维度，独立于界面语言；持久化 localStorage）
// =============================================================================
type DocLang = 'zh' | 'en' | 'zh-TW';

const DOC_LANG_STORAGE_KEY = 'devdocs.docLang';

function restoreDocLang(): DocLang {
  try {
    const v = window.localStorage.getItem(DOC_LANG_STORAGE_KEY);
    if (v === 'zh' || v === 'en' || v === 'zh-TW') return v;
  } catch {
    /* 隐私模式等 localStorage 不可用：仅本次会话生效 */
  }
  return 'zh';
}

const docLang = ref<DocLang>(restoreDocLang());

function setDocLang(lang: DocLang): void {
  if (docLang.value === lang) return;
  docLang.value = lang;
  try {
    window.localStorage.setItem(DOC_LANG_STORAGE_KEY, lang);
  } catch {
    /* 忽略持久化失败 */
  }
  if (currentPath.value) void fetchDoc(currentPath.value, false);
}

// =============================================================================
// 翻译任务态（202 → 轮询 → done 重取；error → 降级提示）
// =============================================================================
const translateTask = ref<DevDocsTranslateTask | null>(null);
/** 503/任务失败的服务端降级文案（含「中文原文可用」提示）。 */
const translateDegrade = ref('');

let pollTimer: number | null = null;
/** 轮询代次：换文档/切语言后旧轮询立即失效。 */
let pollGeneration = 0;
/** 联邦源节点任务 404 的回退重取计数（上限防打转）。 */
let federatedRetries = 0;

function stopPolling(): void {
  if (pollTimer !== null) {
    window.clearTimeout(pollTimer);
    pollTimer = null;
  }
}

function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

async function loadIndex(): Promise<void> {
  indexLoading.value = true;
  indexError.value = '';
  try {
    index.value = await endpoints.devdocsIndex();
  } catch (e) {
    index.value = null;
    indexError.value = friendlyError(e);
  } finally {
    indexLoading.value = false;
  }
}

/** 过滤后的目录树：分类 → 文档列表（搜索命中标题/路径/分类）。 */
const tree = computed<{ category: string; docs: DevDocEntry[] }[]>(() => {
  const idx = index.value;
  if (!idx) return [];
  const q = query.value.trim().toLowerCase();
  const matched = q
    ? idx.docs.filter(
        (d) =>
          d.title.toLowerCase().includes(q) ||
          d.path.toLowerCase().includes(q) ||
          d.category.toLowerCase().includes(q),
      )
    : idx.docs;
  return idx.categories
    .map((category) => ({
      category,
      docs: matched.filter((d) => d.category === category),
    }))
    .filter((g) => g.docs.length > 0);
});

const totalDocs = computed(() => index.value?.docs.length ?? 0);
const matchedDocs = computed(() =>
  tree.value.reduce((n, g) => n + g.docs.length, 0),
);

// =============================================================================
// 文档加载（中文原文直读 / 目标语言走翻译管线）+ Markdown 渲染
// =============================================================================
async function openDoc(path: string): Promise<void> {
  if (path === currentPath.value && current.value) return;
  await fetchDoc(path, false);
}

/** 失败后的强制重试（带 ?retry=1——清除服务端失败态重新翻译）。 */
async function retryTranslation(): Promise<void> {
  if (currentPath.value) await fetchDoc(currentPath.value, true);
}

async function fetchDoc(path: string, retry: boolean): Promise<void> {
  currentPath.value = path;
  docLoading.value = true;
  docError.value = '';
  translateDegrade.value = '';
  translateTask.value = null;
  stopPolling();
  current.value = null;
  federatedRetries = 0;
  try {
    if (docLang.value === 'zh') {
      current.value = await endpoints.devdocsDoc(path);
    } else {
      const res = await devdocsDocLang(path, docLang.value, retry);
      if (res.kind === 'doc') {
        current.value = res.doc;
      } else {
        // 202：翻译任务进行中——进入轮询（内容区展示进度）。
        translateTask.value = res.task;
        docLoading.value = false;
        schedulePoll(res.task.id);
        return;
      }
    }
  } catch (e) {
    if (e instanceof ApiError && e.status === 503) {
      // 诚实降级：无本地模型 / 无凭据 / 并发满——服务端文案已含指引。
      translateDegrade.value = friendlyError(e);
    } else {
      docError.value = friendlyError(e);
    }
  } finally {
    docLoading.value = false;
  }
}

/** 轮询翻译任务：done → 重取命中缓存；error → 降级；404（联邦源任务不在
 * 本节点）→ 定时重取文档本身（上限 20 次 ≈ 40s，防打转）。 */
function schedulePoll(taskId: string): void {
  const gen = ++pollGeneration;
  const tick = async (): Promise<void> => {
    if (gen !== pollGeneration) return;
    try {
      const task = await devdocsTranslateTask(taskId);
      translateTask.value = task;
      if (task.status === 'done') {
        if (currentPath.value) void fetchDoc(currentPath.value, false);
        return;
      }
      if (task.status === 'error') {
        translateTask.value = null;
        translateDegrade.value =
          task.error ?? t('devdocs.translateFailed');
        return;
      }
    } catch (e) {
      if (e instanceof ApiError && e.status === 404 && federatedRetries < 20) {
        // 联邦源节点的任务不在本节点任务表：直接重取文档（源节点完成后
        // 会 200 命中其缓存；期间源节点 202 会带回新任务继续走本分支）。
        federatedRetries += 1;
        if (currentPath.value) void fetchDoc(currentPath.value, false);
        return;
      }
      /* 其他错误（网络抖动）：继续轮询 */
    }
    pollTimer = window.setTimeout(tick, 2000);
  };
  pollTimer = window.setTimeout(tick, 2000);
}

/** Markdown → HTML（marked 同步渲染；信任模型见文件头注释）。 */
const renderedHtml = computed<string>(() => {
  if (!current.value) return '';
  return marked.parse(current.value.markdown) as string;
});

/** 字节数人类可读（目录树 tooltip 展示用）。 */
function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

onMounted(() => {
  void loadIndex().then(() => {
    // 首篇：优先开发者指南索引（docs/dev/README.md），否则第一篇。
    const docs = index.value?.docs ?? [];
    const first =
      docs.find((d) => d.path === 'dev/README.md') ?? docs[0];
    if (first) void openDoc(first.path);
  });
});

onUnmounted(stopPolling);
</script>

<template>
  <div class="devdocs-page">
    <!-- ============ 顶部说明条：事实源声明 + 文档语言切换 ============ -->
    <div class="page-head">
      <div>
        <h2>{{ t('devdocs.title') }}</h2>
        <p class="devdocs-banner">
          {{ t('devdocs.banner') }}
          <template v-if="index?.root">
            ——{{ t('devdocs.bannerRoot', { root: index.root }) }}
          </template>
        </p>
      </div>
      <div class="devdocs-meta">
        <!-- 文档语言（内容翻译维度）：中文原文 / English / 繁體中文 -->
        <div class="devdocs-lang" :title="t('devdocs.langLabel')">
          <button
            v-for="l in [
              { value: 'zh', label: t('devdocs.langOriginal') },
              { value: 'en', label: t('devdocs.langEn') },
              { value: 'zh-TW', label: t('devdocs.langTw') },
            ]"
            :key="l.value"
            type="button"
            class="devdocs-lang-btn"
            :class="{ active: docLang === l.value }"
            @click="setDocLang(l.value as 'zh' | 'en' | 'zh-TW')"
          >
            {{ l.label }}
          </button>
        </div>
        <span class="devdocs-count" :title="t('devdocs.docCountTitle')">{{
          t('devdocs.docCount', { n: totalDocs })
        }}</span>
        <button class="btn" type="button" :disabled="indexLoading" @click="loadIndex()">
          {{ indexLoading ? t('devdocs.refreshing') : t('devdocs.refresh') }}
        </button>
      </div>
    </div>

    <!-- 降级提示（无 checkout 节点：113/aliyun 等不 crash，指回主节点） -->
    <div v-if="index && !index.source_available" class="devdocs-degraded">
      {{ index.note ?? t('devdocs.degradedTitle') }}——{{ t('devdocs.degradedHint') }}
    </div>

    <div v-if="indexError" class="devdocs-degraded">
      {{ t('devdocs.indexError', { msg: indexError }) }}
    </div>

    <!-- ============ 主体：左目录树 + 右文档 ============ -->
    <div class="devdocs-body">
      <!-- —— 左：目录树（搜索 + 分类分组）—— -->
      <aside class="devdocs-sidebar">
        <div class="devdocs-search">
          <input
            v-model="query"
            class="devdocs-search-input"
            type="search"
            :placeholder="t('devdocs.searchPlaceholder')"
          />
          <div v-if="query.trim() && index" class="devdocs-search-hint">
            {{ t('devdocs.searchHint', { matched: matchedDocs, total: totalDocs }) }}
          </div>
        </div>
        <div class="devdocs-tree">
          <section v-for="group in tree" :key="group.category" class="devdocs-group">
            <h3 class="devdocs-group-title">{{ group.category }}</h3>
            <button
              v-for="d in group.docs"
              :key="d.path"
              type="button"
              class="devdocs-item"
              :class="{ active: d.path === currentPath }"
              :title="`${d.title}（${d.path} · ${humanSize(d.size)}）`"
              @click="openDoc(d.path)"
            >
              <span class="devdocs-item-title">{{ d.title }}</span>
              <span class="devdocs-item-sub">{{ d.path }}</span>
            </button>
          </section>
          <p v-if="!indexLoading && tree.length === 0 && index" class="devdocs-empty">
            {{ query.trim() ? t('devdocs.noMatch') : t('devdocs.emptyTree') }}
          </p>
          <p v-if="indexLoading" class="devdocs-empty">{{ t('devdocs.treeLoading') }}</p>
        </div>
      </aside>

      <!-- —— 右：文档区（标题/源路径/mtime + Markdown 渲染 / 翻译进度 / 降级）—— -->
      <main class="devdocs-content">
        <template v-if="docLoading">
          <p class="devdocs-empty">{{ t('devdocs.docLoading') }}</p>
        </template>
        <template v-else-if="docError">
          <div class="devdocs-degraded">{{ t('devdocs.docError', { msg: docError }) }}</div>
        </template>

        <!-- AI 翻译生成中：进度轮询（块 i/N + 最近日志行） -->
        <template v-else-if="translateTask">
          <div class="devdocs-translate-box">
            <p class="devdocs-translate-title">⚙️ {{ t('devdocs.translating') }}</p>
            <p class="devdocs-translate-progress">
              {{ t('devdocs.translateProgress', { done: translateTask.chunks_done, total: translateTask.chunks_total }) }}
            </p>
            <p v-if="translateTask.log.length" class="devdocs-translate-log">
              {{ translateTask.log[translateTask.log.length - 1] }}
            </p>
          </div>
        </template>

        <!-- 翻译不可用（无本地模型等）：服务端诚实降级文案 + 重试/回中文 -->
        <template v-else-if="translateDegrade">
          <div class="devdocs-degraded">{{ translateDegrade }}</div>
          <div class="devdocs-translate-actions">
            <button class="btn" type="button" @click="retryTranslation()">
              {{ t('devdocs.translateRetry') }}
            </button>
            <button class="btn" type="button" @click="setDocLang('zh')">
              {{ t('devdocs.langOriginal') }}
            </button>
          </div>
          <p class="devdocs-empty">{{ t('devdocs.translateFallbackHint') }}</p>
        </template>

        <template v-else-if="current">
          <header class="devdocs-doc-head">
            <h1>{{ current.title }}</h1>
            <p class="devdocs-doc-meta">
              {{ t('devdocs.srcPath') }} <code>{{ current.path }}</code>
              <template v-if="current.mtime">
                · {{ t('devdocs.updatedOn', { mtime: current.mtime }) }}
              </template>
              <template v-if="docLang !== 'zh'">
                · <span class="devdocs-ai-badge">{{ t('devdocs.aiTranslated') }}</span>
              </template>
            </p>
          </header>
          <!-- eslint-disable-next-line vue/no-v-html —— 信任模型见文件头注释 -->
          <article class="devdocs-markdown" v-html="renderedHtml"></article>
        </template>
        <p v-else class="devdocs-empty">{{ t('devdocs.pickDoc') }}</p>
      </main>
    </div>
  </div>
</template>

<style scoped>
/* ===== 页面骨架（WindowFrame 内全高）===== */
.devdocs-page {
  height: 100%;
  display: flex;
  flex-direction: column;
  gap: 12px;
  min-height: 0;
}
.devdocs-banner {
  margin: 4px 0 0;
  font-size: 12.5px;
  color: var(--text-muted, #5e5c5f);
}
.devdocs-banner code,
.devdocs-doc-meta code {
  padding: 1px 6px;
  border-radius: 6px;
  background: var(--bg-code, #fafafa);
  color: var(--accent, #e95420);
  font-size: 11.5px;
}
.devdocs-meta {
  display: flex;
  align-items: center;
  gap: 10px;
}
.devdocs-count {
  font-size: 12px;
  color: var(--text-muted, #5e5c5f);
  white-space: nowrap;
}

/* 文档语言切换（分段按钮） */
.devdocs-lang {
  display: flex;
  border: 1px solid var(--border, #d9d9d9);
  border-radius: 8px;
  overflow: hidden;
}
.devdocs-lang-btn {
  padding: 5px 10px;
  border: none;
  background: var(--bg-card, #fff);
  color: var(--text-muted, #5e5c5f);
  font-size: 12px;
  cursor: pointer;
}
.devdocs-lang-btn + .devdocs-lang-btn {
  border-left: 1px solid var(--border, #d9d9d9);
}
.devdocs-lang-btn.active {
  background: rgba(233, 84, 32, 0.12);
  color: var(--accent, #e95420);
  font-weight: 600;
}

/* 降级/错误提示条 */
.devdocs-degraded {
  padding: 10px 14px;
  border-radius: 8px;
  background: var(--border-soft, #f3f4f6);
  font-size: 13px;
  line-height: 1.6;
  color: var(--text, #2b2b2b);
}

/* ===== 主体双栏 ===== */
.devdocs-body {
  flex: 1;
  min-height: 0;
  display: flex;
  gap: 12px;
}

/* —— 左：目录树 —— */
.devdocs-sidebar {
  width: 260px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-height: 0;
}
.devdocs-search {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.devdocs-search-input {
  width: 100%;
  padding: 8px 10px;
  border: 1px solid var(--border, #d9d9d9);
  border-radius: 8px;
  font-size: 13px;
  background: var(--bg-card, #fff);
  color: var(--text, #2b2b2b);
}
.devdocs-search-hint {
  font-size: 11px;
  color: var(--text-muted, #5e5c5f);
  padding-left: 4px;
}
.devdocs-tree {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 10px;
  border: 1px solid var(--border, #d9d9d9);
  border-radius: 10px;
  background: var(--bg-card, #fff);
}
.devdocs-group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.devdocs-group-title {
  margin: 0;
  font-size: 11px;
  font-weight: 700;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--text-muted, #5e5c5f);
  padding: 0 4px;
}
.devdocs-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  text-align: left;
  padding: 7px 9px;
  border: none;
  border-radius: 8px;
  background: transparent;
  cursor: pointer;
}
.devdocs-item:hover {
  background: var(--border-soft, #f3f4f6);
}
.devdocs-item.active {
  background: rgba(233, 84, 32, 0.1);
}
.devdocs-item-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text, #2b2b2b);
  line-height: 1.4;
}
.devdocs-item.active .devdocs-item-title {
  color: var(--accent, #e95420);
}
.devdocs-item-sub {
  font-size: 10.5px;
  color: var(--text-muted, #5e5c5f);
  font-family: 'Ubuntu Mono', Consolas, monospace;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}

/* —— 右：文档区 —— */
.devdocs-content {
  flex: 1;
  min-width: 0;
  overflow-y: auto;
  padding: 16px 22px 28px;
  border: 1px solid var(--border, #d9d9d9);
  border-radius: 10px;
  background: var(--bg-card, #fff);
}
.devdocs-doc-head {
  border-bottom: 1px solid var(--border-soft, #f3f4f6);
  padding-bottom: 10px;
  margin-bottom: 14px;
}
.devdocs-doc-head h1 {
  margin: 0;
  font-size: 20px;
  color: var(--text, #2b2b2b);
}
.devdocs-doc-meta {
  margin: 6px 0 0;
  font-size: 12px;
  color: var(--text-muted, #5e5c5f);
}
.devdocs-ai-badge {
  display: inline-block;
  padding: 1px 8px;
  border-radius: 999px;
  background: rgba(233, 84, 32, 0.1);
  color: var(--accent, #e95420);
  font-size: 11px;
  font-weight: 600;
}
.devdocs-empty {
  margin: 24px 0;
  text-align: center;
  font-size: 13px;
  color: var(--text-muted, #5e5c5f);
}

/* AI 翻译进度盒 */
.devdocs-translate-box {
  margin: 32px auto;
  max-width: 420px;
  padding: 18px 22px;
  border: 1px solid var(--border, #d9d9d9);
  border-radius: 10px;
  text-align: center;
}
.devdocs-translate-title {
  margin: 0 0 8px;
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2b2b2b);
}
.devdocs-translate-progress {
  margin: 0 0 6px;
  font-size: 13px;
  color: var(--accent, #e95420);
  font-variant-numeric: tabular-nums;
}
.devdocs-translate-log {
  margin: 0;
  font-size: 11.5px;
  color: var(--text-muted, #5e5c5f);
  font-family: 'Ubuntu Mono', Consolas, monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.devdocs-translate-actions {
  display: flex;
  justify-content: center;
  gap: 10px;
  margin-top: 12px;
}

/* ===== Markdown 渲染（scoped 下 v-html 内容需 :deep）=====
   代码块等宽深底（复用 CodeHub 接入说明 ob-pre 风格）。 */
.devdocs-markdown {
  font-size: 14px;
  line-height: 1.75;
  color: var(--text, #2b2b2b);
  overflow-wrap: break-word;
}
.devdocs-markdown :deep(h1),
.devdocs-markdown :deep(h2),
.devdocs-markdown :deep(h3),
.devdocs-markdown :deep(h4) {
  margin: 22px 0 10px;
  line-height: 1.35;
  color: var(--text, #2b2b2b);
}
.devdocs-markdown :deep(h1) { font-size: 20px; }
.devdocs-markdown :deep(h2) { font-size: 17px; border-bottom: 1px solid var(--border-soft, #f3f4f6); padding-bottom: 6px; }
.devdocs-markdown :deep(h3) { font-size: 15px; }
.devdocs-markdown :deep(h4) { font-size: 14px; }
.devdocs-markdown :deep(p) { margin: 10px 0; }
.devdocs-markdown :deep(ul),
.devdocs-markdown :deep(ol) { margin: 10px 0; padding-left: 24px; }
.devdocs-markdown :deep(li) { margin: 4px 0; }
.devdocs-markdown :deep(a) { color: var(--accent, #e95420); text-decoration: none; }
.devdocs-markdown :deep(a:hover) { text-decoration: underline; }
.devdocs-markdown :deep(blockquote) {
  margin: 12px 0;
  padding: 8px 14px;
  border-left: 3px solid var(--accent, #e95420);
  background: var(--border-soft, #f3f4f6);
  border-radius: 0 8px 8px 0;
  color: var(--text-muted, #5e5c5f);
}
.devdocs-markdown :deep(code) {
  padding: 1px 6px;
  border-radius: 6px;
  background: var(--bg-code, #fafafa);
  color: var(--accent, #e95420);
  font-family: 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace;
  font-size: 12.5px;
}
.devdocs-markdown :deep(pre) {
  margin: 12px 0;
  padding: 12px 14px;
  border-radius: 8px;
  background: #26292f; /* 深底（ob-pre 同款） */
  overflow-x: auto;
}
.devdocs-markdown :deep(pre code) {
  padding: 0;
  background: transparent;
  color: #e8e4e8; /* 深底上的浅字 */
  font-size: 12.5px;
  line-height: 1.55;
  white-space: pre;
}
.devdocs-markdown :deep(table) {
  border-collapse: collapse;
  margin: 12px 0;
  font-size: 13px;
  width: 100%;
}
.devdocs-markdown :deep(th),
.devdocs-markdown :deep(td) {
  border: 1px solid var(--border, #d9d9d9);
  padding: 6px 10px;
  text-align: left;
}
.devdocs-markdown :deep(th) { background: var(--border-soft, #f3f4f6); }
.devdocs-markdown :deep(hr) {
  border: none;
  border-top: 1px solid var(--border, #d9d9d9);
  margin: 18px 0;
}
.devdocs-markdown :deep(img) { max-width: 100%; }

/* 窄窗（WindowFrame 缩放）时目录树收窄 */
@media (max-width: 720px) {
  .devdocs-sidebar { width: 190px; }
}
</style>
