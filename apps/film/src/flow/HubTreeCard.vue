<script setup lang="ts">
// =============================================================================
// HubTreeCard.vue —— Hub 文件树卡片（公共组件，v0.1.36；v0.1.37.1 注册表式；
// v0.1.5 原位大展开）。
//
// 从 HubBrowse.vue 抽出的左栏文件树：GET :id/files 平铺清单 → buildHubTree
// 嵌套树；目录可折叠（根级缺省展开 + 卡头「全收」钮一键收起）；图标按类型
// md📝/json🧾/图🖼/音🎵/视频▶/其余📄（flowFiles 纯函数复用）。
// v0.1.37.1（用户诉求「像 Windows 注册表一样」）：紧凑态树=纯导航——点击文件
// 一律 emit file-click(path)，由宿主决定打开方式：
//   · HubBrowse 左栏：内容区展示该文件（openFile）；
//   · SideNav 底部常开树卡：emit file-click → FilmStudio select('hub')
//     + pendingHubFile 定位——跳 Hub 浏览页在网页主区打开该文件；
//   · browseLink 形态卡头显示「完整浏览」链接 emit browse。
// v0.1.5（用户诉求「树卡可完整展开成大卡」）：expandable 形态（SideNav 传）
// 卡头加「⤢ 展开」钮——展开态卡片在左栏内撑满剩余全高（flex 布局原位放大，
// 宽度仍限左栏内**绝不向右溢出**），卡内上下分栏：上 55% 树 / 下 45% 内容区
// （文本 pre 滚动·textHeadLines 截断 / 图片 contain / 音频控件；视频与二进制
// 提示走「完整浏览」）。展开态树行点击 = 下区加载内容（不再跳 Hub 浏览）；
// emit expanded-change 由宿主（SideNav）适配左栏布局。
// v0.1.39：title prop 由 SideNav 传项目标题（卡头「🗂 <项目标题>」）；未传/
// 空时回退 i18n filmhub.treeTitleFallback（navTreeTitle 键已弃用删除）。
// selected 变化时自动展开祖先目录（外部定位进入时目标行可见）。
// 宿主不传 projectId（无 FlowContext 的挂载面）→ 空态，不崩。
// =============================================================================
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { filmContentText, filmFileDataUrl, filmGetFile, filmListFiles } from '../api'
import { useFlow } from './flowContext'
import {
  buildHubTree,
  fmtBytes,
  hubFileEntries,
  hubFileIcon,
  hubPreviewKind,
  isAudioPath,
  isImagePath,
  isVideoPath,
  textHeadLines,
  type HubTreeNode,
} from './flowFiles'

const props = withDefaults(
  defineProps<{
    /** 项目 id（空 → 空态不加载）。 */
    projectId?: string
    /** 当前选中文件（高亮 + 祖先目录自动展开；HubBrowse 传）。 */
    selected?: string
    /** 卡头显示「完整浏览」链接（emit browse）。 */
    browseLink?: boolean
    /** 卡头标题（缺省回退 i18n treeTitleFallback；SideNav 传项目标题）。 */
    title?: string
    /** 重载版本号（宿主 refreshTick 递增时重拉树）。 */
    reloadKey?: number
    /** 允许原位大展开（v0.1.5：卡头「⤢ 展开」钮 + 下区内容预览；SideNav 传）。 */
    expandable?: boolean
  }>(),
  {
    projectId: '',
    selected: '',
    browseLink: false,
    title: '',
    reloadKey: 0,
    expandable: false,
  },
)

const emit = defineEmits<{
  (e: 'file-click', path: string): void
  (e: 'browse'): void
  (e: 'expanded-change', expanded: boolean): void
}>()

const { t } = useI18n()
const ctx = useFlow()

/** 统一错误文案（ctx 可用时走 404/405 口径；无宿主上下文降级 String）。 */
function errText(e: unknown): string {
  return ctx ? ctx.errMsg(e) : String(e)
}

