<script setup lang="ts">
// =============================================================================
// RepoCard —— 仓库卡（v0.1.32 P1 Explore 列表页）。
//
// 普通仓库：名称 / 描述 / 分支数 / 最近提交 / 大小 + clone URL 复制 + 快捷入口
// （浏览 Code / 提交 / Issues / PR —— 均跳仓库详情页对应 Tab）。
// 应用仓库（nexos-app-*，前端 join /api/v1/apps/catalog）：额外渲染 manifest
// 徽章（icon / 版本 / 类别 / 引擎）与一键部署 DeployButton。
// =============================================================================

import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { copyText } from '@/utils/clipboard';
import { runtimeIcons } from '@/appRegistry';
import AppIcon from '@/components/AppIcon.vue';
import { useNexhub } from '@/views/nexhub/context';
import type { RepoDetailTab } from '@/views/nexhub/context';
import { formatBytes, isAppRepo, type Repo } from '@/views/nexhub/model';
import DeployButton from '@/views/nexhub/components/DeployButton.vue';
import CiBadge from '@/views/nexhub/components/CiBadge.vue';

const props = defineProps<{
  /** 裸仓库（GET /api/v1/coderepo/repos 元素）。 */
  repo: Repo;
}>();

const { t } = useI18n();
const ctx = useNexhub();

const name = computed(() => props.repo.name ?? '');

/** 是否应用仓库（nexos-app-* 前缀，与后端 CATALOG_REPO_PREFIX 同约定）。 */
const isApp = computed(() => isAppRepo(name.value));

/** join 目录条目（应用仓库的 manifest 徽章数据源；普通仓库为 undefined）。 */
const appEntry = computed(() => (isApp.value ? ctx.deploy.catalogEntry(name.value) : undefined));

// —— 卡头应用图标（manifest.icon 双协议统一渲染，v0.1.34 修裸 SVG/裸词溢出）——
//   含 '<' → SVG 内联标记（qrtransfer/streaming manifest 风格）：净化后 v-html
//           进 24x24 stroke 壳（与 AppIcon / appRuntime registerRuntimeIcon 同构）；
//   否则    → 图标名：按本名查 runtimeIcons，未命中且应用已装则回退按 id
//           （entry.js registerApp 以 id 注册），再交 AppIcon 走
//           ICONS → runtimeIcons → fallback 兜底链；
//   未命中 / 无 icon → 通用 package 盒子图标——绝不把原始字符串当文本输出。
//   （manifest 来自本机 NexHub 仓，与 appRuntime 同信任级，本可直 v-html；
//   净化层仅纵深防御。）

/** 通用应用包盒子图标（lucide package 风，24x24 stroke 内部标记）。 */
const PKG_ICON_INNER =
  '<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/>' +
  '<path d="M3.3 7l8.7 5 8.7-5"/>' +
  '<path d="M7.5 4.27l9 5.15"/>' +
  '<path d="M12 22V12"/>';

/**
 * SVG 内联标记净化（简单 regex 纵深防御）：剥 script/iframe/foreignObject 等
 * 宿主标签与注释、on* 事件属性、javascript: 链接。纯形状标记
 * （rect/path/circle 及其样式属性）不受影响。
 */
