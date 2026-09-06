<script setup lang="ts">
// =============================================================================
// StreamingCenter.vue —— 流媒体中心
//
// 7 Tab：直播 / 拉流源 / 多机位 / 转码 / 推流 / 拉流转推流 / 总览
// 后端：/api/v1/streaming/*（StreamingRouteHandler）+ /api/v1/live/*
// （LiveRouteHandler，直播 Tab 挂 LivePanel 组件——本地大厅 + 联邦大厅，
// 原「直播」独立桌面应用已并入此处，旧 /live 链接重定向到本 Tab）
//
// 设计：Ubuntu Yaru 风格 .card / .page-head，统计卡 + 表格 + 对话框，三态加载。
// =============================================================================
// 应用包（apps/streaming）：主前端内部模块依赖已解耦——
//   - @/api/client → 本包 api.ts（宿主桥 __NEXOS_HOST__.api 原语 + 同名 endpoints）
//   - @/components/DataTable(.vue) → 本包 DataTable.vue / data-table.ts（原样迁入）
//   - @/components/LivePanel.vue → 本包 LivePanel.vue（直播 Tab 随包走）
//   - vue-router useRoute → location.search（应用包宿主无 vue-router——桌面嵌入
//     模式的守卫把 /streaming?tab=x 重定向为 /?app=streaming&tab=x，query 保留在
//     location.search；独立模式 standalone.html?tab=x 同理——两种载体同一读取路径）
//   - 能力徽章/置灰走 @nexos/app-sdk（hostSdk()，apps/film 0.1.3 范式）
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from './DataTable.vue';
import LivePanel from './LivePanel.vue';
import type { Column } from './data-table';
import { endpoints, hostSdk, type AppSdk } from './api';

const { t } = useI18n();

// =============================================================================
// 独立运行外链 + 能力快照与降级三态（@nexos/app-sdk，apps/film 0.1.2/0.1.3 范式）
// =============================================================================

/** 独立模式标记（standalone/standalone-host.ts 置位）——该模式下不显示外链。 */
const isStandalone = Boolean(
  (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__,
);

/** 在新浏览器标签页打开独立全页版本（脱离 NexOS 桌面壳，宿主桥自给自足）。 */
function openStandalone(): void {
  window.open('/apps-assets/streaming/standalone.html', '_blank', 'noopener');
}

/** 降级三态（null=尚未判定）。 */
const deg = ref<import('@nexos/app-sdk').DegradedState | null>(null);
/** 能力订阅退订函数（onBeforeUnmount 调）。 */
let unsubCaps: (() => void) | null = null;

/** 某能力键是否缺失（未判定时按不缺失处理——不强置灰）。 */
function capMissing(key: string): boolean {
  return deg.value?.missing.includes(key) ?? false;
}

/** 离线态（探测连败 3 次——转码入口停用）。 */
const isOffline = computed(() => deg.value?.mode === 'offline');

/** 转码类是否可用（ffmpeg——转码/拉流转推流服务端都要 ffmpeg）。 */
const transcodeAvailable = computed(
  () => !isOffline.value && !capMissing('media.ffmpeg'),
);

/** 转码按钮 tooltip（不缺失返回 undefined=不显）。 */
const transcodeDisabledTip = computed(() => {
  if (!deg.value || deg.value.mode === 'full') return undefined;
  if (deg.value.mode === 'offline') return t('streaming.capsOfflineTip');
  if (capMissing('media.ffmpeg')) return t('streaming.capsFfmpegTip');
  return undefined;
});

/** 启动能力判定（SDK 在桥上才启用；旧宿主静默跳过=无徽章全功能）。 */
async function initCaps(): Promise<void> {
  const sdk: AppSdk | null = hostSdk();
  if (!sdk) return;
  try {
    deg.value = await sdk.degraded.state();
  } catch {
    /* degraded.state 不抛错（offline 三态收敛）；防御旧宿主形态 */
  }
  unsubCaps = sdk.capabilities.subscribe(() => {
    /* 快照变化时重算（degraded.state 内部 5s 缓存——这里轻量刷新） */
    void sdk.degraded.refresh().then((d) => {
      deg.value = d;
    }).catch(() => {});
  });
}

// =============================================================================
// 数据模型
// =============================================================================
type Protocol = 'rtsp' | 'rtmp' | 'srt' | 'http' | 'webrtc' | string;
type ResolutionTag = 'sd' | '720p' | '1080p' | '2k' | '4k' | 'panorama' | string;
type SourceStatus = 'idle' | 'connecting' | 'live' | 'error' | string;
type TcStatus = 'queued' | 'running' | 'completed' | 'failed' | 'cancelled' | string;

interface StreamSource {
  id?: string;
  name?: string;
  url?: string;
  protocol?: Protocol;
  resolution_tag?: ResolutionTag;
  status?: SourceStatus;
  recording?: boolean;
  record_local?: boolean;
  record_path?: string;
  record_pid?: number;
  created_at?: string;
  [k: string]: unknown;
}
interface LadderRung {
  label?: string;
  width?: number;
  height?: number;
  bitrate?: number;
}
interface TranscodeTask {
  id?: string;
  name?: string;
  input?: string;
  output_dir?: string;
  mode?: 'vod' | 'live' | string;
  codec?: string;
  ladder?: LadderRung[];
  status?: TcStatus;
  progress?: number;
  pid?: number;
  error?: string;
  created_at?: string;
  [k: string]: unknown;
}
interface StreamOutput {
  id?: string;
  name?: string;
  url?: string;
  protocol?: Protocol;
  source_id?: string;
  enabled?: boolean;
  status?: string;
  pid?: number;
  record_local?: boolean;
  record_path?: string;
  record_pid?: number;
  created_at?: string;
  [k: string]: unknown;
}
interface ProgramInfo {
  active_source_id?: string | null;
  sources_preview?: string[];
}
interface StreamingStats {
  sources_total?: number;
  sources_live?: number;
  sources_recording?: number;
  transcodes_total?: number;
  transcodes_running?: number;
  transcodes_completed?: number;
  transcodes_failed?: number;
  outputs_total?: number;
  outputs_pushing?: number;
  program_has_active?: boolean;
}

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'live' | 'sources' | 'multi' | 'transcode' | 'relay' | 'outputs' | 'overview';
const tabs: { key: TabKey; label: string }[] = [
  { key: 'live', label: '直播' },
  { key: 'sources', label: '拉流源' },
  { key: 'multi', label: '多机位' },
  { key: 'transcode', label: '转码' },
  { key: 'relay', label: '拉流转推流' },
  { key: 'outputs', label: '推流' },
  { key: 'overview', label: '总览' },
];
// 深链支持：?tab=live（旧 /live 路由重定向到 /streaming?tab=live——旧链接不断）；
// 非法/缺省回落默认 Tab（直播未指定时不抢占既有落地行为）。
// 载体差异：应用包宿主无 vue-router——桌面嵌入模式 URL 为 /?app=streaming&tab=x、
// 独立模式为 standalone.html?tab=x，统一从 location.search 读取。
const initialTab = new URLSearchParams(location.search).get('tab') || 'sources';
const activeTab = ref<TabKey>(
  tabs.some((t) => t.key === initialTab) ? (initialTab as TabKey) : 'sources',
);

// =============================================================================
// 全局消息
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该流媒体接口';
  }
  return m;
}

// =============================================================================
// Tab1：拉流源
// =============================================================================
const sources = ref<StreamSource[]>([]);
const sourcesLoading = ref(false);
const sourcesError = ref('');

