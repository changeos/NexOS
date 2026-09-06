<script setup lang="ts">
// =============================================================================
// SideNav.vue —— 工作室左侧竖向选项卡栏（FilmHub v0.1.35）。
//
// NexHub 左侧导航形态：窄图标 + 文字，可折叠（折叠后仅图标 56px；localStorage
// 记忆）。七个选项卡：五流程阶段（①剧情②分镜③定妆④音频⑤合成）+ 工作台
// （原五区：镜头面板/监视器/时间轴）+ 设置/成员。阶段项右侧徽章读 README
// 阶段：已过=✓ 绿、当前=橙实心序号、未来=灰序号；当前视图项高亮。点击 emit
// select(view)——FilmStudio 切页（不走路由）。
// =============================================================================
import { ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { FLOW_STAGES, stageBadge, stageIndex, type FilmStage, type FlowView } from './flowTypes'

const props = defineProps<{
  /** 当前视图（高亮源）。 */
  view: FlowView
  /** README 阶段（''=未知——徽章退化为纯序号）。 */
  stage: FilmStage | ''
}>()

const emit = defineEmits<{ (e: 'select', view: FlowView): void }>()

const { t } = useI18n()

/** 折叠态（localStorage 记忆；默认展开）。 */
const collapsed = ref(false)
try {
  collapsed.value = localStorage.getItem('nexos.film.navCollapsed') === '1'
} catch {
  /* 隐私模式等：忽略 */
}
watch(collapsed, (v) => {
  try {
    localStorage.setItem('nexos.film.navCollapsed', v ? '1' : '0')
  } catch {
    /* 忽略 */
  }
})

/** 阶段项图标。 */
const STAGE_ICONS: Record<FilmStage, string> = {
  story: '📖',
  storyboard: '🎞',
  casting: '👗',
  audio: '🎵',
  compose: '🎬',
}

/** 阶段项 i18n 标签（显式映射——vue-i18n 键不做运行时拼接）。 */
function labelFor(s: FilmStage): string {
  switch (s) {
    case 'story':
      return t('film.flowStory')
    case 'storyboard':
      return t('film.flowStoryboard')
    case 'casting':
      return t('film.flowCasting')
    case 'audio':
      return t('film.flowAudio')
    case 'compose':
      return t('film.flowCompose')
  }
}

/** 阶段徽章态：done（README 阶段已过）/ current（README 当前阶段）/ todo。 */
function badgeState(s: FilmStage): 'done' | 'current' | 'todo' {
  if (!props.stage) return 'todo'
  const cur = stageIndex(props.stage)
  const idx = stageIndex(s)
  return idx < cur ? 'done' : idx === cur ? 'current' : 'todo'
}

/** 阶段徽章文案（已过=✓；其余=①②③④⑤）。 */
function badgeText(s: FilmStage): string {
  return badgeState(s) === 'done' ? '✓' : stageBadge(stageIndex(s))
}
</script>

<template>
  <nav class="fh-nav" :class="{ 'is-collapsed': collapsed }" :aria-label="t('film.flowNavAria')">
    <button
      class="fh-nav-toggle"
      type="button"
      :title="collapsed ? t('film.flowNavExpand') : t('film.flowNavCollapse')"
      :aria-label="collapsed ? t('film.flowNavExpand') : t('film.flowNavCollapse')"
      @click="collapsed = !collapsed"
    >{{ collapsed ? '»' : '«' }}</button>

    <!-- 五流程阶段 -->
    <button
      v-for="s in FLOW_STAGES"
      :key="s"
      class="fh-nav-item"
      :class="{ 'is-active': view === s }"
      type="button"
      :title="collapsed ? labelFor(s) : undefined"
      @click="emit('select', s)"
    >
      <span class="fh-nav-icon" aria-hidden="true">{{ STAGE_ICONS[s] }}</span>
      <span class="fh-nav-label">{{ labelFor(s) }}</span>
      <span
        class="fh-nav-badge"
        :class="{
          'is-done': badgeState(s) === 'done',
          'is-current': badgeState(s) === 'current',
        }"
        :title="stage === s ? t('film.flowStageCurrent') : undefined"
      >{{ badgeText(s) }}</span>
    </button>

    <div class="fh-nav-sep" role="separator" />

    <!-- 工作台（原五区：镜头面板/监视器/时间轴） -->
    <button
      class="fh-nav-item"
      :class="{ 'is-active': view === 'workbench' }"
      type="button"
      :title="collapsed ? t('film.flowWorkbench') : t('film.flowWorkbenchTip')"
      @click="emit('select', 'workbench')"
    >
      <span class="fh-nav-icon" aria-hidden="true">🛠</span>
      <span class="fh-nav-label">{{ t('film.flowWorkbench') }}</span>
    </button>

    <!-- 设置/成员（多人分工：成员/认领/活动流） -->
    <button
      class="fh-nav-item"
      :class="{ 'is-active': view === 'settings' }"
      type="button"
      :title="collapsed ? t('film.flowSettings') : t('film.flowSettingsTip')"
      @click="emit('select', 'settings')"
    >
      <span class="fh-nav-icon" aria-hidden="true">⚙</span>
      <span class="fh-nav-label">{{ t('film.flowSettings') }}</span>
    </button>

    <div class="fh-nav-hint fh-ellipsis">{{ t('film.flowNavHint') }}</div>
  </nav>
</template>
