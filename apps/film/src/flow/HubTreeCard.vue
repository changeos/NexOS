<script setup lang="ts">
// =============================================================================
// HubTreeCard.vue —— Hub 文件树卡片（公共组件，v0.1.36）。
//
// 从 HubBrowse.vue 抽出的左栏文件树：GET :id/files 平铺清单 → buildHubTree
// 嵌套树；目录可折叠（根级缺省展开 + 卡头「全收」钮一键收起）；图标按类型
// md📝/json🧾/图🖼/音🎵/视频▶/其余📄（flowFiles 纯函数复用）。两个消费方：
//   · HubBrowse 左栏（miniPreview=false——点击 emit file-click，内容由
//     页面右侧内容区展示）；
//   · SideNav 底部常开树卡（miniPreview=true——点击文件弹卡内迷你预览
//     浮层：文本前 50 行等宽 / 图片直显 / 音频控件；视频与复杂二进制提示
//     去「完整浏览」；browseLink 显示「完整浏览」链接 emit browse）。
// 宿主不传 projectId（无 FlowContext 的挂载面）→ 空态，不崩。
// =============================================================================
import { computed, onMounted, reactive, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { b64ToText, filmFileDataUrl, filmGetFile, filmListFiles } from '../api'
import { useFlow } from './flowContext'
import {
  buildHubTree,
  fmtBytes,
  hubFileIcon,
  hubPreviewKind,
  textHeadLines,
  type HubPreviewKind,
  type HubTreeNode,
} from './flowFiles'

const props = withDefaults(
  defineProps<{
    /** 项目 id（空 → 空态不加载）。 */
    projectId?: string
    /** 当前选中文件（高亮；HubBrowse 传）。 */
    selected?: string
    /** 点击文件弹卡内迷你预览浮层（SideNav 形态）。 */
    miniPreview?: boolean
    /** 卡头显示「完整浏览」链接（emit browse）。 */
    browseLink?: boolean
    /** 卡头标题（缺省 🗂 Hub 树）。 */
    title?: string
    /** 重载版本号（宿主 refreshTick 递增时重拉树）。 */
    reloadKey?: number
  }>(),
  {
    projectId: '',
    selected: '',
    miniPreview: false,
    browseLink: false,
    title: '',
    reloadKey: 0,
  },
)

const emit = defineEmits<{
  (e: 'file-click', path: string): void
  (e: 'browse'): void
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
    tree.value = buildHubTree(Array.isArray(entries) ? entries : [])
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

const cardTitle = computed(() => props.title || t('filmhub.navTreeTitle'))

// —— 迷你预览浮层（miniPreview 形态：点击文件卡内预览） ——
const mini = reactive<{
  path: string
  status: 'loading' | 'idle' | 'error'
  kind: HubPreviewKind | ''
  text: string
  truncated: boolean
  dataUrl: string
  error: string
}>({ path: '', status: 'idle', kind: '', text: '', truncated: false, dataUrl: '', error: '' })

function closeMini(): void {
  mini.path = ''
  mini.status = 'idle'
  mini.kind = ''
  mini.text = ''
  mini.truncated = false
  mini.dataUrl = ''
  mini.error = ''
}

async function openMini(path: string): Promise<void> {
  if (!props.projectId) return
  mini.path = path
  mini.status = 'loading'
  mini.kind = ''
  mini.text = ''
  mini.truncated = false
  mini.dataUrl = ''
  mini.error = ''
  try {
    if (isMediaPath(path)) {
      mini.dataUrl = await filmFileDataUrl(props.projectId, path)
      mini.kind = mediaKindOf(path)
    } else {
      const env = await filmGetFile(props.projectId, path)
      const mime = env.mime || env.mime_type || ''
      const kind = hubPreviewKind(path, mime)
      if (kind === 'text') {
        const head = textHeadLines(b64ToText(env.content_b64 ?? ''), 50)
        mini.kind = 'text'
        mini.text = head.text
        mini.truncated = head.truncated
      } else if (kind === 'image' && env.content_b64) {
        // mime 标注图片但扩展名不在直显表——按信封 mime 组 data URL 直显
        mini.dataUrl = await filmFileDataUrl(props.projectId, path)
        mini.kind = 'image'
      } else {
        mini.kind = 'binary'
      }
    }
    mini.status = 'idle'
  } catch (e) {
    mini.status = 'error'
    mini.error = errText(e)
  }
}

function isMediaPath(path: string): boolean {
  return ['image', 'audio', 'video'].includes(hubPreviewKind(path, ''))
}

function mediaKindOf(path: string): HubPreviewKind {
  return hubPreviewKind(path, '')
}

function onRowClick(node: HubTreeNode): void {
  if (node.isDir) {
    toggleDir(node)
    return
  }
  emit('file-click', node.path)
  if (props.miniPreview) void openMini(node.path)
}

onMounted(() => void loadTree())
watch(
  () => props.projectId,
  (id) => {
    if (id) void loadTree()
  },
)
watch(
  () => props.reloadKey,
  () => {
    if (props.projectId) void loadTree()
  },
)

/** 宿主页头按钮联动面（HubBrowse 页头刷新钮）。 */
defineExpose({
  reload: loadTree,
  isLoading: computed(() => treeLoading.value),
})
</script>

<template>
  <div class="hub-tree-card">
    <div class="hub-tree-head">
      <button
        class="hub-tree-caret hub-tree-card-toggle"
        type="button"
        :title="cardCollapsed ? t('filmhub.navTreeTitle') : t('filmhub.collapseAll')"
        :aria-label="cardCollapsed ? t('filmhub.navTreeTitle') : t('filmhub.collapseAll')"
        @click="cardCollapsed = !cardCollapsed"
      >{{ cardCollapsed ? '▸' : '▾' }}</button>
      <span class="hub-tree-card-title fh-ellipsis">🗂 {{ cardTitle }}</span>
      <span class="fh-muted fh-small">{{ fileCount }}</span>
      <div class="fh-head-actions">
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

    <!-- 「完整浏览」链接（SideNav 形态：跳 Hub 浏览页） -->
    <button
      v-if="browseLink && !cardCollapsed"
      class="hub-tree-browse"
      type="button"
      :title="t('filmhub.hubViewTip')"
      @click="emit('browse')"
    >{{ t('filmhub.browseAll') }} ↗</button>

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
            'is-selected': !row.node.isDir && row.node.path === selected,
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

    <!-- ============ 迷你预览浮层（miniPreview 形态；浮在卡右侧 ~360px） ============ -->
    <div v-if="miniPreview && mini.path" class="hub-mini-pop" role="dialog" :aria-label="mini.path">
      <div class="hub-mini-head">
        <span class="hub-mini-path fh-ellipsis" :title="mini.path">{{ mini.path }}</span>
        <button
          class="fh-btn fh-btn-mini hub-mini-close"
          type="button"
          :aria-label="t('filmhub.miniClose')"
          @click="closeMini"
        >✕</button>
      </div>
      <div class="hub-mini-body">
        <div v-if="mini.status === 'loading'" class="fh-empty">{{ t('film.loading') }}</div>
        <div v-else-if="mini.status === 'error'" class="fh-error-box">
          {{ t('filmhub.fileLoadFailed') }}{{ mini.error }}
        </div>
        <template v-else>
          <template v-if="mini.kind === 'text'">
            <pre class="hub-mini-text">{{ mini.text }}</pre>
            <div v-if="mini.truncated" class="fh-muted fh-small hub-mini-note">
              {{ t('filmhub.miniTruncated') }}
            </div>
          </template>
          <img
            v-else-if="mini.kind === 'image'"
            class="hub-mini-img"
            :src="mini.dataUrl"
            :alt="mini.path"
          >
          <audio
            v-else-if="mini.kind === 'audio'"
            class="hub-mini-audio"
            :src="mini.dataUrl"
            controls
          />
          <div v-else-if="mini.kind === 'binary' || mini.kind === 'video'" class="fh-empty">
            {{ t('filmhub.miniComplexHint') }}
            <button
              class="fh-btn fh-btn-mini"
              style="margin-top: 6px"
              type="button"
              @click="emit('browse')"
            >{{ t('filmhub.browseAll') }} ↗</button>
          </div>
        </template>
      </div>
    </div>
  </div>
</template>
