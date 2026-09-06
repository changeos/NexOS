<script setup lang="ts">
// =============================================================================
// FileBrowser —— 文件浏览共享组件（自 Files.vue 抽出）
//
// 功能：
//   1. 目录导航：面包屑（根 > 子目录…）、行点击进目录、返回上级、
//      目录/文件图标区分、大小人性化、修改时间显示；名称/大小/时间可排序
//   2. 工具条：当前路径（面包屑）、返回上级、刷新、新建文件夹（mkdir，
//      readonly 时隐藏）、上传（input file multiple → 逐个 POST，进度条）
//   3. 删除 / 重命名：接已有 delete / rename 端点；删除走两步确认（同键 3s），
//      readonly 时隐藏操作列
//   4. 下载：文件行「下载」按钮 → client.ts filesDownload（base64 信封 →
//      Blob → 另存）；目录行不提供下载（zip 打包暂未就绪）
//   5. 目录用量：hover 目录名懒加载 GET /files/usage，显示 "≈N 项 · X MiB"
//      （后端超限截断时前缀 ≥），结果按路径缓存
//   6. 三态：加载骨架 / 空目录 / 错误（含重试）；5s 轮询（document.hidden
//      暂停，弹窗/重命名/上传期间暂停，静默刷新不闪骨架）
//
// Props：
//   - root     起始目录。'/'（默认）= 后端根映射（/tank，回退 /var/lib/os/files）；
//              其他值为绝对路径（如 '/tank'，后端按真实绝对路径解析）
//   - readonly 只读模式：隐藏上传/新建/删除/重命名等写操作，仅浏览+下载
//
// 后端：GET /api/v1/files/list|stat|usage|download?path= / POST mkdir|delete|rename|upload
//
// 上传/下载传输形态：经网关 JSON 通道 base64 装载（multipart 穿不过网关，
// 见 crates/os-api/src/handlers/files.rs 模块注释与 client.ts filesUpload/
// filesDownload 注释）。上传目标目录后端自动创建；重名自动 -1/-2 后缀；
// 单文件 >2 GiB 前后端皆拒（413）。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { endpoints } from '@/api/client';

interface FileEntry {
  name?: string;
  path?: string;
  is_dir?: boolean;
  size_bytes?: number;
  modified_at?: string;
  mime_type?: string;
  [k: string]: unknown;
}

/** GET /api/v1/files/usage 响应体。 */
interface DirUsage {
  path?: string;
  total_bytes?: number;
  file_count?: number;
  dir_count?: number;
  partial?: boolean;
  [k: string]: unknown;
}

const props = withDefaults(
  defineProps<{
    /** 起始目录：'/'（默认）= 根映射 /tank；否则为绝对路径（如 '/tank'）。 */
    root?: string;
    /** 只读模式：隐藏新建/删除/重命名等写操作，仅浏览。 */
    readonly?: boolean;
  }>(),
  { root: '/', readonly: false },
);

/** 规范化后的起始目录（去尾部斜杠；空串 → '/'）。 */
const rootPath = computed(() => {
  const r = (props.root ?? '/').replace(/\/+$/, '');
  return r === '' ? '/' : r;
});

// =============================================================================
// 当前目录与列表（三态：loading / 空 / error）
// =============================================================================
const currentPath = ref<string>(rootPath.value);
const entries = ref<FileEntry[]>([]);
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

/** 导航序号：慢请求返回时若已有更新的导航，丢弃结果（防竞态串目录）。 */
let listSeq = 0;

/**
 * 面包屑段（基于 rootPath + currentPath 切分）。
 * 根 crumb：root='/' 显示「根目录」，否则显示根路径本身（如 /tank）。
 */
const breadcrumbs = computed<{ label: string; path: string }[]>(() => {
  const root = rootPath.value;
  const p = currentPath.value || '/';
  const crumbs: { label: string; path: string }[] = [
    { label: root === '/' ? '根目录' : root, path: root },
  ];
  // 取 currentPath 中位于根之后的部分（防御：不在根前缀下时只显示根）
  let rest = p;
  if (root !== '/') rest = p.startsWith(root) ? p.slice(root.length) : '';
  let acc = root === '/' ? '' : root;
  for (const s of rest.split('/').filter((x) => x.length > 0)) {
    acc += '/' + s;
    crumbs.push({ label: s, path: acc });
  }
  return crumbs;
});

