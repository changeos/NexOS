<script setup lang="ts">
// =============================================================================
// RepoWebhooksTab —— 仓库设置区 Webhook 管理卡（v0.1.50，NexHub top1 前端面）。
//
// 功能（对标 GitHub 仓库 Settings → Webhooks 的最小集）：
// - 列钩子（GET /api/v1/coderepo/repos/:name/webhooks，admin）：URL / 事件勾选 /
//   启停 / 最近投递状态（last_delivery/last_status + 最近投递环形日志 20 条）；
// - 建钩子（URL + 事件勾选 + secret）；
// - 删钩子 / 启停翻转。
// 管理端点全部 admin（服务端网关鉴权——全局 token 或默认 admin 模式）。
// =============================================================================
import { computed, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg } from '@/views/nexhub/model';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

/** 订阅事件面（与后端 WebhookEvent::all_names 同源）。 */
const EVENT_KEYS = ['push', 'issues', 'pr', 'release'] as const;
type EventKey = (typeof EVENT_KEYS)[number];

interface WebhookRow {
  id: string;
  repo: string;
  url: string;
  secret: string;
  events: string[];
  enabled: boolean;
  created_at: string;
  last_delivery?: string | null;
  last_status?: string | null;
}

interface DeliveryRow {
  id: number;
  event: string;
  url: string;
  ok: boolean;
  status: string;
  attempts: number;
  delivered_at: string;
}

const hooks = ref<WebhookRow[]>([]);
const deliveries = ref<Record<string, DeliveryRow[]>>({});
const loading = ref(false);
const loadError = ref('');

// —— 创建表单 ——
const showCreate = ref(false);
const creating = ref(false);
const formUrl = ref('');
const formSecret = ref('');
const formEvents = ref<EventKey[]>(['push']);
const formError = ref('');

// —— 删除确认 ——
const deleteTarget = ref<WebhookRow | null>(null);
const deleting = ref(false);

const eventLabel = (k: EventKey): string => t(`nexhub.webhooks.event_${k}`);

const formValid = computed(() => {
  const url = formUrl.value.trim();
  return (url.startsWith('http://') || url.startsWith('https://')) && formEvents.value.length > 0;
});

async function load(): Promise<void> {
  if (!props.repoName.trim()) return;
  loading.value = true;
  loadError.value = '';
  try {
    const r = (await endpoints.codeRepoWebhooks(props.repoName.trim())) as {
      webhooks?: { webhook?: WebhookRow; deliveries?: DeliveryRow[] }[];
    };
    hooks.value = (r.webhooks ?? []).map((w) => w.webhook ?? (w as unknown as WebhookRow));
    const map: Record<string, DeliveryRow[]> = {};
    (r.webhooks ?? []).forEach((w, i) => {
      const h = hooks.value[i];
      if (h) map[h.id] = w.deliveries ?? [];
    });
    deliveries.value = map;
  } catch (e) {
    loadError.value = `${t('nexhub.webhooks.loadFailed')}: ${errMsg(e)}`;
  } finally {
    loading.value = false;
  }
}

watch(() => props.repoName, () => void load(), { immediate: true });

function toggleFormEvent(k: EventKey): void {
  const i = formEvents.value.indexOf(k);
  if (i >= 0) formEvents.value.splice(i, 1);
  else formEvents.value.push(k);
}

function openCreate(): void {
  formUrl.value = '';
  formSecret.value = '';
  formEvents.value = ['push'];
  formError.value = '';
  showCreate.value = true;
}

async function doCreate(): Promise<void> {
  if (!formValid.value) {
    formError.value = t('nexhub.webhooks.formInvalid');
    return;
  }
  creating.value = true;
  formError.value = '';
  try {
    await endpoints.createCodeRepoWebhook(props.repoName.trim(), {
      url: formUrl.value.trim(),
      secret: formSecret.value.trim() || undefined,
      events: [...formEvents.value],
    });
    ctx.showMsg('ok', t('nexhub.webhooks.created'));
    showCreate.value = false;
    await load();
  } catch (e) {
    formError.value = `${t('nexhub.webhooks.createFailed')}: ${errMsg(e)}`;
  } finally {
    creating.value = false;
  }
}

async function doToggle(h: WebhookRow): Promise<void> {
  ctx.clearMsg();
  try {
    await endpoints.toggleCodeRepoWebhook(props.repoName.trim(), h.id);
    ctx.showMsg('ok', t('nexhub.webhooks.toggled'));
    await load();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.webhooks.toggleFailed')}: ${errMsg(e)}`);
  }
}

async function doDelete(): Promise<void> {
  if (!deleteTarget.value) return;
  deleting.value = true;
  try {
    await endpoints.deleteCodeRepoWebhook(props.repoName.trim(), deleteTarget.value.id);
    ctx.showMsg('ok', t('nexhub.webhooks.deleted'));
    deleteTarget.value = null;
    await load();
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.webhooks.deleteFailed')}: ${errMsg(e)}`);
  } finally {
    deleting.value = false;
  }
}

