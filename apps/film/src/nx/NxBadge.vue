<script setup lang="ts">
// =============================================================================
// NxBadge.vue —— 徽章基座（MaxKB 聚合徽章思路：一词一色一图标位）。
//
// variant 六色（neutral/accent/success/warning/danger/info，soft 浅底变体为主，
// tone=solid 给 accent/success 实底档）；dot 变体内嵌状态点，pulse=true 时状态点
// 带呼吸光晕（StatusDot——运行中任务的动态标识）。尺寸 sm/md。
// 管线聚合徽章（排队/清理中/分章中/向量化中/成功/失败）即本组件的一组消费方。
// =============================================================================
withDefaults(
  defineProps<{
    variant?: 'neutral' | 'accent' | 'success' | 'warning' | 'danger' | 'info'
    tone?: 'soft' | 'solid'
    size?: 'sm' | 'md'
    dot?: boolean
    /** 状态点呼吸光晕（运行中）；dot=false 无效。 */
    pulse?: boolean
  }>(),
  { variant: 'neutral', tone: 'soft', size: 'md', dot: false, pulse: false },
)
</script>

<template>
  <span
    class="nx-badge"
    :class="[
      tone === 'solid' ? `nx-badge--solid-${variant}` : `nx-badge--${variant}`,
      size === 'sm' ? 'nx-badge--sm' : '',
    ]"
  ><span
      v-if="dot"
      class="nx-badge__dot"
      :class="{ 'nx-badge__dot--pulse': pulse }"
      aria-hidden="true"
    /><slot /></span>
</template>