/** 是否在起始根目录（根不可再向上）。 */
const atRoot = computed(() => currentPath.value === rootPath.value);

/**
 * 加载目录列表。
 * @param path 目标目录（空/undefined = 根映射）；省略 = 刷新当前目录
 * @param opts.silent 静默模式（轮询）：不转骨架、不清错误提示
 */
async function loadList(path?: string, opts: { silent?: boolean } = {}): Promise<void> {
  const target = path && path !== '' ? path : rootPath.value;
  const silent = opts.silent === true;
  const seq = ++listSeq;
  if (!silent) {
    loading.value = true;
    error.value = '';
    msg.value = null;
  }
  if (target !== currentPath.value) usageCache.clear(); // 换目录后旧用量缓存失效
  try {
    const raw = await endpoints.filesList(target === '/' ? undefined : target);
    if (seq !== listSeq) return; // 已被更新的导航覆盖
    entries.value = Array.isArray(raw) ? (raw as FileEntry[]) : [];
    currentPath.value = target;
  } catch (e) {
    if (seq !== listSeq) return;
    entries.value = [];
    error.value = friendlyError(e);
  } finally {
    if (seq === listSeq && !silent) loading.value = false;
  }
}

/** 手动刷新当前目录（供工具条/父页面调用）。 */
function refresh(): void {
  void loadList(currentPath.value);
}

/** 进入子目录（行点击或名称点击）。 */
function enterDir(row: FileEntry): void {
  if (!row.is_dir) return;
  const p = String(row.path ?? '');
  if (p && p !== currentPath.value) void loadList(p);
}

/** 行点击：目录才进入（文件行无操作；上传下载见文件头 TODO）。 */
function onRowClick(row: FileEntry): void {
  enterDir(row);
}

/** 面包屑跳转。 */
function gotoCrumb(p: string): void {
  if (p !== currentPath.value) void loadList(p);
}

/** 返回上一级目录（到起始根为止）。 */
function goUp(): void {
  if (atRoot.value || loading.value) return;
  const p = currentPath.value.replace(/\/+$/, '');
  const idx = p.lastIndexOf('/');
  void loadList(idx <= 0 ? '/' : p.slice(0, idx));
}

// =============================================================================
// 轮询（5s；document.hidden 暂停；弹窗/重命名期间暂停；静默不闪骨架）
//   —— 场景：迅雷等经 SMB 往 /tank/downloads 持续落盘，页面无需手动刷新即可看到新文件
// =============================================================================
const POLL_MS = 5000;
let pollTimer: ReturnType<typeof setInterval> | null = null;

function onPollTick(): void {
  if (typeof document !== 'undefined' && document.hidden) return; // 隐藏暂停
  if (showMkdir.value || renameTarget.value) return; // 操作进行中暂停，避免行变动打断
  if (uploadState.value) return; // 上传批次进行中暂停（结束后统一刷新）
  void loadList(currentPath.value, { silent: true });
}

// =============================================================================
// 目录用量（hover 目录名懒加载 + 按路径缓存；partial=true 显示为下界 ≥）
// =============================================================================
const usageCache = reactive(new Map<string, DirUsage>());
const usageBusy = new Set<string>();
const USAGE_HOVER_DELAY_MS = 400;
let usageHoverTimer: ReturnType<typeof setTimeout> | null = null;

function onDirHover(row: FileEntry): void {
  const p = String(row.path ?? '');
  if (!row.is_dir || !p || usageCache.has(p) || usageBusy.has(p)) return;
  if (usageHoverTimer) clearTimeout(usageHoverTimer);
  usageHoverTimer = setTimeout(() => {
    usageHoverTimer = null;
    void fetchUsage(p);
  }, USAGE_HOVER_DELAY_MS);
}
function onDirLeave(): void {
  if (usageHoverTimer) {
    clearTimeout(usageHoverTimer);
    usageHoverTimer = null;
  }
}
async function fetchUsage(p: string): Promise<void> {
  if (usageCache.has(p) || usageBusy.has(p)) return;
  usageBusy.add(p);
  try {
    usageCache.set(p, (await endpoints.filesUsage(p)) as DirUsage);
  } catch {
    /* 用量是增强信息，失败静默（大小列继续显示 —） */
  } finally {
    usageBusy.delete(p);
  }
}
/** 目录行大小列文本："≈12 项 · 3.4 MiB"（partial → "≥…"）。 */
function usageText(row: FileEntry): string {
  const u = usageCache.get(String(row.path ?? ''));
  if (!u) return '';
  const n = (u.file_count ?? 0) + (u.dir_count ?? 0);
  const prefix = u.partial ? '≥' : '≈';
  return `${prefix}${n} 项 · ${formatBytes(u.total_bytes ?? 0)}`;
}

