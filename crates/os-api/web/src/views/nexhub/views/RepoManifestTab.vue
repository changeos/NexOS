<script setup lang="ts">
// =============================================================================
// RepoManifestTab —— 应用仓库专属 Manifest Tab（v0.1.32 P1，nexos-app-* 专属）。
// 拉取 manifest.json（GET /api/v1/coderepo/repos/:name/file?path=manifest.json）
// 并全字段展示：已知字段（id/name/version/category/icon/engine/sdk/min_os_api
// /description/entry）结构化渲染，未知字段收敛进原始 JSON 折叠块。
// =============================================================================
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg } from '@/views/nexhub/model';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

const MANIFEST_PATH = 'manifest.json';

const raw = ref('');
const parsed = ref<Record<string, unknown> | null>(null);
const loading = ref(false);
const loadError = ref('');

/** 已知字段（结构化渲染顺序）。 */
const KNOWN_KEYS = ['id', 'name', 'version', 'category', 'icon', 'engine', 'sdk', 'min_os_api', 'entry', 'description'] as const;

/** 结构化展示的键值对（manifest 中存在且值非 undefined）。 */
const fields = computed<{ key: string; value: string }[]>(() => {
  const m = parsed.value;
  if (!m) return [];
  const out: { key: string; value: string }[] = [];
  for (const k of KNOWN_KEYS) {
    const v = m[k];
    if (v === undefined || v === null || v === '') continue;
    out.push({ key: k, value: typeof v === 'string' ? v : JSON.stringify(v) });
  }
  return out;
});

/** 目录条目对照（installed 状态 + 安装目录等运行时信息）。 */
const appEntry = computed(() => ctx.deploy.catalogEntry(props.repoName));

async function loadManifest(): Promise<void> {
  if (!props.repoName.trim()) return;
  loading.value = true;
  loadError.value = '';
  raw.value = '';
  parsed.value = null;
  try {
    const r = (await endpoints.codeRepoFile(props.repoName.trim(), MANIFEST_PATH)) as {
      content?: string;
      exists?: boolean;
    };
    raw.value = r.content ?? '';
    if (r.exists === false || !raw.value.trim()) {
      loadError.value = t('nexhub.manifest.missing');
      return;
    }
    try {
      parsed.value = JSON.parse(raw.value) as Record<string, unknown>;
    } catch {
      // JSON 解析失败：保留原文展示（不假成功）
      parsed.value = null;
    }
  } catch (e) {
    loadError.value = `${t('nexhub.manifest.loadFailed')}: ${errMsg(e)}`;
  } finally {
    loading.value = false;
  }
}

watch(() => props.repoName, () => void loadManifest(), { immediate: true });
</script>

<template>
  <section class="manifest-tab">
    <div v-if="loading" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="loadError" class="card empty-card">{{ loadError }}</div>
    <template v-else>
      <!-- 结构化字段 -->
      <div class="card mf-card">
        <div class="panel-head">
          <span class="panel-title">manifest.json</span>
          <code class="mf-repo">{{ props.repoName }}</code>
        </div>
        <div v-if="fields.length" class="mf-fields">
          <div v-for="f in fields" :key="f.key" class="mf-row">
            <span class="mf-k">{{ f.key }}</span>
            <code class="mf-v">{{ f.value }}</code>
          </div>
        </div>
        <!-- 已装状态对照（catalog join） -->
        <div v-if="appEntry" class="mf-installed">
          <span class="mf-k">{{ t('nexhub.manifest.installState') }}</span>
          <span v-if="appEntry.installed" class="pill pill-ok">
            {{ t('nexhub.manifest.installedAs', { v: appEntry.installed_version || appEntry.version }) }}
          </span>
          <span v-else class="pill pill-muted">{{ t('nexhub.manifest.notInstalled') }}</span>
        </div>
      </div>

      <!-- 原始 JSON（未知字段 / 原文对照） -->
      <details class="card mf-raw">
        <summary>{{ t('nexhub.manifest.rawTitle') }}</summary>
        <pre class="mf-pre"><code>{{ parsed ? JSON.stringify(parsed, null, 2) : raw }}</code></pre>
      </details>
    </template>
  </section>
</template>

<style scoped>
.manifest-tab { display: flex; flex-direction: column; gap: 12px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; padding: 12px 16px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.mf-repo { font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12px; color: var(--text-muted, #5E5C5F); }
.mf-card { display: flex; flex-direction: column; }
.mf-fields { display: flex; flex-direction: column; padding: 8px 16px 12px; }
.mf-row { display: flex; align-items: baseline; gap: 12px; padding: 5px 0; border-bottom: 1px dashed var(--border-soft, #EDEDED); flex-wrap: wrap; }
.mf-row:last-child { border-bottom: none; }
.mf-k { min-width: 110px; flex-shrink: 0; font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12px; color: var(--accent, #E95420); font-weight: 600; }
.mf-v { font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12.5px; color: var(--text, #2B2B2B); word-break: break-all; }
.mf-installed { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-top: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.mf-raw { padding: 0; }
.mf-raw summary { cursor: pointer; padding: 10px 16px; font-size: 13px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.mf-pre {
  margin: 0; padding: 12px 16px; border-top: 1px dashed var(--border-soft, #EDEDED);
  background: var(--bg-code, #fafafa); font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 12px; line-height: 1.55; overflow: auto; white-space: pre-wrap; word-break: break-word;
}
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
</style>