async function loadSources(): Promise<void> {
  sourcesLoading.value = true;
  sourcesError.value = '';
  try {
    const raw = await endpoints.streamingSources();
    sources.value = Array.isArray(raw) ? (raw as StreamSource[]) : [];
  } catch (e) {
    sources.value = [];
    sourcesError.value = friendlyError(e);
  } finally {
    sourcesLoading.value = false;
  }
}

const sourceStats = computed(() => ({
  total: sources.value.length,
  live: sources.value.filter((s) => s.status === 'live').length,
  recording: sources.value.filter((s) => s.recording).length,
}));

const srcBusyId = ref<string>('');

async function startRec(id: string): Promise<void> {
  srcBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.startRecording(id);
    await loadSources();
    msg.value = { kind: 'ok', text: '已开始录制' };
  } catch (e) {
    msg.value = { kind: 'err', text: '开始录制失败：' + friendlyError(e) };
  } finally {
    srcBusyId.value = '';
  }
}
async function stopRec(id: string): Promise<void> {
  if (!window.confirm('确定停止录制？')) return;
  srcBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.stopRecording(id);
    await loadSources();
    msg.value = { kind: 'ok', text: '已停止录制' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止录制失败：' + friendlyError(e) };
  } finally {
    srcBusyId.value = '';
  }
}
async function removeSource(id: string): Promise<void> {
  if (!window.confirm('确定删除该拉流源？')) return;
  srcBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteStreamingSource(id);
    await loadSources();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    srcBusyId.value = '';
  }
}

// 添加源对话框
const showSourceCreate = ref(false);
const srcForm = ref({
  name: '',
  url: '',
  protocol: 'rtsp',
  resolution_tag: '1080p',
  record_local: false,
  record_path: '',
});
const srcSubmitting = ref(false);

const protocolOptions: Protocol[] = ['rtsp', 'rtmp', 'srt', 'http', 'webrtc'];
const resolutionOptions: ResolutionTag[] = ['sd', '720p', '1080p', '2k', '4k', 'panorama'];

function openSourceCreate(): void {
  srcForm.value = {
    name: '',
    url: '',
    protocol: 'rtsp',
    resolution_tag: '1080p',
    record_local: false,
    record_path: '',
  };
  msg.value = null;
  showSourceCreate.value = true;
}
function closeSourceCreate(): void {
  if (srcSubmitting.value) return;
  showSourceCreate.value = false;
}
async function submitSourceCreate(): Promise<void> {
  const name = srcForm.value.name.trim();
  const url = srcForm.value.url.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!url) { msg.value = { kind: 'err', text: '请填写流地址' }; return; }
  srcSubmitting.value = true;
  msg.value = { kind: 'info', text: '添加中…' };
  try {
    await endpoints.addStreamingSource({
      name,
      url,
      protocol: srcForm.value.protocol,
      resolution_tag: srcForm.value.resolution_tag,
      record_local: !!srcForm.value.record_local,
      record_path: srcForm.value.record_path.trim() || undefined,
    });
    showSourceCreate.value = false;
    await loadSources();
    msg.value = { kind: 'ok', text: '已添加' };
  } catch (e) {
    msg.value = { kind: 'err', text: '添加失败：' + friendlyError(e) };
  } finally {
    srcSubmitting.value = false;
  }
}

const srcColumns: Column<StreamSource>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'url', title: 'URL', accessor: (r) => r.url ?? '—' },
  { key: 'protocol', title: '协议', width: '90px', align: 'center', accessor: (r) => r.protocol ?? '—' },
  { key: 'resolution_tag', title: '分辨率', width: '100px', align: 'center', accessor: (r) => r.resolution_tag ?? '—' },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'recording', title: '录制', width: '80px', align: 'center', accessor: (r) => (r.recording ? '是' : '否') },
  { key: 'actions', title: '操作', width: '180px', align: 'right' },
];

// =============================================================================
// Tab2：多机位
// =============================================================================
const program = ref<ProgramInfo>({ active_source_id: null, sources_preview: [] });
const programLoading = ref(false);
const programError = ref('');
const programBusyId = ref<string>('');

async function loadProgram(): Promise<void> {
  programLoading.value = true;
  programError.value = '';
  try {
    const raw = await endpoints.streamingProgram();
    program.value = (raw ?? program.value) as ProgramInfo;
  } catch (e) {
    programError.value = friendlyError(e);
  } finally {
    programLoading.value = false;
  }
}

async function switchActive(id: string): Promise<void> {
  if (program.value.active_source_id === id) return;
  programBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.switchProgram(id);
    await loadProgram();
    msg.value = { kind: 'ok', text: '已切换主输出' };
  } catch (e) {
    msg.value = { kind: 'err', text: '切换失败：' + friendlyError(e) };
  } finally {
    programBusyId.value = '';
  }
}

const activeSource = computed<StreamSource | null>(() => {
  const id = program.value.active_source_id;
  if (!id) return null;
  return sources.value.find((s) => s.id === id) ?? null;
});

// =============================================================================
// Tab3：转码
// =============================================================================
const transcodes = ref<TranscodeTask[]>([]);
const tcLoading = ref(false);
const tcError = ref('');
const tcBusyId = ref<string>('');

async function loadTranscodes(): Promise<void> {
  tcLoading.value = true;
  tcError.value = '';
  try {
    const raw = await endpoints.streamingTranscodes();
    transcodes.value = Array.isArray(raw) ? (raw as TranscodeTask[]) : [];
  } catch (e) {
    transcodes.value = [];
    tcError.value = friendlyError(e);
  } finally {
    tcLoading.value = false;
  }
}

const tcStats = computed(() => ({
  total: transcodes.value.length,
  running: transcodes.value.filter((t) => t.status === 'running').length,
  completed: transcodes.value.filter((t) => t.status === 'completed').length,
  failed: transcodes.value.filter((t) => t.status === 'failed').length,
}));

async function removeTranscode(id: string): Promise<void> {
  if (!window.confirm('确定删除该转码任务？')) return;
  tcBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteTranscode(id);
    await loadTranscodes();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    tcBusyId.value = '';
  }
}

const tcColumns: Column<TranscodeTask>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'input', title: '输入', accessor: (r) => r.input ?? '—' },
  { key: 'mode', title: '模式', width: '80px', align: 'center', accessor: (r) => r.mode ?? '—' },
  { key: 'codec', title: '编码器', width: '120px', align: 'center', accessor: (r) => r.codec ?? '—' },
  { key: 'ladder', title: 'Ladder', width: '160px', accessor: (r) => ladderSummary(r.ladder) },
  { key: 'status', title: '状态', width: '100px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'progress', title: '进度', width: '170px', accessor: (r) => r.progress ?? 0 },
  { key: 'actions', title: '操作', width: '90px', align: 'right' },
];

function ladderSummary(lad?: LadderRung[]): string {
  if (!Array.isArray(lad) || !lad.length) return '—';
  return lad.map((r) => r.label || (r.height ? `${r.height}p` : '?')).join(' / ');
}

// 创建转码对话框
const showTcCreate = ref(false);
const tcForm = ref({
  name: '',
  input: '',
  mode: 'vod' as 'vod' | 'live',
  codec: 'h264_nvenc',
  ladder: ['1080p', '720p'] as string[],
  output_dir: '/tank/hls',
  autostart: true,
});
const tcSubmitting = ref(false);

// 转码输入源：真实本地视频文件（GET /transcode/sources 扫描 /tank/media/video）
interface TranscodeSourceItem { path?: string; name?: string; size_bytes?: number; [k: string]: unknown; }
const tcSources = ref<TranscodeSourceItem[]>([]);
const tcSourcesLoading = ref(false);
const tcSourcesError = ref('');