// =============================================================================
// 新建文件夹（mkdir；readonly 模式下入口按钮隐藏）
// =============================================================================
const showMkdir = ref(false);
const mkdirName = ref('');
const mkdirSubmitting = ref(false);

function openMkdir(): void {
  mkdirName.value = '';
  msg.value = null;
  showMkdir.value = true;
}
function closeMkdir(): void {
  if (mkdirSubmitting.value) return;
  showMkdir.value = false;
}
async function submitMkdir(): Promise<void> {
  const name = mkdirName.value.trim();
  if (!name) {
    msg.value = { kind: 'err', text: '请填写文件夹名' };
    return;
  }
  if (name.includes('/') || name.includes('..')) {
    msg.value = { kind: 'err', text: '文件夹名不可包含 / 或 ..' };
    return;
  }
  // 拼接新目录绝对路径：根目录直接 /name，否则 currentPath/name
  const base = currentPath.value === '/' ? '' : currentPath.value;
  const target = `${base}/${name}`;
  mkdirSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.filesMkdir(target);
    showMkdir.value = false;
    await loadList(currentPath.value);
    msg.value = { kind: 'ok', text: `文件夹「${name}」已创建` };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    mkdirSubmitting.value = false;
  }
}

// =============================================================================
// 删除（两步确认：同键第二次点击才真正删除，3s 不点自动复位 —— 页面惯例）
// =============================================================================
const pendingDelete = ref<string>('');
let pendingDeleteTimer: ReturnType<typeof setTimeout> | null = null;
const deleting = ref<string>('');