// —— 文件树（加载 / 展开 / 拍平） ——
const tree = ref<HubTreeNode[]>([])
const treeLoading = ref(false)
const treeError = ref('')
/** 目录展开态（path → 是否展开；根级目录缺省展开）。 */
const expanded = reactive<Record<string, boolean>>({})
/** 卡整体收起态（SideNav 窄卡防挤占——收起后只剩卡头）。 */
const cardCollapsed = ref(false)

async function loadTree(): Promise<void> {
  if (!props.projectId || treeLoading.value) return
  treeLoading.value = true
  try {
    const entries = await filmListFiles(props.projectId)
    // v0.1.39.1 归一：{root, files} 信封 → 条目数组（裸数组/undefined 直通防御）
    tree.value = buildHubTree(hubFileEntries(entries))
    // 根级目录缺省展开（深层收起）
    for (const n of tree.value) if (n.isDir) expanded[n.path] = true
    treeError.value = ''
  } catch (e) {
    tree.value = []
    treeError.value = errText(e)
  } finally {
    treeLoading.value = false
  }
}

/** 可见行（拍平：折叠目录的子树跳过；缩进 = 深度）。 */
const visibleNodes = computed<{ node: HubTreeNode; depth: number }[]>(() => {
  const out: { node: HubTreeNode; depth: number }[] = []
  const walk = (nodes: HubTreeNode[], depth: number): void => {
    for (const n of nodes) {
      out.push({ node: n, depth })
      if (n.isDir && expanded[n.path]) walk(n.children, depth + 1)
    }
  }
  walk(tree.value, 0)
  return out
})

const fileCount = computed(() => visibleNodes.value.filter((r) => !r.node.isDir).length)

function toggleDir(node: HubTreeNode): void {
  expanded[node.path] = !expanded[node.path]
}

/** 折叠全收（所有目录收起，只留根级）。 */
function collapseAll(): void {
  for (const n of tree.value) if (n.isDir) expanded[n.path] = false
}

const cardTitle = computed(() => props.title || t('filmhub.treeTitleFallback'))

// —— v0.1.5 原位大展开：展开态卡片撑满左栏剩余全高（宿主 SideNav 经
//    expanded-change 适配布局），卡内上下分栏（上树 55% / 下内容 45%）——
//    展开态树行点击 = 下区加载内容；紧凑态保持注册表式 emit file-click。 ——
const cardExpanded = ref(false)

/** 下区内容预览行数上限（超长文本截断 + 「已截断」提示；完整内容走「完整浏览」）。 */
const PREVIEW_MAX_LINES = 300

function toggleExpanded(): void {
  cardExpanded.value = !cardExpanded.value
  if (!cardExpanded.value) resetPreview()
  emit('expanded-change', cardExpanded.value)
}

/** 下区内容预览态（形态判定复用 hubPreviewKind：文本 pre 滚动 / 图片 contain /
 *  音频控件；视频与二进制 → complex 提示走「完整浏览」）。 */
const preview = reactive<{
  path: string
  status: 'idle' | 'loading' | 'error'
  kind: 'text' | 'image' | 'audio' | 'complex' | ''
  text: string
  truncated: boolean
  dataUrl: string
  error: string
}>({ path: '', status: 'idle', kind: '', text: '', truncated: false, dataUrl: '', error: '' })

function resetPreview(): void {
  preview.path = ''
  preview.status = 'idle'
  preview.kind = ''
  preview.text = ''
  preview.truncated = false
  preview.dataUrl = ''
  preview.error = ''
}

/** 展开态点击文件 → 下区加载内容（不再跳 Hub 浏览；与 HubBrowse.openFile
 *  同口径：扩展名优先走 data URL，文本信封 hubPreviewKind + textHeadLines）。 */
