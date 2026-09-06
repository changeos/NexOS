<script setup lang="ts">
// =============================================================================
// LivePanel.vue —— 直播面板（可嵌入组件，流媒体中心「直播」Tab 挂载）
//
// 两段式大厅（镜像 IM 三会话架构的本地/联邦分组展示）：
//   本地大厅  本节点房间（GET /api/v1/live/rooms → local；可开播）
//   联邦大厅  远端节点房间（→ federated；live_lobby 宣告合并，TTL 90s；
//             显示来源节点名/观看数/状态，点击观看走跨节点中继）
//
// 链路：主播 getUserMedia/getDisplayMedia + MediaRecorder(webm;codecs=vp8,opus,
// timeslice 1000ms) → WS /ws/live/:id/publish 二进制上行 → 服务端内存扇出
// → 观众 WS /ws/live/:id/view → MediaSource+SourceBuffer 顺序 append。
// 观看远端房间：WS 仍连本节点 view 端点（本节点经 live_relay_* 中继注入
// 影子房间），room_id 用联邦形态 id（`<节点短前缀>:<room_id>`）——无感知差异。
//
// 前身：views/LiveView.vue（独立「直播」桌面应用，已移除；本组件承接其
// 全部逻辑与 i18n live.* 命名空间）。房间状态纯内存（服务重启即清空），
// 所有计数为服务端真实值——本组件不伪造任何状态。
// =============================================================================
// 应用包（apps/streaming）：主前端内部模块依赖已解耦——
//   - @/api/client → 本包 api.ts（宿主桥 __NEXOS_HOST__.api 原语，live 函数与
//     类型随包迁入；/api/v1/live/* 端点后端常开，不随应用门控）
//   - @/utils/format → 本包 format.ts（原样迁入）
//   - i18n live.* 命名空间随包迁移（entry.ts addI18n 注入）
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import {
  liveCreateRoom,
  liveEndRoom,
  liveListRooms,
  liveWsUrl,
  type FederatedLiveRoom,
  type LiveRoom,
  type LiveRoomCreated,
} from './api';
import { formatBytes } from './format';

const { t } = useI18n();

function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

// =============================================================================
// 两段式大厅（GET /api/v1/live/rooms；5s 轮询，页面隐藏时暂停）
// =============================================================================
const localRooms = ref<LiveRoom[]>([]);
const fedRooms = ref<FederatedLiveRoom[]>([]);
const listError = ref('');
const refreshing = ref(false);
let pollTimer: ReturnType<typeof setInterval> | null = null;

async function loadRooms(): Promise<void> {
  refreshing.value = true;
  try {
    const lobby = await liveListRooms();
    localRooms.value = Array.isArray(lobby?.local) ? lobby.local : [];
    fedRooms.value = Array.isArray(lobby?.federated) ? lobby.federated : [];
    listError.value = '';
  } catch (e) {
    listError.value = friendlyError(e);
  } finally {
    refreshing.value = false;
  }
}

function startPolling(): void {
  pollTimer = setInterval(() => {
    if (document.hidden) return; // 后台标签不轮询
    void loadRooms();
  }, 5000);
}

function statusClass(s: string): string {
  return s === 'live' ? 'pill-green' : 'pill-muted';
}

function statusLabel(r: { status: string; publisher_online: boolean }): string {
  if (r.status === 'live') return r.publisher_online ? t('live.statusLive') : t('live.statusWaiting');
  return t('live.statusEnded');
}

// =============================================================================
// 主播面板（本地大厅专属）：创建房间 → 采集 → MediaRecorder → WS publish
// =============================================================================
const pubTitle = ref('');
const pubSource = ref<'screen' | 'camera'>('screen');
const publishing = ref(false);
const pubError = ref('');
const pubRoom = ref<LiveRoomCreated | null>(null);
/** 已上行字节数（前端本地计；服务端计数见房间列表统计）。 */
const sentBytes = ref(0);

let pubStream: MediaStream | null = null;
let pubRecorder: MediaRecorder | null = null;
let pubWs: WebSocket | null = null;

