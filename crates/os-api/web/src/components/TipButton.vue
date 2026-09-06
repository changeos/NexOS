<script setup lang="ts">
// =============================================================================
// TipButton —— 统一打赏按钮 + 弹窗（四大厅共用，docs/TIPS.md）
//
// 打赏 = 一条真实账本记录（from 链上身份 → to 目标所有者，服务端反查）：
//   - POST /api/v1/tips {target_kind, target_ref, amount, message?, txid?} → 202
//   - GET  /api/v1/tips/target/:kind/:ref → {total, count}（挂载时并行拉取真实累计数）
// target_kind ∈ im_message | lobby_entry | node；ref 格式见 docs/TIPS.md 映射表。
// amount 为站内积分记账；txid 为用户自报链上凭证（服务端不验真——已知限制）。
// 样式遵守项目 CSS 变量体系（--bg-card/--border/--accent/--radius…）。
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints, ApiError } from '@/api/client';
import type { TipTargetKind, TipsTargetResp } from '@/api/client';

const props = defineProps<{
  /** 打赏目标类型：im_message | lobby_entry | node。 */
  targetKind: TipTargetKind;
  /** 目标引用（消息 id / 条目 ref / NodeID——格式与后端解析一致）。 */
  targetRef: string;
  /** 链上 token 获取器（IM ensureAuthenticated / NexHub ensureNexhubToken
   *  皆可——服务端两桶依次验；缺省回落网关 Principal/测试期 admin）。 */
  getToken?: () => Promise<string | undefined>;
  /** 按钮尺寸（跟随宿主操作区样式）。 */
  size?: 'small' | 'normal';
}>();

const emit = defineEmits<{
  /** 打赏成功（携带账本 id 与金额——宿主可据此刷新本地累计）。 */
  (e: 'tipped', payload: { id: number; amount: number }): void;
}>();

const { t } = useI18n();

const show = ref(false);
const amount = ref<number>(10);
const message = ref('');
const txid = ref('');
const submitting = ref(false);
const error = ref('');
const done = ref(false);
// 真实累计数（挂载即拉；失败静默为 0——不打断宿主页面）
const total = ref(0);
const count = ref(0);

const quickAmounts = [5, 10, 50, 100];

const canSubmit = computed(
  () => !submitting.value && Number.isInteger(amount.value) && amount.value > 0,
);

async function loadTotal(): Promise<void> {
  try {
    const agg: TipsTargetResp = await endpoints.tipsTarget(
      props.targetKind,
      props.targetRef,
    );
    total.value = agg.total;
    count.value = agg.count;
  } catch {
    // 目标聚合失败（目标尚无打赏/网络抖动）静默降级为 0，不阻塞宿主 UI
    total.value = 0;
    count.value = 0;
  }
}

function open(): void {
  show.value = true;
  done.value = false;
  error.value = '';
  void loadTotal();
}

function close(): void {
  show.value = false;
}

async function submit(): Promise<void> {
  if (!canSubmit.value) return;
  submitting.value = true;
  error.value = '';
  try {
    // 链上身份优先：宿主提供了取 token 器则先取（IM/NexHub 挑战-签名 token，
    // 服务端反查 from pubkey）；取不到回落网关 Principal（测试期默认 admin）
    let token: string | undefined;
    try {
      token = props.getToken ? await props.getToken() : undefined;
    } catch {
      token = undefined; // 认证失败不阻塞打赏——回落 Principal 归因
    }
    const resp = await endpoints.tipCreate(
      {
        target_kind: props.targetKind,
        target_ref: props.targetRef,
        amount: amount.value,
        message: message.value.trim() || undefined,
        txid: txid.value.trim() || undefined,
      },
      token ? { token } : undefined,
    );
    total.value += resp.amount;
    count.value += 1;
    done.value = true;
    emit('tipped', { id: resp.id, amount: resp.amount });
  } catch (e) {
    error.value = e instanceof ApiError ? e.message : t('tips.errGeneric');
  } finally {
    submitting.value = false;
  }
}

onMounted(() => {
  void loadTotal();
});
</script>