function resetPendingDelete(): void {
  if (pendingDeleteTimer) {
    clearTimeout(pendingDeleteTimer);
    pendingDeleteTimer = null;
  }
  pendingDelete.value = '';
}
function onClickDelete(row: FileEntry): void {
  const p = String(row.path ?? '');
  if (!p || deleting.value) return;
  if (pendingDelete.value === p) {
    // 第二步：确认删除
    resetPendingDelete();
    void doDelete(p, row);
    return;
  }
  // 第一步：进入待确认态，3s 后自动复位
  resetPendingDelete();
  pendingDelete.value = p;
  pendingDeleteTimer = setTimeout(() => {
    pendingDelete.value = '';
  }, 3000);
}
async function doDelete(p: string, row: FileEntry): Promise<void> {
  const isDir = row.is_dir === true;
  msg.value = { kind: 'info', text: `正在删除「${row.name ?? p}」…` };
  deleting.value = p;
  try {
    await endpoints.filesDelete(p);
    await loadList(currentPath.value);
    msg.value = { kind: 'ok', text: isDir ? '目录已删除' : '文件已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    deleting.value = '';
  }
}

// =============================================================================
// 重命名（行内浮条编辑）
// =============================================================================
const renaming = ref<string>('');
const renameTarget = ref<string>('');
const renameNewName = ref('');

function openRename(row: FileEntry): void {
  resetPendingDelete();
  renameTarget.value = String(row.path ?? '');
  renameNewName.value = String(row.name ?? '');
  msg.value = null;
}
function closeRename(): void {
  if (renaming.value) return;
  renameTarget.value = '';
}
async function submitRename(): Promise<void> {
  const from = renameTarget.value;
  const newName = renameNewName.value.trim();
  if (!from || !newName) {
    msg.value = { kind: 'err', text: '新名称不可为空' };
    return;
  }
  if (newName.includes('/') || newName.includes('..')) {
    msg.value = { kind: 'err', text: '新名称不可包含 / 或 ..' };
    return;
  }
  // 拼接 to 路径：保留父目录
  const parent = from.lastIndexOf('/') >= 0 ? from.slice(0, from.lastIndexOf('/')) : '';
  const to = `${parent}/${newName}`;
  renaming.value = from;
  try {
    await endpoints.filesRename(from, to);
    renameTarget.value = '';
    await loadList(currentPath.value);
    msg.value = { kind: 'ok', text: '已重命名' };
  } catch (e) {
    msg.value = { kind: 'err', text: '重命名失败：' + friendlyError(e) };
  } finally {
    renaming.value = '';
  }
}

// =============================================================================
// 上传（工具条按钮 → 隐藏 input[multiple] → 逐个 POST /files/upload）
//   —— 串行逐个传（base64 大 payload 并发易压垮浏览器内存）；
//      当前文件 XHR 上传进度 + 批次计数；单个失败记入列表继续后续文件
// =============================================================================
const fileInput = ref<HTMLInputElement | null>(null);
/** 上传批次进行中状态（null = 空闲；状态条据此显示）。 */
const uploadState = ref<{
  total: number;
  /** 已处理完（含失败） */
  done: number;
  failed: number;
  /** 当前正在传的文件名 */
  current: string;
  /** 当前文件上传进度（0-100，XHR upload.onprogress） */
  pct: number;
} | null>(null);

/** 前端 2 GiB 预检上限（与后端一致；超限直接列错，避免无谓 base64 编码）。 */
const MAX_UPLOAD_BYTES = 2 * 1024 * 1024 * 1024;

/** 触发文件选择框（同一文件可重复选择：change 后清空 input.value）。 */
function openUpload(): void {
  msg.value = null;
  fileInput.value?.click();
}

async function onFilesPicked(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const files = Array.from(input.files ?? []);
  input.value = ''; // 允许再次选择同一文件
  if (files.length === 0) return;
  await runUploads(files);
}

/** 逐个上传一批文件到当前目录；结束后刷新列表并列出全部失败项。 */
async function runUploads(files: File[]): Promise<void> {
  if (uploadState.value) return; // 已有批次进行中，忽略
  uploadState.value = { total: files.length, done: 0, failed: 0, current: '', pct: 0 };
  const failures: string[] = [];
  for (const f of files) {
    if (!uploadState.value) break; // 防御（组件卸载等）
    uploadState.value.current = f.name;
    uploadState.value.pct = 0;
    if (f.size > MAX_UPLOAD_BYTES) {
      failures.push(`${f.name}（超过 2 GiB 上限，大文件请走 SMB 共享）`);
      uploadState.value.done += 1;
      uploadState.value.failed += 1;
      continue;
    }
    try {
      await endpoints.filesUpload(currentPath.value, f, (loaded, total) => {
        if (uploadState.value && total > 0) {
          uploadState.value.pct = Math.round((loaded / total) * 100);
        }
      });
    } catch (e) {
      failures.push(`${f.name}（${friendlyError(e)}）`);
      if (uploadState.value) uploadState.value.failed += 1;
    }
    if (uploadState.value) uploadState.value.done += 1;
  }
  uploadState.value = null;
  await loadList(currentPath.value, { silent: true });
  if (failures.length === 0) {
    msg.value = { kind: 'ok', text: `已上传 ${files.length} 个文件` };
  } else {
    const ok = files.length - failures.length;
    msg.value = {
      kind: failures.length === files.length ? 'err' : 'ok',
      text: `上传结束：成功 ${ok}，失败 ${failures.length} —— ${failures.join('；')}`,
    };
  }
}

// =============================================================================
// 下载（文件行按钮 → client.ts filesDownload：base64 信封 → Blob → 另存）
//   —— 目录行不提供下载（后端 zip 打包暂未支持）
// =============================================================================
const downloading = ref<string>('');

async function downloadRow(row: FileEntry): Promise<void> {
  if (row.is_dir) return;
  const p = String(row.path ?? '');
  if (!p || downloading.value) return;
  downloading.value = p;
  msg.value = { kind: 'info', text: `正在下载「${row.name ?? p}」…` };
  try {
    await endpoints.filesDownload(p);
    msg.value = { kind: 'ok', text: `「${row.name ?? p}」已开始下载` };
  } catch (e) {
    msg.value = { kind: 'err', text: '下载失败：' + friendlyError(e) };
  } finally {
    downloading.value = '';
  }
}

// =============================================================================
// 表格列 + 格式化工具
// =============================================================================
const columns = computed<Column<FileEntry>[]>(() => {
  const cols: Column<FileEntry>[] = [
    { key: 'name', title: '名称', sortable: true, accessor: (r) => r.name ?? '' },
    {
      key: 'size',
      title: '大小',
      width: '150px',
      align: 'right',
      sortable: true,
      // 目录恒排前（-1）；文件按字节数数值排序（展示走 cell-size 插槽）
      accessor: (r) => (r.is_dir ? -1 : r.size_bytes ?? 0),
    },
    {
      key: 'modified_at',
      title: '修改时间',
      width: '170px',
      sortable: true,
      accessor: (r) => r.modified_at ?? '',
    },
    {
      key: 'mime_type',
      title: '类型',
      width: '170px',
      accessor: (r) => (r.is_dir ? '文件夹' : r.mime_type ?? '—'),
    },
  ];
  // 操作列恒存在：下载（只读模式也可用）；重命名/删除仅非 readonly 时渲染
  const writeCols = props.readonly ? 0 : 2;
  cols.push({
    key: 'actions',
    title: '操作',
    width: writeCols === 0 ? '95px' : '230px',
    align: 'right',
  });
  return cols;
});

function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function formatDate(iso?: string): string {
  if (!iso) return '—';
  // 取 YYYY-MM-DD HH:MM 部分
  return iso.replace('T', ' ').slice(0, 16);
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该文件接口';
  }
  if (/413/.test(m)) {
    return '文件超过 2 GiB 通道上限（大文件请走 SMB 共享）';
  }
  if (/401|未授权/.test(m)) return m;
  return m;
}

