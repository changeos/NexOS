<script setup lang="ts">
// =============================================================================
// NxToast.vue —— 全局通知渲染器（v0.1.11；状态唯一事实源 src/nx/toast.ts）。
//
// 应用根（FilmStudio 根模板末尾）挂一次：桌面嵌入模式与 standalone 独立模式
// 的根组件都是 FilmStudio——两载体同享，无需各挂。Teleport 到 body 右上角
// 堆叠（最多 4 条，新在顶），enter/leave 过渡（右侧滑入淡出）；z-index 600
// （压过弹窗 100 / popover 520——弹窗内操作的成败反馈也可见）。
//
// 视觉全走 nx token（--nx-* 定义在 :root——Teleport 出 .nx-page 作用域仍可读，
// 深浅主题随 <html data-theme> 换值）。无障碍：容器 aria-live=polite；每条
// role=status（error 用 role=alert——断言级即时播报）。loading 态渲染旋转 ↻。
// =============================================================================
import { useI18n } from 'vue-i18n'
import { dismiss, useToastState, type ToastItem } from './toast'

const { t } = useI18n()
const state = useToastState()

/** 各 kind 图标（loading 为 ↻ + 旋转）。 */
const KIND_ICON: Record<ToastItem['kind'], string> = {
  success: '✓',
  error: '✕',
  warning: '⚠',
  info: 'ℹ',
  loading: '↻',
}
/** 各 kind 图标色（状态四色 token；loading 走主色）。 */
const KIND_COLOR: Record<ToastItem['kind'], string> = {
  success: 'var(--nx-success)',
  error: 'var(--nx-danger)',
  warning: 'var(--nx-warning)',
  info: 'var(--nx-info)',
  loading: 'var(--nx-accent)',
}

function icon(item: ToastItem): string {
  return KIND_ICON[item.kind]
}
function iconColor(item: ToastItem): string {
  return KIND_COLOR[item.kind]
}
/** 动作钮点击后随手关掉该条（动作已完成，留条无意义）。 */
function onAction(item: ToastItem): void {
  item.action?.onClick()
  dismiss(item.id)
}
</script>

<template>
  <Teleport to="body">
    <div class="nx-toasts" aria-live="polite" aria-atomic="false">
      <TransitionGroup name="nx-toast">
        <div
          v-for="item in state.items"
          :key="item.id"
          class="nx-toast"
          :class="`nx-toast--${item.kind}`"
          :role="item.kind === 'error' ? 'alert' : 'status'"
        >
          <span
            class="nx-toast__icon"
            :class="{ 'nx-toast__spin': item.kind === 'loading' }"
            :style="{ color: iconColor(item) }"
            aria-hidden="true"
          >{{ icon(item) }}</span>
          <div class="nx-toast__body">
            <div v-if="item.title" class="nx-toast__title">{{ item.title }}</div>
            <div class="nx-toast__msg">{{ item.message }}</div>
            <button
              v-if="item.action"
              class="nx-toast__action"
              type="button"
              @click="onAction(item)"
            >{{ item.action.label }}</button>
          </div>
          <button
            class="nx-toast__close"
            type="button"
            :aria-label="t('toast.close')"
            :title="t('toast.close')"
            @click="dismiss(item.id)"
          >×</button>
        </div>
      </TransitionGroup>
    </div>
  </Teleport>
</template>

<style scoped>
/* 堆叠容器：右上角固定列，新在顶；容器不挡点击（条目自身恢复） */
.nx-toasts {
  position: fixed;
  top: 14px;
  right: 14px;
  z-index: 600;
  display: flex;
  flex-direction: column;
  gap: 8px;
  width: min(360px, calc(100vw - 28px));
  pointer-events: none;
}
/* 单条：nx 卡刻面（token 底/描边/圆角/阴影）+ 左侧 kind 色条 */
.nx-toast {
  pointer-events: auto;
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 10px 12px;
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-regular);
  border-left: 3px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-lg);
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
  word-break: break-word;
}
.nx-toast--success { border-left-color: var(--nx-success); }
.nx-toast--error { border-left-color: var(--nx-danger); }
.nx-toast--warning { border-left-color: var(--nx-warning); }
.nx-toast--info { border-left-color: var(--nx-info); }
.nx-toast--loading { border-left-color: var(--nx-accent); }

.nx-toast__icon {
  flex-shrink: 0;
  font-size: 14px;
  line-height: 1.5;
  font-weight: 600;
}
.nx-toast__spin {
  display: inline-block;
  animation: nx-toast-spin 0.8s linear infinite;
}
@keyframes nx-toast-spin {
  to { transform: rotate(360deg); }
}
.nx-toast__body {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.nx-toast__title {
  font-weight: 600;
  font-size: var(--nx-font-size-sm);
  line-height: 1.45;
}
.nx-toast__msg {
  color: var(--nx-text-secondary);
  line-height: 1.5;
}
/* 动作钮（如失败任务「查看任务」）：次级描边小钮 */
.nx-toast__action {
  align-self: flex-start;
  margin-top: 4px;
  padding: 2px 10px;
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  border: 1px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-badge);
  background: var(--nx-bg-card);
  color: var(--nx-text-secondary);
  cursor: pointer;
  line-height: 1.6;
  transition: background 0.15s ease, color 0.15s ease;
}
.nx-toast__action:hover { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.nx-toast__action:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; }
/* 关闭钮（error 常驻；其余 hover 显） */
.nx-toast__close {
  flex-shrink: 0;
  background: transparent;
  border: none;
  font-size: 16px;
  line-height: 1.2;
  color: var(--nx-text-tertiary);
  cursor: pointer;
  padding: 0 2px;
  opacity: 0.6;
  transition: opacity 0.15s ease, color 0.15s ease;
}
.nx-toast:hover .nx-toast__close { opacity: 1; }
.nx-toast__close:hover { color: var(--nx-text-primary); }
.nx-toast__close:focus-visible { opacity: 1; outline: 2px solid var(--nx-ring-color); outline-offset: 1px; border-radius: 4px; }
.nx-toast--error .nx-toast__close { opacity: 1; }

/* enter/leave 过渡：右侧滑入淡出 + 轻微缩放；挤出旧条时其余平滑上移 */
.nx-toast-enter-active,
.nx-toast-leave-active {
  transition:
    opacity 0.18s ease,
    transform 0.22s ease;
}
.nx-toast-enter-from {
  opacity: 0;
  transform: translateX(16px) scale(0.98);
}
.nx-toast-leave-to {
  opacity: 0;
  transform: translateX(24px) scale(0.98);
}
.nx-toast-move {
  transition: transform 0.22s ease;
}
</style>
