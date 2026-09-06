<script setup lang="ts">
// =============================================================================
// RepoCodeTab —— 仓库详情 Code Tab（v0.1.32，原「代码浏览」归组进仓库域）。
//
// 文件树（可展开目录 + 缩进）+ 文件内容面板；进入时自动尝试渲染根 README.md
// （marked GFM + DOMPurify 消毒后 v-html——MarkdownView 组件）。README.md 在
// 内容面板中以渲染态展示，其余文件保持原始 <pre> 展示。
// =============================================================================

import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, type FileTreeNode, type FlatNode } from '@/views/nexhub/model';
import MarkdownView from '@/views/nexhub/components/MarkdownView.vue';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

const tree = ref<FileTreeNode[]>([]);
const branches = ref<string[]>([]);
const defaultBranch = ref('');
const selectedPath = ref('');
const fileContent = ref('');
const fileLoading = ref(false);
const expandedDirs = ref<Set<string>>(new Set());

/** 根 README.md 路径（自动渲染入口）。 */
const README_PATH = 'README.md';
/** 当前选中的是根 README.md（内容面板走 markdown 渲染而非裸 <pre>）。 */
const viewingReadme = computed(() => selectedPath.value === README_PATH);

/** 加载文件树；成功后自动选中根 README.md（无 README 则空态）。 */
async function loadTree(): Promise<void> {
  if (!props.repoName.trim()) return;
  fileContent.value = '';
  selectedPath.value = '';
  try {
    const r = (await endpoints.codeRepoContents(props.repoName.trim())) as {
      tree?: FileTreeNode[];
      branches?: string[];
      default_branch?: string;
    };
    tree.value = r.tree ?? [];
    branches.value = r.branches ?? [];
    defaultBranch.value = r.default_branch ?? '';
    expandedDirs.value = new Set();
    // README 主角（GitHub 风）：默认打开渲染
    if (tree.value.some((n) => n.path === README_PATH && !n.is_dir)) {
      await openFile(README_PATH);
    }
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.code.treeLoadFailed')}: ${errMsg(e)}`);
    tree.value = [];
  }
}

async function openFile(path: string): Promise<void> {
  selectedPath.value = path;
  fileLoading.value = true;
  try {
    const r = (await endpoints.codeRepoFile(props.repoName.trim(), path)) as {
      content?: string;
      ok?: boolean;
    };
    fileContent.value = r.content ?? '';
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.code.fileLoadFailed')}: ${errMsg(e)}`);
    fileContent.value = '';
  } finally {
    fileLoading.value = false;
  }
}

function toggleDir(path: string): void {
  const s = new Set(expandedDirs.value);
  if (s.has(path)) {
    s.delete(path);
  } else {
    s.add(path);
  }
  expandedDirs.value = s;
}

function childrenOf(all: FileTreeNode[], dirPath: string): FileTreeNode[] {
  return all.filter((n) => {
    const p = n.path ?? '';
    if (!p.startsWith(dirPath + '/')) return false;
    const rest = p.slice(dirPath.length + 1);
    return !rest.includes('/');
  });
}

/** 树 → 拍平缩进列表（目录优先、名称排序；展开目录递归下钻）。 */
const flatTree = computed<FlatNode[]>(() => {
  const out: FlatNode[] = [];
  const all = tree.value;
  const roots = all.filter((n) => {
    const p = n.path ?? '';
    return !p.includes('/');
  });
  const walk = (nodes: FileTreeNode[], depth: number) => {
    const sorted = [...nodes].sort((a, b) => {
      if (a.is_dir && !b.is_dir) return -1;
      if (!a.is_dir && b.is_dir) return 1;
      return (a.name ?? '').localeCompare(b.name ?? '');
    });
    for (const n of sorted) {
      out.push({ node: n, depth });
      if (n.is_dir && n.path && expandedDirs.value.has(n.path)) {
        walk(childrenOf(all, n.path), depth + 1);
      }
    }
  };
  walk(roots, 0);
  return out;
});

// 仓库切换：重载文件树
watch(() => props.repoName, () => void loadTree(), { immediate: true });
</script>