onMounted(() => {
  void loadList();
  pollTimer = setInterval(onPollTick, POLL_MS);
});

onBeforeUnmount(() => {
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
  if (pendingDeleteTimer) {
    clearTimeout(pendingDeleteTimer);
    pendingDeleteTimer = null;
  }
  if (usageHoverTimer) {
    clearTimeout(usageHoverTimer);
    usageHoverTimer = null;
  }
});

/** 仅供父页面工具条复用（如存储页顶部「刷新」按钮）。 */
defineExpose({ refresh });
</script>

<template>
  <div class="file-browser">
    <!-- 工具条：面包屑（当前路径）+ 导航/操作按钮 -->
    <section class="card breadcrumb-bar" aria-label="当前路径">
      <div class="crumbs">
        <template v-for="(c, i) in breadcrumbs" :key="c.path">
          <span v-if="i > 0" class="crumb-sep">/</span>
          <button
            class="crumb"
            :class="{ active: i === breadcrumbs.length - 1 }"
            type="button"
            @click="gotoCrumb(c.path)"
          >{{ c.label }}</button>
        </template>
      </div>
      <div class="bar-actions">
        <button class="btn btn-small" :disabled="atRoot || loading" title="返回上一级目录" @click="goUp">
          ↑ 返回上级
        </button>
        <button class="btn btn-small" :disabled="loading" @click="refresh">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
        <!-- 上传：input file multiple → 逐个 POST（见 client.ts filesUpload） -->
        <button
          v-if="!readonly"
          class="btn btn-small"
          :disabled="uploadState !== null"
          title="上传文件到当前目录"
          @click="openUpload"
        >
          ↑ 上传
        </button>
        <button v-if="!readonly" class="btn btn-small btn-primary" @click="openMkdir">＋ 新建文件夹</button>
        <!-- 隐藏文件选择框（multiple 多选；display:none 见样式） -->
        <input
          ref="fileInput"
          type="file"
          multiple
          class="upload-input"
          aria-hidden="true"
          tabindex="-1"
          @change="onFilesPicked"
        />
      </div>
    </section>

    <!-- 上传批次状态条（busy：计数 + 当前文件 + XHR 进度 + 失败数） -->
    <div v-if="uploadState" class="upload-bar card" role="status" aria-live="polite">
      <span class="spin spinning" aria-hidden="true">↻</span>
      <span class="upload-text" :title="uploadState.current">
        上传中 {{ Math.min(uploadState.done + 1, uploadState.total) }}/{{ uploadState.total }}：{{ uploadState.current || '…' }}
      </span>
      <div class="upload-progress" aria-hidden="true">
        <div class="upload-progress-fill" :style="{ width: uploadState.pct + '%' }"></div>
      </div>
      <span class="upload-pct">{{ uploadState.pct }}%</span>
      <span v-if="uploadState.failed > 0" class="upload-failed">失败 {{ uploadState.failed }}</span>
    </div>

    <div v-if="error" class="error-box">
      加载失败：{{ error }}
      <button class="btn btn-small error-retry" :disabled="loading" @click="refresh">重试</button>
    </div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 文件表格（加载骨架 / 空目录 / 数据三态由 DataTable 承担；错误态上方已提示） -->
    <section v-if="!error" class="panel">
      <div class="card card-table">
        <DataTable
          :columns="columns"
          :rows="entries"
          :loading="loading"
          :row-key="(r: FileEntry) => String(r.path ?? r.name ?? '')"
          empty-text="该目录为空。"
          @row-click="onRowClick"
        >
          <template #cell-name="{ row }">
            <button
              class="name-cell"
              :class="{ dir: row.is_dir }"
              type="button"
              :disabled="!row.is_dir"
              :title="row.is_dir ? '点击进入目录' : ''"
              @click.stop="enterDir(row)"
              @mouseenter="onDirHover(row)"
              @mouseleave="onDirLeave"
            >
              <span class="file-ico" aria-hidden="true">{{ row.is_dir ? '📁' : '📄' }}</span>
              {{ row.name ?? '—' }}
            </button>
          </template>
          <template #cell-size="{ row }">
            <span v-if="row.is_dir" class="size-dir" :title="usageText(row) || '鼠标悬停目录名统计用量'">
              {{ usageText(row) || '—' }}
            </span>
            <span v-else>{{ formatBytes(row.size_bytes ?? 0) }}</span>
          </template>
          <template #cell-modified_at="{ row }">{{ formatDate(row.modified_at) }}</template>
          <template #cell-actions="{ row }">
            <!-- 下载：仅文件行（目录 zip 打包暂未支持）；readonly 模式保留 -->
            <button
              v-if="!row.is_dir"
              class="btn btn-small"
              :disabled="downloading !== ''"
              :title="`下载 ${row.name ?? ''}`"
              @click.stop="downloadRow(row)"
            >
              {{ downloading === (row.path ?? '') ? '下载中…' : '下载' }}
            </button>
            <button
              v-if="!readonly"
              class="btn btn-small"
              :disabled="renaming === (row.path ?? '')"
              @click.stop="openRename(row)"
            >重命名</button>
            <button
              v-if="!readonly"
              class="btn btn-small btn-danger"
              :class="{ 'btn-danger-armed': pendingDelete === (row.path ?? '') }"
              :disabled="deleting === (row.path ?? '')"
              @click.stop="onClickDelete(row)"
            >
              {{
                deleting === (row.path ?? '')
                  ? '删除中…'
                  : pendingDelete === (row.path ?? '')
                    ? '确认删除？'
                    : '删除'
              }}
            </button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- 重命名浮条（行内编辑） -->
    <div v-if="renameTarget" class="rename-bar card">
      <label class="rename-label" for="rename-input">重命名为：</label>
      <input
        id="rename-input"
        v-model="renameNewName"
        type="text"
        class="rename-input"
        :disabled="renaming === renameTarget"
        @keyup.enter="submitRename"
        @keyup.esc="closeRename"
      />
      <button
        class="btn btn-small btn-primary"
        :disabled="renaming === renameTarget"
        @click="submitRename"
      >{{ renaming === renameTarget ? '处理中…' : '确定' }}</button>
      <button class="btn btn-small" :disabled="renaming === renameTarget" @click="closeRename">取消</button>
    </div>

    <!-- ============ 新建文件夹对话框 ============ -->
    <div v-if="showMkdir" class="modal-backdrop" @click.self="closeMkdir">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="mkdir-title">
        <div class="modal-head">
          <h3 id="mkdir-title">新建文件夹</h3>
          <button class="modal-close" type="button" :disabled="mkdirSubmitting" @click="closeMkdir">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitMkdir">
          <div class="field">
            <label for="mkdir-name">文件夹名（在 {{ currentPath === '/' ? '根目录' : currentPath }} 下创建）</label>
            <input
              id="mkdir-name"
              v-model="mkdirName"
              type="text"
              placeholder="例如 documents"
              :disabled="mkdirSubmitting"
              @keyup.esc="closeMkdir"
            />
          </div>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="mkdirSubmitting" @click="closeMkdir">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="mkdirSubmitting">
              {{ mkdirSubmitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </form>
      </div>
    </div>
  </div>
</template>

<style scoped>
.file-browser {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }

.breadcrumb-bar {
  padding: 10px 16px;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  flex-wrap: wrap;
  font-size: 13px;
}
.crumbs {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-wrap: wrap;
  min-width: 0;
}
.bar-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; flex: none; }
.crumb {
  background: transparent;
  border: none;
  color: var(--accent, #E95420);
  cursor: pointer;
  font-family: inherit;
  font-size: 13px;
  padding: 2px 6px;
  border-radius: 6px;
}
.crumb:hover { background: rgba(0, 0, 0, 0.05); }
.crumb.active { color: var(--text, #2B2B2B); font-weight: 600; cursor: default; }
.crumb-sep { color: var(--text-muted, #5E5C5F); }

.panel { display: flex; flex-direction: column; gap: 12px; }

.name-cell {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  background: transparent;
  border: none;
  font-family: inherit;
  font-size: 14px;
  color: var(--text, #2B2B2B);
  cursor: pointer;
  padding: 2px 4px;
  border-radius: 6px;
}
.name-cell:disabled { cursor: default; }
.name-cell.dir { color: var(--accent, #E95420); font-weight: 500; }
.name-cell.dir:hover { text-decoration: underline; }
.file-ico { font-size: 15px; }

.size-dir { color: var(--text-muted, #5E5C5F); font-size: 12.5px; }

/* 隐藏的上传文件选择框（工具条按钮代为触发） */
.upload-input { display: none; }

/* 上传批次状态条：计数 + 当前文件 + 进度条 + 失败数 */
.upload-bar {
  padding: 8px 16px;
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  font-size: 13px;
}
.upload-text {
  min-width: 0;
  max-width: 45%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.upload-progress {
  flex: 1 1 120px;
  min-width: 100px;
  height: 6px;
  border-radius: 3px;
  background: rgba(0, 0, 0, 0.08);
  overflow: hidden;
}
.upload-progress-fill {
  height: 100%;
  background: var(--accent, #E95420);
  transition: width 0.2s ease;
}
.upload-pct { color: var(--text-muted, #5E5C5F); flex: none; min-width: 38px; text-align: right; }
.upload-failed { color: #b91c1c; flex: none; font-weight: 500; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }

.error-box {
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.error-retry { flex: none; }

.rename-bar {
  padding: 12px 16px;
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
}
.rename-label { font-size: 13px; font-weight: 600; }
.rename-input {
  flex: 1;
  min-width: 200px;
  padding: 6px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-family: inherit;
  font-size: 14px;
}

.btn {
  padding: 6px 14px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2B2B2B);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  transition: background 0.15s ease;
}
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
/* 两步删除第一步：待确认态高亮，提示再点一次才真正删除 */
.btn-danger-armed {
  background: #b91c1c;
  color: #fff;
  border-color: #b91c1c;
  font-weight: 600;
}

.field { display: flex; flex-direction: column; gap: 4px; }
.field label { font-size: 13px; font-weight: 500; }
.field input {
  width: 100%; padding: 7px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-family: inherit; font-size: 14px;
}
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop {
  position: fixed; inset: 0;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(2px);
  display: flex; align-items: center; justify-content: center;
  z-index: 100; padding: 16px;
}
.modal {
  width: min(480px, 100%);
  max-height: 90vh; overflow: auto;
  background: var(--bg-card, #fff);
  border-radius: var(--radius, 16px);
  box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25);
  display: flex; flex-direction: column;
}
.modal-head {
  display: flex; align-items: center; justify-content: space-between;
  padding: 16px 20px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close {
  background: transparent; border: none; font-size: 24px; line-height: 1;
  color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px;
}
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
</style>
