<script setup lang="ts">
// =============================================================================
// QrTransfer.vue —— 二维码文件传输（文件 → 跳动 QR 视频 + 解码回文件）
//
// 2 Tab：
//   - 编码：选文件路径 + FPS/chunk_size → 生成"跳动 QR 视频" → 内嵌 <video> 播放 + 下载
//   - 解码：拖拽/点击上传 QR 视频/图片 → 解码 → 显示进度（decoded/total）→ 下载文件
//
// 后端：/api/v1/qr/* （QrTransferRouteHandler）
//   POST /qr/encode / GET /qr/encode/:id / GET /qr/encode/:id/video
//   POST /qr/decode / GET /qr/decode/:id / GET /qr/decode/:id/file
//   GET  /qr/stats
//
// 设计：Ubuntu Yaru 风格 .card / .page-head / Tabs / 表单 / 状态徽章。
//       降级：Python/ffmpeg/pyzbar 不存在时后端任务 status=failed，前端展示 error 不崩溃。
// =============================================================================
// 应用包（apps/qrtransfer）：主前端内部模块依赖已解耦——
//   - @/api/client → 本包 api.ts（宿主桥 __NEXOS_HOST__.api 原语 + 同名 endpoints）
//   - @/utils/clipboard → 本包 clipboard.ts（原样迁入）
//   - useI18n 键名 qr.*（本包 i18n/，entry.ts addI18n 注入）
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints, ApiError } from './api';
import { copyText } from './clipboard';

const { t } = useI18n();

// =============================================================================
// 独立运行外链（右上角图标；仅桌面嵌入模式显示，apps/film 0.1.2 同款范式）
// =============================================================================