function sanitizeSvgInner(raw: string): string {
  return raw
    .replace(/<script\b[\s\S]*?<\/script\s*>/gi, '')
    .replace(/<\/?(?:iframe|object|embed|foreignObject)\b[^>]*>/gi, '')
    .replace(/<!--[\s\S]*?-->/g, '')
    .replace(/\son[a-z]+\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/gi, '')
    .replace(/((?:xlink:)?href\s*=\s*)(?:"javascript:[^"]*"|'javascript:[^']*')/gi, '$1""');
}

/** manifest.icon 原始值（trim；无 entry / 无 icon = 空串）。 */
const iconRaw = computed(() => (appEntry.value?.icon ?? '').trim());

/** 含 '<' → 内联 SVG 标记协议。 */
const iconIsSvg = computed(() => iconRaw.value.includes('<'));

/** 内联 SVG 分支的净化后标记。 */
const iconInner = computed(() => sanitizeSvgInner(iconRaw.value));

/** 图标名分支：本名优先，未命中且应用已装回退按 id（AppIcon 内再走 ICONS → fallback）。 */
const iconName = computed(() => {
  const raw = iconRaw.value;
  if (!raw || iconIsSvg.value) return '';
  if (runtimeIcons[raw]) return raw;
  const id = appEntry.value?.id ?? '';
  return id && runtimeIcons[id] ? id : raw;
});

/** 快捷入口（→ 详情页内嵌 Tab）。 */
function openDetail(tab: RepoDetailTab): void {
  ctx.openRepo(name.value, tab);
}

/** 复制的反馈态（按钮「已复制」1.5s）。 */
const copied = ref(false);
let copiedTimer: ReturnType<typeof setTimeout> | undefined;

/** 复制 SSH clone URL（既有端点 codeRepoCloneUrl 动态获取后写剪贴板）。 */
async function copyCloneUrl(): Promise<void> {
  try {
    const r = (await endpoints.codeRepoCloneUrl(name.value)) as { clone_url_ssh?: string };
    const url = r.clone_url_ssh ?? '';
    if (!url) {
      ctx.showMsg('error', t('nexhub.explore.cloneUrlMissing'));
      return;
    }
    if (!(await copyText(url))) {
      ctx.showMsg('error', t('nexhub.common.copyFailed'));
      return;
    }
    copied.value = true;
    ctx.showMsg('ok', t('nexhub.explore.cloneUrlCopied', { url }));
    clearTimeout(copiedTimer);
    copiedTimer = setTimeout(() => (copied.value = false), 2000);
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.common.copyFailed')}: ${e instanceof Error ? e.message : String(e)}`);
  }
}
</script>

<template>
  <div class="card repo-card" :class="{ 'is-app': isApp }">
    <div class="repo-card-head">
      <div class="repo-title">
        <!-- 应用图标：manifest.icon 双协议（内联 SVG 净化 v-html / 名字走 AppIcon 兜底链），
             未命中或无 icon 时渲染通用 package 盒子——绝不裸输出原始字符串（v0.1.34）。 -->
        <span v-if="isApp" class="app-icon" aria-hidden="true">
          <svg
            v-if="iconIsSvg"
            xmlns="http://www.w3.org/2000/svg"
            width="18"
            height="18"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
            v-html="iconInner"
          />
          <AppIcon v-else :name="iconName" :size="18" :fallback="PKG_ICON_INNER" />
        </span>
        <button class="repo-name" type="button" :title="t('nexhub.detail.open')" @click="openDetail('code')">
          {{ name }}
        </button>
        <!-- 应用仓库专属徽章：类别 / 引擎（join catalog） -->
        <span v-if="isApp" class="badge badge-app">{{ t('nexhub.explore.appBadge') }}</span>
        <!-- 内置 CI 徽章（最新 run：灰 无 / 黄 运行中 / 绿 通过+耗时 / 红 失败） -->
        <CiBadge :repo="name" />
      </div>
      <span class="repo-meta-num" :title="t('nexhub.explore.branches')" aria-label="branches">⎇ {{ repo.branch_count ?? 0 }}</span>
    </div>
    <p class="repo-desc">{{ repo.description || t('nexhub.explore.noDescription') }}</p>
    <div class="repo-meta">
      <span class="meta-chip">{{ formatBytes(repo.size_bytes) }}</span>
      <span class="meta-chip" :title="t('nexhub.explore.commits')">◷ {{ repo.commit_count ?? 0 }}</span>
      <span class="meta-chip" :title="repo.last_commit_date ?? ''">
        {{ repo.last_commit ? repo.last_commit : t('nexhub.explore.emptyRepo') }}
      </span>
      <!-- 应用 manifest 徽章：版本 / 类别 / 引擎 -->
      <template v-if="appEntry">
        <span class="meta-chip chip-ver" :title="t('nexhub.explore.manifestVersion')">
          {{ appEntry.installed ? `v${appEntry.installed_version}` : `v${appEntry.version}` }}
        </span>
        <span v-if="appEntry.category" class="meta-chip">{{ appEntry.category }}</span>
        <span v-if="appEntry.engine" class="meta-chip chip-engine" :title="t('nexhub.explore.engine')">
          ⚙ {{ appEntry.engine }}
        </span>
      </template>
    </div>
    <div class="clone-row">
      <code class="clone-url" :title="repo.clone_url_ssh">{{ repo.clone_url_ssh }}</code>
      <button
        class="btn btn-small btn-ghost"
        :class="{ copied }"
        type="button"
        @click="copyCloneUrl"
      >{{ copied ? t('nexhub.common.copied') : t('nexhub.common.copy') }}</button>
    </div>
    <div class="repo-card-actions">
      <button class="btn btn-small btn-ghost" type="button" @click="openDetail('code')">{{ t('nexhub.explore.browse') }}</button>
      <button class="btn btn-small btn-ghost" type="button" @click="openDetail('commits')">{{ t('nexhub.explore.commitsTab') }}</button>
      <button class="btn btn-small btn-ghost" type="button" @click="openDetail('issues')">Issues</button>
      <button class="btn btn-small btn-ghost" type="button" @click="openDetail('pulls')">PR</button>
      <!-- 应用仓库：一键部署（未装=部署到本节点 / 升级 / 已装 vX） -->
      <DeployButton v-if="isApp" :repo="name" size="small" class="card-deploy" />
    </div>
  </div>
</template>

<style scoped>
.repo-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.repo-card.is-app { border-color: rgba(233, 84, 32, 0.45); }
.repo-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.repo-title { display: flex; align-items: center; gap: 8px; min-width: 0; flex-wrap: wrap; }
.app-icon { display: inline-flex; flex-shrink: 0; align-items: center; line-height: 0; color: var(--text, #2B2B2B); }
.app-icon svg { display: block; }
.repo-name {
  background: transparent; border: none; padding: 0; cursor: pointer; font-family: inherit;
  font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); word-break: break-all; text-align: left;
}
.repo-name:hover { color: var(--accent, #E95420); text-decoration: underline; }
.badge {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; font-weight: 600;
}
.badge-app { background: rgba(233, 84, 32, 0.14); color: var(--accent, #E95420); }
.repo-meta-num { font-size: 12px; color: var(--accent, #E95420); font-weight: 600; }
.repo-desc { margin: 0; font-size: 13px; line-height: 1.5; color: var(--text-muted, #5E5C5F); min-height: 20px; }
.repo-meta { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.meta-chip {
  display: inline-block; padding: 1px 8px; border-radius: var(--radius-pill, 20px);
  font-size: 11px; color: var(--text-muted, #5E5C5F); background: var(--border-soft, #F3F4F6);
}
.chip-ver { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); font-weight: 600; }
.chip-engine { background: rgba(119, 41, 83, 0.1); color: #772953; font-weight: 600; }
.clone-row { display: flex; align-items: center; gap: 6px; }
.clone-url {
  flex: 1; min-width: 0; font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11px;
  color: var(--text-muted, #5E5C5F); background: var(--bg-code, #fafafa);
  padding: 4px 8px; border-radius: var(--radius-sm, 6px); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
.repo-card-actions { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; margin-top: 2px; }
.card-deploy { margin-left: auto; }

.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.btn-ghost:hover { background: rgba(233, 84, 32, 0.08); }
.btn-ghost.copied { color: #166534; }
.muted { color: var(--text-muted, #5E5C5F); }
</style>