/** 当前主播房间的实时视图（从本地大厅取，全部为服务端真实计数）。 */
const pubStats = computed<LiveRoom | null>(() => {
  if (!pubRoom.value) return null;
  return localRooms.value.find((r) => r.id === pubRoom.value?.id) ?? null;
});

const canStart = computed(
  () => !publishing.value && pubTitle.value.trim() !== '' && !watching.value,
);

async function startPublish(): Promise<void> {
  if (!canStart.value) return;
  pubError.value = '';
  const title = pubTitle.value.trim();
  let room: LiveRoomCreated;
  try {
    // 1) 创建房间（admin；返回 publish token——仅此一次下发）
    room = await liveCreateRoom({ title, source_kind: pubSource.value });
  } catch (e) {
    pubError.value = t('live.errCreate') + friendlyError(e);
    return;
  }
  pubRoom.value = room;
  try {
    // 2) 采集：屏幕（getDisplayMedia）或摄像头（getUserMedia）
    pubStream =
      pubSource.value === 'screen'
        ? await navigator.mediaDevices.getDisplayMedia({
            video: { frameRate: 30 },
            audio: false,
          })
        : await navigator.mediaDevices.getUserMedia({
            video: { width: { ideal: 1280 }, height: { ideal: 720 } },
            audio: true,
          });
  } catch (e) {
    pubError.value = t('live.errMedia') + friendlyError(e);
    await cleanupRoom(room.id);
    pubRoom.value = null;
    return;
  }
  // 用户点浏览器自带「停止共享」→ 自动收尾
  pubStream.getVideoTracks()[0]?.addEventListener('ended', () => {
    void stopPublish();
  });

  // 3) WS 上行（token 精确匹配；服务端首个 chunk 自动缓存为 header）
  const ws = new WebSocket(liveWsUrl(room.id, 'publish', room.publish_token));
  ws.binaryType = 'arraybuffer';
  pubWs = ws;
  ws.onopen = () => {
    // 4) MediaRecorder：webm vp8+opus，1s 切片——chunk 即 WS 二进制帧
    const mime = 'video/webm;codecs=vp8,opus';
    const rec = new MediaRecorder(pubStream!, {
      mimeType: MediaRecorder.isTypeSupported(mime) ? mime : undefined,
      videoBitsPerSecond: 2_500_000,
    });
    rec.ondataavailable = (ev: BlobEvent) => {
      if (ev.data && ev.data.size > 0 && ws.readyState === WebSocket.OPEN) {
        ws.send(ev.data);
        sentBytes.value += ev.data.size;
      }
    };
    rec.onstop = () => {
      if (ws.readyState === WebSocket.OPEN) {
        ws.send(JSON.stringify({ kind: 'stop' }));
      }
      setTimeout(() => closeWs(), 150); // 让 stop 控制帧先落地
    };
    rec.start(1000);
    pubRecorder = rec;
    publishing.value = true;
    void loadRooms();
  };
  ws.onerror = () => {
    pubError.value = t('live.errWs');
  };
  ws.onclose = () => {
    if (publishing.value) void stopPublish(true);
  };
}

async function stopPublish(wsAlreadyClosed = false): Promise<void> {
  const roomId = pubRoom.value?.id;
  publishing.value = false;
  try {
    if (pubRecorder && pubRecorder.state !== 'inactive') pubRecorder.stop(); // onstop 发 stop 帧
    else if (!wsAlreadyClosed) closeWs();
  } catch {
    closeWs();
  }
  pubRecorder = null;
  stopStream();
  if (roomId) await cleanupRoom(roomId);
  pubRoom.value = null;
  void loadRooms();
}

function closeWs(): void {
  if (pubWs && pubWs.readyState <= WebSocket.OPEN) pubWs.close();
  pubWs = null;
}

function stopStream(): void {
  pubStream?.getTracks().forEach((tr) => tr.stop());
  pubStream = null;
}

async function cleanupRoom(id: string): Promise<void> {
  try {
    await liveEndRoom(id); // DELETE：结束直播（踢断全部连接，房间出表）
  } catch {
    /* 房间可能已被服务端回收（无主播且观众清零）——忽略 */
  }
}

