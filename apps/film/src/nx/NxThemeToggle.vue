<script setup lang="ts">
// =============================================================================
// NxThemeToggle.vue —— 深浅主题切换钮（试点页顶栏；localStorage 记忆）。
//
// ghost 钮 + 日/月 SVG 图标（当前主题态：深色显月、浅色显日）；
// aria-pressed 表达开关态。主题逻辑唯一事实源：../design/theme.ts。
// =============================================================================
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import NxButton from './NxButton.vue'
import { nxTheme, toggleNxTheme } from '../design/theme'

const { t } = useI18n()
const isDark = computed(() => nxTheme.value === 'dark')
const label = computed(() => (isDark.value ? t('film.themeDark') : t('film.themeLight')))
</script>

<template>
  <NxButton
    variant="ghost"
    size="sm"
    :title="t('film.themeToggleTip')"
    :aria-label="label"
    :aria-pressed="isDark"
    @click="toggleNxTheme()"
  >
    <!-- 日（浅色态显示：点击切到深色）/ 月（深色态） -->
    <svg
      v-if="!isDark"
      viewBox="0 0 24 24"
      width="14"
      height="14"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
    </svg>
    <svg
      v-else
      viewBox="0 0 24 24"
      width="14"
      height="14"
      fill="none"
      stroke="currentColor"
      stroke-width="2"
      stroke-linecap="round"
      stroke-linejoin="round"
      aria-hidden="true"
    >
      <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
    </svg>
    <span class="nx-theme-toggle-label">{{ label }}</span>
  </NxButton>
</template>

<style scoped>
.nx-theme-toggle-label {
  font-size: 12px;
}
@media (max-width: 1080px) {
  /* 窄容器只留图标（aria-label 保语义） */
  .nx-theme-toggle-label {
    display: none;
  }
}
</style>
