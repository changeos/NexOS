<script setup lang="ts">
// =============================================================================
// Surveillance.vue —— 监控摄像头（RTSP/ONVIF 真实拉流 + 录像 + 回放 + 发现）
//
// 功能：
//   1. 顶部统计卡（摄像头总数/在线/录制中/占用）
//   2. 录像存储设置卡（recording_dir 可改 + 可写性 + 占用概览；只影响新录像）
//   3. 摄像头卡片网格：
//      - 实时画面（stream 在跑：`<video>` 播 HLS m3u8；否则最近快照/占位图 + 启动按钮）
//      - 状态徽章（online 绿 / offline 红 / recording 橙；后端经 /proc 自愈死 pid）
//      - 操作：探测（显示编码/分辨率）/ 启动实时 / 停止 / 开始录像 / 停止录像 /
//        快照 / 查看录像 / 删除
//   4. 添加对话框（顶部选项栏「自动扫描｜手动添加」二选一，切换不丢已填内容）：
//      - 自动扫描页签：「扫描网段」POST /scan 发现摄像头（端口签名推厂商）；
//        单选「填入表单」自动切到手动页签（预填 RTSP 模板）；多选统一账号密码
//        「批量添加」→ POST /cameras/batch 逐台反馈
//      - 手动添加页签：名称 + RTSP URL + 账号/密码 + 协议（账号密码提交前自动并入 URL）
//   5. 录像回放对话框（文件列表 + 下载/播放，含旧路径存量录像）
//
// 后端（16 条路由）：
//   GET/POST /api/v1/surveillance/cameras · POST /cameras/batch · DELETE /:id
//   POST /:id/probe|stream|stop-stream|record|stop-record|snapshot
//   GET /:id/recordings · GET /:id/snapshot · GET/POST /settings · POST /scan · GET /stats
//
// HLS 播放：Safari 原生支持 m3u8；Chrome/Firefox 经 hls.js（CDN 按需动态加载，
// 不新增 npm 依赖）。HLS 段经网关静态服务（`/hls/<id>/index.m3u8` → /tank/hls/<id>/）；
// 网关未提供该映射时 `<video>` 静默失败，不报错（与"不在线降级不 panic"一致）。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { endpoints } from '@/api/client';

interface Camera {
  id?: string;
  name?: string;
  url?: string;
  protocol?: string;
  enabled?: boolean;
  status?: string;
  recording?: boolean;
  record_pid?: number | null;
  stream_pid?: number | null;
  hls_dir?: string | null;
  created_at?: string;
  error?: string | null;
  [k: string]: unknown;
}

interface RecordingEntry {
  name: string;
  size_bytes: number;
  modified_at: string;
  path: string;
  date: string;
  [k: string]: unknown;
}

/** 扫描命中条目（POST /scan 返回 hits 元素）。 */
interface ScanHit {
  ip: string;
  ports: number[];
  vendor_guess: string;
  rtsp_template: string;
  added: boolean;
  [k: string]: unknown;
}

/** 扫描报告（POST /scan 响应）。 */
interface ScanReport {
  subnet: string;
  scanned: number;
  found: number;
  timed_out: boolean;
  hits: ScanHit[];
  [k: string]: unknown;
}

/** 全局设置视图（GET /settings 响应）。 */
interface SettingsInfo {
  recording_dir: string;
  default_recording_dir?: string;
  writable?: boolean;
  usage_bytes?: number;
  file_count?: number;
  legacy_dirs?: string[];
  note?: string;
  [k: string]: unknown;
}

// =============================================================================
// 列表 + 统计
// =============================================================================
const cameras = ref<Camera[]>([]);
const stats = ref<{ camera_count: number; online: number; recording: number; storage_used_bytes: number }>({
  camera_count: 0, online: 0, recording: 0, storage_used_bytes: 0,
});
const loading = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadCameras(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.surveillanceCameras();
    const next = Array.isArray(raw) ? (raw as Camera[]) : [];
    // 流被后端停掉（stream_pid 消失）时，清理本地 hls 实例
    const liveIds = new Set(next.filter(isStreaming).map((c) => String(c.id ?? '')));
    for (const id of hlsInstances.keys()) {
      if (!liveIds.has(id)) destroyHls(id);
    }
    cameras.value = next;
    // 未推流卡片展示最近快照（异步静默，失败不影响列表）
    void loadSnapshots();
  } catch (e) {
    cameras.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.surveillanceStats();
    stats.value = (raw ?? stats.value) as typeof stats.value;
  } catch {
    /* 统计非关键 */
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadCameras(), loadStats(), loadSettings()]);
}

// =============================================================================
// 录像存储设置
// =============================================================================
const settingsInfo = ref<SettingsInfo | null>(null);
const settingsDirInput = ref('');
const savingSettings = ref(false);

async function loadSettings(): Promise<void> {
  try {
    const raw = await endpoints.surveillanceSettings();
    settingsInfo.value = (raw ?? null) as SettingsInfo | null;
    if (settingsDirInput.value === '' && settingsInfo.value?.recording_dir) {
      settingsDirInput.value = settingsInfo.value.recording_dir;
    }
  } catch {
    /* 设置非关键（后端未就绪时静默） */
  }
}

