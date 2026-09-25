<script setup lang="ts">
// =============================================================================
// SideNav.vue —— 工作室左侧竖向选项卡栏（FilmHub v0.1.35；v0.1.1 加大厅项；
// v0.1.36 底部常开 Hub 树卡；v0.1.37.1 树卡注册表式；v0.1.38 重排 + 模型项；
// v0.1.39 工作台回流程尾部 + 树卡标题用项目名；v0.1.5 删「Hub 浏览」项 +
// 视图外链 + 树卡大展开适配）。
//
// NexHub 左侧导航形态：窄图标 + 文字，可折叠（折叠后仅图标 56px；localStorage
// 记忆）。v0.1.5 排序（用户诉求：Hub 浏览项与左下树卡重复——删除该项）：
//   🎬 FilmHub（回大厅）→ 📖 剧情 → 🎞 分镜 → 👗 定妆 → 🎵 音频 → 🎬 合成 →
//   🛠 工作台（流程尾部普通项）→ ⚙ 设置/成员 → 🤖 模型设置
// （hub 视图本身保留：树卡「完整浏览」/ 树卡点文件 / 深链 view=hub 仍可达。）
// 阶段项徽章只保留完成态 ✓（README 阶段已过；序号 ①②③④⑤ 已按用户诉求去掉），
// 当前阶段高亮 is-active。点击 emit select(view)——FilmStudio 切页（不走路由）；
// 品牌项 emit home（FilmStudio 回项目大厅）。
// v0.1.5 视图外链（用户诉求「选项卡均可在网页上打开」）：每个视图项行尾
// hover 显 ⧉ 小钮——window.open(standalone.html?p=<项目>&view=<view>)，独立
// 页深链直达该视图（嵌入桌面模式同源打开 /apps-assets/film/ 前缀——token
// 同源共享天然可用；工作台/各页顶栏不加额外按钮，SideNav 入口足够）。
// v0.1.36：选项卡列表之下的**常开区**——公共 HubTreeCard（🗂 <项目标题>，
// v0.1.39 树卡头不再写死「Hub 树」，改用 FlowContext 项目标题；无项目回退
// i18n treeTitleFallback）。
// v0.1.37.1（用户诉求「像 Windows 注册表一样」）：紧凑态树卡纯导航——点击
// 文件 emit file-click(path)（FilmStudio select('hub') + pendingHubFile 定位，
// 在 Hub 浏览页网页主区打开该文件）；「完整浏览」emit browse；项目 id 从
// FlowContext 取，无上下文/折叠态时空置不崩。
// v0.1.5 树卡大展开：expandable 传入（展开态卡内下区加载内容，见
// HubTreeCard）——expanded-change 回写本组件 treeExpanded，根 nav 与树卡
// 容器挂 is-tree-expanded 类：树卡撑满左栏剩余全高、选项卡列表可压缩滚动。
// =============================================================================
import { computed, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { FLOW_STAGES, stageIndex, type FilmStage, type FlowView } from './flowTypes'
import HubTreeCard from './HubTreeCard.vue'
import { useFlow } from './flowContext'
import { buildStandaloneUrl } from './flowFiles'

const props = defineProps<{
  /** 当前视图（高亮源）。 */
  view: FlowView
  /** README 阶段（''=未知——完成态徽章退化为无）。 */
  stage: FilmStage | ''
}>()

const emit = defineEmits<{
  (e: 'select', view: FlowView): void
  (e: 'home'): void
  (e: 'file-click', path: string): void
}>()

const { t } = useI18n()
const ctx = useFlow()

/** 树卡项目 id（FlowContext 当前项目；无上下文/未进项目 → 空=空态）。 */
const treeProjectId = computed(() => ctx?.project.value?.id ?? '')

/** 树卡标题（v0.1.39 用户诉求：不用「Hub 树」——用项目标题，如「🗂 诛仙」；
 *  无项目上下文时回退 i18n filmhub.treeTitleFallback）。 */
const treeTitle = computed(() => ctx?.project.value?.title ?? '')

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

// —— v0.1.5 视图外链（⧉ 钮 → 新标签页独立页深链） ——

/** 独立模式标记（与 FilmStudio 同口径——独立页内不需要再开自己）。 */
const isStandalone = Boolean(
  (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__,
)

/** 视图独立页 URL：嵌入桌面模式 = /apps-assets/film/ 前缀（与 FilmStudio
 *  openStandalone 同口径）；独立模式 = 按当前地址推导（vite preview 根路径
 *  与 /apps-assets/film/ 两种挂载形态都正确解析）。 */
function standaloneViewUrl(view: FlowView): string {
  const rel = buildStandaloneUrl(treeProjectId.value, view)
  if (isStandalone) {
    const href = (globalThis as { location?: Location }).location?.href ?? '/'
    try {
      return new URL(rel, href).href
    } catch {
      return rel
    }
  }
  return `/apps-assets/film/${rel}`
}

/** 行尾 ⧉：新标签页打开该视图独立页（stopPropagation——不触发本行切页）。 */
function openViewLink(view: FlowView, ev: Event): void {
  ev.stopPropagation()
  if (!treeProjectId.value) return
  window.open(standaloneViewUrl(view), '_blank', 'noopener')
}

// —— v0.1.5 树卡大展开：expanded-change 回写（根 nav + 树卡容器布局适配） ——

/** 树卡大展开态（HubTreeCard expanded-change 回写）。 */
const treeExpanded = ref(false)

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

/** 阶段是否已过（README 阶段早于该阶段 → ✓ 完成徽章；序号已去）。 */
function isStageDone(s: FilmStage): boolean {
  if (!props.stage) return false
  return stageIndex(s) < stageIndex(props.stage)
}
</script>

<template>
  <nav
    class="fh-nav"
    :class="{ 'is-collapsed': collapsed, 'is-tree-expanded': treeExpanded }"
    :aria-label="t('film.flowNavAria')"
  >
    <button
      class="fh-nav-toggle"
      type="button"
      :title="collapsed ? t('film.flowNavExpand') : t('film.flowNavCollapse')"
      :aria-label="collapsed ? t('film.flowNavExpand') : t('film.flowNavCollapse')"
      @click="collapsed = !collapsed"
    >{{ collapsed ? '»' : '«' }}</button>

    <!-- 选项卡列表（可滚动区——树卡展开时本区可压缩 overflow 滚动） -->
    <div class="fh-nav-items">
      <!-- 🎬 FilmHub 品牌项（回大厅——显性导航层级顶点） -->
      <button
        class="fh-nav-item fh-nav-home"
        type="button"
        :title="t('filmhub.lobby')"
        @click="emit('home')"
      >
        <span class="fh-nav-icon" aria-hidden="true">🎬</span>
        <span class="fh-nav-label">FilmHub</span>
      </button>

      <div class="fh-nav-sep" role="separator" />

      <!-- 五流程阶段（v0.1.38 去序号徽章——仅保留完成态 ✓；行尾 ⧉ 外链钮） -->
      <template v-for="s in FLOW_STAGES" :key="s">
        <button
          class="fh-nav-item"
          :class="{ 'is-active': view === s }"
          type="button"
          :title="collapsed ? labelFor(s) : undefined"
          @click="emit('select', s)"
        >
          <span class="fh-nav-icon" aria-hidden="true">{{ STAGE_ICONS[s] }}</span>
          <span class="fh-nav-label">{{ labelFor(s) }}</span>
          <span v-if="isStageDone(s)" class="fh-nav-badge is-done">✓</span>
          <span
            class="fh-nav-ext"
            role="button"
            :tabindex="-1"
            :title="t('film.viewLink')"
            :aria-label="t('film.viewLink')"
            :data-view="s"
            @click.stop="openViewLink(s, $event)"
          >⧉</span>
        </button>
      </template>

      <!-- 🛠 工作台（v0.1.39 流程尾部普通项；原五区：镜头面板/监视器/时间轴） -->
      <button
        class="fh-nav-item"
        :class="{ 'is-active': view === 'workbench' }"
        type="button"
        :title="collapsed ? t('film.flowWorkbench') : t('film.flowWorkbenchTip')"
        @click="emit('select', 'workbench')"
      >
        <span class="fh-nav-icon" aria-hidden="true">🛠</span>
        <span class="fh-nav-label">{{ t('film.flowWorkbench') }}</span>
        <span
          class="fh-nav-ext"
          role="button"
          :tabindex="-1"
          :title="t('film.viewLink')"
          :aria-label="t('film.viewLink')"
          data-view="workbench"
          @click.stop="openViewLink('workbench', $event)"
        >⧉</span>
      </button>

      <div class="fh-nav-sep" role="separator" />

      <!-- v0.1.5：原「Hub 浏览」导航项已删（与左下树卡重复）——hub 视图经
           树卡「完整浏览」/ 树卡点文件 / 深链 view=hub 到达，HubBrowse 保留 -->

      <!-- 🧭 协作（v0.1.10：PR/Issues——issue 列表/详情评论流 + PR diff/merge；
           任务中心失败任务「提 Issue」成功后的跳转落点） -->
      <button
        class="fh-nav-item"
        :class="{ 'is-active': view === 'collab' }"
        type="button"
        :title="collapsed ? t('collab.navCollab') : t('collab.navTip')"
        @click="emit('select', 'collab')"
      >
        <span class="fh-nav-icon" aria-hidden="true">🧭</span>
        <span class="fh-nav-label">{{ t('collab.navCollab') }}</span>
        <span
          class="fh-nav-ext"
          role="button"
          :tabindex="-1"
          :title="t('film.viewLink')"
          :aria-label="t('film.viewLink')"
          data-view="collab"
          @click.stop="openViewLink('collab', $event)"
        >⧉</span>
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
        <span
          class="fh-nav-ext"
          role="button"
          :tabindex="-1"
          :title="t('film.viewLink')"
          :aria-label="t('film.viewLink')"
          data-view="settings"
          @click.stop="openViewLink('settings', $event)"
        >⧉</span>
      </button>

      <!-- 🤖 模型设置（v0.1.39 导航尾部最后一项；项目级八能力位默认模型源） -->
      <button
        class="fh-nav-item"
        :class="{ 'is-active': view === 'models' }"
        type="button"
        :title="collapsed ? t('models.navModelSettings') : t('models.navTip')"
        @click="emit('select', 'models')"
      >
        <span class="fh-nav-icon" aria-hidden="true">🤖</span>
        <span class="fh-nav-label">{{ t('models.navModelSettings') }}</span>
        <span
          class="fh-nav-ext"
          role="button"
          :tabindex="-1"
          :title="t('film.viewLink')"
          :aria-label="t('film.viewLink')"
          data-view="models"
          @click.stop="openViewLink('models', $event)"
        >⧉</span>
      </button>
    </div>

    <!-- v0.1.36 底部常开区 / v0.1.37.1 注册表式 / v0.1.5 大展开：🗂 项目标题
         树卡（紧凑态纯导航——点文件 emit file-click → FilmStudio 跳 Hub 浏览页
         打开；「完整浏览」同跳不带定位；展开态卡内下区加载内容 + 撑满左栏
         剩余全高；折叠态收起；v0.1.39 卡头标题=项目标题） -->
    <div class="fh-nav-tree" :class="{ 'is-tree-expanded': treeExpanded }">
      <HubTreeCard
        :project-id="treeProjectId"
        :title="treeTitle"
        :reload-key="ctx?.refreshTick.value ?? 0"
        browse-link
        expandable
        @browse="emit('select', 'hub')"
        @file-click="(path: string) => emit('file-click', path)"
        @expanded-change="(v: boolean) => (treeExpanded = v)"
      />
    </div>
  </nav>
</template>