<template>
  <section class="code-tab">
    <div class="browser-toolbar">
      <span v-if="defaultBranch" class="muted small branch-chip">
        {{ t('nexhub.code.defaultBranch') }}: {{ defaultBranch }}
        <template v-if="branches.length">（{{ branches.join(', ') }}）</template>
      </span>
      <span v-if="fileLoading" class="muted small">{{ t('common.loading') }}</span>
    </div>

    <div v-if="!props.repoName" class="card empty-card">{{ t('nexhub.code.noRepo') }}</div>
    <div v-else-if="tree.length === 0" class="card empty-card">
      {{ t('nexhub.code.emptyRepo') }}
    </div>
    <div v-else class="browser-layout">
      <aside class="card tree-panel">
        <div class="panel-head">
          <span class="panel-title">{{ t('nexhub.code.treeTitle') }}</span>
          <span class="muted small">{{ tree.length }}</span>
        </div>
        <div v-if="tree.length === 0" class="empty-tree muted small">（{{ t('nexhub.code.empty') }}）</div>
        <div v-else class="tree-list">
          <button
            v-for="(item, idx) in flatTree"
            :key="(item.node.path ?? '') + idx"
            class="tree-btn"
            :class="{
              selected: selectedPath === item.node.path,
              'is-dir': item.node.is_dir,
            }"
            :style="{ paddingLeft: `${item.depth * 14 + 8}px` }"
            type="button"
            @click="item.node.is_dir
              ? toggleDir(item.node.path ?? '')
              : openFile(item.node.path ?? '')"
          >
            <span class="tree-icon" aria-hidden="true">
              {{ item.node.is_dir
                ? (expandedDirs.has(item.node.path ?? '') ? '▾' : '▸')
                : '📄' }}
            </span>
            <span class="tree-name">{{ item.node.name ?? item.node.path }}</span>
          </button>
        </div>
      </aside>

      <div class="card content-panel">
        <div class="panel-head">
          <span class="panel-title">
            {{ selectedPath || t('nexhub.code.pickFile') }}
          </span>
          <span v-if="viewingReadme" class="meta-chip">README</span>
        </div>
        <!-- README.md → markdown 渲染（marked + DOMPurify 消毒后 v-html） -->
        <MarkdownView v-if="viewingReadme && fileContent" :source="fileContent" class="readme-body" />
        <pre v-else-if="fileContent" class="code-block"><code>{{ fileContent }}</code></pre>
        <div v-else class="empty-content muted small">
          {{ t('nexhub.code.pickFileHint') }}
        </div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.code-tab { display: flex; flex-direction: column; gap: 12px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.browser-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.branch-chip { padding: 2px 8px; background: var(--border-soft, #F3F4F6); border-radius: var(--radius-sm, 6px); }
.meta-chip {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; color: var(--accent, #E95420); background: rgba(233, 84, 32, 0.1); font-weight: 600;
}
.browser-layout { display: grid; grid-template-columns: 280px 1fr; gap: 14px; align-items: start; }
.tree-panel { padding: 0; max-height: 60vh; overflow: auto; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; padding: 12px 16px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.empty-tree { padding: 16px; }
.tree-list { padding: 4px 0 8px; display: flex; flex-direction: column; }
.tree-btn {
  display: flex; align-items: center; gap: 6px; width: 100%; text-align: left;
  background: transparent; border: none; padding: 4px 12px; font-size: 13px;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit;
}
.tree-btn:hover { background: var(--border-soft, #F3F4F6); }
.tree-btn.selected { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); font-weight: 600; }
.tree-icon { font-size: 12px; width: 14px; display: inline-block; flex-shrink: 0; }
.tree-name { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.tree-btn.is-dir .tree-name { font-weight: 600; }
.content-panel { padding: 0; min-height: 300px; display: flex; flex-direction: column; }
.empty-content { padding: 24px; flex: 1; display: flex; align-items: center; justify-content: center; }
.code-block {
  margin: 0; padding: 14px 16px; flex: 1; overflow: auto; font-family: 'Ubuntu Mono', 'Cascadia Code',
  Consolas, monospace; font-size: 12.5px; line-height: 1.55; color: var(--text, #2B2B2B);
  background: var(--bg-code, #fafafa); white-space: pre-wrap; word-break: break-word;
}
.readme-body { padding: 16px 20px; overflow: auto; }

@media (max-width: 720px) {
  .browser-layout { grid-template-columns: 1fr; }
  .tree-panel { max-height: 40vh; }
}
</style>