async function loadTranscodeSources(): Promise<void> {
  tcSourcesLoading.value = true;
  tcSourcesError.value = '';
  try {
    const raw = await endpoints.streamingTranscodeSources();
    tcSources.value = Array.isArray(raw) ? (raw as TranscodeSourceItem[]) : [];
  } catch (e) {
    tcSources.value = [];
    tcSourcesError.value = friendlyError(e);
  } finally {
    tcSourcesLoading.value = false;
  }
}

function pickTranscodeSource(path: string): void {
  tcForm.value.input = path;
}

function fmtSize(bytes?: number): string {
  if (!bytes || bytes <= 0) return '—';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

const codecOptions = ['h264_nvenc', 'hevc_nvenc', 'av1_nvenc', 'libx264'];
const ladderPresets = ['4K', '2K', '1080P', '720P'];

function openTcCreate(): void {
  tcForm.value = {
    name: '',
    input: '',
    mode: 'vod',
    codec: 'h264_nvenc',
    ladder: ['1080P', '720P'],
    output_dir: '/tank/hls',
    autostart: true,
  };
  msg.value = null;
  showTcCreate.value = true;
  // 打开对话框时加载真实本地视频文件供选择
  void loadTranscodeSources();
}
function closeTcCreate(): void {
  if (tcSubmitting.value) return;
  showTcCreate.value = false;
}
function toggleLadder(preset: string): void {
  const set = new Set(tcForm.value.ladder);
  if (set.has(preset)) set.delete(preset);
  else set.add(preset);
  tcForm.value.ladder = ladderPresets.filter((p) => set.has(p));
}
function ladderSpec(preset: string): { width: number; height: number; bitrate: number } {
  switch (preset) {
    case '4K': return { width: 3840, height: 2160, bitrate: 15_000_000 };
    case '2K': return { width: 2560, height: 1440, bitrate: 8_000_000 };
    case '1080P': return { width: 1920, height: 1080, bitrate: 4_500_000 };
    case '720P': return { width: 1280, height: 720, bitrate: 2_500_000 };
    default: return { width: 0, height: 0, bitrate: 0 };
  }
}
async function submitTcCreate(): Promise<void> {
  const name = tcForm.value.name.trim();
  const input = tcForm.value.input.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!input) { msg.value = { kind: 'err', text: '请填写输入' }; return; }
  const ladder = tcForm.value.ladder.map((preset) => ({ label: preset, ...ladderSpec(preset) }));
  if (!ladder.length) { msg.value = { kind: 'err', text: '请至少选择一档 Ladder' }; return; }
  tcSubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createTranscode({
      name,
      input,
      output_dir: tcForm.value.output_dir.trim() || undefined,
      mode: tcForm.value.mode,
      codec: tcForm.value.codec,
      ladder,
      autostart: !!tcForm.value.autostart,
    });
    showTcCreate.value = false;
    await loadTranscodes();
    msg.value = { kind: 'ok', text: tcForm.value.autostart ? '已创建并启动转码' : '已创建' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    tcSubmitting.value = false;
  }
}

// =============================================================================
// Tab4：推流
// =============================================================================
const outputs = ref<StreamOutput[]>([]);
const outLoading = ref(false);
const outError = ref('');
const outBusyId = ref<string>('');

async function loadOutputs(): Promise<void> {
  outLoading.value = true;
  outError.value = '';
  try {
    const raw = await endpoints.streamingOutputs();
    outputs.value = Array.isArray(raw) ? (raw as StreamOutput[]) : [];
  } catch (e) {
    outputs.value = [];
    outError.value = friendlyError(e);
  } finally {
    outLoading.value = false;
  }
}

async function removeOutput(id: string): Promise<void> {
  if (!window.confirm('确定删除该推流目标？')) return;
  outBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteStreamingOutput(id);
    await loadOutputs();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    outBusyId.value = '';
  }
}

// 启动推流（拉流转推）：需先绑定拉流源
async function startOutput(row: StreamOutput): Promise<void> {
  const id = String(row.id ?? '');
  if (!row.source_id) {
    msg.value = { kind: 'err', text: '请先绑定拉流源' };
    window.alert('请先绑定拉流源');
    return;
  }
  outBusyId.value = id;
  msg.value = null;
  try {
    const res = await endpoints.startStreamingOutput(id) as { error?: string; status?: string };
    await loadOutputs();
    if (res?.status === 'error' || res?.error) {
      msg.value = { kind: 'err', text: '启动推流失败：' + (res?.error ?? friendlyError(res)) };
    } else {
      msg.value = { kind: 'ok', text: '已启动推流' };
    }
  } catch (e) {
    msg.value = { kind: 'err', text: '启动推流失败：' + friendlyError(e) };
  } finally {
    outBusyId.value = '';
  }
}

// 停止推流：杀掉 ffmpeg 子进程
async function stopOutput(id: string): Promise<void> {
  outBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.stopStreamingOutput(id);
    await loadOutputs();
    msg.value = { kind: 'ok', text: '已停止推流' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止推流失败：' + friendlyError(e) };
  } finally {
    outBusyId.value = '';
  }
}

const outColumns: Column<StreamOutput>[] = [
  { key: 'name', title: '名称', accessor: (r) => r.name ?? '—' },
  { key: 'url', title: 'URL', accessor: (r) => r.url ?? '—' },
  { key: 'protocol', title: '协议', width: '90px', align: 'center', accessor: (r) => r.protocol ?? '—' },
  { key: 'source_id', title: '绑定源', width: '140px', accessor: (r) => sourceLabel(r.source_id) },
  { key: 'enabled', title: '启用', width: '70px', align: 'center', accessor: (r) => (r.enabled ? '是' : '否') },
  { key: 'status', title: '状态', width: '90px', align: 'center', accessor: (r) => r.status ?? '—' },
  { key: 'actions', title: '操作', width: '180px', align: 'right' },
];

function sourceLabel(id?: string): string {
  if (!id) return '—';
  const s = sources.value.find((x) => x.id === id);
  return s?.name ?? id;
}

// 添加推流对话框
const showOutCreate = ref(false);
const outForm = ref({
  name: '',
  url: '',
  protocol: 'rtmp',
  source_id: '',
  enabled: true,
  record_local: false,
  record_path: '',
});
const outSubmitting = ref(false);

function openOutCreate(): void {
  outForm.value = {
    name: '',
    url: '',
    protocol: 'rtmp',
    source_id: '',
    enabled: true,
    record_local: false,
    record_path: '',
  };
  msg.value = null;
  showOutCreate.value = true;
}
function closeOutCreate(): void {
  if (outSubmitting.value) return;
  showOutCreate.value = false;
}
async function submitOutCreate(): Promise<void> {
  const name = outForm.value.name.trim();
  const url = outForm.value.url.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!url) { msg.value = { kind: 'err', text: '请填写推流地址' }; return; }
  outSubmitting.value = true;
  msg.value = { kind: 'info', text: '添加中…' };
  try {
    await endpoints.addStreamingOutput({
      name,
      url,
      protocol: outForm.value.protocol,
      source_id: outForm.value.source_id.trim() || undefined,
      enabled: !!outForm.value.enabled,
      record_local: !!outForm.value.record_local,
      record_path: outForm.value.record_path.trim() || undefined,
    });
    showOutCreate.value = false;
    await loadOutputs();
    msg.value = { kind: 'ok', text: '已添加' };
  } catch (e) {
    msg.value = { kind: 'err', text: '添加失败：' + friendlyError(e) };
  } finally {
    outSubmitting.value = false;
  }
}

