<script setup lang="ts">
// =============================================================================
// DeployButton —— 应用一键部署按钮（v0.1.32，NexHub 网页化 P1）。
//
// 状态机（设计书 §5.2）：
//   未安装 ──点击──▶ 安装中(spinner,禁用) ──201──▶ 已装 vX ✓（运行中于桌面）
//   catalog 版本 > installed_version ──▶ [⬆ 升级到 vX.Y.Z] ──▶ 已装新版本
//   同版本重复点击 ──▶ 后端 action=noop ──▶ 提示「已是最新」
//   400(manifest 校验) / 409(同 id 异源) ──▶ 红条提示（壳 msg，原样透传后端消息）
//
// 数据源：壳共享的 appsCatalog（一次请求同时拿到资格与安装态）；安装流程走
// 共享 useAppDeploy（与 AppStore 同一链路：install → 轮询 → appRuntime 热注册，
// 部署成功后桌面 / Launchpad 免刷新可见）。结果反馈（成功/失败红条）统一写
// 壳的 ctx.msg；本组件只负责按钮态与升级判定。
// 非应用仓库（无 catalog 条目）渲染为空；manifest 校验失败（entry.error）
// 按钮禁用并提示原因——不假成功。
// =============================================================================

import { computed } from 'vue';
import { useI18n } from 'vue-i18n';
import { useNexhub } from '@/views/nexhub/context';
import { compareVersions } from '@/views/nexhub/model';

const props = defineProps<{
  /** NexHub 仓库名（应用仓库 = nexos-app-*）。 */
  repo: string;
  /** 尺寸（卡片上 small；详情页头 normal）。 */
  size?: 'small' | 'normal';
}>();

const { t } = useI18n();
const ctx = useNexhub();

/** 目录条目（找不到 = 非应用仓库 → 不渲染）。 */
const entry = computed(() => ctx.deploy.catalogEntry(props.repo));

/** 安装中（所有 DeployButton 共享 installingRepo，同时只有一个安装流）。 */
const busy = computed(() => ctx.deploy.installingRepo.value === props.repo);

/** 全局有安装流在进行（其余按钮一并禁用，保证同时仅一条安装链路）。 */
const anyBusy = computed(() => ctx.deploy.installingRepo.value !== '');

/** 未安装。 */
const notInstalled = computed(() => !!entry.value && !entry.value.installed);

/** 可升级：已装且 catalog 版本更新。 */
const canUpgrade = computed(() => {
  const e = entry.value;
  if (!e?.installed || !e.version || !e.installed_version) return false;
  return compareVersions(e.version, e.installed_version) > 0;
});

/** manifest 校验失败（catalog error 透传；按钮禁用 + 红条提示原因）。 */
const manifestError = computed(() => (entry.value?.error ?? '').trim());

/** 按钮主文案。 */
const label = computed<string>(() => {
  if (busy.value) return t('nexhub.deploy.installing');
  if (notInstalled.value) return t('nexhub.deploy.deploy');
  if (canUpgrade.value) {
    return t('nexhub.deploy.upgradeTo', { v: entry.value?.version ?? '' });
  }
  return t('nexhub.deploy.installed', {
    v: entry.value?.installed_version || entry.value?.version || '',
  });
});

/** 按钮语义色：未装/升级 = primary；已装最新 = ok（绿）。 */
const tone = computed<'primary' | 'ok'>(() => {
  if (notInstalled.value || canUpgrade.value) return 'primary';
  return 'ok';
});

/** 点击部署 / 升级（同一时刻仅允许一个安装流；结果提示由壳回调写 ctx.msg）。 */
function onClick(): void {
  const e = entry.value;
  if (!e || busy.value || ctx.deploy.installingRepo.value) return;
  void ctx.deploy.install({ repo: e.repo, id: e.id, name: e.name ?? e.id });
}
</script>

<template>
  <!-- 非应用仓库（无 catalog 条目）：不渲染 -->
  <span v-if="entry" class="deploy-wrap">
    <button
      class="btn deploy-btn"
      :class="[`is-${tone}`, size === 'small' ? 'btn-small' : '', { busy }]"
      type="button"
      :disabled="busy || anyBusy || !!manifestError"
      :title="manifestError || undefined"
      @click="onClick"
    >
      <span class="spin" :class="{ spinning: busy }" aria-hidden="true">{{ busy ? '↻' : canUpgrade ? '⬆' : '⬇' }}</span>
      {{ label }}
    </button>
    <span v-if="!busy && !manifestError && !notInstalled && !canUpgrade" class="deploy-hint muted">
      {{ t('nexhub.deploy.runningHint') }}
    </span>
    <span v-else-if="manifestError" class="deploy-error">{{ manifestError }}</span>
  </span>
</template>

<style scoped>
.deploy-wrap { display: inline-flex; align-items: center; gap: 8px; flex-wrap: wrap; min-width: 0; }
.deploy-btn { white-space: nowrap; }
.deploy-btn.is-primary { background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff; }
.deploy-btn.is-primary:hover:not(:disabled) { background: #d44a1c; }
.deploy-btn.is-ok { background: #dcfce7; border-color: rgba(22, 101, 52, 0.35); color: #166534; }
.deploy-btn:disabled { opacity: 0.55; cursor: not-allowed; }
.deploy-hint { font-size: 11.5px; }
.deploy-error {
  font-size: 11.5px; color: #b91c1c; background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.25); border-radius: var(--radius-sm, 6px);
  padding: 2px 8px; max-width: 320px; word-break: break-word;
}
.spin { display: inline-block; }
.spin.spinning { animation: deploy-spin 1s linear infinite; }
@keyframes deploy-spin { to { transform: rotate(360deg); } }
.muted { color: var(--text-muted, #5E5C5F); }
</style>