<template>
  <button
    type="button"
    class="tip-btn"
    :class="size === 'small' ? 'tip-btn-small' : ''"
    :title="t('tips.btnTitle')"
    @click.stop="open"
  >
    💝 {{ t('tips.btn') }}<span v-if="count > 0" class="tip-total">{{ total }}</span>
  </button>

  <Teleport to="body">
    <div v-if="show" class="tip-modal-backdrop" @click.self="close">
      <div class="tip-modal" role="dialog" aria-modal="true" :aria-label="t('tips.title')">
        <div class="tip-modal-head">
          <h3>💝 {{ t('tips.title') }}</h3>
          <button type="button" class="tip-modal-close" :aria-label="t('tips.close')" @click="close">
            ×
          </button>
        </div>
        <div class="tip-modal-body">
          <p class="tip-target mono" :title="`${targetKind}:${targetRef}`">
            {{ t('tips.target') }} {{ targetKind }} · {{ targetRef }}
          </p>
          <p v-if="done" class="tip-done">
            ✅ {{ t('tips.done', { amount }) }}<br />
            <span class="tip-sub">{{ t('tips.doneSub') }}</span>
          </p>
          <template v-else>
            <label class="tip-label">
              {{ t('tips.amountLabel') }}
              <div class="tip-amount-row">
                <input
                  v-model.number="amount"
                  type="number"
                  min="1"
                  step="1"
                  class="tip-input tip-amount-input"
                />
                <span
                  v-for="q in quickAmounts"
                  :key="q"
                  class="tip-quick"
                  :class="{ active: amount === q }"
                  @click="amount = q"
                  >{{ q }}</span
                >
              </div>
            </label>
            <label class="tip-label">
              {{ t('tips.messageLabel') }}
              <input
                v-model="message"
                type="text"
                maxlength="500"
                class="tip-input"
                :placeholder="t('tips.messagePh')"
              />
            </label>
            <label class="tip-label">
              {{ t('tips.txidLabel') }}
              <input
                v-model="txid"
                type="text"
                maxlength="128"
                class="tip-input mono"
                :placeholder="t('tips.txidPh')"
              />
              <span class="tip-sub">{{ t('tips.txidHint') }}</span>
            </label>
            <p v-if="error" class="tip-error">⚠️ {{ error }}</p>
            <div class="tip-actions">
              <button type="button" class="btn" @click="close">{{ t('tips.cancel') }}</button>
              <button
                type="button"
                class="btn btn-primary"
                :disabled="!canSubmit"
                @click="submit"
              >
                {{ submitting ? t('tips.submitting') : t('tips.submit') }}
              </button>
            </div>
          </template>
        </div>
        <div class="tip-modal-foot">
          <span class="tip-sub">
            {{ t('tips.stats', { total, count }) }}
          </span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* 触发按钮：与宿主操作区 btn 体系同构（CSS 变量令牌） */
.tip-btn {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 4px 10px;
  border: 1px solid var(--accent-border, rgba(170, 59, 255, 0.5));
  border-radius: var(--radius-sm, 6px);
  background: var(--accent-bg, rgba(170, 59, 255, 0.1));
  color: var(--accent, #aa3bff);
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  white-space: nowrap;
  transition: background 0.15s ease, border-color 0.15s ease;
}
.tip-btn:hover {
  background: var(--accent, #aa3bff);
  color: #fff;
}
.tip-btn-small {
  padding: 3px 8px;
  font-size: 11px;
}
.tip-total {
  min-width: 16px;
  padding: 0 5px;
  border-radius: var(--radius-pill, 16px);
  background: var(--accent, #aa3bff);
  color: #fff;
  font-size: 10px;
  line-height: 15px;
  text-align: center;
}

/* 弹窗（Teleport 到 body——宿主 overflow 裁剪不影响） */
.tip-modal-backdrop {
  position: fixed;
  inset: 0;
  z-index: 120;
  display: flex;
  align-items: center;
  justify-content: center;
  padding: 16px;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(2px);
}
.tip-modal {
  display: flex;
  flex-direction: column;
  width: min(440px, 100%);
  max-height: 90vh;
  overflow: auto;
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #e5e4e7);
  border-radius: var(--radius, 10px);
  box-shadow: var(--shadow, 0 20px 60px rgba(0, 0, 0, 0.25));
}
.tip-modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 18px;
  border-bottom: 1px solid var(--border-soft, #ededed);
}
.tip-modal-head h3 {
  font-size: 15px;
  font-weight: 600;
  color: var(--text-h, #08060d);
}
.tip-modal-close {
  background: transparent;
  border: none;
  font-size: 22px;
  line-height: 1;
  color: var(--text-muted, #5e5c5f);
  cursor: pointer;
}
.tip-modal-body {
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px 18px;
}
.tip-target {
  font-size: 11px;
  color: var(--text-muted, #5e5c5f);
  word-break: break-all;
  margin: 0;
}
.tip-label {
  display: flex;
  flex-direction: column;
  gap: 6px;
  font-size: 12px;
  font-weight: 600;
  color: var(--text, #6b6375);
}
.tip-input {
  padding: 8px 10px;
  border: 1px solid var(--border, #e5e4e7);
  border-radius: var(--radius-sm, 6px);
  background: var(--bg, #fff);
  color: var(--text-h, #08060d);
  font-size: 13px;
}
.tip-input:focus {
  outline: none;
  border-color: var(--accent, #aa3bff);
}
.tip-amount-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.tip-amount-input {
  width: 110px;
}
.tip-quick {
  padding: 5px 10px;
  border: 1px solid var(--border, #e5e4e7);
  border-radius: var(--radius-pill, 16px);
  font-size: 12px;
  cursor: pointer;
  color: var(--text, #6b6375);
  user-select: none;
}
.tip-quick.active {
  border-color: var(--accent, #aa3bff);
  background: var(--accent-bg, rgba(170, 59, 255, 0.1));
  color: var(--accent, #aa3bff);
  font-weight: 600;
}
.tip-sub {
  font-size: 11px;
  font-weight: 400;
  color: var(--text-muted, #5e5c5f);
}
.tip-error {
  margin: 0;
  font-size: 12px;
  color: #b91c1c;
}
.tip-done {
  margin: 8px 0;
  font-size: 14px;
  font-weight: 600;
  color: #15803d;
}
.tip-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.tip-modal-foot {
  padding: 10px 18px;
  border-top: 1px solid var(--border-soft, #ededed);
}
.mono {
  font-family: var(--mono, ui-monospace, Consolas, monospace);
}
</style>
