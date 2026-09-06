<script setup lang="ts">
// BLE mesh 网状中继枢纽（IM 应用内 Tab）。
// 架构：开放 mesh（无需配对），手机即中继节点；OS 作 mesh 节点 + 互联网网关。
// API（os-api /api/v1/ble/*）：
//   GET  /api/v1/ble/status              → mesh Hub 状态
//   POST /api/v1/ble/start|stop          → 启停 GATT mesh relay（admin）
//   GET  /api/v1/ble/nodes               → mesh 节点（直接 + 间接）
//   DELETE /api/v1/ble/nodes/:id          → 移除节点（admin）
//   POST /api/v1/ble/discover            → 节点发现通告（内部）
//   GET  /api/v1/ble/routing             → 路由表（hop + via）
//   POST /api/v1/ble/messages            → 消息中继（flooding + 去重）
//   GET  /api/v1/ble/messages            → 消息历史
//   GET  /api/v1/ble/stats               → 统计
// mesh 连接 QR 直连 /api/v1/qr/encode-text（QR 传输 UI 已剥离为应用包
// apps/qrtransfer，端点常开——此处直接用 client 的 post 原语调用）。
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { endpoints, post } from '@/api/client';

/** 文本 → QR 图片（POST /api/v1/qr/encode-text，与 apps/qrtransfer 包内同款封装）。 */
function qrEncodeText(text: string, errorLevel = 'L'): Promise<unknown> {
  return post('/api/v1/qr/encode-text', { text, error_level: errorLevel });
}

interface BleMeshStatus {
  running: boolean;
  adapter: string;
  address: string;
  node_count: number;
  direct_connections: number;
  pid?: number;
}
interface BleMeshNode {
  id: string;
  name: string;
  address: string;
  direct: boolean;
  hop: number;
  via?: string;
  reachable: string[];
  online: boolean;
  last_seen?: string;
}
interface RoutingEntry {
  node_id: string;
  hop: number;
  via: string;
  direct: boolean;
}
interface BleMessage {
  id: string;
  msg_id: string;
  source_id: string;
  target_id?: string;
  content: string;
  msg_type: string;
  hop_count: number;
  path: string[];
  direction: string;
  created_at: string;
}

const status = ref<BleMeshStatus | null>(null);
const nodes = ref<BleMeshNode[]>([]);
const routing = ref<RoutingEntry[]>([]);
const messages = ref<BleMessage[]>([]);
const loading = ref(false);
const error = ref('');
const busy = ref(false);

// mesh 连接 QR
const qrData = ref('');
const qrImage = ref('');
const qrLoading = ref(false);
const qrError = ref('');

// 模拟发现/发消息
const discoverNodeId = ref('');
const discoverReachable = ref('');
const relayTarget = ref('');
const relayContent = ref('');

let timer: ReturnType<typeof setInterval> | null = null;

async function refreshAll() {
  loading.value = true;
  error.value = '';
  try {
    const [s, n, r, m] = await Promise.all([
      endpoints.bleStatus() as Promise<BleMeshStatus>,
      endpoints.bleNodes() as Promise<BleMeshNode[]>,
      endpoints.bleRouting() as Promise<{ entries: RoutingEntry[] }>,
      endpoints.bleMessages() as Promise<BleMessage[]>,
    ]);
    status.value = s;
    nodes.value = n ?? [];
    routing.value = r?.entries ?? [];
    messages.value = m ?? [];
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    loading.value = false;
  }
}

async function startHub() {
  busy.value = true;
  try {
    await endpoints.bleStart();
    await refreshAll();
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = false;
  }
}

async function stopHub() {
  busy.value = true;
  try {
    await endpoints.bleStop();
    await refreshAll();
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  } finally {
    busy.value = false;
  }
}