async function saveSettings(): Promise<void> {
  const dir = settingsDirInput.value.trim();
  if (!dir) { msg.value = { kind: 'err', text: '请填写录像存储路径（绝对路径）' }; return; }
  savingSettings.value = true;
  msg.value = { kind: 'info', text: '保存中…' };
  try {
    await endpoints.updateSurveillanceSettings(dir);
    await loadSettings();
    await loadStats();
    msg.value = { kind: 'ok', text: '存储路径已更新（新录像生效；存量录像仍在旧路径，列表可见）' };
  } catch (e) {
    msg.value = { kind: 'err', text: '保存失败：' + friendlyError(e) };
  } finally {
    savingSettings.value = false;
  }
}

// =============================================================================
// 操作
// =============================================================================
const busyId = ref<string>('');

async function probe(id: string): Promise<void> {
  busyId.value = id;
  msg.value = { kind: 'info', text: '探测中…（最长约 8s）' };
  try {
    const raw = await endpoints.probeCamera(id);
    await refreshAll();
    // probe_detail：后端从 ffmpeg stderr 解析的编码/分辨率
    const d = (raw ?? {}) as { probe_detail?: { online?: boolean; codec?: string | null; resolution?: string | null } };
    const detail = d.probe_detail;
    if (detail?.online) {
      const parts = ['在线'];
      if (detail.codec) parts.push(String(detail.codec).toUpperCase());
      if (detail.resolution) parts.push(detail.resolution);
      msg.value = { kind: 'ok', text: parts.join(' · ') };
    } else {
      msg.value = { kind: 'err', text: '离线（RTSP 不可达或 ffmpeg 不可用）' };
    }
  } catch (e) {
    msg.value = { kind: 'err', text: '探测失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 快照（POST 抓帧 → data URL；卡片占位图直接展示最近快照）
// =============================================================================
const snapshots = ref<Record<string, string>>({});

async function takeSnapshot(c: Camera): Promise<void> {
  const id = String(c.id ?? '');
  busyId.value = id;
  msg.value = { kind: 'info', text: '抓取快照中…（最长约 8s）' };
  try {
    const raw = await endpoints.cameraSnapshot(id);
    const url = String((raw as { data_url?: string })?.data_url ?? '');
    if (url) snapshots.value = { ...snapshots.value, [id]: url };
    msg.value = { kind: 'ok', text: '快照已更新' };
  } catch (e) {
    msg.value = { kind: 'err', text: '快照失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

/** 拉取未推流摄像头的最近快照（404 静默；失败不影响列表）。 */
async function loadSnapshots(): Promise<void> {
  const ids = cameras.value.filter((c) => !isStreaming(c) && c.id).map((c) => String(c.id));
  await Promise.allSettled(
    ids.map(async (id) => {
      try {
        const raw = await endpoints.cameraSnapshotLatest(id);
        const url = String((raw as { data_url?: string })?.data_url ?? '');
        if (url) snapshots.value = { ...snapshots.value, [id]: url };
      } catch {
        /* 无快照（404）静默 */
      }
    }),
  );
}

async function startStream(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.startStream(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '实时流已启动' };
  } catch (e) {
    msg.value = { kind: 'err', text: '启动实时失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

async function stopStream(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    destroyHls(id);
    await endpoints.stopStream(id);
    await refreshAll();
  } catch (e) {
    msg.value = { kind: 'err', text: '停止实时失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

async function startRecord(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.startRecord(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '录像已开始' };
  } catch (e) {
    msg.value = { kind: 'err', text: '开始录像失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

async function stopRecord(id: string): Promise<void> {
  busyId.value = id;
  msg.value = null;
  try {
    await endpoints.stopRecord(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '录像已停止' };
  } catch (e) {
    msg.value = { kind: 'err', text: '停止录像失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

async function remove(id: string): Promise<void> {
  if (!window.confirm('确定删除该摄像头？（将停止其录像与实时拉流）')) return;
  busyId.value = id;
  msg.value = null;
  try {
    destroyHls(id);
    await endpoints.deleteCamera(id);
    await refreshAll();
    msg.value = { kind: 'ok', text: '已删除' };
  } catch (e) {
    msg.value = { kind: 'err', text: '删除失败：' + friendlyError(e) };
  } finally {
    busyId.value = '';
  }
}

// =============================================================================
// 录像回放对话框
// =============================================================================
const showRecordings = ref(false);
const recordingsFor = ref<string>('');
const recordingsName = ref<string>('');
const recordings = ref<RecordingEntry[]>([]);
const recordingsLoading = ref(false);
const playSrc = ref<string>('');

async function viewRecordings(c: Camera): Promise<void> {
  const id = String(c.id ?? '');
  recordingsFor.value = id;
  recordingsName.value = c.name ?? id;
  showRecordings.value = true;
  playSrc.value = '';
  recordingsLoading.value = true;
  recordings.value = [];
  try {
    const raw = await endpoints.cameraRecordings(id);
    recordings.value = Array.isArray(raw) ? (raw as RecordingEntry[]) : [];
  } catch (e) {
    recordings.value = [];
    msg.value = { kind: 'err', text: '加载录像列表失败：' + friendlyError(e) };
  } finally {
    recordingsLoading.value = false;
  }
}

function closeRecordings(): void {
  showRecordings.value = false;
  playSrc.value = '';
}

function playEntry(e: RecordingEntry): void {
  // 录像 mp4 经网关文件服务（`/api/v1/surveillance/rec-file/<id>/<date>/<name>` → 落盘文件）。
  // 网关未提供该映射时 `<video>` 静默失败，不报错。
  const id = encodeURIComponent(recordingsFor.value);
  const date = encodeURIComponent(e.date);
  const name = encodeURIComponent(e.name);
  playSrc.value = `/api/v1/surveillance/rec-file/${id}/${date}/${name}`;
}

function downloadUrl(e: RecordingEntry): string {
  const id = encodeURIComponent(recordingsFor.value);
  const date = encodeURIComponent(e.date);
  const name = encodeURIComponent(e.name);
  return `/api/v1/surveillance/rec-file/${id}/${date}/${name}`;
}

// =============================================================================
// 添加对话框
// =============================================================================
const showCreate = ref(false);
const createForm = ref({ name: '', url: '', username: '', password: '', protocol: 'rtsp' });
const createSubmitting = ref(false);

// —— 选项栏（沿用页级 Tab 惯例）：自动扫描 / 手动添加；两区状态独立，切换互不丢内容 ——
type CreateTab = 'scan' | 'manual';
const createTab = ref<CreateTab>('scan');
const createTabs: { key: CreateTab; label: string }[] = [
  { key: 'scan', label: '自动扫描' },
  { key: 'manual', label: '手动添加' },
];

function openCreate(): void {
  createForm.value = { name: '', url: '', username: '', password: '', protocol: 'rtsp' };
  createTab.value = 'scan'; // 默认落在「自动扫描」页签
  msg.value = null;
  showCreate.value = true;
}
function closeCreate(): void {
  if (createSubmitting.value) return;
  showCreate.value = false;
}
/**
 * 提交前把表单里的账号密码合并进流地址。规则（enc = encodeURIComponent）：
 *   1. 账号为空 → URL 原样返回（兼容已带凭证、或无凭证流的写法）
 *   2. URL 不含 '://' → 原样返回（非标准地址不碰）
 *   3. URL 含厂商模板的 user:pass 字面占位 →
 *        密码非空：替换为 enc(账号):enc(密码)
 *        密码为空：替换为 enc(账号)（得到 user@host 形态，去掉 ':pass'）
 *   4. URL 无 userinfo（authority 中无 '@'）→ 在 '://' 后插入
 *        enc(账号):enc(密码)@（密码为空则 enc(账号)@）
 *   5. URL 已带真实 userinfo 且无占位 → 原样返回（不覆盖用户手填的凭证）
 */
function mergeCredentials(url: string, username: string, password: string): string {
  if (!username || !url.includes('://')) return url;
  const cred = password
    ? `${encodeURIComponent(username)}:${encodeURIComponent(password)}`
    : encodeURIComponent(username);
  if (url.includes('user:pass')) {
    return url.replace('user:pass', cred);
  }
  // authority：'://' 之后到首个 '/'、'?'、'#' 之间；含 '@' 说明已有 userinfo
  const schemeEnd = url.indexOf('://') + 3;
  const rest = url.slice(schemeEnd);
  const authorityEnd = rest.search(/[/?#]/);
  const authority = authorityEnd === -1 ? rest : rest.slice(0, authorityEnd);
  if (authority.includes('@')) return url;
  return url.slice(0, schemeEnd) + cred + '@' + rest;
}

async function submitCreate(): Promise<void> {
  const name = createForm.value.name.trim();
  const url = createForm.value.url.trim();
  if (!name) { msg.value = { kind: 'err', text: '请填写名称' }; return; }
  if (!url) { msg.value = { kind: 'err', text: '请填写流地址' }; return; }
  createSubmitting.value = true;
  msg.value = { kind: 'info', text: '添加中…' };
  try {
    await endpoints.addCamera({
      name,
      url: mergeCredentials(url, createForm.value.username.trim(), createForm.value.password),
      protocol: createForm.value.protocol || undefined,
    });
    showCreate.value = false;
    await refreshAll();
    msg.value = { kind: 'ok', text: '已添加' };
  } catch (e) {
    msg.value = { kind: 'err', text: '添加失败：' + friendlyError(e) };
  } finally {
    createSubmitting.value = false;
  }
}

// =============================================================================
// 网段扫描（添加对话框内）：发现 → 单选预填表单 / 多选批量添加
// =============================================================================
const scanSubnet = ref('');
const scanning = ref(false);
const scanReport = ref<ScanReport | null>(null);
const scanSelected = ref<string[]>([]); // 勾选的 hit ip

async function doScan(): Promise<void> {
  scanning.value = true;
  scanReport.value = null;
  scanSelected.value = [];
  msg.value = { kind: 'info', text: '扫描网段中…（最长约 8s）' };
  try {
    const subnet = scanSubnet.value.trim();
    const raw = await endpoints.surveillanceScan(subnet ? { subnet } : {});
    scanReport.value = (raw ?? null) as ScanReport | null;
    const r = scanReport.value;
    if (!r) throw new Error('扫描响应为空');
    const tail = r.timed_out ? '（已达时间上限，返回部分结果）' : '';
    msg.value = {
      kind: r.found > 0 ? 'ok' : 'info',
      text: `扫描 ${r.subnet}（${r.scanned} 台主机）：发现 ${r.found} 个疑似摄像头${tail}`,
    };
  } catch (e) {
    msg.value = { kind: 'err', text: '扫描失败：' + friendlyError(e) };
  } finally {
    scanning.value = false;
  }
}

const selectedHits = computed<ScanHit[]>(() =>
  (scanReport.value?.hits ?? []).filter((h) => scanSelected.value.includes(h.ip)),
);

/** 恰好勾选 1 条 → 预填厂商 RTSP 模板进表单并自动切到「手动添加」页签（再填账号密码即可添加）。 */
function useSelectedInForm(): void {
  const hits = selectedHits.value;
  if (hits.length !== 1) {
    msg.value = { kind: 'err', text: '请先勾选恰好一条扫描结果' };
    return;
  }
  const h = hits[0];
  createForm.value.url = h.rtsp_template;
  createForm.value.protocol = 'rtsp';
  if (!createForm.value.name.trim()) {
    createForm.value.name = `${vendorLabel(h.vendor_guess)} ${h.ip}`;
  }
  createTab.value = 'manual';
  msg.value = { kind: 'ok', text: '已预填厂商 RTSP 模板——填好账号密码即可添加' };
}

// —— 批量添加：多选 + 统一账号密码（模板 user:pass 占位由后端替换）——
const batchUser = ref('');
const batchPass = ref('');
const batchPrefix = ref('');
const batchSubmitting = ref(false);

async function submitBatch(): Promise<void> {
  const hits = selectedHits.value.filter((h) => !h.added);
  if (!hits.length) { msg.value = { kind: 'err', text: '请先勾选要添加的扫描结果' }; return; }
  batchSubmitting.value = true;
  msg.value = { kind: 'info', text: `批量添加 ${hits.length} 台中…` };
  try {
    const raw = await endpoints.addCamerasBatch({
      items: hits.map((h) => ({ ip: h.ip, rtsp_url: h.rtsp_template, vendor: h.vendor_guess })),
      username: batchUser.value.trim() || undefined,
      password: batchPass.value || undefined,
      name_prefix: batchPrefix.value.trim() || undefined,
    });
    const r = (raw ?? {}) as { created?: number; failed?: number; results?: { ok?: boolean; name?: string; error?: string | null }[] };
    const created = Number(r.created ?? 0);
    const failed = Number(r.failed ?? 0);
    const errs = (r.results ?? []).filter((x) => !x.ok).map((x) => `${x.name}: ${x.error ?? '失败'}`);
    msg.value = {
      kind: failed > 0 ? 'info' : 'ok',
      text: `批量添加完成：成功 ${created} / 失败 ${failed}` + (errs.length ? `（${errs.slice(0, 3).join('；')}）` : ''),
    };
    if (created > 0) {
      scanSelected.value = [];
      await refreshAll();
    }
  } catch (e) {
    msg.value = { kind: 'err', text: '批量添加失败：' + friendlyError(e) };
  } finally {
    batchSubmitting.value = false;
  }
}

function vendorLabel(v: string): string {
  switch (v) {
    case 'hikvision': return '海康';
    case 'dahua': return '大华';
    case 'onvif': return 'ONVIF';
    case 'generic': return '通用';
    default: return v || '未知';
  }
}

// =============================================================================
// HLS 播放（hls.js 按需从 CDN 动态加载；Safari 走原生）
// =============================================================================
const hlsInstances = new Map<string, unknown>();
let hlsJsPromise: Promise<unknown> | null = null;

function loadHlsJs(): Promise<unknown> {
  if (hlsJsPromise) return hlsJsPromise;
  hlsJsPromise = new Promise((resolve, reject) => {
    const w = window as unknown as { Hls?: unknown };
    if (w.Hls) { resolve(w.Hls); return; }
    const s = document.createElement('script');
    s.src = 'https://cdn.jsdelivr.net/npm/hls.js@1/dist/hls.min.js';
    s.async = true;
    s.onload = () => resolve(w.Hls);
    s.onerror = () => reject(new Error('hls.js 加载失败'));
    document.head.appendChild(s);
  });
  return hlsJsPromise;
}

/** 稳定的 function ref：仅在 `<video>` 真正挂载/卸载时触发（避免每次重渲染重挂）。 */
function videoRef(el: unknown): void {
  if (!el || typeof el !== 'object') return;
  const video = el as HTMLVideoElement;
  const id = video.dataset?.camId ?? '';
  if (!id) return;
  void attachHls(id, video);
}

async function attachHls(id: string, video: HTMLVideoElement): Promise<void> {
  const src = hlsUrl(id);
  destroyHls(id);
  if (video.canPlayType('application/vnd.apple.mpegurl')) {
    video.src = src; // Safari 原生 HLS
    return;
  }
  try {
    const Hls = (await loadHlsJs()) as { new (): { loadSource(s: string): void; attachMedia(v: HTMLVideoElement): void; destroy(): void }; isSupported?: () => boolean };
    const hls = new Hls();
    hls.loadSource(src);
    hls.attachMedia(video);
    hlsInstances.set(id, hls);
  } catch {
    // 降级：直配 src，浏览器不支持则静默
    video.src = src;
  }
}

function destroyHls(id: string): void {
  const h = hlsInstances.get(id) as { destroy?: () => void } | undefined;
  if (h) {
    try { h.destroy?.(); } catch { /* ignore */ }
    hlsInstances.delete(id);
  }
}

onBeforeUnmount(() => {
  for (const id of [...hlsInstances.keys()]) destroyHls(id);
});

// =============================================================================
// 工具
// =============================================================================
function isStreaming(c: Camera): boolean {
  return c.stream_pid != null;
}

function hlsUrl(idOrCam: string | Camera): string {
  const id = typeof idOrCam === 'string' ? idOrCam : String(idOrCam.id ?? '');
  return `/hls/${encodeURIComponent(id)}/index.m3u8`;
}

function statusClass(s?: string): string {
  switch (s) {
    case 'online': return 'pill-ok';
    case 'recording': return 'pill-warn';
    case 'offline': return 'pill-err';
    default: return 'pill-muted';
  }
}
function statusLabel(s?: string): string {
  switch (s) {
    case 'online': return '在线';
    case 'recording': return '录制中';
    case 'offline': return '离线';
    default: return s ?? '—';
  }
}
function protocolLabel(p?: string): string {
  return p && p.length ? p.toUpperCase() : 'RTSP';
}
function formatBytes(bytes: number): string {
  if (!bytes || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该摄像头接口';
  }
  return m;
}

const storageText = computed(() => formatBytes(stats.value.storage_used_bytes));

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="surveillance-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">监控摄像头</h2>
        <div class="page-sub muted">RTSP/ONVIF 真实拉流 · 实时画面 · 计划录像 · 回放</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          刷新
        </button>
        <button class="btn btn-small btn-primary" @click="openCreate">＋ 添加摄像头</button>
      </div>
    </div>

    <!-- 统计卡 -->
    <section class="stat-grid">
      <div class="card stat-card">
        <div class="stat-label">摄像头</div>
        <div class="stat-value">{{ stats.camera_count }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">在线</div>
        <div class="stat-value stat-ok">{{ stats.online }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">录制中</div>
        <div class="stat-value stat-warn">{{ stats.recording }}</div>
      </div>
      <div class="card stat-card">
        <div class="stat-label">录制占用（估）</div>
        <div class="stat-value">{{ storageText }}</div>
      </div>
    </section>

    <!-- 录像存储设置卡 -->
    <section v-if="settingsInfo" class="card settings-card">
      <div class="settings-row">
        <div class="settings-main">
          <div class="stat-label">录像存储路径（新录像生效，存量录像留在原路径仍可查看）</div>
          <input
            v-model="settingsDirInput"
            class="settings-input mono"
            type="text"
            placeholder="/tank/recordings"
            :disabled="savingSettings"
          />
        </div>
        <button
          class="btn btn-small btn-primary settings-save"
          :disabled="savingSettings || settingsDirInput.trim() === (settingsInfo.recording_dir || '')"
          @click="saveSettings"
        >
          {{ savingSettings ? '保存中…' : '保存路径' }}
        </button>
        <div class="settings-usage muted">
          已用 {{ formatBytes(Number(settingsInfo.usage_bytes) || 0) }} · {{ settingsInfo.file_count ?? 0 }} 个文件
          <span v-if="settingsInfo.writable === false" class="pill pill-err">不可写</span>
        </div>
      </div>
    </section>

    <div v-if="error" class="error-box">{{ error }}</div>
    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- 摄像头卡片网格 -->
    <section class="cam-grid">
      <div v-if="loading && !cameras.length" class="card empty-card">加载中…</div>
      <div v-else-if="!cameras.length" class="card empty-card">
        暂无摄像头，点击右上角「添加摄像头」。
      </div>

      <div v-for="c in cameras" :key="c.id" class="card cam-card">
        <div class="cam-card-head">
          <div class="cam-name" :title="c.name ?? ''">{{ c.name ?? '—' }}
            <span class="proto-tag">{{ protocolLabel(c.protocol) }}</span>
          </div>
          <span class="pill" :class="statusClass(c.status)">{{ statusLabel(c.status) }}</span>
        </div>

        <!-- 实时画面 -->
        <div class="cam-video-wrap">
          <video
            v-if="isStreaming(c)"
            :ref="videoRef"
            :data-cam-id="c.id"
            class="cam-video"
            controls
            autoplay
            muted
            playsinline
          ></video>
          <div v-else class="cam-video-placeholder">
            <img
              v-if="snapshots[String(c.id ?? '')]"
              :src="snapshots[String(c.id ?? '')]"
              class="cam-snap"
              alt="最近快照"
            />
            <span v-else class="placeholder-icon" aria-hidden="true">📹</span>
            <span class="placeholder-text">{{ snapshots[String(c.id ?? '')] ? '最近快照' : '实时未启动' }}</span>
            <button class="btn btn-small btn-primary" :disabled="busyId === c.id" @click="startStream(String(c.id ?? ''))">
              启动实时
            </button>
          </div>
        </div>

        <div class="cam-url mono" :title="c.url ?? ''">{{ c.url ?? '—' }}</div>
        <div v-if="c.error" class="cam-error" :title="c.error">⚠ {{ c.error }}</div>

        <div class="cam-actions">
          <button class="btn btn-small" :disabled="busyId === c.id" @click="probe(String(c.id ?? ''))">探测</button>
          <button class="btn btn-small" :disabled="busyId === c.id" @click="takeSnapshot(c)">快照</button>
          <button v-if="!isStreaming(c)" class="btn btn-small btn-primary" :disabled="busyId === c.id" @click="startStream(String(c.id ?? ''))">启动实时</button>
          <button v-else class="btn btn-small" :disabled="busyId === c.id" @click="stopStream(String(c.id ?? ''))">停止实时</button>
          <button v-if="!c.recording" class="btn btn-small btn-record" :disabled="busyId === c.id" @click="startRecord(String(c.id ?? ''))">开始录像</button>
          <button v-else class="btn btn-small btn-record-on" :disabled="busyId === c.id" @click="stopRecord(String(c.id ?? ''))">停止录像</button>
          <button class="btn btn-small" :disabled="busyId === c.id" @click="viewRecordings(c)">查看录像</button>
          <button class="btn btn-small btn-danger" :disabled="busyId === c.id" @click="remove(String(c.id ?? ''))">删除</button>
        </div>
      </div>
    </section>

    <!-- ============ 添加摄像头对话框 ============ -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-cam-title">
        <div class="modal-head">
          <h3 id="create-cam-title">添加摄像头</h3>
          <button class="modal-close" type="button" :disabled="createSubmitting" @click="closeCreate">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreate">
          <!-- —— 选项栏：自动扫描 / 手动添加 —— -->
          <nav class="tabs" role="tablist">
            <button
              v-for="t in createTabs"
              :key="t.key"
              class="tab"
              :class="{ active: createTab === t.key }"
              type="button"
              role="tab"
              :aria-selected="createTab === t.key"
              @click="createTab = t.key"
            >{{ t.label }}</button>
          </nav>

          <!-- —— 页签 1：自动扫描（扫描 + 结果 + 单选填表/多选批量） —— -->
          <section v-show="createTab === 'scan'" class="create-tab">
            <div class="scan-box">
              <div class="scan-title">扫描网段发现摄像头</div>
              <div class="scan-controls">
                <input
                  v-model="scanSubnet"
                  type="text"
                  class="scan-input mono"
                  placeholder="留空自动取本机网段（如 192.0.2.0/24）"
                  :disabled="scanning || createSubmitting"
                  @keyup.enter="doScan"
                />
                <button type="button" class="btn btn-small" :disabled="scanning" @click="doScan">
                  <span class="spin" :class="{ spinning: scanning }" aria-hidden="true">↻</span>
                  {{ scanning ? '扫描中…（≤8s）' : '扫描网段' }}
                </button>
              </div>
              <template v-if="scanReport">
                <div class="scan-meta muted">
                  {{ scanReport.subnet }} · 扫描 {{ scanReport.scanned }} 台 · 发现 {{ scanReport.found }} 个疑似摄像头
                  <span v-if="scanReport.timed_out" class="pill pill-warn">超时截断</span>
                </div>
                <div v-if="!scanReport.hits.length" class="empty-line">
                  未发现疑似摄像头（可换网段重试，或切到「手动添加」页签）
                </div>
                <ul v-else class="scan-list">
                  <li v-for="h in scanReport.hits" :key="h.ip" class="scan-item">
                    <label class="scan-label" :class="{ 'is-added': h.added }">
                      <input type="checkbox" v-model="scanSelected" :value="h.ip" :disabled="h.added" />
                      <span class="mono scan-ip">{{ h.ip }}</span>
                      <span class="proto-tag">{{ vendorLabel(h.vendor_guess) }}</span>
                      <span class="muted scan-ports mono">{{ h.ports.join('/') }}</span>
                      <span v-if="h.added" class="pill pill-muted">已添加</span>
                    </label>
                  </li>
                </ul>
                <div v-if="scanReport.hits.some((h) => !h.added)" class="scan-actions">
                  <button
                    type="button"
                    class="btn btn-small"
                    :disabled="scanSelected.length !== 1"
                    @click="useSelectedInForm"
                  >填入表单（单选）</button>
                  <span class="muted scan-tip">勾选 {{ selectedHits.length }} 台，统一账号密码批量添加：</span>
                  <input v-model="batchUser" type="text" class="scan-input cred" placeholder="账号" :disabled="batchSubmitting" />
                  <input v-model="batchPass" type="password" class="scan-input cred" placeholder="密码" :disabled="batchSubmitting" />
                  <input v-model="batchPrefix" type="text" class="scan-input cred" placeholder="名称前缀" :disabled="batchSubmitting" />
                  <button
                    type="button"
                    class="btn btn-small btn-primary"
                    :disabled="batchSubmitting || selectedHits.length === 0"
                    @click="submitBatch"
                  >{{ batchSubmitting ? '添加中…' : `批量添加(${selectedHits.length})` }}</button>
                </div>
              </template>
            </div>
          </section>

          <!-- —— 页签 2：手动添加（名称/流地址/账号/密码/协议） —— -->
          <section v-show="createTab === 'manual'" class="create-tab">
            <div class="field">
              <label for="cam-name">名称</label>
              <input id="cam-name" v-model="createForm.name" type="text" placeholder="前门" :disabled="createSubmitting" />
            </div>
            <div class="field">
              <label for="cam-url">流地址（RTSP / ONVIF）</label>
              <input
                id="cam-url"
                v-model="createForm.url"
                type="text"
                placeholder="rtsp://user:pass@192.168.1.50:554/stream1"
                :disabled="createSubmitting"
              />
            </div>
            <div class="field-row">
              <div class="field">
                <label for="cam-user">账号（可留空）</label>
                <input
                  id="cam-user"
                  v-model="createForm.username"
                  type="text"
                  placeholder="如 admin"
                  autocomplete="off"
                  :disabled="createSubmitting"
                />
              </div>
              <div class="field">
                <label for="cam-pass">密码（可留空）</label>
                <input
                  id="cam-pass"
                  v-model="createForm.password"
                  type="password"
                  placeholder="如 123456"
                  autocomplete="new-password"
                  :disabled="createSubmitting"
                />
              </div>
            </div>
            <div class="field-hint muted">账号密码会自动填入流地址；留空则按流地址原样提交</div>
            <div class="field">
              <label for="cam-proto">协议</label>
              <select id="cam-proto" v-model="createForm.protocol" :disabled="createSubmitting">
                <option value="rtsp">RTSP</option>
                <option value="onvif">ONVIF</option>
              </select>
            </div>
          </section>
          <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createSubmitting" @click="closeCreate">取消</button>
            <button v-if="createTab === 'manual'" type="submit" class="btn btn-primary" :disabled="createSubmitting">
              {{ createSubmitting ? '添加中…' : '添加' }}
            </button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 录像回放对话框 ============ -->
    <div v-if="showRecordings" class="modal-backdrop" @click.self="closeRecordings">
      <div class="modal modal-wide" role="dialog" aria-modal="true" aria-labelledby="rec-title">
        <div class="modal-head">
          <h3 id="rec-title">录像回放 · {{ recordingsName }}</h3>
          <button class="modal-close" type="button" @click="closeRecordings">×</button>
        </div>
        <div class="modal-body">
          <div v-if="recordingsLoading" class="empty-line">加载中…</div>
          <div v-else-if="!recordings.length" class="empty-line">暂无录像文件</div>
          <ul v-else class="rec-list">
            <li v-for="(e, idx) in recordings" :key="idx" class="rec-item">
              <div class="rec-main">
                <div class="rec-name mono">{{ e.name }}</div>
                <div class="rec-meta muted">
                  <span>{{ e.date }}</span>
                  <span class="sep">·</span>
                  <span>{{ formatBytes(Number(e.size_bytes) || 0) }}</span>
                  <span class="sep">·</span>
                  <span>{{ e.modified_at || '—' }}</span>
                </div>
              </div>
              <div class="rec-actions">
                <button class="btn btn-small btn-primary" @click="playEntry(e)">播放</button>
                <a class="btn btn-small" :href="downloadUrl(e)" :download="e.name">下载</a>
              </div>
            </li>
          </ul>

          <div v-if="playSrc" class="rec-player">
            <video class="cam-video" :src="playSrc" controls autoplay playsinline></video>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.surveillance-page {
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

.stat-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 14px; }
.stat-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 6px; }
.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.stat-ok { color: #15803d; }
.stat-warn { color: #b45309; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; }
.empty-line { padding: 16px 4px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 13px; }

.cam-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 14px; }
.cam-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.cam-card-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.cam-name { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); display: flex; align-items: center; gap: 8px; min-width: 0; }
.cam-name { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.proto-tag { font-size: 10px; font-weight: 700; letter-spacing: 0.5px; color: var(--accent, #E95420); border: 1px solid var(--border, #D9D9D9); padding: 1px 6px; border-radius: 6px; }

.cam-video-wrap { position: relative; width: 100%; aspect-ratio: 16 / 9; background: #111; border-radius: var(--radius-sm, 8px); overflow: hidden; }
.cam-video { width: 100%; height: 100%; object-fit: cover; display: block; background: #000; }
.cam-video-placeholder { position: absolute; inset: 0; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 8px; color: #c7c7cc; background: linear-gradient(135deg, #1f1f22 0%, #2b2b30 100%); }
.placeholder-icon { font-size: 30px; }
.placeholder-text { font-size: 13px; }

.cam-url { font-size: 12px; color: var(--text-muted, #5E5C5F); word-break: break-all; }
.cam-error { font-size: 12px; color: #b91c1c; word-break: break-all; }
.cam-actions { display: flex; flex-wrap: wrap; gap: 6px; padding-top: 4px; border-top: 1px solid var(--border-soft, #EDEDED); margin-top: 2px; }

.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; white-space: nowrap; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-warn { color: #b45309; background: #fef3c7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-err { color: #b91c1c; background: #fee2e2; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }

.btn {
  padding: 6px 14px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
  transition: background 0.15s ease; text-decoration: none; display: inline-block;
}
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #c9480f); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
.btn-record { color: #b45309; border-color: rgba(180, 83, 9, 0.35); background: #fffbeb; }
.btn-record:hover:not(:disabled) { background: #fef3c7; }
.btn-record-on { color: #fff; background: #b45309; border-color: #b45309; }
.btn-record-on:hover:not(:disabled) { background: #92400e; }

.field { display: flex; flex-direction: column; gap: 4px; flex: 1; }
.field label { font-size: 13px; font-weight: 500; }
.field input, .field select { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff); color: var(--text, #2B2B2B); }
.field-row { display: flex; gap: 12px; }
.field-hint { font-size: 12px; margin-top: -8px; }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

.modal-backdrop { position: fixed; inset: 0; background: rgba(0, 0, 0, 0.35); backdrop-filter: blur(2px); display: flex; align-items: center; justify-content: center; z-index: 100; padding: 16px; }
.modal { width: min(520px, 100%); max-height: 90vh; overflow: auto; background: var(--bg-card, #fff); border-radius: var(--radius, 16px); box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25); display: flex; flex-direction: column; }
.modal-wide { width: min(720px, 100%); }
.modal-head { display: flex; align-items: center; justify-content: space-between; padding: 16px 20px; border-bottom: 1px solid var(--border-soft, #EDEDED); }
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close { background: transparent; border: none; font-size: 24px; line-height: 1; color: var(--text-muted, #5E5C5F); cursor: pointer; padding: 0 6px; }
.modal-body { padding: 18px 20px; display: flex; flex-direction: column; gap: 14px; }

.rec-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 8px; }
.rec-item { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 12px; border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }
.rec-main { min-width: 0; flex: 1; }
.rec-name { font-size: 13px; font-weight: 600; color: var(--text, #2B2B2B); word-break: break-all; }
.rec-meta { font-size: 12px; display: flex; flex-wrap: wrap; gap: 6px; margin-top: 2px; }
.rec-meta .sep { color: var(--text-faint, #c7c7cc); }
.rec-actions { display: flex; gap: 6px; flex-shrink: 0; }
.rec-player { margin-top: 4px; }
.rec-player .cam-video { aspect-ratio: 16 / 9; border-radius: var(--radius-sm, 8px); }

/* —— 录像存储设置卡 —— */
.settings-card { padding: 12px 16px; }
.settings-row { display: flex; align-items: flex-end; gap: 12px; flex-wrap: wrap; }
.settings-main { flex: 1; min-width: 240px; display: flex; flex-direction: column; gap: 4px; }
.settings-input { width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-size: 13px; background: var(--bg-card, #fff); color: var(--text, #2B2B2B); }
.settings-save { flex-shrink: 0; }
.settings-usage { font-size: 12px; display: flex; align-items: center; gap: 8px; padding-bottom: 6px; }

/* —— 快照占位图 —— */
.cam-snap { max-width: 100%; max-height: 55%; object-fit: contain; border-radius: var(--radius-sm, 8px); display: block; }

/* —— 添加对话框：网段扫描 —— */
.scan-box { display: flex; flex-direction: column; gap: 8px; padding: 10px 12px; border: 1px dashed var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); }
.scan-title { font-size: 13px; font-weight: 600; }
.scan-controls { display: flex; gap: 8px; align-items: center; }
.scan-input { flex: 1; min-width: 0; padding: 6px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-size: 13px; font-family: inherit; background: var(--bg-card, #fff); color: var(--text, #2B2B2B); }
.scan-input.cred { flex: 0 0 auto; width: 96px; }
.scan-meta { font-size: 12px; display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.scan-list { list-style: none; margin: 0; padding: 0; max-height: 180px; overflow: auto; display: flex; flex-direction: column; gap: 4px; }
.scan-item { border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }
.scan-label { display: flex; align-items: center; gap: 8px; padding: 6px 10px; cursor: pointer; font-size: 13px; }
.scan-label.is-added { opacity: 0.55; cursor: not-allowed; }
.scan-ip { font-weight: 600; min-width: 104px; }
.scan-ports { font-size: 11px; }
.scan-actions { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; padding-top: 4px; border-top: 1px solid var(--border-soft, #EDEDED); }
.scan-tip { font-size: 12px; }

/* —— 添加对话框：选项栏（沿用 Provisioning/LlmModels 的 Tab 惯例） —— */
.tabs { display: flex; gap: 4px; border-bottom: 1px solid var(--border-soft, #EDEDED); flex-wrap: wrap; }
.tab {
  padding: 8px 16px; background: transparent; border: none; border-bottom: 2px solid transparent;
  font-size: 14px; font-weight: 500; color: var(--text-muted, #5E5C5F); cursor: pointer;
  font-family: inherit; transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover { color: var(--text, #2B2B2B); }
.tab.active { color: var(--accent, #E95420); border-bottom-color: var(--accent, #E95420); }
.create-tab { display: flex; flex-direction: column; gap: 14px; }

.mono { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
</style>