/** 独立模式标记（standalone/standalone-host.ts 置位）——该模式下不显示外链。 */
const isStandalone = Boolean(
  (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__,
);

/** 在新浏览器标签页打开独立全页版本（脱离 NexOS 桌面壳，宿主桥自给自足）。 */
function openStandalone(): void {
  window.open('/apps-assets/qrtransfer/standalone.html', '_blank', 'noopener');
}

// =============================================================================
// 数据模型
// =============================================================================
interface EncodeTask {
  id?: string;
  file_path?: string;
  status?: string;
  total_frames?: number;
  file_size?: number;
  fps?: number;
  chunk_size?: number;
  video_path?: string | null;
  video_url?: string | null;
  error?: string | null;
  created_at?: string;
}
interface DecodeTask {
  id?: string;
  status?: string;
  source?: string;
  total_frames?: number;
  decoded_frames?: number;
  crc_ok?: boolean;
  output_path?: string | null;
  output_url?: string | null;
  error?: string | null;
  created_at?: string;
}
interface QrStats {
  encode_total?: number;
  encode_completed?: number;
  encode_failed?: number;
  decode_total?: number;
  decode_completed?: number;
  decode_failed?: number;
}
interface TextEncodeResult {
  qr_count: number;
  qr_images: string[];
  original_size: number;
  compressed_size: number;
}
interface TextDecodeResult {
  text?: string;
  char_count?: number;
  partial?: boolean;
  seq?: number;
  total?: number;
}

// =============================================================================
// Tab 状态
// =============================================================================
type TabKey = 'encode' | 'decode' | 'text';
const activeTab = ref<TabKey>('encode');
const tabs: { key: TabKey; label: string }[] = [
  { key: 'encode', label: '编码（文件→视频）' },
  { key: 'decode', label: '解码（视频→文件）' },
  { key: 'text', label: '文本' },
];

// =============================================================================
// 统计
// =============================================================================
const stats = ref<QrStats>({});
async function loadStats() {
  try {
    stats.value = (await endpoints.qrStats()) as QrStats;
  } catch (e) {
    // 静默：统计非关键
    console.warn('qrStats failed', e);
  }
}

// =============================================================================
// 编码
// =============================================================================
const encodeForm = ref({
  file_path: '',
  fps: 5,
  chunk_size: 2048,
});
const encodeTask = ref<EncodeTask | null>(null);
const encodeBusy = ref(false);
const encodeError = ref('');
const encodeVideoMeta = ref<{ size?: number; content_type?: string } | null>(null);
let encodePollTimer: ReturnType<typeof setInterval> | null = null;

const encodeStatusText = computed(() => {
  const s = encodeTask.value?.status;
  if (!s) return '空闲';
  return { pending: '排队中', encoding: '编码中…', completed: '已完成', failed: '失败' }[s] || s;
});
const encodeStatusClass = computed(() => {
  const s = encodeTask.value?.status;
  return s === 'completed' ? 'pill-ok' : s === 'failed' ? 'pill-err' : 'pill-blue';
});
const encodeSizeHuman = computed(() => humanBytes(encodeTask.value?.file_size));

async function startEncode() {
  encodeError.value = '';
  if (!encodeForm.value.file_path.trim()) {
    encodeError.value = '请输入待编码的文件绝对路径';
    return;
  }
  encodeBusy.value = true;
  encodeVideoMeta.value = null;
  try {
    const t = (await endpoints.qrEncode({
      file_path: encodeForm.value.file_path.trim(),
      fps: encodeForm.value.fps,
      chunk_size: encodeForm.value.chunk_size,
    })) as EncodeTask;
    encodeTask.value = t;
    if (t.id) startPollEncode(t.id);
  } catch (e) {
    encodeError.value = errMsg(e);
  } finally {
    encodeBusy.value = false;
  }
}

function startPollEncode(id: string) {
  stopPollEncode();
  encodePollTimer = setInterval(async () => {
    try {
      const t = (await endpoints.qrEncodeStatus(id)) as EncodeTask;
      encodeTask.value = t;
      if (t.status === 'completed' || t.status === 'failed') {
        stopPollEncode();
        if (t.status === 'completed') loadEncodeVideo(id);
        loadStats();
      }
    } catch (e) {
      console.warn('qrEncodeStatus poll failed', e);
    }
  }, 1500);
}
function stopPollEncode() {
  if (encodePollTimer) {
    clearInterval(encodePollTimer);
    encodePollTimer = null;
  }
}

async function loadEncodeVideo(id: string) {
  try {
    const meta = (await endpoints.qrEncodeVideo(id)) as {
      size?: number;
      content_type?: string;
    };
    encodeVideoMeta.value = meta;
  } catch (e) {
    console.warn('qrEncodeVideo meta failed', e);
  }
}

// 选择文件列表中的文件（从 /api/v1/files/list 取，简化为弹窗输入路径）
const filePickerOpen = ref(false);
const filePickerPath = ref('/');
const filePickerEntries = ref<{ name: string; is_dir: boolean; size: number }[]>([]);
const filePickerBusy = ref(false);
const filePickerError = ref('');

async function openFilePicker() {
  filePickerOpen.value = true;
  filePickerError.value = '';
  await browseFiles(filePickerPath.value);
}
async function browseFiles(dir: string) {
  filePickerBusy.value = true;
  filePickerError.value = '';
  try {
    const clean = dir.trim() || '/';
    const res = (await endpoints.filesList(clean)) as {
      name: string;
      is_dir: boolean;
      size_bytes: number;
    }[];
    const arr = Array.isArray(res) ? res : [];
    filePickerEntries.value = arr.map((e) => ({
      name: e.name,
      is_dir: !!e.is_dir,
      size: e.size_bytes || 0,
    }));
    filePickerPath.value = clean;
  } catch (e) {
    filePickerError.value = errMsg(e);
    filePickerEntries.value = [];
  } finally {
    filePickerBusy.value = false;
  }
}
function pickerEnter(entry: { name: string; is_dir: boolean }) {
  if (entry.is_dir) {
    const base = filePickerPath.value.replace(/\/$/, '');
    browseFiles(`${base}/${entry.name}`);
  } else {
    const base = filePickerPath.value.replace(/\/$/, '');
    encodeForm.value.file_path = `${base}/${entry.name}`;
    filePickerOpen.value = false;
  }
}
function pickerUp() {
  const base = filePickerPath.value.replace(/\/$/, '');
  const idx = base.lastIndexOf('/');
  if (idx <= 0) browseFiles('/');
  else browseFiles(base.slice(0, idx));
}

// =============================================================================
// 解码
// =============================================================================
const decodeDropping = ref(false);
const decodeFile = ref<File | null>(null);
const decodeTask = ref<DecodeTask | null>(null);
const decodeBusy = ref(false);
const decodeError = ref('');
const decodeOutputMeta = ref<{ size?: number } | null>(null);
let decodePollTimer: ReturnType<typeof setInterval> | null = null;

const decodeStatusText = computed(() => {
  const s = decodeTask.value?.status;
  if (!s) return '空闲';
  return (
    { pending: '排队中', decoding: '解码中…', completed: '已完成', failed: '失败' }[s] || s
  );
});
const decodeStatusClass = computed(() => {
  const s = decodeTask.value?.status;
  return s === 'completed' ? 'pill-ok' : s === 'failed' ? 'pill-err' : 'pill-blue';
});

function onDecodeDrop(ev: DragEvent) {
  ev.preventDefault();
  decodeDropping.value = false;
  const f = ev.dataTransfer?.files?.[0];
  if (f) setDecodeFile(f);
}
function onDecodeChange(ev: Event) {
  const input = ev.target as HTMLInputElement;
  const f = input.files?.[0];
  if (f) setDecodeFile(f);
}
function setDecodeFile(f: File) {
  decodeFile.value = f;
  decodeError.value = '';
  decodeTask.value = null;
  decodeOutputMeta.value = null;
}

async function startDecode() {
  decodeError.value = '';
  if (!decodeFile.value) {
    decodeError.value = '请先上传 QR 视频/图片';
    return;
  }
  decodeBusy.value = true;
  try {
    const b64 = await fileToBase64(decodeFile.value);
    const t = (await endpoints.qrDecode({
      media_base64: b64,
      filename: decodeFile.value.name,
    })) as DecodeTask;
    decodeTask.value = t;
    if (t.id) startPollDecode(t.id);
  } catch (e) {
    decodeError.value = errMsg(e);
  } finally {
    decodeBusy.value = false;
  }
}

function startPollDecode(id: string) {
  stopPollDecode();
  decodePollTimer = setInterval(async () => {
    try {
      const t = (await endpoints.qrDecodeStatus(id)) as DecodeTask;
      decodeTask.value = t;
      if (t.status === 'completed' || t.status === 'failed') {
        stopPollDecode();
        if (t.status === 'completed') loadDecodeOutput(id);
        loadStats();
      }
    } catch (e) {
      console.warn('qrDecodeStatus poll failed', e);
    }
  }, 1500);
}
function stopPollDecode() {
  if (decodePollTimer) {
    clearInterval(decodePollTimer);
    decodePollTimer = null;
  }
}
async function loadDecodeOutput(id: string) {
  try {
    const meta = (await endpoints.qrDecodeFile(id)) as { size?: number };
    decodeOutputMeta.value = meta;
  } catch (e) {
    console.warn('qrDecodeFile meta failed', e);
  }
}

// =============================================================================
// 文本编解码（Tab3）：文本 ⇄ QR 图片（即时）
// =============================================================================
const TEXT_MAX_BYTES = 50000;
const textInput = ref('');
const textErrorLevel = ref<'L' | 'M' | 'Q' | 'H'>('L');
const textEncodeBusy = ref(false);
const textEncodeError = ref('');
const textEncodeResult = ref<TextEncodeResult | null>(null);

const textCharCount = computed(() => textInput.value.length);
const textByteCount = computed(
  () => new TextEncoder().encode(textInput.value).length,
);
const textOverLimit = computed(() => textByteCount.value > TEXT_MAX_BYTES);
const textQrSrcList = computed(() => {
  if (!textEncodeResult.value) return [];
  return textEncodeResult.value.qr_images.map((b) => `data:image/png;base64,${b}`);
});

async function startTextEncode() {
  textEncodeError.value = '';
  if (!textInput.value.trim()) {
    textEncodeError.value = '请输入文本内容';
    return;
  }
  if (textOverLimit.value) {
    textEncodeError.value = '文本超过 50KB，请使用文件传输';
    return;
  }
  textEncodeBusy.value = true;
  textEncodeResult.value = null;
  try {
    const res = (await endpoints.qrEncodeText(
      textInput.value,
      textErrorLevel.value,
    )) as TextEncodeResult;
    textEncodeResult.value = res;
  } catch (e) {
    textEncodeError.value = errMsg(e);
  } finally {
    textEncodeBusy.value = false;
  }
}

function downloadAllTextQr() {
  if (!textEncodeResult.value) return;
  textEncodeResult.value.qr_images.forEach((b, i) => {
    const a = document.createElement('a');
    a.href = `data:image/png;base64,${b}`;
    a.download = `qr-text-${i + 1}.png`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
  });
}

// 文本解码
const textDecodeDropping = ref(false);
const textDecodeFile = ref<File | null>(null);
const textDecodeBusy = ref(false);
const textDecodeError = ref('');
const textDecodeResult = ref<TextDecodeResult | null>(null);
const textDecodedText = ref('');

function onTextDecodeDrop(ev: DragEvent) {
  ev.preventDefault();
  textDecodeDropping.value = false;
  const f = ev.dataTransfer?.files?.[0];
  if (f) setTextDecodeFile(f);
}
function onTextDecodeChange(ev: Event) {
  const input = ev.target as HTMLInputElement;
  const f = input.files?.[0];
  if (f) setTextDecodeFile(f);
}
function setTextDecodeFile(f: File) {
  textDecodeFile.value = f;
  textDecodeError.value = '';
  textDecodeResult.value = null;
  textDecodedText.value = '';
}

async function startTextDecode() {
  textDecodeError.value = '';
  if (!textDecodeFile.value) {
    textDecodeError.value = '请先上传 QR 图片';
    return;
  }
  textDecodeBusy.value = true;
  textDecodeResult.value = null;
  textDecodedText.value = '';
  try {
    const b64 = await fileToBase64(textDecodeFile.value);
    const res = (await endpoints.qrDecodeText(b64)) as TextDecodeResult;
    textDecodeResult.value = res;
    textDecodedText.value = res.text ?? '';
  } catch (e) {
    textDecodeError.value = errMsg(e);
  } finally {
    textDecodeBusy.value = false;
  }
}

/** 复制解码文本（剪贴板工具带回退；失败静默——同旧行为）。 */
async function copyDecodedText() {
  await copyText(textDecodedText.value);
}

// =============================================================================
// 工具
// =============================================================================
function errMsg(e: unknown): string {
  if (e instanceof ApiError) return e.message;
  if (e instanceof Error) return e.message;
  return String(e);
}
function humanBytes(n?: number): string {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${u[i]}`;
}
function fileToBase64(f: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const s = r.result as string;
      // 去掉 data:...;base64, 前缀
      const idx = s.indexOf(',');
      resolve(idx >= 0 ? s.slice(idx + 1) : s);
    };
    r.onerror = () => reject(r.error);
    r.readAsDataURL(f);
  });
}

onMounted(loadStats);
onBeforeUnmount(() => {
  stopPollEncode();
  stopPollDecode();
});
</script>

<template>
  <div class="qr-page">
    <div class="page-head">
      <div>
        <div class="page-title">{{ t('qr.title') }}</div>
        <div class="page-sub muted small">{{ t('qr.subtitle') }}</div>
      </div>
      <div class="head-stats small muted">
        <button
          v-if="!isStandalone"
          class="btn btn-small btn-ext"
          type="button"
          :title="t('qr.openStandalone')"
          :aria-label="t('qr.openStandalone')"
          @click="openStandalone"
        >↗</button>
        <span>编码 {{ stats.encode_total ?? 0 }}（✓{{ stats.encode_completed ?? 0 }} / ✗{{ stats.encode_failed ?? 0 }}）</span>
        <span>解码 {{ stats.decode_total ?? 0 }}（✓{{ stats.decode_completed ?? 0 }} / ✗{{ stats.decode_failed ?? 0 }}）</span>
      </div>
    </div>

    <div class="tabs">
      <button
        v-for="t in tabs"
        :key="t.key"
        type="button"
        class="tab"
        :class="{ active: activeTab === t.key }"
        @click="activeTab = t.key"
      >
        {{ t.label }}
      </button>
    </div>

    <!-- ===================== Tab: 编码 ===================== -->
    <div v-if="activeTab === 'encode'" class="tab-panel">
      <div class="card encode-form-card">
        <div class="panel-head">
          <span class="panel-title">选择文件并生成 QR 视频</span>
          <button class="btn link small" type="button" @click="openFilePicker">
            从文件管理器选…
          </button>
        </div>
        <div class="field">
          <label>源文件路径（绝对路径）</label>
          <input
            v-model="encodeForm.file_path"
            class="text-input"
            type="text"
            placeholder="/tank/media/video/test.mp4"
          />
        </div>
        <div class="field-row">
          <div class="field">
            <label>帧率 FPS（每秒 QR 帧数）</label>
            <input v-model.number="encodeForm.fps" class="text-input" type="number" min="1" max="30" />
          </div>
          <div class="field">
            <label>分块大小（Base64 字符数，建议 ≤ 2048）</label>
            <input v-model.number="encodeForm.chunk_size" class="text-input" type="number" min="64" />
          </div>
        </div>
        <div class="form-actions">
          <button class="btn btn-primary" type="button" :disabled="encodeBusy" @click="startEncode">
            {{ encodeBusy ? '提交中…' : '生成视频' }}
          </button>
        </div>
        <div v-if="encodeError" class="task-error">{{ encodeError }}</div>
      </div>

      <!-- 编码任务状态 -->
      <div v-if="encodeTask" class="card task-card">
        <div class="task-detail-head">
          <span class="panel-title">编码任务 {{ encodeTask.id }}</span>
          <span class="pill" :class="encodeStatusClass">{{ encodeStatusText }}</span>
        </div>
        <div class="kv-grid">
          <div><span class="muted small">源文件</span><br />{{ encodeTask.file_path }}</div>
          <div><span class="muted small">文件大小</span><br />{{ encodeSizeHuman }}</div>
          <div><span class="muted small">帧率 / 分块</span><br />{{ encodeTask.fps }} fps / {{ encodeTask.chunk_size }}</div>
          <div><span class="muted small">总帧数</span><br />{{ encodeTask.total_frames ?? 0 }}</div>
        </div>
        <div v-if="encodeTask.error" class="task-error">{{ encodeTask.error }}</div>

        <!-- 视频播放 + 下载 -->
        <div v-if="encodeTask.status === 'completed' && encodeTask.video_url" class="video-box">
          <video
            :src="encodeTask.video_url"
            controls
            autoplay
            loop
            class="qr-video"
          ></video>
          <div class="video-actions">
            <a class="btn btn-primary" :href="encodeTask.video_url" :download="(encodeTask.id || 'qr') + '.mp4'">下载视频</a>
            <span v-if="encodeVideoMeta" class="muted small">{{ humanBytes(encodeVideoMeta.size) }} · {{ encodeVideoMeta.content_type }}</span>
          </div>
          <div class="muted small hint">
            提示：这是"跳动 QR 视频"——每一帧是一个二维码，按时序编码了文件的不同分块。
          </div>
        </div>
      </div>
    </div>

    <!-- ===================== Tab: 解码 ===================== -->
    <div v-else-if="activeTab === 'decode'" class="tab-panel">
      <div class="card decode-form-card">
        <div class="panel-head">
          <span class="panel-title">上传 QR 视频/图片</span>
        </div>
        <div
          class="dropzone"
          :class="{ active: decodeDropping }"
          @dragover.prevent="decodeDropping = true"
          @dragleave.prevent="decodeDropping = false"
          @drop="onDecodeDrop"
        >
          <input type="file" accept="video/*,image/*" @change="onDecodeChange" />
          <div class="dropzone-text">
            <strong>拖拽文件到此处</strong> 或点击选择
            <div class="muted small">支持 MP4 视频（跳动 QR 帧）或单张 QR 图片（PNG/JPG）</div>
          </div>
          <div v-if="decodeFile" class="chosen-file">
            已选：<strong>{{ decodeFile.name }}</strong>
            <span class="muted small">（{{ humanBytes(decodeFile.size) }}）</span>
          </div>
        </div>
        <div class="form-actions">
          <button class="btn btn-primary" type="button" :disabled="decodeBusy || !decodeFile" @click="startDecode">
            {{ decodeBusy ? '提交中…' : '解码' }}
          </button>
        </div>
        <div v-if="decodeError" class="task-error">{{ decodeError }}</div>
      </div>

      <!-- 解码任务状态 -->
      <div v-if="decodeTask" class="card task-card">
        <div class="task-detail-head">
          <span class="panel-title">解码任务 {{ decodeTask.id }}</span>
          <span class="pill" :class="decodeStatusClass">{{ decodeStatusText }}</span>
        </div>
        <div class="kv-grid">
          <div><span class="muted small">输入</span><br />{{ decodeTask.source }}</div>
          <div><span class="muted small">总帧数</span><br />{{ decodeTask.total_frames ?? 0 }}</div>
          <div><span class="muted small">已解码帧</span><br />{{ decodeTask.decoded_frames ?? 0 }}</div>
          <div>
            <span class="muted small">CRC 校验</span><br />
            <span :class="decodeTask.crc_ok === false ? 'crc-bad' : 'crc-ok'">
              {{ decodeTask.crc_ok === false ? '有不符' : '通过' }}
            </span>
          </div>
        </div>

        <!-- 进度条 -->
        <div v-if="decodeTask.status === 'decoding'" class="progress">
          <div class="progress-bar">
            <div
              class="progress-fill"
              :style="{ width: ((decodeTask.decoded_frames || 0) / Math.max(1, decodeTask.total_frames || 1) * 100) + '%' }"
            ></div>
          </div>
          <div class="muted small">
            {{ decodeTask.decoded_frames ?? 0 }} / {{ decodeTask.total_frames ?? 0 }} 帧
          </div>
        </div>

        <div v-if="decodeTask.error" class="task-error">{{ decodeTask.error }}</div>

        <!-- 下载解码文件 -->
        <div v-if="decodeTask.status === 'completed' && decodeTask.output_url" class="output-box">
          <div class="muted small">输出文件：{{ decodeTask.output_path }}</div>
          <a class="btn btn-primary" :href="decodeTask.output_url" :download="(decodeTask.id || 'decoded') + '.bin'">
            下载文件
          </a>
          <span v-if="decodeOutputMeta" class="muted small">{{ humanBytes(decodeOutputMeta.size) }}</span>
        </div>
      </div>
    </div>

    <!-- ===================== Tab: 文本 ===================== -->
    <div v-else-if="activeTab === 'text'" class="tab-panel">
      <!-- 上半区：编码（文本 → QR） -->
      <div class="card text-form-card">
        <div class="panel-head">
          <span class="panel-title">文本 → 二维码</span>
          <span class="muted small">单张 QR ≤ 2953 字节，超出自动 gzip 分块</span>
        </div>
        <div class="field">
          <label>粘贴文本（URL / 密码 / 配置…）</label>
          <textarea
            v-model="textInput"
            class="text-area"
            rows="6"
            placeholder="粘贴文本、URL、密码、配置..."
          ></textarea>
          <div class="counter-row">
            <span class="muted small">{{ textCharCount }} 字符 / {{ textByteCount }} 字节</span>
            <span v-if="textOverLimit" class="warn-text">超过 50KB，请使用文件传输</span>
          </div>
        </div>
        <div class="field-row">
          <div class="field">
            <label>纠错级别（L 容量最大 / H 容错最强）</label>
            <select v-model="textErrorLevel" class="text-input">
              <option value="L">L — 容量最大（约 2953 字节）</option>
              <option value="M">M</option>
              <option value="Q">Q</option>
              <option value="H">H — 容错最强</option>
            </select>
          </div>
        </div>
        <div class="form-actions">
          <button
            class="btn btn-primary"
            type="button"
            :disabled="textEncodeBusy || textOverLimit"
            @click="startTextEncode"
          >
            {{ textEncodeBusy ? '生成中…' : '生成二维码' }}
          </button>
        </div>
        <div v-if="textEncodeError" class="task-error">{{ textEncodeError }}</div>

        <!-- 编码结果 -->
        <div v-if="textEncodeResult" class="qr-result">
          <div class="muted small qr-meta">
            {{ textEncodeResult.qr_count }} 张 QR · 原文
            {{ humanBytes(textEncodeResult.original_size) }}
            <span v-if="textEncodeResult.compressed_size !== textEncodeResult.original_size">
              → 压缩后 {{ humanBytes(textEncodeResult.compressed_size) }}
            </span>
          </div>
          <!-- 单张 -->
          <div v-if="textEncodeResult.qr_count === 1" class="qr-single">
            <img :src="textQrSrcList[0]" alt="QR" class="qr-img" />
          </div>
          <!-- 多张网格 -->
          <div v-else class="qr-grid">
            <div v-for="(src, i) in textQrSrcList" :key="i" class="qr-grid-item">
              <img :src="src" :alt="`QR ${i + 1}`" class="qr-img-sm" />
              <span class="muted small">#{{ i + 1 }}</span>
            </div>
          </div>
          <div class="form-actions">
            <button class="btn btn-primary" type="button" @click="downloadAllTextQr">
              下载全部 PNG
            </button>
          </div>
        </div>
      </div>

      <!-- 下半区：解码（QR → 文本） -->
      <div class="card text-form-card">
        <div class="panel-head">
          <span class="panel-title">二维码 → 文本</span>
        </div>
        <div
          class="dropzone"
          :class="{ active: textDecodeDropping }"
          @dragover.prevent="textDecodeDropping = true"
          @dragleave.prevent="textDecodeDropping = false"
          @drop="onTextDecodeDrop"
        >
          <input type="file" accept="image/*" @change="onTextDecodeChange" />
          <div class="dropzone-text">
            <strong>拖拽 QR 图片到此处</strong> 或点击选择
            <div class="muted small">PNG / JPG 单张二维码</div>
          </div>
          <div v-if="textDecodeFile" class="chosen-file">
            已选：<strong>{{ textDecodeFile.name }}</strong>
            <span class="muted small">（{{ humanBytes(textDecodeFile.size) }}）</span>
          </div>
        </div>
        <div class="form-actions">
          <button
            class="btn btn-primary"
            type="button"
            :disabled="textDecodeBusy || !textDecodeFile"
            @click="startTextDecode"
          >
            {{ textDecodeBusy ? '解码中…' : '解码' }}
          </button>
        </div>
        <div v-if="textDecodeError" class="task-error">{{ textDecodeError }}</div>

        <!-- 解码结果 -->
        <div v-if="textDecodeResult" class="decode-text-out">
          <div v-if="textDecodeResult.partial" class="warn-text">
            多块文本之一（第 {{ (textDecodeResult.seq ?? 0) + 1 }} / {{ textDecodeResult.total }} 块）。
            需依次解码全部 {{ textDecodeResult.total }} 张 QR 后拼接还原；当前仅显示该块数据片段。
          </div>
          <textarea v-model="textDecodedText" class="text-area" rows="6"></textarea>
          <div class="counter-row">
            <span class="muted small">{{ textDecodedText.length }} 字符</span>
            <button class="btn link small" type="button" @click="copyDecodedText">复制</button>
          </div>
        </div>
      </div>
    </div>

    <!-- ===================== 文件选择器弹层 ===================== -->
    <div v-if="filePickerOpen" class="picker-mask" @click.self="filePickerOpen = false">
      <div class="picker-modal card">
        <div class="picker-head">
          <span class="panel-title">选择文件</span>
          <button class="btn link small" type="button" @click="pickerUp">上级 ⬆</button>
          <button class="folder-close-btn" type="button" @click="filePickerOpen = false">×</button>
        </div>
        <div class="picker-path muted small">{{ filePickerPath }}</div>
        <div v-if="filePickerError" class="task-error">{{ filePickerError }}</div>
        <div class="picker-list">
          <div v-if="filePickerBusy" class="muted small">加载中…</div>
          <button
            v-for="e in filePickerEntries"
            :key="e.name"
            type="button"
            class="picker-item"
            @click="pickerEnter(e)"
          >
            <span class="picker-icon">{{ e.is_dir ? '📁' : '📄' }}</span>
            <span class="picker-name">{{ e.name }}</span>
            <span v-if="!e.is_dir" class="muted small">{{ humanBytes(e.size) }}</span>
          </button>
          <div v-if="!filePickerBusy && filePickerEntries.length === 0" class="empty-card">
            （空目录）
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.qr-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-stats { display: flex; gap: 16px; align-items: center; }
.btn-ext { padding: 4px 8px; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.link { color: var(--accent, #E95420); cursor: pointer; text-decoration: underline; }

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

/* 卡片 */
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.encode-form-card, .decode-form-card { padding: 18px 20px; }
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; margin-bottom: 12px; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }

/* 表单 */
.field-row { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
.field { display: flex; flex-direction: column; gap: 4px; margin-bottom: 12px; }
.field label { font-size: 13px; font-weight: 500; }
.text-input {
  width: 100%; padding: 7px 10px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px; background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
}
.text-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.form-actions { display: flex; justify-content: flex-end; gap: 8px; align-items: center; }

/* 按钮 */
.btn {
  padding: 7px 16px; border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  font-weight: 500; cursor: pointer; border: 1px solid transparent; text-decoration: none;
  display: inline-flex; align-items: center; gap: 6px; transition: background 0.15s ease;
}
.btn:disabled { opacity: 0.55; cursor: not-allowed; }
.btn-primary { background: var(--accent, #E95420); color: #fff; }
.btn-primary:hover:not(:disabled) { background: #c7421a; }

/* 徽章 */
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #fde7d7; }
.pill-err { color: #b91c1c; background: #fee2e2; }

/* 任务卡片 */
.task-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; }
.task-detail-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.kv-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px; font-size: 13px; }
.task-error {
  color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 8px 12px; border-radius: var(--radius-sm, 8px); font-size: 13px; word-break: break-word;
}

/* 视频 */
.video-box { display: flex; flex-direction: column; gap: 8px; }
.qr-video {
  width: 100%; max-width: 480px; background: #000; border-radius: var(--radius-sm, 8px);
  aspect-ratio: 1 / 1; object-fit: contain;
}
.video-actions { display: flex; align-items: center; gap: 10px; }
.hint { line-height: 1.5; }

/* 拖拽区 */
.dropzone {
  border: 2px dashed var(--border, #d1d5db); border-radius: var(--radius-md, 12px);
  padding: 28px; text-align: center; display: flex; flex-direction: column; align-items: center; gap: 10px;
  transition: border-color 0.15s ease, background 0.15s ease; position: relative;
}
.dropzone.active { border-color: var(--accent, #E95420); background: rgba(233, 84, 32, 0.05); }
.dropzone input[type='file'] { position: absolute; inset: 0; opacity: 0; cursor: pointer; }
.dropzone-text { display: flex; flex-direction: column; gap: 4px; }
.chosen-file { font-size: 13px; }

/* 进度条 */
.progress { display: flex; flex-direction: column; gap: 4px; }
.progress-bar { width: 100%; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.progress-fill { height: 100%; background: var(--accent, #E95420); transition: width 0.3s ease; }

/* 输出 */
.output-box { display: flex; flex-direction: column; gap: 8px; }
.crc-ok { color: #15803d; font-weight: 600; }
.crc-bad { color: #b91c1c; font-weight: 600; }

/* 文件选择器弹层 */
.picker-mask {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); z-index: 1000;
  display: flex; align-items: center; justify-content: center;
}
.picker-modal { width: min(560px, 92vw); max-height: 70vh; display: flex; flex-direction: column; padding: 16px 18px; }
.picker-head { display: flex; align-items: center; gap: 8px; justify-content: space-between; }
.picker-head .panel-title { flex: 1; }
.folder-close-btn { background: transparent; border: none; font-size: 20px; cursor: pointer; color: var(--text-muted, #5E5C5F); }
.picker-path { margin: 8px 0; word-break: break-all; }
.picker-list { overflow: auto; display: flex; flex-direction: column; gap: 2px; }
.picker-item {
  display: flex; align-items: center; gap: 10px; padding: 8px 10px; background: transparent;
  border: none; border-radius: var(--radius-sm, 8px); cursor: pointer; font-family: inherit;
  font-size: 13px; color: var(--text, #2B2B2B); text-align: left;
}
.picker-item:hover { background: rgba(0, 0, 0, 0.04); }
.picker-icon { font-size: 16px; }
.picker-name { flex: 1; word-break: break-all; }
.empty-card { padding: 20px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 13px; }

/* 文本 Tab */
.text-form-card { padding: 18px 20px; display: flex; flex-direction: column; gap: 4px; }
.text-area {
  width: 100%; padding: 9px 11px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B); resize: vertical;
  min-height: 96px; line-height: 1.5;
}
.text-area:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.counter-row { display: flex; align-items: center; justify-content: space-between; gap: 10px; margin-top: 4px; }
.warn-text { color: #b91c1c; font-size: 12px; font-weight: 600; }
.qr-result { display: flex; flex-direction: column; gap: 10px; margin-top: 10px; }
.qr-meta { line-height: 1.5; }
.qr-single { display: flex; justify-content: center; }
.qr-img { max-width: 320px; width: 100%; background: #fff; border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }
.qr-grid {
  display: grid; grid-template-columns: repeat(auto-fill, minmax(150px, 1fr)); gap: 12px;
  justify-items: center;
}
.qr-grid-item { display: flex; flex-direction: column; align-items: center; gap: 4px; }
.qr-img-sm { width: 100%; max-width: 150px; background: #fff; border: 1px solid var(--border-soft, #EDEDED); border-radius: var(--radius-sm, 8px); }
.decode-text-out { display: flex; flex-direction: column; gap: 8px; margin-top: 10px; }
</style>