/** 投递状态徽章文案。 */
function deliveryBadge(h: WebhookRow): { cls: string; text: string } {
  if (!h.last_status) return { cls: 'pill-muted', text: t('nexhub.webhooks.never') };
  const ok = /^\d{3}$/.test(h.last_status);
  return {
    cls: ok ? 'pill-ok' : 'pill-fail',
    text: ok ? h.last_status : `${t('nexhub.webhooks.failShort')}: ${h.last_status}`,
  };
}
</script>

<template>
  <section class="webhooks-tab">
    <div v-if="loading" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="loadError" class="card empty-card">{{ loadError }}</div>
    <template v-else>
      <!-- 说明 + 新建入口 -->
      <div class="card wh-head">
        <div class="wh-hint">{{ t('nexhub.webhooks.hint') }}</div>
        <button class="btn btn-primary" type="button" @click="openCreate">
          {{ t('nexhub.webhooks.create') }}
        </button>
      </div>

      <!-- 空态 -->
      <div v-if="!hooks.length" class="card empty-card">{{ t('nexhub.webhooks.empty') }}</div>

      <!-- 钩子卡列表 -->
      <div v-for="h in hooks" :key="h.id" class="card wh-card">
        <div class="wh-row-main">
          <code class="wh-url" :title="h.url">{{ h.url }}</code>
          <span v-if="h.repo === '*'" class="pill pill-global" :title="t('nexhub.webhooks.globalTitle')">
            {{ t('nexhub.webhooks.globalBadge') }}
          </span>
          <span class="pill" :class="h.enabled ? 'pill-ok' : 'pill-muted'">
            {{ h.enabled ? t('nexhub.webhooks.stateEnabled') : t('nexhub.webhooks.stateDisabled') }}
          </span>
          <span class="wh-spacer" />
          <span class="pill" :class="deliveryBadge(h).cls">{{ deliveryBadge(h).text }}</span>
        </div>
        <div class="wh-row-meta">
          <!-- 事件订阅面 -->
          <div class="wh-events">
            <span
              v-for="k in EVENT_KEYS"
              :key="k"
              class="ev-chip"
              :class="{ on: h.events.includes(k) }"
            >{{ eventLabel(k) }}</span>
          </div>
          <!-- 最近投递时间 -->
          <span class="wh-last">
            {{ t('nexhub.webhooks.lastDelivery') }}:
            {{ h.last_delivery || t('nexhub.webhooks.never') }}
          </span>
        </div>
        <div class="wh-row-actions">
          <button class="btn btn-small" type="button" @click="void doToggle(h)">
            {{ h.enabled ? t('nexhub.webhooks.disableAction') : t('nexhub.webhooks.enableAction') }}
          </button>
          <button class="btn btn-small btn-danger" type="button" @click="deleteTarget = h">
            {{ t('nexhub.webhooks.deleteAction') }}
          </button>
        </div>
        <!-- 投递环形日志（最近 20 条） -->
        <details v-if="(deliveries[h.id] ?? []).length" class="wh-logs">
          <summary>{{ t('nexhub.webhooks.deliveries') }}（{{ t('nexhub.webhooks.deliveriesHint', { n: 20 }) }}）</summary>
          <table class="wh-log-table">
            <thead>
              <tr>
                <th>{{ t('nexhub.webhooks.colEvent') }}</th>
                <th>{{ t('nexhub.webhooks.colStatus') }}</th>
                <th>{{ t('nexhub.webhooks.colAttempts') }}</th>
                <th>{{ t('nexhub.webhooks.colTime') }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="d in deliveries[h.id]" :key="d.id">
                <td><code>{{ d.event }}</code></td>
                <td><span class="pill" :class="d.ok ? 'pill-ok' : 'pill-fail'">{{ d.status }}</span></td>
                <td>{{ d.attempts }}</td>
                <td class="wh-time">{{ d.delivered_at }}</td>
              </tr>
            </tbody>
          </table>
        </details>
      </div>
    </template>

    <!-- 新建弹层（站内卡，非原生对话框） -->
    <div v-if="showCreate" class="wh-modal-mask" @click.self="showCreate = false">
      <div class="card wh-modal">
        <h3 class="wh-modal-title">{{ t('nexhub.webhooks.createTitle', { repo: props.repoName }) }}</h3>
        <label class="wh-field">
          <span class="wh-label">URL</span>
          <input
            v-model="formUrl"
            type="url"
            class="wh-input"
            :placeholder="t('nexhub.webhooks.urlPlaceholder')"
          />
        </label>
        <label class="wh-field">
          <span class="wh-label">{{ t('nexhub.webhooks.secretLabel') }}</span>
          <input
            v-model="formSecret"
            type="text"
            class="wh-input"
            autocomplete="off"
            :placeholder="t('nexhub.webhooks.secretPlaceholder')"
          />
          <span class="wh-field-hint">{{ t('nexhub.webhooks.secretHint') }}</span>
        </label>
        <div class="wh-field">
          <span class="wh-label">{{ t('nexhub.webhooks.eventsLabel') }}</span>
          <div class="wh-events">
            <button
              v-for="k in EVENT_KEYS"
              :key="k"
              class="ev-chip ev-toggle"
              :class="{ on: formEvents.includes(k) }"
              type="button"
              @click="toggleFormEvent(k)"
            >{{ eventLabel(k) }}</button>
          </div>
        </div>
        <p v-if="formError" class="wh-form-error">{{ formError }}</p>
        <div class="wh-modal-actions">
          <button class="btn" type="button" @click="showCreate = false">
            {{ t('common.cancel') }}
          </button>
          <button class="btn btn-primary" type="button" :disabled="creating || !formValid" @click="void doCreate()">
            {{ creating ? t('nexhub.webhooks.creating') : t('nexhub.webhooks.createAction') }}
          </button>
        </div>
      </div>
    </div>

    <!-- 删除确认（站内弹窗） -->
    <NexhubConfirm
      :open="deleteTarget !== null"
      :title="t('nexhub.webhooks.deleteConfirmTitle')"
      :body="t('nexhub.webhooks.deleteConfirmBody', { url: deleteTarget?.url ?? '' })"
      :danger="true"
      :confirm-text="t('nexhub.webhooks.deleteAction')"
      @confirm="void doDelete()"
    />
  </section>
</template>

<style scoped>
.webhooks-tab { display: flex; flex-direction: column; gap: 12px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12px; }
.btn-primary {
  background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff;
}
.btn-primary:hover { filter: brightness(1.06); background: var(--accent, #E95420); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.3); }
.btn-danger:hover { background: #fee2e2; }
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 11.5px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-fail { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-global { color: #7c3aed; background: #ede9fe; }

.wh-head { display: flex; align-items: center; gap: 12px; padding: 14px 16px; flex-wrap: wrap; }
.wh-hint { flex: 1; min-width: 240px; font-size: 13px; line-height: 1.55; color: var(--text-muted, #5E5C5F); }
.wh-card { display: flex; flex-direction: column; gap: 8px; padding: 12px 16px; }
.wh-row-main { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.wh-url {
  font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 12.5px;
  color: var(--text, #2B2B2B); word-break: break-all;
}
.wh-spacer { flex: 1; }
.wh-row-meta { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.wh-events { display: flex; gap: 6px; flex-wrap: wrap; }
.ev-chip {
  padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 11.5px; font-weight: 600;
  background: #f3f4f6; color: #9ca3af; border: 1px solid transparent;
}
.ev-chip.on { background: rgba(233, 84, 32, 0.12); color: var(--accent, #E95420); }
.ev-toggle { cursor: pointer; border-color: var(--border, #d1d5db); font-family: inherit; }
.wh-last { font-size: 12px; color: var(--text-muted, #5E5C5F); }
.wh-row-actions { display: flex; gap: 8px; }
.wh-logs { border-top: 1px dashed var(--border-soft, #EDEDED); }
.wh-logs summary { cursor: pointer; padding: 8px 0 4px; font-size: 12.5px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.wh-log-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.wh-log-table th {
  text-align: left; padding: 4px 10px 4px 0; color: var(--text-muted, #5E5C5F);
  font-weight: 600; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.wh-log-table td { padding: 4px 10px 4px 0; border-bottom: 1px dashed var(--border-soft, #EDEDED); }
.wh-time { color: var(--text-muted, #5E5C5F); white-space: nowrap; }
.wh-log-table code { font-family: 'Ubuntu Mono', Consolas, monospace; font-size: 11.5px; }

.wh-modal-mask {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); z-index: 60;
  display: flex; align-items: center; justify-content: center; padding: 20px;
}
.wh-modal { width: min(480px, 100%); padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.wh-modal-title { margin: 0; font-size: 15px; font-weight: 700; color: var(--text, #2B2B2B); }
.wh-field { display: flex; flex-direction: column; gap: 5px; }
.wh-label { font-size: 12.5px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.wh-input {
  padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  font-size: 13px; font-family: 'Ubuntu Mono', Consolas, monospace; color: var(--text, #2B2B2B);
  background: var(--bg-card, #fff);
}
.wh-input:focus { outline: 2px solid rgba(233, 84, 32, 0.25); border-color: var(--accent, #E95420); }
.wh-field-hint { font-size: 11.5px; color: var(--text-muted, #5E5C5F); line-height: 1.5; }
.wh-form-error { margin: 0; font-size: 12.5px; color: #b91c1c; }
.wh-modal-actions { display: flex; justify-content: flex-end; gap: 8px; }
</style>
