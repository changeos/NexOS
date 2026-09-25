<script setup lang="ts">
// =============================================================================
// MarkdownView —— Markdown 渲染组件（marked GFM + DOMPurify 消毒，v0.1.32）。
//
// 与 DevDocs.vue / LlmModels.vue 的 marked 用法同源（gfm、同步渲染），但按
// v0.1.32 安全基线加 DOMPurify：NexHub 仓库内容可被任何持有 push token 的
// agent 写入，信任边界弱于内置文档——v-html 前一律消毒（防 XSS 落地）。
// 用途：仓库详情页 README.md 渲染。
// =============================================================================

import { computed } from 'vue';
import { marked } from 'marked';
import DOMPurify from 'dompurify';

const props = defineProps<{
  /** 原始 markdown 文本。 */
  source?: string | null;
}>();

// GFM（表格 / 任务列表 / 删除线）；同步渲染（async:false → string）。
marked.setOptions({ gfm: true, breaks: false, async: false });

/** markdown → 消毒后 HTML（DOMPurify 默认白名单；剥 script/事件属性/javascript: URI）。 */
const safeHtml = computed<string>(() => {
  if (!props.source) return '';
  const raw = marked.parse(props.source) as string;
  return DOMPurify.sanitize(raw);
});
</script>

<template>
  <!-- 消毒后 HTML 直插（marked GFM；XSS 面由 DOMPurify 白名单兜住） -->
  <div class="md-view" v-html="safeHtml" />
</template>

<style scoped>
/* GitHub 风 README 排版（scoped 深度选择器覆盖 marked 产出的子元素） */
.md-view {
  font-size: 14px;
  line-height: 1.65;
  color: var(--text, #2B2B2B);
  word-break: break-word;
}
.md-view :deep(h1),
.md-view :deep(h2),
.md-view :deep(h3),
.md-view :deep(h4) {
  margin: 18px 0 8px;
  font-weight: 700;
  line-height: 1.3;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  padding-bottom: 6px;
}
.md-view :deep(h1) { font-size: 20px; }
.md-view :deep(h2) { font-size: 17px; }
.md-view :deep(h3) { font-size: 15px; }
.md-view :deep(h4) { font-size: 14px; border-bottom: none; }
.md-view :deep(p) { margin: 8px 0; }
.md-view :deep(a) { color: var(--accent, #E95420); }
.md-view :deep(ul),
.md-view :deep(ol) { margin: 8px 0; padding-left: 24px; }
.md-view :deep(li) { margin: 3px 0; }
.md-view :deep(code) {
  font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 12.5px;
  background: var(--bg-code, #f6f6f6);
  padding: 1px 5px;
  border-radius: 4px;
}
.md-view :deep(pre) {
  margin: 10px 0;
  padding: 12px 14px;
  background: #26292F;
  color: #E8E4E8;
  border-radius: var(--radius-sm, 8px);
  overflow: auto;
}
.md-view :deep(pre code) {
  background: transparent;
  color: inherit;
  padding: 0;
  font-size: 12.5px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-word;
}
.md-view :deep(blockquote) {
  margin: 10px 0;
  padding: 4px 14px;
  border-left: 3px solid var(--border, #D9D9D9);
  color: var(--text-muted, #5E5C5F);
}
.md-view :deep(table) {
  border-collapse: collapse;
  margin: 10px 0;
  width: 100%;
  font-size: 13px;
}
.md-view :deep(th),
.md-view :deep(td) {
  border: 1px solid var(--border, #D9D9D9);
  padding: 5px 10px;
  text-align: left;
}
.md-view :deep(th) { background: var(--border-soft, #F3F4F6); }
.md-view :deep(img) { max-width: 100%; }
.md-view :deep(hr) { border: none; border-top: 1px solid var(--border-soft, #EDEDED); margin: 14px 0; }
</style>

