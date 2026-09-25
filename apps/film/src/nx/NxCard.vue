<script setup lang="ts">
// =============================================================================
// NxCard.vue —— 卡片基座（三态阴影 + 0.5px 描边）。
//
// 三态（.nx-card 区唯一事实源 design/theme.css）：
//   静默：bg-card + 0.5px line-regular 描边 + shadow-xs（近扁平）；
//   hover（is-interactive）：浮起——shadow-md + 描边加深 + 换底 subtle + -1px 位移；
//   选中（is-selected / :focus-visible）：accent 描边 + shadow-lg + 焦点环。
// flat 变体：subtle 底 + 分隔线描边无阴影（页内嵌面）。
// 头/体由调用方以 .nx-card__head / .nx-card__body 类排版（本组件保持壳层薄）。
// =============================================================================
withDefaults(
  defineProps<{
    interactive?: boolean
    selected?: boolean
    flat?: boolean
  }>(),
  { interactive: false, selected: false, flat: false },
)
</script>

<template>
  <section
    class="nx-card"
    :class="{
      'is-interactive': interactive,
      'is-selected': selected,
      'is-flat': flat,
    }"
  ><slot /></section>
</template>