// =============================================================================
// 观看区（本地/联邦房间同一路径）：WS view → header 重放 + 实时帧 → MSE
// =============================================================================
/** 可观看的房间（本地或联邦——两者都有 id/title/status，WS 端点同一）。 */
interface WatchableRoom {
  id: string;
  title: string;
  status: string;
}

const watching = ref(false);
const watchRoom = ref<WatchableRoom | null>(null);
const watchRemote = ref(false);
const watchState = ref<'connecting' | 'playing' | 'ended' | 'error'>('connecting');
const watchError = ref('');
const videoEl = ref<HTMLVideoElement | null>(null);

let viewWs: WebSocket | null = null;
let mediaSource: MediaSource | null = null;
let sourceBuffer: SourceBuffer | null = null;
let objectUrl = '';
let sbQueue: Uint8Array<ArrayBuffer>[] = [];
let sbBusy = false;

const MSE_MIME = 'video/webm; codecs="vp8, opus"';

function appendNext(): void {
  if (!sourceBuffer || sbBusy || sbQueue.length === 0) return;
  sbBusy = true;
  const chunk = sbQueue.shift()!;
  try {
    sourceBuffer.appendBuffer(chunk);
  } catch {
    sbBusy = false; // QuotaExceeded 等瞬态：丢本块，下一帧重试（保实时）
  }
}

async function startWatch(room: WatchableRoom, remote = false): Promise<void> {
  if (watching.value || publishing.value) return;
  watchRoom.value = room;
  watchRemote.value = remote;
  watchState.value = 'connecting';
  watchError.value = '';
  if (!window.MediaSource || !MediaSource.isTypeSupported(MSE_MIME)) {
    watchState.value = 'error';
    watchError.value = t('live.mimeUnsupported');
    return;
  }
  watching.value = true;
  sbQueue = [];
  sbBusy = false;

  const ms = new MediaSource();
  mediaSource = ms;
  objectUrl = URL.createObjectURL(ms);
  const video = videoEl.value;
  if (video) {
    video.src = objectUrl;
    void video.play().catch(() => undefined); // 自动播放被拦时由用户点播放
  }
  ms.addEventListener('sourceopen', () => {
    try {
      const sb = ms.addSourceBuffer(MSE_MIME);
      sb.mode = 'sequence'; // 顺序 append（服务端已保证 header → cluster 次序）
      sb.addEventListener('updateend', () => {
        sbBusy = false;
        appendNext();
      });
      sourceBuffer = sb;
    } catch (e) {
      watchState.value = 'error';
      watchError.value = friendlyError(e);
    }
  });

  // 联邦房间同样连本节点 view 端点——服务端已中继注入影子房间
  const ws = new WebSocket(liveWsUrl(room.id, 'view'));
  ws.binaryType = 'arraybuffer';
  viewWs = ws;
  ws.onmessage = (ev) => {
    if (typeof ev.data === 'string') {
      // 控制帧：{"kind":"ended"}（主播断开 / DELETE / 中继结束）或 {"kind":"error"}
      let kind = '';
      try {
        kind = (JSON.parse(ev.data) as { kind?: string }).kind ?? '';
      } catch {
        kind = '';
      }
      if (kind === 'ended') {
        watchState.value = 'ended';
        leaveWatch();
      } else if (kind === 'error') {
        watchError.value = ev.data;
      }
      return;
    }
    if (watchState.value !== 'playing') watchState.value = 'playing';
    sbQueue.push(new Uint8Array(ev.data as ArrayBuffer));
    appendNext();
  };
  ws.onerror = () => {
    if (watching.value) {
      watchState.value = 'error';
      watchError.value = t('live.errWs');
    }
  };
  ws.onclose = () => {
    if (watching.value && watchState.value !== 'ended') {
      watchState.value = 'ended';
    }
  };
}