async function openPreview(path: string): Promise<void> {
  if (!props.projectId) return
  preview.path = path
  preview.status = 'loading'
  preview.kind = ''
  preview.text = ''
  preview.dataUrl = ''
  preview.truncated = false
  preview.error = ''
  try {
    if (isImagePath(path) || isAudioPath(path)) {
      preview.dataUrl = await filmFileDataUrl(props.projectId, path)
      preview.kind = isImagePath(path) ? 'image' : 'audio'
    } else if (isVideoPath(path)) {
      preview.kind = 'complex' // 视频体积大——下区不内联，提示走「完整浏览」
    } else {
      const env = await filmGetFile(props.projectId, path)
      const kind = hubPreviewKind(path, env.mime || env.mime_type || '')
      if (kind === 'text') {
        const head = textHeadLines(filmContentText(env), PREVIEW_MAX_LINES)
        preview.kind = 'text'
        preview.text = head.text
        preview.truncated = head.truncated
      } else if (kind === 'image') {
        preview.dataUrl = await filmFileDataUrl(props.projectId, path)
        preview.kind = 'image'
      } else {
        preview.kind = 'complex' // 二进制：不支持预览——提示走「完整浏览」
      }
    }
    preview.status = 'idle'
  } catch (e) {
    preview.status = 'error'
    preview.error = errText(e)
  }
}

function onRowClick(node: HubTreeNode): void {
  if (node.isDir) {
    toggleDir(node)
    return
  }
  // v0.1.5 展开态：下区加载内容；紧凑态注册表式——点击文件交给宿主打开
  if (cardExpanded.value) {
    void openPreview(node.path)
    return
  }
  emit('file-click', node.path)
}

onMounted(() => void loadTree())
watch(
  () => props.projectId,
  (id) => {
    resetPreview() // 换项目：下区内容预览随树一起失效
    if (id) void loadTree()
  },
)
watch(
  () => props.reloadKey,
  () => {
    if (props.projectId) void loadTree()
  },
)
// 外部定位进入（selected 置位）：展开目标路径的各级祖先目录，选中行可见
watch(
  () => props.selected,
  (path) => {
    if (!path) return
    const segs = path.split('/').filter(Boolean)
    for (let i = 1; i < segs.length; i++) expanded[segs.slice(0, i).join('/')] = true
  },
  { immediate: true },
)

/** 宿主页头按钮联动面（HubBrowse 页头刷新钮）。 */
defineExpose({
  reload: loadTree,
  isLoading: computed(() => treeLoading.value),
})
</script>

