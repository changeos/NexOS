<script setup lang="ts">
// =============================================================================
// NxButton.vue —— 按钮基座（v0.1.7 UI 重设计第一批）。
//
// 变体 × 尺寸：primary（点睛蓝实底）/ secondary（描边次级）/ ghost（弱化）/
// destructive（危险描边）× sm / md。全态覆盖：hover 换底换字、active 压深、
// disabled 三色齐退、focus-visible 2px 焦点环（token --nx-ring-color）。
// loading 态渲染环形 spinner（nx-btn__spin；兼挂 fh-spin/is-spinning 钩子，
// 与旧任务条动画/冒烟断言同一选择器）并强制 disabled。
// 视觉规则唯一事实源：src/design/theme.css `.nx-btn` 区。
// =============================================================================
withDefaults(
  defineProps<{
    variant?: 'primary' | 'secondary' | 'ghost' | 'destructive'
    size?: 'sm' | 'md'
    loading?: boolean
    disabled?: boolean
    type?: 'button' | 'submit'
    title?: string
    ariaLabel?: string
  }>(),
  { variant: 'secondary', size: 'md', loading: false, disabled: false, type: 'button' },
)
</script>

<template>
  <button
    :type="type"
    class="nx-btn nx-focus"
    :class="[`nx-btn--${variant}`, size === 'sm' ? 'nx-btn--sm' : '']"
    :disabled="disabled || loading"
    :title="title"
    :aria-label="ariaLabel"
    :aria-busy="loading || undefined"
  >
    <span v-if="loading" class="nx-btn__spin fh-spin is-spinning" aria-hidden="true" />
    <slot />
  </button>
</template>
