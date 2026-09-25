<script setup lang="ts">
// =============================================================================
// OnboardingPage —— 接入指南（v0.1.32，原 Tab8 静态页动态化）。
//
// P1 安全/体验整改：
//   - 服务地址按 window.location 动态推导（删除硬编码 IP 192.0.2.106 /
//     203.0.113.2）；
//   - 令牌不再展示任何示例值（删除 change-me-admin-token）——改为「设置 → API 令牌」
//     查看指引（不给假值）；
//   - clone 地址用既有 codeRepoCloneUrl 端点按仓库动态生成（非 admin 时回退
//     推导的 Smart HTTP 地址——匿名读开放）。
// =============================================================================
import { computed, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { copyText } from '@/utils/clipboard';
import { useNexhub } from '@/views/nexhub/context';

const { t } = useI18n();
const ctx = useNexhub();

/** 节点基址（按浏览器地址动态推导——任一可达节点都能照此接入）。 */
const baseUrl = computed(() => `${window.location.protocol}//${window.location.host}`);

/** 令牌占位（不展示任何真实/示例 token；$NEXHUB_TOKEN 由用户自行注入）。 */
const TOKEN_PLACEHOLDER = '$NEXHUB_TOKEN';

/** 示例仓库名（可编辑；命令示例随其联动）。 */
const exampleRepo = ref('my-project');

/** 示例命令块（含尖括号占位符——存 script 常量经文本插值渲染，避免被当 HTML）。 */
const snippets = computed(() => ({
  /** 通道 A ①：创建裸仓库（admin token） */
  a1: `curl -X POST ${baseUrl.value}/api/v1/coderepo/repos -H 'Authorization: Bearer ${TOKEN_PLACEHOLDER}' -H 'Content-Type: application/json' -d '{"name":"${exampleRepo.value}"}'`,
  /** 通道 A ②：本地仓库挂 remote 并推送（HTTP Basic：用户名任意，密码=令牌） */
  a2: `git remote add hub http://agent:${TOKEN_PLACEHOLDER}@${window.location.host}/git/${exampleRepo.value}.git\ngit push hub main`,
  /** 通道 A ③：发布到大厅（元数据快照；发布者由服务端从令牌反查归因） */
  a3: `curl -X POST ${baseUrl.value}/api/v1/nexhub/lobby/publish -H 'Authorization: Bearer ${TOKEN_PLACEHOLDER}' -H 'Content-Type: application/json' -d '{"repo":"${exampleRepo.value}","description":"...","tags":["rust"]}'`,
  /** 通道 B ①：领 nonce */
  b1: `curl -X POST ${baseUrl.value}/api/v1/nexhub/auth/challenge -H 'Content-Type: application/json' -d '{"pubkey":"0x<66hex-pubkey>"}'`,
  /** 通道 B ③：签名换 token */
  b2: `curl -X POST ${baseUrl.value}/api/v1/nexhub/auth/verify -H 'Content-Type: application/json' -d '{"pubkey":"0x<66hex-pubkey>","nonce":"<nonce>","signature":"<130hex>"}'`,
}));

/** 示例代码块键型（a1..b2；随 exampleRepo / baseUrl 响应式联动）。 */
type SnippetKey = 'a1' | 'a2' | 'a3' | 'b1' | 'b2';

/** 刚复制成功的代码块 key（右上角按钮 ✓ 反馈 1.5s）。 */
const copiedSnippet = ref('');
let snippetTimer: ReturnType<typeof setTimeout> | undefined;

/** 复制接入示例代码块（剪贴板工具带回退；成功 ✓ 反馈 1.5s）。 */
async function copySnippet(key: SnippetKey): Promise<void> {
  if (!(await copyText(snippets.value[key]))) {
    ctx.showMsg('error', t('nexhub.common.copyFailed'));
    return;
  }
  copiedSnippet.value = key;
  clearTimeout(snippetTimer);
  snippetTimer = setTimeout(() => (copiedSnippet.value = ''), 1500);
}

// —— 动态 clone 地址（既有 codeRepoCloneUrl 端点；非 admin 回退推导地址）——
const cloneLoading = ref(false);
const cloneUrls = ref<{ ssh: string; http: string; derived: boolean } | null>(null);

/** 按仓库动态获取双通道 clone 地址；403（非 admin）回退 Smart HTTP 推导值。 */
async function fetchCloneUrl(): Promise<void> {
  const repo = exampleRepo.value.trim();
  if (!repo) {
    ctx.showMsg('error', t('nexhub.explore.nameRequired'));
    return;
  }
  cloneLoading.value = true;
  cloneUrls.value = null;
  try {
    const r = (await endpoints.codeRepoCloneUrl(repo)) as {
      clone_url_ssh?: string;
      clone_url_http?: string;
    };
    cloneUrls.value = {
      ssh: r.clone_url_ssh ?? '',
      http: r.clone_url_http ?? '',
      derived: false,
    };
  } catch (e) {
    // 非 admin / 端点不可达：回退按 Host 推导的 Smart HTTP 地址（匿名读开放）
    cloneUrls.value = {
      ssh: '',
      http: `${baseUrl.value}/git/${repo}.git`,
      derived: true,
    };
    if (!(e as { status?: number })?.status) {
      console.warn('clone-url fetch failed', e);
    }
  } finally {
    cloneLoading.value = false;
  }
}

/** 复制动态 clone 地址。 */
async function copyCloneUrl(kind: 'ssh' | 'http'): Promise<void> {
  if (!cloneUrls.value) return;
  const url = kind === 'ssh' ? cloneUrls.value.ssh : cloneUrls.value.http;
  if (!url) {
    ctx.showMsg('error', t('nexhub.explore.cloneUrlMissing'));
    return;
  }
  if (!(await copyText(url))) {
    ctx.showMsg('error', t('nexhub.common.copyFailed'));
    return;
  }
  ctx.showMsg('ok', t('nexhub.lobby.cloneUrlCopied', { kind: kind.toUpperCase(), url }));
}
</script>

<template>
  <section class="ob-page">
    <p class="ob-intro muted">
      {{ t('nexhub.onboarding.intro') }}
    </p>

    <!-- 基础信息（全部动态推导） -->
    <div class="card ob-card">
      <div class="ob-card-title">📡 {{ t('nexhub.onboarding.basicsTitle') }}</div>
      <div class="ob-kv">
        <span class="ob-k">{{ t('nexhub.onboarding.serverAddr') }}</span>
        <code class="ob-v">{{ baseUrl }}</code>
        <span class="muted small">{{ t('nexhub.onboarding.serverAddrHint') }}</span>
      </div>
      <div class="ob-kv">
        <span class="ob-k">{{ t('nexhub.onboarding.token') }}</span>
        <span class="muted small">{{ t('nexhub.onboarding.tokenHint') }}
          <RouterLink class="ob-link" to="/settings">{{ t('nexhub.onboarding.tokenWhere') }}</RouterLink>
        </span>
      </div>
      <p class="ob-card-sub">
        {{ t('nexhub.onboarding.baseVarHint') }}
        <code class="ob-inline">B={{ baseUrl }}</code>
      </p>
    </div>

    <!-- 动态 clone 地址（既有端点按仓库生成） -->
    <div class="card ob-card">
      <div class="ob-card-title">⎇ {{ t('nexhub.onboarding.cloneTitle') }}</div>
      <div class="ob-kv">
        <span class="ob-k">{{ t('nexhub.onboarding.repoName') }}</span>
        <input
          v-model="exampleRepo"
          class="search-input ob-repo-input"
          list="ob-repo-list"
          :placeholder="t('nexhub.onboarding.repoNamePlaceholder')"
        />
        <datalist id="ob-repo-list">
          <option v-for="n in ctx.repos.value.map((r) => r.name)" :key="n" :value="n" />
        </datalist>
        <button class="btn btn-small" type="button" :disabled="cloneLoading" @click="fetchCloneUrl">
          {{ cloneLoading ? t('nexhub.onboarding.fetching') : t('nexhub.onboarding.fetchClone') }}
        </button>
      </div>
      <template v-if="cloneUrls">
        <div v-if="cloneUrls.ssh" class="clone-row">
          <code class="clone-url" :title="cloneUrls.ssh">SSH&nbsp;{{ cloneUrls.ssh }}</code>
          <button class="btn btn-small btn-ghost" type="button" @click="copyCloneUrl('ssh')">{{ t('nexhub.common.copy') }}</button>
        </div>
        <div v-if="cloneUrls.http" class="clone-row">
          <code class="clone-url" :title="cloneUrls.http">HTTP&nbsp;{{ cloneUrls.http }}</code>
          <button class="btn btn-small btn-ghost" type="button" @click="copyCloneUrl('http')">{{ t('nexhub.common.copy') }}</button>
        </div>
        <p v-if="cloneUrls.derived" class="ob-note muted small">{{ t('nexhub.onboarding.derivedNote') }}</p>
        <p class="ob-card-sub">{{ t('nexhub.onboarding.gitCloneHint') }}</p>
      </template>
    </div>

    <!-- 通道 A：三步上架（admin token） -->
    <div class="card ob-card">
      <div class="ob-card-title">🚀 {{ t('nexhub.onboarding.pathATitle') }}</div>
      <p class="ob-card-sub">{{ t('nexhub.onboarding.pathASub') }}</p>
      <ol class="ob-steps">
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepCreateRepo') }}</span>
          <div class="ob-code">
            <pre class="ob-pre">{{ snippets.a1 }}</pre>
            <button
              class="btn btn-small ob-copy"
              type="button"
              :class="{ copied: copiedSnippet === 'a1' }"
              @click="copySnippet('a1')"
            >{{ copiedSnippet === 'a1' ? '✓' : t('nexhub.common.copy') }}</button>
          </div>
        </li>
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepPush') }}</span>
          <div class="ob-code">
            <pre class="ob-pre">{{ snippets.a2 }}</pre>
            <button
              class="btn btn-small ob-copy"
              type="button"
              :class="{ copied: copiedSnippet === 'a2' }"
              @click="copySnippet('a2')"
            >{{ copiedSnippet === 'a2' ? '✓' : t('nexhub.common.copy') }}</button>
          </div>
        </li>
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepPublish') }}</span>
          <div class="ob-code">
            <pre class="ob-pre">{{ snippets.a3 }}</pre>
            <button
              class="btn btn-small ob-copy"
              type="button"
              :class="{ copied: copiedSnippet === 'a3' }"
              @click="copySnippet('a3')"
            >{{ copiedSnippet === 'a3' ? '✓' : t('nexhub.common.copy') }}</button>
          </div>
        </li>
      </ol>
      <p class="ob-note muted small">
        {{ t('nexhub.onboarding.pathANote') }}
      </p>
    </div>

    <!-- 通道 B：链上身份 -->
    <div class="card ob-card">
      <div class="ob-card-title">🔐 {{ t('nexhub.onboarding.pathBTitle') }}</div>
      <p class="ob-card-sub">{{ t('nexhub.onboarding.pathBSub') }}</p>
      <ol class="ob-steps">
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepNonce') }}</span>
          <div class="ob-code">
            <pre class="ob-pre">{{ snippets.b1 }}</pre>
            <button
              class="btn btn-small ob-copy"
              type="button"
              :class="{ copied: copiedSnippet === 'b1' }"
              @click="copySnippet('b1')"
            >{{ copiedSnippet === 'b1' ? '✓' : t('nexhub.common.copy') }}</button>
          </div>
        </li>
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepSign') }}</span>
          <p class="ob-card-sub">
            {{ t('nexhub.onboarding.signHintPre') }}
            <code class="ob-inline">SHA-256(nonce)</code>
            {{ t('nexhub.onboarding.signHintMid') }}
            <code class="ob-inline">r||s||v</code>
            {{ t('nexhub.onboarding.signHintPost') }}
          </p>
        </li>
        <li>
          <span class="ob-step-title">{{ t('nexhub.onboarding.stepVerify') }}</span>
          <div class="ob-code">
            <pre class="ob-pre">{{ snippets.b2 }}</pre>
            <button
              class="btn btn-small ob-copy"
              type="button"
              :class="{ copied: copiedSnippet === 'b2' }"
              @click="copySnippet('b2')"
            >{{ copiedSnippet === 'b2' ? '✓' : t('nexhub.common.copy') }}</button>
          </div>
        </li>
      </ol>
      <ul class="ob-points">
        <li>{{ t('nexhub.onboarding.pointSharedKey') }}</li>
        <li>{{ t('nexhub.onboarding.pointTokenTtl') }}</li>
        <li>{{ t('nexhub.onboarding.pointOwnership') }}</li>
        <li>{{ t('nexhub.onboarding.pointAdminFallback') }}</li>
      </ul>
    </div>

    <!-- 可选能力 -->
    <div class="card ob-card">
      <div class="ob-card-title">🧩 {{ t('nexhub.onboarding.extrasTitle') }}</div>
      <ul class="ob-points">
        <li>{{ t('nexhub.onboarding.extraBrowse') }} <code class="ob-inline">GET /api/v1/nexhub/lobby?q=&amp;tag=</code></li>
        <li>{{ t('nexhub.onboarding.extraClone') }} <code class="ob-inline">POST /api/v1/nexhub/lobby/:name/clone</code></li>
        <li>{{ t('nexhub.onboarding.extraMonetize') }} <code class="ob-inline">price_sats</code> / <code class="ob-inline">currency</code></li>
        <li>{{ t('nexhub.onboarding.extraBounty') }}</li>
        <li>{{ t('nexhub.onboarding.extraFederation') }}</li>
      </ul>
    </div>

    <!-- 坑位提示（默认折叠） -->
    <details class="ob-pitfalls">
      <summary>⚠ {{ t('nexhub.onboarding.pitfallsTitle') }}</summary>
      <ul class="ob-points">
        <li>
          {{ t('nexhub.onboarding.pitfallGitBash') }}
          <code class="ob-inline">--data-binary @file.json</code>
        </li>
        <li>
          {{ t('nexhub.onboarding.pitfallDoc') }}
          <code class="ob-inline">docs/NEXHUB_ONBOARDING.md</code>
        </li>
      </ul>
    </details>
  </section>
</template>

<style scoped>
.ob-page { display: flex; flex-direction: column; gap: 14px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.ob-intro { margin: 0; font-size: 13px; line-height: 1.6; }
.ob-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; }
.ob-card-title { font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); }
.ob-card-sub { margin: 0; font-size: 13px; line-height: 1.6; color: var(--text-muted, #5E5C5F); }
.ob-kv { display: flex; align-items: baseline; gap: 10px; flex-wrap: wrap; font-size: 13px; }
.ob-k { flex-shrink: 0; min-width: 72px; font-weight: 600; color: var(--text, #2B2B2B); }
.ob-v {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12.5px; word-break: break-all;
  padding: 2px 8px; border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa); color: var(--text, #2B2B2B);
}
.ob-inline {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12px; word-break: break-all;
  padding: 1px 6px; border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa); color: var(--accent, #E95420);
}
.ob-link { color: var(--accent, #E95420); font-weight: 600; }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.ob-repo-input { min-width: 180px; }
.ob-steps { margin: 0; padding: 0; list-style: none; display: flex; flex-direction: column; gap: 14px; }
.ob-steps li { display: flex; flex-direction: column; gap: 6px; }
.ob-step-title { font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
.ob-code { position: relative; }
.ob-pre {
  margin: 0; padding: 12px 64px 12px 14px; border-radius: var(--radius-sm, 8px);
  background: #26292F; color: #E8E4E8;
  font-family: 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace;
  font-size: 12.5px; line-height: 1.55; white-space: pre-wrap; word-break: break-word;
}
.ob-copy {
  position: absolute; top: 6px; right: 6px; padding: 2px 9px; font-size: 11px;
  background: rgba(255, 255, 255, 0.1); border: 1px solid rgba(255, 255, 255, 0.25);
  color: #E8E4E8;
}
.ob-copy:hover { background: rgba(255, 255, 255, 0.2); }
.ob-copy.copied { color: #4ade80; border-color: rgba(74, 222, 128, 0.55); background: rgba(74, 222, 128, 0.12); }
.ob-note {
  margin: 0; padding: 8px 12px; border-radius: var(--radius-sm, 8px);
  background: var(--border-soft, #F3F4F6); line-height: 1.6;
}
.ob-points {
  margin: 0; padding-left: 20px; display: flex; flex-direction: column; gap: 6px;
  font-size: 13px; line-height: 1.6; color: var(--text, #2B2B2B);
}
.ob-pitfalls {
  padding: 10px 16px; border-radius: var(--radius-md, 12px);
  background: var(--bg-card, #fff); border: 1px dashed var(--border, #D9D9D9);
  font-size: 13px; color: var(--text-muted, #5E5C5F);
}
.ob-pitfalls summary { cursor: pointer; font-weight: 600; color: var(--text, #2B2B2B); }
.ob-pitfalls[open] .ob-points { margin-top: 10px; }
.clone-row { display: flex; align-items: center; gap: 6px; }
.clone-url {
  flex: 1; min-width: 0; font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11.5px;
  color: var(--text, #2B2B2B); background: var(--bg-code, #fafafa);
  padding: 4px 8px; border-radius: var(--radius-sm, 6px); overflow: hidden;
  text-overflow: ellipsis; white-space: nowrap;
}
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
</style>