<template>
  <div class="hub-tree-card" :class="{ 'is-expanded': cardExpanded }">
    <div class="hub-tree-head">
      <button
        class="hub-tree-caret hub-tree-card-toggle"
        type="button"
        :title="cardCollapsed ? cardTitle : t('filmhub.collapseAll')"
        :aria-label="cardCollapsed ? cardTitle : t('filmhub.collapseAll')"
        @click="cardCollapsed = !cardCollapsed"
      >{{ cardCollapsed ? '▸' : '▾' }}</button>
      <span class="hub-tree-card-title fh-ellipsis">🗂 {{ cardTitle }}</span>
      <span class="fh-muted fh-small">{{ fileCount }}</span>
      <div class="fh-head-actions">
        <!-- ⤢ 原位大展开（v0.1.5：展开态卡片撑满左栏剩余全高 + 卡内下区内容预览） -->
        <button
          v-if="expandable"
          class="fh-btn fh-btn-mini hub-tree-expand"
          type="button"
          :class="{ 'is-on': cardExpanded }"
          :title="cardExpanded ? t('filmhub.treeCollapse') : t('filmhub.treeExpand')"
          :aria-label="cardExpanded ? t('filmhub.treeCollapse') : t('filmhub.treeExpand')"
          :aria-expanded="cardExpanded"
          @click="toggleExpanded"
        >{{ cardExpanded ? '⤡' : '⤢' }}</button>
        <button
          class="fh-btn fh-btn-mini hub-tree-refresh"
          type="button"
          :title="t('filmhub.refreshTree')"
          :disabled="treeLoading"
          @click="loadTree"
        >
          <span class="fh-spin" :class="{ 'is-spinning': treeLoading }" aria-hidden="true">↻</span>
        </button>
        <button
          class="fh-btn fh-btn-mini hub-tree-collapseall"
          type="button"
          :title="t('filmhub.collapseAll')"
          :disabled="!visibleNodes.length"
          @click="collapseAll"
        >⇤</button>
      </div>
    </div>

    <!-- 「完整浏览」链接（SideNav 形态：跳 Hub 浏览页；展开态复杂内容出口） -->
    <button
      v-if="browseLink && !cardCollapsed"
      class="hub-tree-browse"
      type="button"
      :title="t('filmhub.hubViewTip')"
      @click="emit('browse')"
    >{{ t('filmhub.browseAll') }} ↗</button>

    <!-- 上区：文件树（展开态占 55%；紧凑态撑满） -->
    <div v-show="!cardCollapsed" class="hub-tree-body">
      <div v-if="!projectId" class="fh-empty">{{ t('filmhub.treeEmpty') }}</div>
      <div v-else-if="treeLoading && !visibleNodes.length" class="fh-empty">
        {{ t('film.loading') }}
      </div>
      <div v-else-if="treeError" class="fh-error-box">
        {{ t('filmhub.filesLoadFailed') }}{{ treeError }}
        <button class="fh-btn fh-btn-mini" type="button" @click="loadTree">{{ t('film.retry') }}</button>
      </div>
      <div v-else-if="!visibleNodes.length" class="fh-empty">{{ t('filmhub.treeEmpty') }}</div>
      <template v-else>
        <button
          v-for="row in visibleNodes"
          :key="row.node.path"
          class="hub-tree-row"
          :class="{
            'is-selected':
              !row.node.isDir &&
              row.node.path === (cardExpanded ? preview.path : selected),
          }"
          type="button"
          :style="{ paddingLeft: `${8 + row.depth * 14}px` }"
          :title="row.node.path"
          @click="onRowClick(row.node)"
        >
          <span class="hub-tree-caret" aria-hidden="true">
            {{ row.node.isDir ? (expanded[row.node.path] ? '▾' : '▸') : '' }}
          </span>
          <span aria-hidden="true">
            {{ row.node.isDir ? (expanded[row.node.path] ? '📂' : '📁') : hubFileIcon(row.node.path) }}
          </span>
          <span class="hub-tree-name">{{ row.node.name }}</span>
          <span v-if="!row.node.isDir && typeof row.node.bytes === 'number'" class="hub-tree-bytes">
            {{ fmtBytes(row.node.bytes) }}
          </span>
        </button>
      </template>
    </div>

    <!-- 下区：内容预览（仅展开态；45%——卡片内分栏，绝不向右溢出） -->
    <div v-if="cardExpanded && !cardCollapsed" class="hub-tree-preview">
      <div class="hub-tree-preview-head">
        <span class="hub-tree-preview-title fh-ellipsis" :title="preview.path">
          {{ preview.path || t('filmhub.previewTitle') }}
        </span>
        <span
          v-if="preview.kind === 'text' && preview.truncated"
          class="fh-muted fh-small hub-tree-preview-trunc"
        >{{ t('filmhub.previewTruncated', { n: PREVIEW_MAX_LINES }) }}</span>
      </div>
      <div class="hub-tree-preview-body">
        <div v-if="preview.status === 'loading'" class="fh-empty">{{ t('film.loading') }}</div>
        <div v-else-if="preview.status === 'error'" class="fh-error-box">
          {{ t('filmhub.fileLoadFailed') }}{{ preview.error }}
          <button class="fh-btn fh-btn-mini" type="button" @click="openPreview(preview.path)">
            {{ t('film.retry') }}
          </button>
        </div>
        <div v-else-if="!preview.path" class="fh-empty">{{ t('filmhub.previewEmpty') }}</div>
        <pre v-else-if="preview.kind === 'text'" class="fh-pre hub-tree-preview-text">{{ preview.text }}</pre>
        <img
          v-else-if="preview.kind === 'image'"
          class="hub-tree-preview-img"
          :src="preview.dataUrl"
          :alt="preview.path"
        >
        <audio
          v-else-if="preview.kind === 'audio'"
          class="hub-tree-preview-audio"
          :src="preview.dataUrl"
          controls
        />
        <div v-else-if="preview.kind === 'complex'" class="fh-empty">
          {{ t('filmhub.previewComplex') }}
        </div>
      </div>
    </div>
  </div>
</template>
