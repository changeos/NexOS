<script setup lang="ts">
// =============================================================================
// NexhubConfirm —— 站内确认弹窗（v0.1.32，替代原生 window.confirm()）。
//
// 复用项目既有 modal-overlay + card modal-card 弹窗样式（CodeHub 创建/导入
// 对话框同款），供破坏性操作确认（删仓库 / 合并 PR / 大厅下架等）。
// 用法：<NexhubConfirm v-model:open="show" :title="..." :body="..." :danger="true"
//        :confirm-text="..." @confirm="..." />
// =============================================================================

import { useI18n } from 'vue-i18n';

defineProps<{
  /** 弹窗开关（v-model:open）。 */
  open: boolean;
  /** 标题。 */
  title: string;
  /** 正文说明（可含确认语义的后果描述）。 */
  body?: string;
  /** 危险操作（确认钮红色）。 */
  danger?: boolean;
  /** 确认按钮文案（缺省用 common.confirm）。 */
  confirmText?: string;
}>();

const emit = defineEmits<{
  (e: 'update:open', v: boolean): void;
  (e: 'confirm'): void;
}>();

const { t } = useI18n();

function close(): void {
  emit('update:open', false);
}

function confirm(): void {
  emit('confirm');
  close();
}
</script>

<template>
  <div v-if="open" class="modal-overlay" @click.self="close">
    <div class="card modal-card" role="alertdialog" :aria-label="title">
      <div class="modal-head">
        <h3 class="modal-title">{{ title }}</h3>
        <button class="btn btn-small btn-ghost" type="button" @click="close">✕</button>
      </div>
      <p v-if="body" class="confirm-body">{{ body }}</p>
      <div class="modal-actions">
        <button class="btn btn-small" type="button" @click="close">
          {{ t('common.cancel') }}
        </button>
        <button
          class="btn btn-small"
          :class="danger ? 'btn-danger-solid' : 'btn-primary'"
          type="button"
          @click="confirm"
        >
          {{ confirmText || t('common.confirm') }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.modal-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); display: flex;
  align-items: center; justify-content: center; z-index: 1000; padding: 20px;
}
.modal-card { width: 100%; max-width: 420px; padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.modal-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.modal-title { font-size: 16px; font-weight: 700; color: var(--text, #2B2B2B); margin: 0; }
.confirm-body { margin: 0; font-size: 13px; line-height: 1.65; color: var(--text-muted, #5E5C5F); word-break: break-word; }
.modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 4px; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-primary { background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.btn-danger-solid { background: #b91c1c; border-color: #b91c1c; color: #fff; }
.btn-danger-solid:hover { background: #991b1b; }
</style>