function leaveWatch(): void {
  watching.value = false;
  if (viewWs && viewWs.readyState <= WebSocket.OPEN) viewWs.close();
  viewWs = null;
  sbQueue = [];
  sourceBuffer = null;
  if (mediaSource && mediaSource.readyState === 'open') {
    try {
      mediaSource.endOfStream();
    } catch {
      /* 已结束/异常态忽略 */
    }
  }
  mediaSource = null;
  if (objectUrl) URL.revokeObjectURL(objectUrl);
  objectUrl = '';
  const video = videoEl.value;
  if (video) {
    video.removeAttribute('src');
    video.load();
  }
  watchRoom.value = null;
}

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  void loadRooms();
  startPolling();
});

onBeforeUnmount(() => {
  if (pollTimer) clearInterval(pollTimer);
  if (publishing.value) void stopPublish();
  leaveWatch();
});
</script>

<template>
  <div class="live-panel">
    <div class="panel-bar">
      <span class="muted small">{{ t('live.subtitle') }}</span>
      <button class="btn btn-small" :disabled="refreshing" @click="loadRooms()">
        <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
        {{ t('live.refresh') }}
      </button>
    </div>

    <!-- ================== 本地大厅（本节点房间，可开播） ================== -->
    <section class="panel">
      <div class="panel-head">
        <span class="panel-title">{{ t('live.localLobby') }}</span>
        <span class="pill pill-blue">{{ t('live.localTag') }}</span>
      </div>
      <div v-if="listError" class="error-box">{{ listError }}</div>
      <div class="card card-table">
        <table class="table">
          <thead>
            <tr>
              <th>{{ t('live.colTitle') }}</th>
              <th>{{ t('live.colStatus') }}</th>
              <th>{{ t('live.colViewers') }}</th>
              <th>{{ t('live.colSource') }}</th>
              <th>{{ t('live.colBytes') }}</th>
              <th class="col-actions">{{ t('live.colActions') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-if="localRooms.length === 0">
              <td colspan="6" class="empty-cell muted">{{ t('live.empty') }}</td>
            </tr>
            <tr v-for="r in localRooms" :key="r.id">
              <td>
                <div class="room-title">{{ r.title }}</div>
                <div class="muted small mono">{{ r.id }} · {{ r.publisher_identity }}</div>
              </td>
              <td>
                <span class="pill" :class="statusClass(r.status)">{{ statusLabel(r) }}</span>
              </td>
              <td class="mono">{{ r.viewer_count }}</td>
              <td>{{ r.source_kind === 'screen' ? t('live.screen') : t('live.camera') }}</td>
              <td class="mono small">
                ↑{{ formatBytes(r.bytes_in) }} ↓{{ formatBytes(r.bytes_out) }}
                <template v-if="r.dropped_frames > 0">
                  · {{ t('live.dropped') }} {{ r.dropped_frames }}</template
                >
              </td>
              <td class="col-actions">
                <button
                  class="btn btn-small btn-primary"
                  :disabled="watching || publishing || r.status !== 'live'"
                  @click="startWatch(r)"
                >
                  {{ t('live.watch') }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>

      <!-- 主播面板 -->
      <div class="panel-head sub-head">
        <span class="panel-title">{{ t('live.pubPanel') }}</span>
        <span v-if="publishing" class="pill pill-red">{{ t('live.onAir') }}</span>
      </div>
      <div class="card pub-card">
        <div class="pub-form">
          <label class="field">
            <span class="field-label">{{ t('live.roomTitle') }}</span>
            <input
              v-model="pubTitle"
              type="text"
              class="input"
              :placeholder="t('live.titlePh')"
              :disabled="publishing"
              maxlength="80"
            />
          </label>
          <label class="field">
            <span class="field-label">{{ t('live.source') }}</span>
            <select v-model="pubSource" class="input" :disabled="publishing">
              <option value="screen">{{ t('live.screen') }}</option>
              <option value="camera">{{ t('live.camera') }}</option>
            </select>
          </label>
          <button v-if="!publishing" class="btn btn-primary" :disabled="!canStart" @click="startPublish()">
            {{ t('live.start') }}
          </button>
          <button v-else class="btn btn-danger" @click="stopPublish()">{{ t('live.stop') }}</button>
        </div>
        <p v-if="pubError" class="form-msg is-err">{{ pubError }}</p>

        <!-- 实时统计（服务端真实计数，随 5s 轮询刷新） -->
        <div v-if="pubStats" class="pub-stats">
          <div class="stat-item">
            <div class="stat-label">{{ t('live.statViewers') }}</div>
            <div class="stat-value">{{ pubStats.viewer_count }}</div>
          </div>
          <div class="stat-item">
            <div class="stat-label">{{ t('live.statBytesIn') }}</div>
            <div class="stat-value">{{ formatBytes(pubStats.bytes_in) }}</div>
          </div>
          <div class="stat-item">
            <div class="stat-label">{{ t('live.statBytesOut') }}</div>
            <div class="stat-value">{{ formatBytes(pubStats.bytes_out) }}</div>
          </div>
          <div class="stat-item">
            <div class="stat-label">{{ t('live.statDropped') }}</div>
            <div class="stat-value">{{ pubStats.dropped_frames }}</div>
          </div>
          <div class="stat-item">
            <div class="stat-label">{{ t('live.statSent') }}</div>
            <div class="stat-value">{{ formatBytes(sentBytes) }}</div>
          </div>
        </div>
      </div>
    </section>

    <!-- ================== 联邦大厅（远端节点房间，中继观看） ================== -->
    <section class="panel">
      <div class="panel-head">
        <span class="panel-title">{{ t('live.fedLobby') }}</span>
        <span class="pill pill-purple">🌐 {{ t('live.fedTag') }}</span>
      </div>
      <p class="muted small fed-hint">{{ t('live.fedHint') }}</p>
      <div class="card card-table">
        <table class="table">
          <thead>
            <tr>
              <th>{{ t('live.colTitle') }}</th>
              <th>{{ t('live.colNode') }}</th>
              <th>{{ t('live.colStatus') }}</th>
              <th>{{ t('live.colViewers') }}</th>
              <th>{{ t('live.colSource') }}</th>
              <th>{{ t('live.colUpdated') }}</th>
              <th class="col-actions">{{ t('live.colActions') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-if="fedRooms.length === 0">
              <td colspan="7" class="empty-cell muted">{{ t('live.fedEmpty') }}</td>
            </tr>
            <tr v-for="r in fedRooms" :key="r.id">
              <td>
                <div class="room-title">{{ r.title }}</div>
                <div class="muted small mono">{{ r.id }}</div>
              </td>
              <td>
                <span class="pill pill-purple">🌐 {{ r.node_name }}</span>
              </td>
              <td>
                <span class="pill" :class="statusClass(r.status)">{{ statusLabel(r) }}</span>
              </td>
              <td class="mono">{{ r.viewer_count }}</td>
              <td>{{ r.source_kind === 'screen' ? t('live.screen') : t('live.camera') }}</td>
              <td class="mono small">{{ r.updated_at }}</td>
              <td class="col-actions">
                <button
                  class="btn btn-small btn-primary"
                  :disabled="watching || publishing || r.status !== 'live'"
                  @click="startWatch(r, true)"
                >
                  {{ t('live.watch') }}
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- ================== 观看区（MSE；本地/联邦同一路径） ================== -->
    <section v-if="watching || watchState === 'error'" class="panel">
      <div class="panel-head">
        <span class="panel-title">
          {{ t('live.watchPanel') }}<template v-if="watchRoom"> · {{ watchRoom.title }}</template>
          <span v-if="watchRemote" class="pill pill-purple fed-watch-tag">🌐 {{ t('live.fedTag') }}</span>
        </span>
        <button class="btn btn-small" :disabled="!watching" @click="leaveWatch()">
          {{ t('live.leave') }}
        </button>
      </div>
      <div class="card watch-card">
        <video ref="videoEl" class="watch-video" controls autoplay muted playsinline />
        <div v-if="watchState === 'connecting'" class="watch-overlay muted">
          {{ t('live.waiting') }}
        </div>
        <div v-else-if="watchState === 'ended'" class="watch-overlay">
          {{ t('live.ended') }}
        </div>
        <div v-else-if="watchState === 'error'" class="watch-overlay is-err">
          {{ watchError || t('live.errWs') }}
        </div>
      </div>
      <p v-if="watchError && watchState !== 'error'" class="form-msg is-err">{{ watchError }}</p>
    </section>
  </div>
</template>

<style scoped>
.live-panel {
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.panel-bar { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.mono { font-family: var(--mono); }

.panel { display: flex; flex-direction: column; gap: 12px; }
.panel-head { display: flex; justify-content: space-between; align-items: center; gap: 10px; }
.panel-head.sub-head { padding-top: 6px; border-top: 1px dashed var(--border-soft, #EDEDED); }
.panel-title { font-size: 15px; font-weight: 600; }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.card-table { padding: 0; overflow: hidden; }

/* 房间表 */
.table { width: 100%; border-collapse: collapse; font-size: 13px; }
.table th {
  text-align: left; padding: 10px 14px; font-size: 12px; text-transform: uppercase;
  letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.table td { padding: 10px 14px; border-bottom: 1px solid var(--border-soft, #EDEDED); vertical-align: middle; }
.table tr:last-child td { border-bottom: none; }
.col-actions { text-align: right; white-space: nowrap; }
.empty-cell { text-align: center; padding: 26px 0 !important; }
.room-title { font-weight: 600; }

.pill {
  display: inline-block; padding: 2px 10px; border-radius: 999px;
  font-size: 12px; font-weight: 600; white-space: nowrap;
}
.pill-green { background: rgba(14, 132, 32, 0.12); color: #0E8420; }
.pill-blue { background: rgba(199, 66, 26, 0.12); color: #C7421A; }
.pill-purple { background: rgba(124, 58, 237, 0.12); color: #7c3aed; }
.pill-red { background: rgba(224, 27, 36, 0.12); color: #C01C28; }
.pill-muted { background: rgba(94, 92, 95, 0.12); color: var(--text-muted, #5E5C5F); }

.fed-hint { margin: -6px 0 0; }
.fed-watch-tag { margin-left: 8px; }

/* 主播面板 */
.pub-card { padding: 16px 18px; display: flex; flex-direction: column; gap: 14px; }
.pub-form { display: flex; gap: 12px; align-items: flex-end; flex-wrap: wrap; }
.field { display: flex; flex-direction: column; gap: 6px; min-width: 220px; flex: 1; }
.field-label { font-size: 12px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.input {
  padding: 8px 12px; border: 1px solid var(--border, #D9D9D9); border-radius: 8px;
  font-size: 14px; font-family: inherit; background: var(--bg-card, #fff); color: inherit;
}
.input:focus { outline: 2px solid var(--accent, #E95420); outline-offset: -1px; }

.pub-stats { display: grid; grid-template-columns: repeat(auto-fill, minmax(140px, 1fr)); gap: 12px; }
.stat-item { display: flex; flex-direction: column; gap: 2px; padding: 10px 12px; background: rgba(0, 0, 0, 0.03); border-radius: 10px; }
.stat-label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 20px; font-weight: 700; color: var(--text, #2B2B2B); }

/* 观看区 */
.watch-card { position: relative; padding: 0; overflow: hidden; background: #000; }
.watch-video { display: block; width: 100%; max-height: 62vh; background: #000; }
.watch-overlay {
  position: absolute; inset: 0; display: flex; align-items: center; justify-content: center;
  font-size: 15px; color: #eee; background: rgba(0, 0, 0, 0.55); pointer-events: none;
}
.watch-overlay.is-err { color: #ff9c9c; }

.form-msg { padding: 8px 12px; border-radius: 8px; font-size: 13px; }
.form-msg.is-err { background: rgba(192, 28, 40, 0.1); color: #C01C28; }

.error-box {
  padding: 10px 14px; border-radius: 8px; font-size: 13px;
  background: rgba(192, 28, 40, 0.1); color: #C01C28;
}

.btn {
  padding: 6px 14px; border-radius: 8px;
  border: 1px solid var(--border, #D9D9D9); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
}
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-danger { color: #C01C28; border-color: rgba(192, 28, 40, 0.35); background: #fff5f5; }

.spin { display: inline-block; }
.spinning { animation: live-spin 1s linear infinite; }
@keyframes live-spin { to { transform: rotate(360deg); } }
</style>