// =============================================================================
// Tab4：拉流转推流（Relay）—— 把 output.source_id → source 展示为一条完整链路
// =============================================================================
// 一条 relay 链路 = 一个绑定了 source_id 的 output；source + output 拼成"源 → 目标"
interface RelayChain {
  output: StreamOutput;
  source: StreamSource | null;
}

const relayChains = computed<RelayChain[]>(() => {
  return outputs.value
    .filter((o) => !!o.source_id)
    .map((o) => ({
      output: o,
      source: sources.value.find((s) => s.id === o.source_id) ?? null,
    }));
});

const relayBusyId = ref<string>('');

async function startRelay(chain: RelayChain): Promise<void> {
  const id = String(chain.output.id ?? '');
  relayBusyId.value = id;
  msg.value = null;
  try {
    const res = await endpoints.startStreamingOutput(id) as {
      error?: string; status?: string; record_warning?: string; record_error?: string;
    };
    await loadOutputs();
    if (res?.status === 'error' || res?.error) {
      msg.value = { kind: 'err', text: '启动失败：' + (res?.error ?? friendlyError(res)) };
    } else if (res?.record_error) {
      msg.value = { kind: 'err', text: '推流已启动，但本地录制失败：' + res.record_error };
    } else if (res?.record_warning) {
      msg.value = { kind: 'info', text: '推流已启动（' + res.record_warning + '）' };
    } else {
      msg.value = { kind: 'ok', text: '已启动转推' };
    }
  } catch (e) {
    msg.value = { kind: 'err', text: '启动失败：' + friendlyError(e) };
  } finally {
    relayBusyId.value = '';
  }
}

async function stopRelay(chain: RelayChain): Promise<void> {
  const id = String(chain.output.id ?? '');
  relayBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.stopStreamingOutput(id);
    await loadOutputs();
    msg.value = { kind: 'ok', text: '已停止转推' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止失败：' + friendlyError(e) };
  } finally {
    relayBusyId.value = '';
  }
}

async function removeRelay(chain: RelayChain): Promise<void> {
  const id = String(chain.output.id ?? '');
  if (!window.confirm('确定删除该转推链路？')) return;
  relayBusyId.value = id;
  msg.value = null;
  try {
    await endpoints.deleteStreamingOutput(id);
    await loadOutputs();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    relayBusyId.value = '';
  }
}

// 创建转推链路对话框：同时指定拉流源 + 推流目标 + 是否同时保存本地
const showRelayCreate = ref(false);
const relayForm = ref({
  source_id: '',
  // 允许手动输入源 URL（当 sources 为空或想用未注册源时）
  manual_source_url: '',
  manual_source_protocol: 'rtsp',
  use_manual_source: false,
  name: '',
  target_url: '',
  target_protocol: 'rtmp',
  record_local: false,
  record_path: '',
});
const relaySubmitting = ref(false);

function openRelayCreate(): void {
  relayForm.value = {
    source_id: sources.value[0]?.id ?? '',
    manual_source_url: '',
    manual_source_protocol: 'rtsp',
    use_manual_source: sources.value.length === 0,
    name: '',
    target_url: '',
    target_protocol: 'rtmp',
    record_local: false,
    record_path: '',
  };
  msg.value = null;
  showRelayCreate.value = true;
}
function closeRelayCreate(): void {
  if (relaySubmitting.value) return;
  showRelayCreate.value = false;
}
async function submitRelayCreate(): Promise<void> {
  const name = relayForm.value.name.trim();
  const targetUrl = relayForm.value.target_url.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!targetUrl) { msg.value = { kind: 'err', text: '请填写推流目标地址' }; return; }
  relaySubmitting.value = true;
  msg.value = { kind: 'info', text: '创建中…' };
  try {
    // 若用手动源：先创建拉流源，拿到 id 再绑定到 output
    let sourceId = relayForm.value.source_id;
    if (relayForm.value.use_manual_source) {
      const srcUrl = relayForm.value.manual_source_url.trim();
      if (!srcUrl) {
        msg.value = { kind: 'err', text: '请填写拉流源地址或选择已有源' };
        relaySubmitting.value = false;
        return;
      }
      const created = await endpoints.addStreamingSource({
        name: `${name}-源`,
        url: srcUrl,
        protocol: relayForm.value.manual_source_protocol,
      }) as { id?: string };
      sourceId = created?.id ?? '';
    }
    if (!sourceId) {
      msg.value = { kind: 'err', text: '未绑定拉流源，请先添加或选择一个' };
      relaySubmitting.value = false;
      return;
    }
    await endpoints.addStreamingOutput({
      name,
      url: targetUrl,
      protocol: relayForm.value.target_protocol,
      source_id: sourceId || undefined,
      enabled: true,
      record_local: !!relayForm.value.record_local,
      record_path: relayForm.value.record_path.trim() || undefined,
    });
    showRelayCreate.value = false;
    await Promise.all([loadSources(), loadOutputs()]);
    msg.value = { kind: 'ok', text: '已创建转推链路' };
  } catch (e) {
    msg.value = { kind: 'err', text: '创建失败：' + friendlyError(e) };
  } finally {
    relaySubmitting.value = false;
  }
}

// =============================================================================
// Tab6：总览
// =============================================================================
const stats = ref<StreamingStats>({});
const statsLoading = ref(false);
const statsError = ref('');

async function loadStats(): Promise<void> {
  statsLoading.value = true;
  statsError.value = '';
  try {
    const raw = await endpoints.streamingStats();
    stats.value = (raw ?? {}) as StreamingStats;
  } catch (e) {
    statsError.value = friendlyError(e);
  } finally {
    statsLoading.value = false;
  }
}

// =============================================================================
// 徽章映射
// =============================================================================
function statusClass(s?: string): string {
  switch (s) {
    case 'live': case 'running': case 'pushing': case 'completed': return 'pill-ok';
    case 'connecting': case 'queued': case 'idle': return 'pill-blue';
    case 'error': case 'failed': return 'pill-err';
    case 'cancelled': return 'pill-muted';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'live': return '直播';
    case 'connecting': return '连接中';
    case 'idle': return '空闲';
    case 'error': return '错误';
    case 'running': return '运行中';
    case 'queued': return '排队';
    case 'completed': return '已完成';
    case 'failed': return '失败';
    case 'cancelled': return '已取消';
    case 'pushing': return '推流中';
    default: return s ?? '—';
  }
}
function resClass(tag?: string): string {
  switch (tag) {
    case '4k': return 'pill-gold';
    case '2k': return 'pill-purple';
    case 'panorama': return 'pill-cyan';
    case '1080p': return 'pill-blue';
    case '720p': return 'pill-ok';
    case 'sd': return 'pill-muted';
    default: return 'pill-muted';
  }
}
function codecClass(codec?: string): string {
  switch (codec) {
    case 'h264_nvenc': return 'pill-ok';
    case 'hevc_nvenc': return 'pill-purple';
    case 'av1_nvenc': return 'pill-pink';
    case 'libx264': return 'pill-muted';
    default: return 'pill-muted';
  }
}
function modeClass(mode?: string): string {
  return mode === 'live' ? 'pill-orange' : 'pill-blue';
}
function modeLabel(mode?: string): string {
  return mode === 'live' ? '直播' : mode === 'vod' ? '点播' : (mode ?? '—');
}

// =============================================================================
// 刷新与初始化
// =============================================================================
async function refreshAll(): Promise<void> {
  await Promise.all([
    loadSources(),
    loadProgram(),
    loadTranscodes(),
    loadOutputs(),
    loadStats(),
  ]);
}