async function removeNode(id: string) {
  if (!confirm(`移除 mesh 节点 ${id}？`)) return;
  try {
    await endpoints.deleteBleNode(id);
    await refreshAll();
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

/** 生成 mesh 连接 QR（复用 QR 文本编码渲染）。开放 mesh，无 token。 */
async function genMeshQr() {
  qrLoading.value = true;
  qrError.value = '';
  qrImage.value = '';
  try {
    const addr = status.value?.address || 'EC:91:61:42:A4:AC';
    qrData.value = `os-ble-mesh://${addr}`;
    const res = (await qrEncodeText(qrData.value, 'M')) as {
      qr_images?: string[];
    };
    if (res?.qr_images?.length) {
      qrImage.value = `data:image/png;base64,${res.qr_images[0]}`;
    } else {
      qrError.value = '未生成 QR 图（后端 qrcode 库可能缺失）';
    }
  } catch (e: unknown) {
    qrError.value = e instanceof Error ? e.message : String(e);
  } finally {
    qrLoading.value = false;
  }
}

/** 模拟节点发现通告（测试/演示：手动注入一个节点 + 可达列表）。 */
async function announceDiscover() {
  if (!discoverNodeId.value.trim()) return;
  try {
    const reachable = discoverReachable.value
      .split(/[,\s]+/)
      .map((s) => s.trim())
      .filter(Boolean);
    await endpoints.bleDiscover({
      node_id: discoverNodeId.value.trim(),
      name: discoverNodeId.value.trim(),
      reachable,
      direct: true,
    });
    discoverNodeId.value = '';
    discoverReachable.value = '';
    await refreshAll();
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

/** 发送一条 mesh 消息（flooding 中继）。 */
async function relayMessage() {
  if (!relayContent.value.trim()) return;
  try {
    await endpoints.bleRelayMessage({
      msg_id: `m-${Date.now()}`,
      source_id: 'os',
      target_id: relayTarget.value.trim() || null,
      content: relayContent.value,
      hop_count: 7,
    });
    relayContent.value = '';
    relayTarget.value = '';
    await refreshAll();
  } catch (e: unknown) {
    error.value = e instanceof Error ? e.message : String(e);
  }
}

const directNodes = computed(() => nodes.value.filter((n) => n.direct));
const indirectNodes = computed(() => nodes.value.filter((n) => !n.direct));

function fmtTime(s?: string): string {
  if (!s) return '';
  try {
    return new Date(s).toLocaleString();
  } catch {
    return s;
  }
}

function dirLabel(d: string): string {
  if (d === 'outbound') return '发出';
  if (d === 'inbound') return '收到';
  return '中转';
}

onMounted(() => {
  refreshAll();
  timer = setInterval(refreshAll, 5000);
});
onBeforeUnmount(() => {
  if (timer) clearInterval(timer);
});
</script>

<template>
  <div class="ble-page">
    <header class="ble-head">
      <h2>🔗 BLE mesh 网状中继</h2>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">⟳ 刷新</button>
        <button
          v-if="!status?.running"
          class="btn btn-small btn-primary"
          :disabled="busy"
          @click="startHub"
        >
          ▶ 启动 mesh
        </button>
        <button v-else class="btn btn-small btn-danger" :disabled="busy" @click="stopHub">
          ■ 停止 mesh
        </button>
      </div>
    </header>

    <div v-if="error" class="error-box">{{ error }}</div>

    <!-- 状态卡 -->
    <section class="card status-card">
      <div class="status-row">
        <span class="status-dot" :class="status?.running ? 'on' : 'off'"></span>
        <span class="status-text">{{ status?.running ? '运行中' : '已停止' }}</span>
      </div>
      <div class="status-grid">
        <div><label>适配器</label><span class="mono">{{ status?.adapter || 'hci0' }}</span></div>
        <div><label>BD 地址</label><span class="mono">{{ status?.address || '—' }}</span></div>
        <div><label>已知节点</label><span>{{ status?.node_count ?? 0 }}</span></div>
        <div><label>直连数</label><span>{{ status?.direct_connections ?? 0 }}</span></div>
        <div v-if="status?.pid"><label>PID</label><span class="mono">{{ status.pid }}</span></div>
      </div>
      <p class="hint">
        开放 mesh（无需配对）。手机扫描下方 QR 连入 mesh，每部手机自动成为中继节点；
        A↔B↔C 多跳可达，消息经 flooding + hop_count 去重转发。OS 兼作互联网网关。
      </p>
    </section>

    <div class="ble-cols">
      <!-- 左：mesh 连接 QR + 节点发现 -->
      <section class="card">
        <h3>连接 / 发现</h3>
        <div class="qr-block">
          <p class="hint">手机扫描连入 mesh（开放，无 token）：</p>
          <button class="btn btn-small" :disabled="qrLoading" @click="genMeshQr">
            {{ qrLoading ? '生成中…' : '生成连接 QR' }}
          </button>
          <div v-if="qrImage" class="qr-img-wrap">
            <img :src="qrImage" alt="mesh QR" class="qr-img" />
            <p class="mono qr-data">{{ qrData }}</p>
          </div>
          <p v-if="qrError" class="error-box">{{ qrError }}</p>
        </div>

        <div class="discover-block">
          <h4>注入节点（演示）</h4>
          <p class="hint">模拟手机上报发现通告（node_id + 可直达节点）：</p>
          <input v-model="discoverNodeId" class="input" placeholder="节点 id（如 B）" />
          <input v-model="discoverReachable" class="input" placeholder="可直达节点（逗号分隔，如 C, D）" />
          <button class="btn btn-small btn-primary" @click="announceDiscover">通告发现</button>
        </div>
      </section>

      <!-- 右：mesh 节点列表 -->
      <section class="card">
        <h3>mesh 节点（{{ nodes.length }}）</h3>
        <p v-if="loading && !nodes.length" class="muted">加载中…</p>
        <p v-else-if="!nodes.length" class="muted">暂无节点。启动 mesh 后手机连入即自动出现。</p>
        <div v-else class="node-list">
          <div class="node-group">
            <div class="group-title">直接连接（1 hop）· {{ directNodes.length }}</div>
            <div v-for="n in directNodes" :key="n.id" class="node-item direct">
              <span class="node-dot on"></span>
              <div class="node-main">
                <span class="node-name">{{ n.name || n.id }}</span>
                <span class="node-sub mono">{{ n.address }} · hop {{ n.hop }}</span>
                <span v-if="n.reachable.length" class="node-reach">
                  可达：{{ n.reachable.join(', ') }}
                </span>
              </div>
              <span class="node-seen">{{ fmtTime(n.last_seen) }}</span>
              <button class="btn btn-small btn-link" @click="removeNode(n.id)">移除</button>
            </div>
          </div>
          <div v-if="indirectNodes.length" class="node-group">
            <div class="group-title">间接可达（多跳）· {{ indirectNodes.length }}</div>
            <div v-for="n in indirectNodes" :key="n.id" class="node-item indirect">
              <span class="node-dot mid"></span>
              <div class="node-main">
                <span class="node-name">{{ n.name || n.id }}</span>
                <span class="node-sub mono">{{ n.address }} · hop {{ n.hop }}<template v-if="n.via"> · 经 {{ n.via }}</template></span>
              </div>
              <button class="btn btn-small btn-link" @click="removeNode(n.id)">移除</button>
            </div>
          </div>
        </div>
      </section>
    </div>

    <!-- 路由表 -->
    <section class="card">
      <h3>路由表</h3>
      <p v-if="!routing.length" class="muted">暂无路由。节点通告发现后自动推导（跨跳传播）。</p>
      <table v-else class="route-table">
        <thead>
          <tr><th>目标节点</th><th>跳数</th><th>经由（第一跳）</th><th>类型</th></tr>
        </thead>
        <tbody>
          <tr v-for="r in routing" :key="r.node_id">
            <td class="mono">{{ r.node_id }}</td>
            <td>{{ r.hop }}</td>
            <td class="mono">{{ r.via }}</td>
            <td>{{ r.direct ? '直接' : '间接' }}</td>
          </tr>
        </tbody>
      </table>
    </section>

    <!-- 消息中继 -->
    <section class="card">
      <h3>消息中继</h3>
      <div class="relay-form">
        <input v-model="relayTarget" class="input" placeholder="目标节点（留空 = 广播）" />
        <input v-model="relayContent" class="input flex-1" placeholder="消息内容" @keydown.enter="relayMessage" />
        <button class="btn btn-small btn-primary" @click="relayMessage">发送</button>
      </div>
      <p v-if="!messages.length" class="muted">暂无消息。</p>
      <ul v-else class="msg-list">
        <li v-for="m in messages" :key="m.id" class="msg-item">
          <span class="msg-dir" :class="m.direction">{{ dirLabel(m.direction) }}</span>
          <span class="msg-from mono">{{ m.source_id }}</span>
          <span class="msg-arrow">→</span>
          <span class="msg-to mono">{{ m.target_id || '广播' }}</span>
          <span class="msg-content">{{ m.content }}</span>
          <span class="msg-hop">hop {{ m.hop_count }}</span>
          <span class="msg-time">{{ fmtTime(m.created_at) }}</span>
        </li>
      </ul>
    </section>
  </div>
</template>

<style scoped>
.ble-page { display: flex; flex-direction: column; gap: 16px; padding: 20px 24px; flex: 1; overflow: auto; }
.ble-head { display: flex; justify-content: space-between; align-items: center; }
.ble-head h2 { margin: 0; font-size: 1.25rem; }
.head-actions { display: flex; gap: 8px; }
.error-box { background: rgba(237, 60, 60, 0.12); color: #c0392b; border: 1px solid rgba(237, 60, 60, 0.4); border-radius: 8px; padding: 8px 12px; font-size: 0.9rem; }
.card { background: var(--card-bg, var(--bg)); border: 1px solid var(--border-soft, #ededed); border-radius: 12px; padding: 16px; box-shadow: 0 1px 3px rgba(0,0,0,0.04); }
.card h3 { margin: 0 0 12px; font-size: 1.05rem; }
.hint { color: var(--text-muted, #888); font-size: 0.85rem; margin: 6px 0; line-height: 1.5; }
.muted { color: var(--text-muted, #888); font-size: 0.9rem; padding: 8px 0; }
.mono { font-family: var(--mono, 'Ubuntu Mono', ui-monospace, monospace); font-size: 0.85rem; }

.status-card .status-row { display: flex; align-items: center; gap: 8px; margin-bottom: 12px; }
.status-dot { width: 10px; height: 10px; border-radius: 50%; display: inline-block; }
.status-dot.on { background: #2ecc71; box-shadow: 0 0 6px #2ecc71; }
.status-dot.off { background: #999; }
.status-text { font-weight: 600; }
.status-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(120px, 1fr)); gap: 12px; }
.status-grid > div { display: flex; flex-direction: column; }
.status-grid label { font-size: 0.75rem; color: var(--text-muted, #888); text-transform: uppercase; letter-spacing: 0.5px; }
.status-grid span { font-size: 0.95rem; }

.ble-cols { display: grid; grid-template-columns: 1fr 1fr; gap: 16px; }
@media (max-width: 760px) { .ble-cols { grid-template-columns: 1fr; } }

.qr-block, .discover-block { display: flex; flex-direction: column; gap: 8px; margin-top: 8px; }
.discover-block { border-top: 1px solid var(--border-soft, #ededed); padding-top: 12px; margin-top: 12px; }
.discover-block h4 { margin: 0; font-size: 0.95rem; }
.qr-img-wrap { display: flex; flex-direction: column; align-items: center; gap: 6px; margin-top: 8px; }
.qr-img { max-width: 220px; width: 100%; background: #fff; border: 1px solid var(--border-soft, #ededed); border-radius: 8px; }
.qr-data { font-size: 0.75rem; word-break: break-all; color: var(--text-muted, #888); }
.input { padding: 6px 10px; border: 1px solid var(--border-soft, #ddd); border-radius: 6px; background: var(--input-bg, #fff); color: var(--text, #111); font-size: 0.9rem; }
.input:focus { outline: 2px solid var(--accent); border-color: var(--accent-border); }
.flex-1 { flex: 1; }

.node-list { display: flex; flex-direction: column; gap: 12px; }
.node-group { display: flex; flex-direction: column; gap: 4px; }
.group-title { font-size: 0.8rem; color: var(--text-muted, #888); font-weight: 600; text-transform: uppercase; letter-spacing: 0.5px; padding: 4px 0; }
.node-item { display: flex; align-items: center; gap: 10px; padding: 8px 10px; border-radius: 8px; background: var(--accent-bg, rgba(170,59,255,0.06)); }
.node-item.indirect { background: rgba(46, 204, 113, 0.08); }
.node-dot { width: 8px; height: 8px; border-radius: 50%; flex-shrink: 0; }
.node-dot.on { background: #2ecc71; }
.node-dot.mid { background: #f39c12; }
.node-main { display: flex; flex-direction: column; flex: 1; min-width: 0; }
.node-name { font-weight: 600; font-size: 0.95rem; }
.node-sub { font-size: 0.78rem; color: var(--text-muted, #888); }
.node-reach { font-size: 0.75rem; color: var(--accent); }
.node-seen { font-size: 0.72rem; color: var(--text-muted, #aaa); white-space: nowrap; }
.btn-link { background: none; border: none; color: #e74c3c; cursor: pointer; font-size: 0.8rem; text-decoration: underline; padding: 2px 4px; }

.route-table { width: 100%; border-collapse: collapse; font-size: 0.88rem; }
.route-table th, .route-table td { text-align: left; padding: 6px 10px; border-bottom: 1px solid var(--border-soft, #ededed); }
.route-table th { color: var(--text-muted, #888); font-weight: 600; font-size: 0.8rem; text-transform: uppercase; }

.relay-form { display: flex; gap: 8px; margin-bottom: 12px; }
.msg-list { list-style: none; padding: 0; margin: 0; display: flex; flex-direction: column; gap: 6px; }
.msg-item { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding: 6px 10px; border-radius: 6px; background: var(--code-bg, rgba(0,0,0,0.03)); font-size: 0.85rem; }
.msg-dir { font-size: 0.72rem; font-weight: 600; padding: 1px 6px; border-radius: 4px; text-transform: uppercase; }
.msg-dir.inbound { background: rgba(46,204,113,0.2); color: #1e8449; }
.msg-dir.outbound { background: var(--accent-bg); color: var(--accent); }
.msg-dir.relay { background: rgba(243,156,18,0.2); color: #b9770e; }
.msg-arrow { color: var(--text-muted, #aaa); }
.msg-content { flex: 1; min-width: 80px; }
.msg-hop { font-size: 0.72rem; color: var(--text-muted, #888); }
.msg-time { font-size: 0.72rem; color: var(--text-muted, #aaa); }
.btn-danger { background: #e74c3c; color: #fff; border: none; }
.btn-danger:hover { background: #c0392b; }
</style>
