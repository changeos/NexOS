<script setup lang="ts">
// =============================================================================
// ToastContainer.vue —— 全局 Toast 浮层容器（Ubuntu Yaru 风）。
//
// 固定定位在视口右上角，渲染模块级 `toasts` 队列。每条 toast 按类型显示
// 彩色左边框（success=绿 / error=红 / info=蓝），白底卡片 + Ubuntu 圆角 +
// 轻阴影，3 秒后淡出。z-index:9999 保证浮于浮窗/Dock 之上。
// =============================================================================
import { toasts, dismissToast, type ToastType } from '@/composables/useToast'

const borderVar: Record<ToastType, string> = {
    success: 'var(--ok)',
    error: 'var(--err)',
    info: 'var(--info)',
}
</script>

<template>
    <div class="toast-container" role="region" aria-label="提示消息" aria-live="polite">
        <TransitionGroup name="toast">
            <div
                v-for="t in toasts"
                :key="t.id"
                class="toast"
                :class="{ 'toast-hidden': !t.visible }"
                :style="{ borderLeftColor: borderVar[t.type] }"
                role="alert"
            >
                <span class="toast-icon" :class="`toast-icon-${t.type}`">
                    <span v-if="t.type === 'success'">✓</span>
                    <span v-else-if="t.type === 'error'">!</span>
                    <span v-else>i</span>
                </span>
                <span class="toast-msg">{{ t.message }}</span>
                <button
                    class="toast-close"
                    type="button"
                    aria-label="关闭"
                    @click="dismissToast(t.id)"
                >×</button>
            </div>
        </TransitionGroup>
    </div>
</template>

<style scoped>
.toast-container {
    position: fixed;
    top: 16px;
    right: 16px;
    z-index: 9999;
    display: flex;
    flex-direction: column;
    gap: 10px;
    max-width: min(380px, calc(100vw - 32px));
    pointer-events: none;
}

.toast {
    pointer-events: auto;
    display: flex;
    align-items: flex-start;
    gap: 10px;
    background: var(--bg-card, #ffffff);
    color: var(--text, #2B2B2B);
    border: 1px solid var(--border-soft, #e5e5e5);
    border-left-width: 4px;
    border-radius: var(--radius, 10px);
    box-shadow: var(--shadow-lg, 0 4px 16px rgba(0, 0, 0, 0.12));
    padding: 12px 14px;
    font-size: 13.5px;
    line-height: 1.45;
    transition: opacity 0.3s ease, transform 0.3s ease;
}
.toast-hidden {
    opacity: 0;
    transform: translateX(16px);
}

.toast-icon {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
    color: #ffffff;
    font-size: 12px;
    font-weight: 700;
    line-height: 1;
}
.toast-icon-success {
    background: var(--ok, #0E8420);
}
.toast-icon-error {
    background: var(--err, #C7162B);
}
.toast-icon-info {
    background: var(--info, #335280);
}

.toast-msg {
    flex: 1;
    min-width: 0;
    word-break: break-word;
}

.toast-close {
    flex-shrink: 0;
    background: none;
    border: none;
    cursor: pointer;
    font-size: 16px;
    line-height: 1;
    color: var(--text-muted, #5E5C5F);
    padding: 0 2px;
    border-radius: var(--radius-sm, 6px);
    transition: color 0.15s ease, background 0.15s ease;
}
.toast-close:hover {
    color: var(--text, #2B2B2B);
    background: rgba(0, 0, 0, 0.05);
}

/* TransitionGroup 进出动画 */
.toast-enter-active,
.toast-leave-active {
    transition: opacity 0.3s ease, transform 0.3s ease;
}
.toast-enter-from {
    opacity: 0;
    transform: translateX(24px);
}
.toast-leave-to {
    opacity: 0;
    transform: translateX(24px);
}
.toast-move {
    transition: transform 0.3s ease;
}
</style>