onMounted(() => {
  void refreshAll();
  void initCaps();
});

onBeforeUnmount(() => {
  unsubCaps?.();
  unsubCaps = null;
});
</script>

<template>
  <div class="streaming-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">{{ t('streaming.title') }}</h2>
        <div class="page-sub muted">{{ t('streaming.subtitle') }}</div>
      </div>
      <div class="head-actions">
        <!-- 能力徽章（独立模式；全能力=无徽章，apps/film 0.1.3 范式） -->
        <span
          v-if="isStandalone && deg && deg.mode !== 'full'"
          class="caps-badge"
          :class="deg.mode === 'offline' ? 'caps-off' : 'caps-deg'"
          :title="deg.mode === 'offline'
            ? t('streaming.capsOfflineTip')
            : t('streaming.capsMissingTip', { caps: deg.missing.join(', ') })"
        >{{ deg.mode === 'offline' ? t('streaming.capsOffline') : t('streaming.capsDegraded') }}</span>
        <button
          v-if="!isStandalone"
          class="btn btn-small btn-ext"
          type="button"
          :title="t('streaming.openStandalone')"
          :aria-label="t('streaming.openStandalone')"
          @click="openStandalone"
        >↗</button>
        <button
          class="btn btn-small"
          :disabled="sourcesLoading || tcLoading || outLoading"
          @click="refreshAll"
        >
          <span
            class="spin"
            :class="{ spinning: sourcesLoading || tcLoading || outLoading }"
            aria-hidden="true"
          >↻</span>
          刷新
        </button>
      </div>
    </div>

    <!-- Tab 切换 -->
    <nav class="tabs" role="tablist">
      <button
        v-for="t in tabs"
        :key="t.key"
        class="tab"
        :class="{ active: activeTab === t.key }"
        role="tab"
        :aria-selected="activeTab === t.key"
        @click="activeTab = t.key"
      >{{ t.label }}</button>
    </nav>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== Tab0 直播（本地大厅 + 联邦大厅，LivePanel） =================== -->
    <section v-show="activeTab === 'live'" class="tab-panel">
      <LivePanel />
    </section>

    <!-- =================== Tab1 拉流源 =================== -->
    <section v-show="activeTab === 'sources'" class="tab-panel">
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">拉流源总数</div>
          <div class="stat-value">{{ sourceStats.total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">在线</div>
          <div class="stat-value">{{ sourceStats.live }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">录制中</div>
          <div class="stat-value">{{ sourceStats.recording }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">拉流源列表</span>
        <button class="btn btn-small btn-primary" @click="openSourceCreate">＋ 添加源</button>
      </div>

      <div v-if="sourcesError" class="error-box">{{ sourcesError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="srcColumns"
            :rows="sources"
            :loading="sourcesLoading"
            empty-text="暂无拉流源，点击右上角「添加源」。"
          >
            <template #cell-protocol="{ row }">
              <span class="pill pill-muted">{{ row.protocol ?? '—' }}</span>
            </template>
            <template #cell-resolution_tag="{ row }">
              <span class="pill" :class="resClass(row.resolution_tag)">{{ row.resolution_tag ?? '—' }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-recording="{ row }">
              <span class="pill" :class="row.recording ? 'pill-err' : 'pill-muted'">
                {{ row.recording ? '录制中' : '否' }}
              </span>
              <span
                v-if="row.recording && row.record_path"
                class="muted pid-hint"
                :title="row.record_path"
              >→ {{ row.record_path }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="!row.recording"
                class="btn btn-small"
                :disabled="srcBusyId === row.id"
                @click.stop="startRec(String(row.id ?? ''))"
              >录制</button>
              <button
                v-else
                class="btn btn-small"
                :disabled="srcBusyId === row.id"
                @click.stop="stopRec(String(row.id ?? ''))"
              >停止</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="srcBusyId === row.id"
                @click.stop="removeSource(String(row.id ?? ''))"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 添加源对话框 -->
      <div v-if="showSourceCreate" class="modal-backdrop" @click.self="closeSourceCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="src-create-title">
          <div class="modal-head">
            <h3 id="src-create-title">添加拉流源</h3>
            <button class="modal-close" type="button" :disabled="srcSubmitting" @click="closeSourceCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitSourceCreate">
            <div class="field">
              <label for="src-name">名称</label>
              <input id="src-name" v-model="srcForm.name" type="text" placeholder="前门摄像头" :disabled="srcSubmitting" />
            </div>
            <div class="field">
              <label for="src-url">流地址</label>
              <input id="src-url" v-model="srcForm.url" type="text" placeholder="rtsp://192.168.1.50/stream" :disabled="srcSubmitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="src-proto">协议</label>
                <select id="src-proto" v-model="srcForm.protocol" :disabled="srcSubmitting">
                  <option v-for="p in protocolOptions" :key="p" :value="p">{{ p }}</option>
                </select>
              </div>
              <div class="field">
                <label for="src-res">分辨率</label>
                <select id="src-res" v-model="srcForm.resolution_tag" :disabled="srcSubmitting">
                  <option v-for="r in resolutionOptions" :key="r" :value="r">{{ r }}</option>
                </select>
              </div>
            </div>
            <div class="field">
              <label class="switch">
                <input v-model="srcForm.record_local" type="checkbox" :disabled="srcSubmitting" />
                <span>同时保存本地录制（开启录制时另起 ffmpeg 落盘）</span>
              </label>
            </div>
            <div v-if="srcForm.record_local" class="field">
              <label for="src-recpath">本地保存目录（留空用默认 /tank/recordings/sources/&lt;id&gt;/）</label>
              <input
                id="src-recpath"
                v-model="srcForm.record_path"
                type="text"
                placeholder="/tank/recordings/sources/<名称>/"
                :disabled="srcSubmitting"
              />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="srcSubmitting" @click="closeSourceCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="srcSubmitting">
                {{ srcSubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab2 多机位 =================== -->
    <section v-show="activeTab === 'multi'" class="tab-panel">
      <div v-if="programError" class="error-box">{{ programError }}</div>

      <!-- 当前主输出 -->
      <div class="card active-card">
        <div class="active-label">当前主输出</div>
        <div class="active-name">{{ activeSource?.name ?? '未选择' }}</div>
        <div class="active-meta">
          <span v-if="activeSource" class="pill" :class="resClass(activeSource.resolution_tag)">
            {{ activeSource.resolution_tag ?? '—' }}
          </span>
          <span v-if="activeSource" class="pill" :class="statusClass(activeSource.status)">
            {{ statusLabel(activeSource.status) }}
          </span>
          <span v-if="program.active_source_id" class="pill pill-live">LIVE</span>
          <span v-else class="pill pill-muted">无主输出</span>
        </div>
      </div>

      <div class="panel-head">
        <span class="panel-title">多机位源（点击切换主输出）</span>
        <span v-if="programLoading" class="muted small">加载中…</span>
      </div>

      <div v-if="programLoading && !sources.length" class="card empty-card">加载中…</div>
      <div v-else-if="!sources.length" class="card empty-card">暂无拉流源，请先到「拉流源」添加。</div>
      <section v-else class="cam-grid">
        <button
          v-for="s in sources"
          :key="s.id"
          type="button"
          class="card cam-card"
          :class="{ 'is-active': program.active_source_id === s.id }"
          :disabled="programBusyId === s.id"
          @click="switchActive(String(s.id ?? ''))"
        >
          <div class="cam-card-head">
            <span class="cam-name">{{ s.name ?? '—' }}</span>
            <span v-if="program.active_source_id === s.id" class="pill pill-live">主</span>
          </div>
          <div class="cam-url mono" :title="s.url ?? ''">{{ s.url ?? '—' }}</div>
          <div class="cam-meta">
            <span class="pill" :class="resClass(s.resolution_tag)">{{ s.resolution_tag ?? '—' }}</span>
            <span class="pill" :class="statusClass(s.status)">{{ statusLabel(s.status) }}</span>
          </div>
        </button>
      </section>
    </section>

    <!-- =================== Tab3 转码 =================== -->
    <section v-show="activeTab === 'transcode'" class="tab-panel">
      <section class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">任务总数</div>
          <div class="stat-value">{{ tcStats.total }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">运行中</div>
          <div class="stat-value">{{ tcStats.running }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">已完成</div>
          <div class="stat-value">{{ tcStats.completed }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">失败</div>
          <div class="stat-value">{{ tcStats.failed }}</div>
        </div>
      </section>

      <div class="panel-head">
        <span class="panel-title">转码任务</span>
        <button
          class="btn btn-small btn-primary"
          :disabled="!transcodeAvailable"
          :title="transcodeDisabledTip"
          @click="openTcCreate"
        >＋ 创建任务</button>
      </div>

      <div v-if="tcError" class="error-box">{{ tcError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="tcColumns"
            :rows="transcodes"
            :loading="tcLoading"
            empty-text="暂无转码任务，点击右上角「创建任务」。"
          >
            <template #cell-mode="{ row }">
              <span class="pill" :class="modeClass(row.mode)">{{ modeLabel(row.mode) }}</span>
            </template>
            <template #cell-codec="{ row }">
              <span class="pill" :class="codecClass(row.codec)">{{ row.codec ?? '—' }}</span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
            </template>
            <template #cell-progress="{ row }">
              <div class="prog-wrap">
                <div class="prog-bar">
                  <div
                    class="prog-fill"
                    :class="{
                      'fill-ok': row.status === 'completed',
                      'fill-err': row.status === 'failed',
                    }"
                    :style="{ width: (row.progress ?? 0) + '%' }"
                  ></div>
                </div>
                <span class="prog-text">{{ row.progress ?? 0 }}%</span>
              </div>
              <span v-if="row.status === 'running' && row.pid" class="muted pid-hint">pid {{ row.pid }}</span>
              <span
                v-else-if="row.status === 'completed' && row.output_dir"
                class="muted pid-hint"
                :title="row.output_dir"
              >HLS: {{ row.output_dir }}/index.m3u8</span>
              <span v-else-if="row.status === 'failed' && row.error" class="muted pid-hint err-text" :title="row.error">
                {{ row.error }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small btn-danger"
                :disabled="tcBusyId === row.id"
                @click.stop="removeTranscode(String(row.id ?? ''))"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 创建转码对话框 -->
      <div v-if="showTcCreate" class="modal-backdrop" @click.self="closeTcCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="tc-create-title">
          <div class="modal-head">
            <h3 id="tc-create-title">创建转码任务</h3>
            <button class="modal-close" type="button" :disabled="tcSubmitting" @click="closeTcCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitTcCreate">
            <div class="field">
              <label for="tc-name">名称</label>
              <input id="tc-name" v-model="tcForm.name" type="text" placeholder="晚间直播 HLS" :disabled="tcSubmitting" />
            </div>
            <div class="field">
              <label for="tc-input">输入（拉流地址或文件路径）</label>
              <input id="tc-input" v-model="tcForm.input" type="text" placeholder="rtsp://... 或 /tank/media/video/xxx.mp4" :disabled="tcSubmitting" />
            </div>
            <div class="field">
              <label>从本地视频选择（扫描 /tank/media/video/）</label>
              <div v-if="tcSourcesLoading" class="muted small">加载本地视频…</div>
              <div v-else-if="tcSourcesError" class="muted small err-text">{{ tcSourcesError }}</div>
              <div v-else-if="!tcSources.length" class="muted small">/tank/media/video/ 下暂无视频文件，可手动输入 URL 或路径。</div>
              <div v-else class="source-pick-row">
                <button
                  v-for="s in tcSources"
                  :key="s.path"
                  type="button"
                  class="btn btn-small source-pick"
                  :class="{ 'is-selected': tcForm.input === s.path }"
                  :disabled="tcSubmitting"
                  :title="s.path"
                  @click="pickTranscodeSource(String(s.path ?? ''))"
                >{{ s.name }} · {{ fmtSize(s.size_bytes) }}</button>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="tc-mode">模式</label>
                <select id="tc-mode" v-model="tcForm.mode" :disabled="tcSubmitting">
                  <option value="vod">点播 (vod)</option>
                  <option value="live">直播 (live)</option>
                </select>
              </div>
              <div class="field">
                <label for="tc-codec">编码器</label>
                <select id="tc-codec" v-model="tcForm.codec" :disabled="tcSubmitting">
                  <option v-for="c in codecOptions" :key="c" :value="c">{{ c }}</option>
                </select>
              </div>
            </div>
            <div class="field">
              <label>Ladder（多档分辨率）</label>
              <div class="check-row">
                <label v-for="p in ladderPresets" :key="p" class="check-item">
                  <input
                    type="checkbox"
                    :checked="tcForm.ladder.includes(p)"
                    :disabled="tcSubmitting"
                    @change="toggleLadder(p)"
                  />
                  <span>{{ p }}</span>
                </label>
              </div>
            </div>
            <div class="field">
              <label for="tc-out">输出目录（留空默认 /tank/hls/&lt;名称&gt;；/tank 不可写时后端降级 /tmp/os-hls/）</label>
              <input id="tc-out" v-model="tcForm.output_dir" type="text" placeholder="/tank/hls" :disabled="tcSubmitting" />
            </div>
            <div class="field">
              <label class="switch">
                <input v-model="tcForm.autostart" type="checkbox" :disabled="tcSubmitting" />
                <span>创建后立即启动 ffmpeg 转码（生成 HLS）</span>
              </label>
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="tcSubmitting" @click="closeTcCreate">取消</button>
              <button
                type="submit"
                class="btn btn-primary"
                :disabled="tcSubmitting || !transcodeAvailable"
                :title="transcodeDisabledTip"
              >
                {{ tcSubmitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab4 拉流转推流（Relay） =================== -->
    <section v-show="activeTab === 'relay'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">拉流转推流链路（源 → 目标）</span>
        <button
          class="btn btn-small btn-primary"
          :disabled="!transcodeAvailable"
          :title="transcodeDisabledTip"
          @click="openRelayCreate"
        >＋ 创建转推链路</button>
      </div>

      <div v-if="!sources.length" class="card empty-card">
        暂无拉流源。请先到「拉流源」Tab 添加，或在创建对话框中手动输入源地址。
      </div>
      <div v-else-if="!relayChains.length" class="card empty-card">
        暂无转推链路。点击右上角「创建转推链路」，把一个拉流源转发到推流目标（RTMP/SRT）。
      </div>
      <section v-else class="relay-grid">
        <div v-for="chain in relayChains" :key="chain.output.id" class="card relay-card">
          <div class="relay-head">
            <span class="relay-name">{{ chain.output.name ?? '—' }}</span>
            <span class="pill" :class="statusClass(chain.output.status)">{{ statusLabel(chain.output.status) }}</span>
            <span v-if="chain.output.record_local" class="pill pill-err" title="同时保存本地录制">录本地</span>
          </div>
          <div class="relay-chain">
            <div class="relay-node">
              <div class="relay-node-label">拉流源</div>
              <div class="relay-node-name">{{ chain.source?.name ?? '（源已删除）' }}</div>
              <div class="relay-node-url mono" :title="chain.source?.url ?? ''">{{ chain.source?.url ?? chain.output.source_id ?? '—' }}</div>
            </div>
            <div class="relay-arrow">→</div>
            <div class="relay-node">
              <div class="relay-node-label">推流目标</div>
              <div class="relay-node-name">{{ chain.output.protocol ?? '—' }}</div>
              <div class="relay-node-url mono" :title="chain.output.url ?? ''">{{ chain.output.url ?? '—' }}</div>
            </div>
          </div>
          <div v-if="chain.output.status === 'pushing' && chain.output.pid" class="muted small">
            推流 pid {{ chain.output.pid }}
            <span v-if="chain.output.record_pid"> · 录制 pid {{ chain.output.record_pid }}</span>
          </div>
          <div v-if="chain.output.record_local && chain.output.record_path" class="muted small" :title="chain.output.record_path">
            录制目录：{{ chain.output.record_path }}
          </div>
          <div class="relay-actions">
            <button
              v-if="chain.output.status === 'pushing'"
              class="btn btn-small btn-warning"
              :disabled="relayBusyId === chain.output.id"
              @click="stopRelay(chain)"
            >停止</button>
            <button
              v-else
              class="btn btn-small btn-primary"
              :disabled="relayBusyId === chain.output.id"
              @click="startRelay(chain)"
            >启动</button>
            <button
              class="btn btn-small btn-danger"
              :disabled="relayBusyId === chain.output.id"
              @click="removeRelay(chain)"
            >删除</button>
          </div>
        </div>
      </section>

      <!-- 创建转推链路对话框 -->
      <div v-if="showRelayCreate" class="modal-backdrop" @click.self="closeRelayCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="relay-create-title">
          <div class="modal-head">
            <h3 id="relay-create-title">创建转推链路</h3>
            <button class="modal-close" type="button" :disabled="relaySubmitting" @click="closeRelayCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitRelayCreate">
            <div class="field">
              <label for="relay-name">链路名称</label>
              <input id="relay-name" v-model="relayForm.name" type="text" placeholder="前门摄像头 → B站" :disabled="relaySubmitting" />
            </div>
            <div class="field">
              <label class="switch">
                <input v-model="relayForm.use_manual_source" type="checkbox" :disabled="relaySubmitting" />
                <span>手动输入源地址（不选则从已有拉流源下拉）</span>
              </label>
            </div>
            <div v-if="!relayForm.use_manual_source" class="field">
              <label for="relay-src">选拉流源</label>
              <select id="relay-src" v-model="relayForm.source_id" :disabled="relaySubmitting || !sources.length">
                <option value="" disabled>{{ sources.length ? '请选择…' : '暂无拉流源（请改用手动输入）' }}</option>
                <option v-for="s in sources" :key="s.id" :value="s.id">{{ s.name }} · {{ s.url }}</option>
              </select>
            </div>
            <div v-else class="field-row">
              <div class="field">
                <label for="relay-manual-url">源地址</label>
                <input id="relay-manual-url" v-model="relayForm.manual_source_url" type="text" placeholder="rtsp://192.168.1.50/stream" :disabled="relaySubmitting" />
              </div>
              <div class="field" style="flex: 0 0 120px;">
                <label for="relay-manual-proto">源协议</label>
                <select id="relay-manual-proto" v-model="relayForm.manual_source_protocol" :disabled="relaySubmitting">
                  <option v-for="p in protocolOptions" :key="p" :value="p">{{ p }}</option>
                </select>
              </div>
            </div>
            <div class="field-row">
              <div class="field">
                <label for="relay-target">推流目标地址</label>
                <input id="relay-target" v-model="relayForm.target_url" type="text" placeholder="rtmp://live.example.com/live/key" :disabled="relaySubmitting" />
              </div>
              <div class="field" style="flex: 0 0 120px;">
                <label for="relay-target-proto">目标协议</label>
                <select id="relay-target-proto" v-model="relayForm.target_protocol" :disabled="relaySubmitting">
                  <option v-for="p in protocolOptions" :key="p" :value="p">{{ p }}</option>
                </select>
              </div>
            </div>
            <div class="field">
              <label class="switch">
                <input v-model="relayForm.record_local" type="checkbox" :disabled="relaySubmitting" />
                <span>同时保存本地录制（推流时另起 ffmpeg 落盘）</span>
              </label>
            </div>
            <div v-if="relayForm.record_local" class="field">
              <label for="relay-recpath">本地保存目录（留空用默认 /tank/recordings/outputs/&lt;id&gt;/）</label>
              <input
                id="relay-recpath"
                v-model="relayForm.record_path"
                type="text"
                placeholder="/tank/recordings/outputs/<名称>/"
                :disabled="relaySubmitting"
              />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="relaySubmitting" @click="closeRelayCreate">取消</button>
              <button
                type="submit"
                class="btn btn-primary"
                :disabled="relaySubmitting || !transcodeAvailable"
                :title="transcodeDisabledTip"
              >
                {{ relaySubmitting ? '创建中…' : '创建' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab5 推流 =================== -->
    <section v-show="activeTab === 'outputs'" class="tab-panel">
      <div class="panel-head">
        <span class="panel-title">推流目标</span>
        <button class="btn btn-small btn-primary" @click="openOutCreate">＋ 添加推流</button>
      </div>

      <div v-if="outError" class="error-box">{{ outError }}</div>
      <div class="panel">
        <div class="card card-table">
          <DataTable
            :columns="outColumns"
            :rows="outputs"
            :loading="outLoading"
            empty-text="暂无推流目标，点击右上角「添加推流」。"
          >
            <template #cell-protocol="{ row }">
              <span class="pill pill-muted">{{ row.protocol ?? '—' }}</span>
            </template>
            <template #cell-enabled="{ row }">
              <span class="pill" :class="row.enabled ? 'pill-ok' : 'pill-muted'">
                {{ row.enabled ? '启用' : '停用' }}
              </span>
            </template>
            <template #cell-status="{ row }">
              <span class="pill" :class="statusClass(row.status)">{{ statusLabel(row.status) }}</span>
              <span v-if="row.status === 'pushing' && row.pid" class="muted pid-hint">pid {{ row.pid }}</span>
            </template>
            <template #cell-actions="{ row }">
              <button
                v-if="row.status === 'pushing'"
                class="btn btn-small btn-warning"
                :disabled="outBusyId === row.id"
                @click.stop="stopOutput(String(row.id ?? ''))"
              >停止</button>
              <button
                v-else
                class="btn btn-small btn-primary"
                :disabled="outBusyId === row.id"
                @click.stop="startOutput(row)"
              >启动</button>
              <button
                class="btn btn-small btn-danger"
                :disabled="outBusyId === row.id"
                @click.stop="removeOutput(String(row.id ?? ''))"
              >删除</button>
            </template>
          </DataTable>
        </div>
      </div>

      <!-- 添加推流对话框 -->
      <div v-if="showOutCreate" class="modal-backdrop" @click.self="closeOutCreate">
        <div class="modal" role="dialog" aria-modal="true" aria-labelledby="out-create-title">
          <div class="modal-head">
            <h3 id="out-create-title">添加推流目标</h3>
            <button class="modal-close" type="button" :disabled="outSubmitting" @click="closeOutCreate">×</button>
          </div>
          <form class="modal-body" @submit.prevent="submitOutCreate">
            <div class="field">
              <label for="out-name">名称</label>
              <input id="out-name" v-model="outForm.name" type="text" placeholder="B站推流" :disabled="outSubmitting" />
            </div>
            <div class="field">
              <label for="out-url">推流地址</label>
              <input id="out-url" v-model="outForm.url" type="text" placeholder="rtmp://live.example.com/live/key" :disabled="outSubmitting" />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="out-proto">协议</label>
                <select id="out-proto" v-model="outForm.protocol" :disabled="outSubmitting">
                  <option v-for="p in protocolOptions" :key="p" :value="p">{{ p }}</option>
                </select>
              </div>
              <div class="field">
                <label for="out-src">绑定源</label>
                <select id="out-src" v-model="outForm.source_id" :disabled="outSubmitting">
                  <option value="">（无）</option>
                  <option v-for="s in sources" :key="s.id" :value="s.id">{{ s.name ?? s.id }}</option>
                </select>
              </div>
            </div>
            <label class="switch">
              <input v-model="outForm.enabled" type="checkbox" :disabled="outSubmitting" />
              <span>启用</span>
            </label>
            <div class="field">
              <label class="switch">
                <input v-model="outForm.record_local" type="checkbox" :disabled="outSubmitting" />
                <span>同时保存本地录制（推流时另起 ffmpeg 落盘）</span>
              </label>
            </div>
            <div v-if="outForm.record_local" class="field">
              <label for="out-recpath">本地保存目录（留空用默认 /tank/recordings/outputs/&lt;id&gt;/）</label>
              <input
                id="out-recpath"
                v-model="outForm.record_path"
                type="text"
                placeholder="/tank/recordings/outputs/<名称>/"
                :disabled="outSubmitting"
              />
            </div>
            <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
            <div class="form-actions">
              <button type="button" class="btn" :disabled="outSubmitting" @click="closeOutCreate">取消</button>
              <button type="submit" class="btn btn-primary" :disabled="outSubmitting">
                {{ outSubmitting ? '添加中…' : '添加' }}
              </button>
            </div>
          </form>
        </div>
      </div>
    </section>

    <!-- =================== Tab5 总览 =================== -->
    <section v-show="activeTab === 'overview'" class="tab-panel">
      <div v-if="statsError" class="error-box">{{ statsError }}</div>
      <div v-if="statsLoading && !Object.keys(stats).length" class="card empty-card">加载中…</div>
      <section v-else class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">拉流源</div>
          <div class="stat-value">{{ stats.sources_total ?? 0 }}</div>
          <div class="stat-foot">在线 {{ stats.sources_live ?? 0 }} · 录制 {{ stats.sources_recording ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">转码任务</div>
          <div class="stat-value">{{ stats.transcodes_total ?? 0 }}</div>
          <div class="stat-foot">
            运行 {{ stats.transcodes_running ?? 0 }} · 完成 {{ stats.transcodes_completed ?? 0 }} · 失败 {{ stats.transcodes_failed ?? 0 }}
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">推流目标</div>
          <div class="stat-value">{{ stats.outputs_total ?? 0 }}</div>
          <div class="stat-foot">推流中 {{ stats.outputs_pushing ?? 0 }}</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">主输出</div>
          <div class="stat-value">{{ stats.program_has_active ? '活跃' : '无' }}</div>
          <div class="stat-foot">MediaMTX 状态占位</div>
        </div>
      </section>
    </section>
  </div>
</template>

<style scoped>
/* 能力徽章（@nexos/app-sdk 降级三态；全能力=不渲染，apps/film 0.1.3 范式） */
.caps-badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  white-space: nowrap;
  flex-shrink: 0;
  cursor: help;
}
.caps-deg {
  color: #92400e;
  background: #fffbeb;
  border: 1px solid #fde68a;
}
.caps-off {
  color: #b91c1c;
  background: #fef2f2;
  border: 1px solid #fecaca;
}
.btn-ext { padding: 4px 8px; }

.streaming-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }

/* Tabs */
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover { color: var(--text, #2B2B2B); }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }

.tab-panel { display: flex; flex-direction: column; gap: 14px; }

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-foot { font-size: 12px; color: var(--text-muted, #5E5C5F); }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; }
.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* 多机位 */
.active-card { padding: 20px 22px; display: flex; flex-direction: column; gap: 6px; }
.active-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.active-name { font-size: 28px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.active-meta { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding-top: 4px; }
.cam-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(280px, 1fr)); gap: 14px; }
.cam-card {
  padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; text-align: left;
  cursor: pointer; transition: border-color 0.15s ease, box-shadow 0.15s ease, transform 0.15s ease;
  font-family: inherit;
}
.cam-card:hover:not(:disabled) { border-color: var(--accent, #E95420); transform: translateY(-2px); }
.cam-card.is-active { border-color: #C7162B; box-shadow: 0 0 0 2px rgba(199, 22, 43, 0.25); }
.cam-card:disabled { opacity: 0.6; cursor: progress; }
.cam-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.cam-name { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); }
.cam-url { font-size: 12px; color: var(--text-muted, #5E5C5F); word-break: break-all; }
.cam-meta { display: flex; gap: 6px; flex-wrap: wrap; }
.mono { font-family: var(--mono); }

/* 进度条 */
.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-ok { background: #0E8420; }
.prog-fill.fill-err { background: #C7162B; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-gold { color: #92600a; background: #fef3c7; }
.pill-purple { color: #7c3aed; background: #ede9fe; }
.pill-pink { color: #be185d; background: #fce7f3; }
.pill-orange { color: #c2410c; background: #ffedd5; }
.pill-cyan { color: #0e7490; background: #cffafe; }
.pill-live { color: #fff; background: #C7162B; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }
.err-text { color: #b91c1c; }

/* 转码本地视频选择 */
.source-pick-row { display: flex; flex-wrap: wrap; gap: 6px; padding-top: 4px; }
.source-pick { font-size: 12px; }
.source-pick.is-selected { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }

/* 拉流转推流链路卡片 */
.relay-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(420px, 1fr)); gap: 14px; }
.relay-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 10px; }
.relay-head { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.relay-name { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); }
.relay-chain { display: flex; align-items: stretch; gap: 8px; }
.relay-node { flex: 1; display: flex; flex-direction: column; gap: 3px; padding: 8px 10px; background: var(--border-soft, #f3f4f6); border-radius: var(--radius-sm, 8px); }
.relay-node-label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.4px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.relay-node-name { font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); }
.relay-node-url { font-size: 11px; color: var(--text-muted, #5E5C5F); word-break: break-all; }
.relay-arrow { display: flex; align-items: center; font-size: 20px; color: var(--accent, #E95420); font-weight: 700; }
.relay-actions { display: flex; gap: 6px; padding-top: 4px; }

.btn {
  padding: 6px 14px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
  transition: background 0.15s ease;
}
.btn:hover:not(:disabled) { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
.btn-warning { color: #92400e; border-color: rgba(245, 158, 11, 0.45); background: #fffbeb; }
.btn-warning:hover:not(:disabled) { background: #fef3c7; }
.btn + .btn { margin-left: 6px; }
.pid-hint { display: block; font-size: 11px; margin-top: 2px; }

.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
}
.field-row { display: flex; gap: 12px; }
.check-row { display: flex; gap: 14px; flex-wrap: wrap; padding-top: 4px; }
.check-item { display: inline-flex; align-items: center; gap: 6px; font-size: 13px; cursor: pointer; }
.check-item input { width: 16px; height: 16px; cursor: pointer; }
.switch { display: inline-flex; align-items: center; gap: 6px; font-size: 13px; cursor: pointer; }
.switch input { width: 16px; height: 16px; cursor: pointer; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(560px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }
</style>
